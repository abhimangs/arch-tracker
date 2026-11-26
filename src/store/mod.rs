//! SQLite storage: subjects, spans (one table for spans and signals) and a rollup cache.

pub mod ledger;

use crate::model::{Kind, Span, Subject};
use anyhow::{Context, Result};
use ledger::Ledger;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::Path;

const SCHEMA_VERSION: i64 = 1;

pub struct Store {
    pub conn: Connection,
    subjects: HashMap<Subject, i64>,
    /// Subjects that already have an id but are written by the next flush, in the same
    /// transaction as the spans that use them (the daemon is the only writer of subjects).
    new_subjects: Vec<(i64, Subject)>,
    next_id: i64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&conn)?;
        let max: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM subjects", [], |r| {
            r.get(0)
        })?;
        Ok(Store {
            conn,
            subjects: HashMap::new(),
            new_subjects: vec![],
            next_id: max + 1,
        })
    }

    /// Returns the id of a subject. A subject seen for the first time gets the next free id
    /// right away and is written by the next flush (no write on the hot path).
    pub fn subject_id(&mut self, s: &Subject) -> Result<i64> {
        if let Some(id) = self.subjects.get(s) {
            return Ok(*id);
        }
        let found: Option<i64> = self
            .conn
            .prepare_cached(
                "SELECT id FROM subjects WHERE app=?1 AND app_name=?2 AND title=?3 AND host=?4
                 AND private=?5 AND program=?6 AND project=?7",
            )?
            .query_row(
                params![
                    s.app, s.app_name, s.title, s.host, s.private, s.program, s.project
                ],
                |r| r.get(0),
            )
            .optional()?;
        let id = match found {
            Some(id) => id,
            None => {
                let id = self.next_id;
                self.next_id += 1;
                self.new_subjects.push((id, s.clone()));
                id
            }
        };
        if self.subjects.len() > 20_000 {
            self.subjects.clear();
            // Unwritten subjects must stay findable: they are not in the database yet.
            for (id, s) in &self.new_subjects {
                self.subjects.insert(s.clone(), *id);
            }
        }
        self.subjects.insert(s.clone(), id);
        Ok(id)
    }

    /// Deletes intervals that ended before `before_ms`, then the titles and other details only
    /// they used, and cached day summaries. Subjects waiting for the next flush are kept, and so
    /// are the subjects in `keep` (those the ledger's open spans still refer to).
    pub fn prune(&mut self, before_ms: i64, keep: &HashSet<i64>) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM spans WHERE end_ms < ?1", params![before_ms])?;
        tx.execute(
            "DELETE FROM claude_usage WHERE at_ms < ?1",
            params![before_ms],
        )?;
        tx.execute(
            "DELETE FROM codex_usage WHERE at_ms < ?1",
            params![before_ms],
        )?;
        tx.execute(
            "DELETE FROM codex_turns WHERE end_ms < ?1",
            params![before_ms],
        )?;
        tx.execute(
            "DELETE FROM codex_threads WHERE updated_ms < ?1",
            params![before_ms],
        )?;
        if n > 0 {
            let orphans: Vec<i64> = {
                let mut st = tx.prepare(
                    "SELECT id FROM subjects WHERE id NOT IN (SELECT DISTINCT subject_id FROM spans)",
                )?;
                st.query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            {
                let mut del = tx.prepare("DELETE FROM subjects WHERE id=?1")?;
                for id in orphans.iter().filter(|id| !keep.contains(id)) {
                    del.execute(params![id])?;
                }
            }
            tx.execute("DELETE FROM rollups", [])?;
        }
        tx.commit()?;
        if n > 0 {
            // Forget ids whose rows are gone; pending ones are written at the next flush.
            let pending: std::collections::HashSet<i64> =
                self.new_subjects.iter().map(|(id, _)| *id).collect();
            let live: std::collections::HashSet<i64> = {
                let mut st = self.conn.prepare("SELECT id FROM subjects")?;
                st.query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            self.subjects
                .retain(|_, id| live.contains(id) || pending.contains(id));
        }
        Ok(n)
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .ok()
            .flatten()
    }

    /// Sets (Some) or removes (None) a meta value.
    pub fn set_meta(&self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(v) => self.conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
                params![key, v],
            )?,
            None => self
                .conn
                .execute("DELETE FROM meta WHERE key=?1", params![key])?,
        };
        Ok(())
    }

    /// Writes new subjects and the ledger's spans in one transaction.
    pub fn flush(&mut self, ledger: &mut Ledger, now: i64) -> Result<()> {
        let new = &self.new_subjects;
        ledger.flush_with(&mut self.conn, now, |tx| {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO subjects(id, app, app_name, title, host, private, program,
                 project) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (id, s) in new {
                let n = ins.execute(params![
                    id, s.app, s.app_name, s.title, s.host, s.private, s.program, s.project
                ])?;
                if n == 0 {
                    tracing::warn!("subject {id} was already stored by someone else");
                }
            }
            Ok(())
        })?;
        self.new_subjects.clear();
        Ok(())
    }
}

