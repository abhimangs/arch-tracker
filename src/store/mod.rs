//! SQLite storage: subjects, spans (one table for spans and signals) and a rollup cache.

pub mod ledger;

use crate::model::{Kind, Span, Subject};
use anyhow::{Context, Result};
use ledger::Ledger;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::collections::HashMap;
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

    /// A value from the meta table.
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
            PRIMARY KEY(day, settings_hash));",
    )?;
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
}