/// A browser the extension connected from.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BrowserLink {
    pub app: String,
    pub name: String,
    pub first_ms: i64,
    pub last_ms: i64,
}

impl Store {
    /// Remembers that the extension is connected from this browser now.
    pub fn note_browser(&self, app: &str, name: &str, at: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO browsers(app, name, first_ms, last_ms) VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT(app) DO UPDATE SET name = excluded.name,
                    first_ms = MIN(first_ms, excluded.first_ms),
                    last_ms = MAX(last_ms, excluded.last_ms)",
            )?
            .execute(params![app, name, at])?;
        Ok(())
    }

    /// Remembers which browser runs an installed web app.
    pub fn note_web_app(&self, app: &str, browser: &str) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR REPLACE INTO web_apps(app, browser) VALUES (?1, ?2)")?
            .execute(params![app, browser])?;
        Ok(())
    }
}

/// A database written by an older version (opened read-only) can lack newer tables.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?
        .exists(params![name])?)
}

/// Every browser the extension has connected from.
pub fn browser_links(conn: &Connection) -> Result<Vec<BrowserLink>> {
    if !has_table(conn, "browsers")? {
        return Ok(vec![]);
    }
    let mut stmt =
        conn.prepare_cached("SELECT app, name, first_ms, last_ms FROM browsers ORDER BY app")?;
    let rows = stmt.query_map([], |r| {
        Ok(BrowserLink {
            app: r.get(0)?,
            name: r.get(1)?,
            first_ms: r.get(2)?,
            last_ms: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Installed web app -> the browser that runs it.
pub fn web_apps(conn: &Connection) -> Result<HashMap<String, String>> {
    if !has_table(conn, "web_apps")? {
        return Ok(HashMap::new());
    }
    let mut stmt = conn.prepare_cached("SELECT app, browser FROM web_apps")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn open_readonly(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS subjects(
            id INTEGER PRIMARY KEY,
            app TEXT NOT NULL, app_name TEXT NOT NULL, title TEXT NOT NULL, host TEXT NOT NULL,
            private INTEGER NOT NULL, program TEXT NOT NULL, project TEXT NOT NULL,
            UNIQUE(app, app_name, title, host, private, program, project));
         CREATE TABLE IF NOT EXISTS spans(
            id INTEGER PRIMARY KEY,
            kind INTEGER NOT NULL,
            subject_id INTEGER NOT NULL,
            start_ms INTEGER NOT NULL,
            end_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS spans_start ON spans(start_ms);
         CREATE TABLE IF NOT EXISTS rollups(
            day TEXT NOT NULL, settings_hash TEXT NOT NULL, json TEXT NOT NULL,
            PRIMARY KEY(day, settings_hash));
         -- Claude Code's token use per reply (counts only; id is a hash of the reply's ids).
         CREATE TABLE IF NOT EXISTS claude_usage(
            id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, model TEXT NOT NULL,
            project TEXT NOT NULL, input INTEGER NOT NULL, output INTEGER NOT NULL,
            cache_5m INTEGER NOT NULL, cache_1h INTEGER NOT NULL, cache_read INTEGER NOT NULL,
            web_search INTEGER NOT NULL, fast INTEGER NOT NULL, us_only INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS claude_usage_at ON claude_usage(at_ms);
         -- Tokens per model over every transcript line (replies repeat on several lines).
         CREATE TABLE IF NOT EXISTS claude_lines(model TEXT PRIMARY KEY, tokens INTEGER NOT NULL);
         -- How far each transcript has been read (key is a hash of its path).
         CREATE TABLE IF NOT EXISTS claude_files(
            key INTEGER PRIMARY KEY, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
            offset INTEGER NOT NULL);
         -- Browsers the extension connected from: first and last time.
         CREATE TABLE IF NOT EXISTS browsers(
            app TEXT PRIMARY KEY, name TEXT NOT NULL, first_ms INTEGER NOT NULL,
            last_ms INTEGER NOT NULL);
         -- Installed web apps (app windows of a browser) and the browser that runs them.
         CREATE TABLE IF NOT EXISTS web_apps(app TEXT PRIMARY KEY, browser TEXT NOT NULL);
         -- Codex's token use per response (counts only; id is a hash of the response id).
         CREATE TABLE IF NOT EXISTS codex_usage(
            id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, model TEXT NOT NULL,
            project TEXT NOT NULL, input INTEGER NOT NULL, cached INTEGER NOT NULL,
            cache_write INTEGER NOT NULL, output INTEGER NOT NULL, reasoning INTEGER NOT NULL,
            fast INTEGER NOT NULL, thread INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX IF NOT EXISTS codex_usage_at ON codex_usage(at_ms);
         -- How far each Codex log has been read, and the model, folder and tier in effect there.
         CREATE TABLE IF NOT EXISTS codex_files(
            key INTEGER PRIMARY KEY, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
            offset INTEGER NOT NULL, model TEXT NOT NULL, cwd TEXT NOT NULL, tier TEXT NOT NULL,
            mode INTEGER NOT NULL, last_total INTEGER NOT NULL, thread INTEGER NOT NULL DEFAULT 0);
         -- Codex's conversations as its own database lists them (key: a hash of their id): the
         -- token total, model and folder, and how long their turns ran in all. Kept when Codex
         -- later drops a conversation's log, so its use still counts.
         CREATE TABLE IF NOT EXISTS codex_threads(
            key INTEGER PRIMARY KEY, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
            model TEXT NOT NULL, project TEXT NOT NULL, tokens INTEGER NOT NULL,
            turn_ms INTEGER NOT NULL);
         -- When their turns ran (none from while tracking was paused).
         CREATE TABLE IF NOT EXISTS codex_turns(
            thread INTEGER NOT NULL, start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL,
            PRIMARY KEY(thread, start_ms));",
    )?;
    // Responses now remember their conversation: older databases gain the column and read the
    // logs again (each response still counts once) to fill it in.
    for table in ["codex_usage", "codex_files"] {
        if !has_column(conn, table, "thread")? {
            conn.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN thread INTEGER NOT NULL DEFAULT 0;"
            ))?;
            conn.execute("DELETE FROM codex_files", [])?;
        }
    }
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if v.is_none() {
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
    }
    Ok(())
}

/// Every span that starts inside [from, to). Spans never cross local midnight (the ledger splits
/// them), so a day query only needs the start index.
pub fn spans_in(conn: &Connection, from: i64, to: i64) -> Result<Vec<Span>> {
    let mut stmt = conn.prepare_cached(
        "SELECT kind, subject_id, start_ms, end_ms FROM spans
         WHERE start_ms >= ?1 AND start_ms < ?2 ORDER BY start_ms",
    )?;
    let rows = stmt.query_map(params![from, to], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (k, subject, start, end) = row?;
        if let Some(kind) = Kind::from_i64(k) {
            out.push(Span {
                kind,
                subject,
                start,
                end: end.min(to),
            });
        }
    }
    Ok(out)
}

pub fn subjects_by_id(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, Subject>> {
    let mut out = HashMap::new();
    let mut stmt = conn.prepare_cached(
        "SELECT app, app_name, title, host, private, program, project FROM subjects WHERE id=?1",
    )?;
    for id in ids {
        if *id == 0 || out.contains_key(id) {
            continue;
        }
        let s = stmt
            .query_row(params![id], |r| {
                Ok(Subject {
                    app: r.get(0)?,
                    app_name: r.get(1)?,
                    title: r.get(2)?,
                    host: r.get(3)?,
                    private: r.get(4)?,
                    program: r.get(5)?,
                    project: r.get(6)?,
                })
            })
            .optional()?;
        if let Some(s) = s {
            out.insert(*id, s);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("at-store-{}-{n}", std::process::id()))
            .join("t.db")
    }

    #[test]
    fn new_subjects_are_written_with_the_flush() {
        let p = path();
        let mut st = Store::open(&p).unwrap();
        let a = Subject::app("editor", "Editor");
        let b = Subject::app("viewer", "Viewer");
        let ia = st.subject_id(&a).unwrap();
        let ib = st.subject_id(&b).unwrap();
        assert_ne!(ia, ib);
        assert_eq!(st.subject_id(&a).unwrap(), ia);
        let count = |st: &Store| -> i64 {
            st.conn
                .query_row("SELECT COUNT(*) FROM subjects", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count(&st), 0, "nothing written before the flush");
        let mut l = Ledger::default();
        l.reconcile(Kind::Focus, &[ia].into_iter().collect(), 1_000);
        st.flush(&mut l, 5_000).unwrap();
        assert_eq!(count(&st), 2);
        let got = subjects_by_id(&st.conn, &[ia, ib]).unwrap();
        assert_eq!(got[&ia], a);
        assert_eq!(got[&ib], b);
        drop(st);
        // A restarted daemon finds stored subjects and continues after the highest id.
        let mut st = Store::open(&p).unwrap();
        assert_eq!(st.subject_id(&b).unwrap(), ib);
        let c = st.subject_id(&Subject::app("other", "Other")).unwrap();
        assert!(c > ia.max(ib));
    }

    #[test]
    fn browser_links_keep_the_first_and_last_time() {
        let st = Store::open(&path()).unwrap();
        assert!(browser_links(&st.conn).unwrap().is_empty());
        st.note_browser("browser-a", "Browser A", 5_000).unwrap();
        st.note_browser("browser-a", "Browser A (beta)", 9_000)
            .unwrap();
        st.note_browser("browser-a", "Browser A (beta)", 7_000)
            .unwrap();
        st.note_browser("browser-b", "Browser B", 1_000).unwrap();
        let links = browser_links(&st.conn).unwrap();
        assert_eq!(
            links[0],
            BrowserLink {
                app: "browser-a".into(),
                name: "Browser A (beta)".into(),
                first_ms: 5_000,
                last_ms: 9_000,
            }
        );
        assert_eq!(links[1].app, "browser-b");
        st.note_web_app("browser-a-app", "browser-a").unwrap();
        st.note_web_app("browser-a-app", "browser-b").unwrap();
        assert_eq!(web_apps(&st.conn).unwrap()["browser-a-app"], "browser-b");
    }

    #[test]
    fn missing_browser_tables_read_as_empty() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(browser_links(&conn).unwrap().is_empty());
        assert!(web_apps(&conn).unwrap().is_empty());
    }

    #[test]
    fn prune_keeps_subjects_that_open_spans_use() {
        let p = path();
        let mut st = Store::open(&p).unwrap();
        let a = Subject::app("editor", "Editor");
        let ia = st.subject_id(&a).unwrap();
        let mut l = Ledger::default();
        l.reconcile(Kind::Focus, &[ia].into_iter().collect(), 1_000);
        l.reconcile(Kind::Focus, &Default::default(), 5_000);
        st.flush(&mut l, 6_000).unwrap(); // the old span is stored
        // Focused again: the span is open in the ledger and not written yet.
        l.reconcile(Kind::Focus, &[ia].into_iter().collect(), 20_000);
        assert!(l.subjects_in_use().contains(&ia));
        assert_eq!(st.prune(10_000, &l.subjects_in_use()).unwrap(), 1);
        assert_eq!(subjects_by_id(&st.conn, &[ia]).unwrap()[&ia], a);
        st.flush(&mut l, 30_000).unwrap();
        let spans = spans_in(&st.conn, 0, 100_000).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].subject, ia);
        assert_eq!(subjects_by_id(&st.conn, &[ia]).unwrap()[&ia], a);
        assert_eq!(st.subject_id(&a).unwrap(), ia);
    }
}
