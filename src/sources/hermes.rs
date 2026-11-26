//! Hermes Agent's token use, read from the database it keeps (~/.hermes/state.db, or $HERMES_HOME,
//! and one per profile under profiles/), so the dashboard can show what its work would cost at
//! list prices. Hermes counts tokens per session and model (session_model_usage; older versions
//! only per session), so that is what is kept: the counts, the model, the folder it ran in, and
//! when the session's use began and last changed. Titles, prompts, replies and keys are not read.
//!
//! The database is opened read-only (immutable while Hermes is not using it), only when it
//! changed, and never while tracking is paused. Rows are only added or raised, never removed, so
//! use stays counted when Hermes later deletes or compresses a session.

use super::claude::{hash, paused_spans};
use super::codex::open_foreign_db;
use crate::daemon::SharedRef;
use crate::sources::terminal::tools::Projects;
use anyhow::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hermes's home: $HERMES_HOME, else ~/.hermes.
pub fn home() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::util::home().join(".hermes"))
}

/// Its databases: the main one and one per profile.
pub fn databases(home: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let main = home.join("state.db");
    if is_file(&main) {
        out.push(main);
    }
    if let Ok(rd) = std::fs::read_dir(home.join("profiles")) {
        for e in rd.flatten() {
            let p = e.path().join("state.db");
            if e.file_type().is_ok_and(|t| t.is_dir()) && is_file(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn is_file(p: &Path) -> bool {
    p.symlink_metadata().is_ok_and(|m| m.file_type().is_file())
}

/// Reads new use every minute while agents.hermes_cost is on; never while paused.
pub fn spawn(shared: SharedRef, db: PathBuf) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Ok((cfg, paused)) = shared.lock().map(|s| {
                (
                    s.config.clone(),
                    s.status["paused"].as_bool().unwrap_or(false),
                )
            }) else {
                continue;
            };
            if !cfg.agents.hermes_cost || paused {
                continue;
            }
            let dbs = databases(&home());
            if dbs.is_empty() {
                continue;
            }
            let keep_from = match cfg.general.keep_days {
                0 => 0,
                d => crate::util::now_ms().saturating_sub(d as i64 * 86_400_000),
            };
            let db = db.clone();
            let r = tokio::task::spawn_blocking(move || -> Result<usize> {
                let mut conn = Connection::open_with_flags(
                    &db,
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                conn.busy_timeout(Duration::from_secs(5))?;
                let n = scan(&mut conn, &dbs, keep_from);
                drop(conn);
                crate::util::trim_heap();
                n
            })
            .await;
            match r {
                Ok(Ok(n)) if n > 0 => tracing::debug!("hermes: {n} usage rows"),
                Ok(Err(e)) => tracing::debug!("hermes usage not read: {e:#}"),
                _ => {}
            }
        }
    });
}

/// One session's use of one model, as stored.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    pub key: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub model: String,
    pub project: String,
    pub calls: i64,
    /// Prompt tokens not read from the cache (Hermes counts cache reads and writes apart).
    pub input: i64,
    pub cached: i64,
    pub cache_write: i64,
    pub output: i64,
    pub reasoning: i64,
}

/// A database's size and change time, with its write-ahead log's: unchanged means nothing new.
pub(crate) fn signature(p: &Path) -> String {
    let mut s = String::new();
    for f in [p.to_path_buf(), {
        let mut w = p.as_os_str().to_os_string();
        w.push("-wal");
        PathBuf::from(w)
    }] {
        match f.metadata() {
            Ok(m) => {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                s.push_str(&format!("{}:{t};", m.len()));
            }
            Err(_) => s.push_str("-;"),
        }
    }
    s
}

pub(crate) fn columns(conn: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    let mut st = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    st.query_map([table], |r| r.get(0))?.collect()
}

/// Reads Hermes's databases (those that changed) and stores their use. Returns the rows written.
pub fn scan(conn: &mut Connection, dbs: &[PathBuf], keep_from: i64) -> Result<usize> {
    let paused = paused_spans(conn)?;
    let mut projects = Projects::default();
    let mut written = 0;
    for path in dbs {
        let key = format!("hermes_seen:{}", hash(&[path.as_os_str().as_bytes()]));
        let sig = signature(path);
        let seen: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |r| {
                r.get(0)
            })
            .optional()?;
        if seen.as_deref() == Some(sig.as_str()) {
            continue;
        }
        let rows = match read(path, &mut projects) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("hermes: a database was not read: {e:#}");
                continue;
            }
        };
        let tx = conn.transaction()?;
        written += store_usage(&tx, "hermes_usage", &rows, &paused, keep_from)?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            params![key, sig],
        )?;
        tx.commit()?;
    }
    Ok(written)
}

/// Stores use rows in a table with hermes_usage's columns, raising rows already there (never
/// lowering or removing them). Nothing from while tracking was paused, nor older than keep_from.
/// Returns the rows written.
pub(crate) fn store_usage(
    tx: &rusqlite::Transaction,
    table: &str,
    rows: &[Usage],
    paused: &[(i64, i64)],
    keep_from: i64,
) -> Result<usize> {
    let mut up = tx.prepare(&format!(
        "INSERT INTO {table}(key, start_ms, end_ms, model, project, calls, input, cached,
             cache_write, output, reasoning)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(key) DO UPDATE SET
             start_ms = min(start_ms, excluded.start_ms),
             end_ms = max(end_ms, excluded.end_ms),
             project = excluded.project,
             calls = max(calls, excluded.calls),
             input = max(input, excluded.input),
             cached = max(cached, excluded.cached),
             cache_write = max(cache_write, excluded.cache_write),
             output = max(output, excluded.output),
             reasoning = max(reasoning, excluded.reasoning)"
    ))?;
    let mut written = 0;
    for u in rows {
        let in_pause = paused
            .iter()
            .any(|&(a, b)| u.start_ms >= a && u.end_ms <= b);
        if in_pause || u.end_ms < keep_from {
            continue;
        }
        written += up.execute(params![
            u.key,
            u.start_ms,
            u.end_ms,
            u.model,
            u.project,
            u.calls,
            u.input,
            u.cached,
            u.cache_write,
            u.output,
            u.reasoning
        ])?;
    }
    Ok(written)
}

/// Seconds (Hermes stores REAL seconds) as milliseconds.
fn ms(t: f64) -> i64 {
    (t * 1000.0) as i64
}

/// The use in one database: per session and model where Hermes keeps that, else per session.
fn read(path: &Path, projects: &mut Projects) -> Result<Vec<Usage>> {
    let src = open_foreign_db(path)?;
    let scols = columns(&src, "sessions")?;
    let need = ["id", "started_at", "input_tokens", "output_tokens"];
    if !need.iter().all(|c| scols.contains(*c)) {
        return Ok(vec![]);
    }
    let col = |c: &str, or: &str| {
        if scols.contains(c) {
            format!("s.{c}")
        } else {
            or.to_string()
        }
    };
    let cwd = col("cwd", "NULL");
    let last = format!(
        "coalesce({}, {}, s.started_at)",
        col("last_activity_at", "NULL"),
        col("ended_at", "NULL")
    );
    let ucols = columns(&src, "session_model_usage")?;
    let per_model = [
        "session_id",
        "model",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ]
    .iter()
    .all(|c| ucols.contains(*c));
    let sql = if per_model {
        let u = |c: &str| {
            if ucols.contains(c) {
                format!("u.{c}")
            } else {
                "NULL".to_string()
            }
        };
        format!(
            "SELECT s.id, u.model || char(31) || {} || char(31) || {}, coalesce({}, s.started_at),
                    coalesce({}, {last}), u.model, {cwd}, coalesce({}, 0), u.input_tokens,
                    u.cache_read_tokens, u.cache_write_tokens, u.output_tokens, coalesce({}, 0)
             FROM session_model_usage u JOIN sessions s ON s.id = u.session_id",
            u("billing_provider"),
            u("task"),
            u("first_seen"),
            u("last_seen"),
            u("api_call_count"),
            u("reasoning_tokens"),
        )
    } else {
        format!(
            "SELECT s.id, '', s.started_at, {last}, coalesce({}, ''), {cwd}, coalesce({}, 0),
                    s.input_tokens, coalesce({}, 0), coalesce({}, 0), s.output_tokens,
                    coalesce({}, 0)
             FROM sessions s",
            col("model", "NULL"),
            col("api_call_count", "NULL"),
            col("cache_read_tokens", "NULL"),
            col("cache_write_tokens", "NULL"),
            col("reasoning_tokens", "NULL"),
        )
    };
    let db_key = path.as_os_str().as_bytes();
    let mut st = src.prepare(&sql)?;
    let rows = st.query_map([], |r| {
        let n = |i: usize| -> rusqlite::Result<i64> {
            Ok(r.get::<_, Option<i64>>(i)?.unwrap_or(0).max(0))
        };
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
            r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            [n(6)?, n(7)?, n(8)?, n(9)?, n(10)?, n(11)?],
        ))
    })?;
    let mut out = vec![];
    for row in rows {
        let (id, route, start, end, model, cwd, [calls, input, cached, write, output, reasoning]) =
            row?;
        if input + cached + write + output == 0 || start <= 0.0 {
            continue;
        }
        let (start_ms, end_ms) = (ms(start), ms(end.max(start)));
        out.push(Usage {
            key: hash(&[b"hermes", db_key, id.as_bytes(), route.as_bytes()]),
            start_ms,
            end_ms,
            model,
            project: if cwd.starts_with('/') {
                projects.of(Path::new(&cwd))
            } else {
                String::new()
            },
            calls,
            input,
            cached,
            cache_write: write,
            output,
            reasoning,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("at-hermes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A Hermes database as small as the reader needs it (made-up sessions).
    fn hermes_db(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT,
                system_prompt TEXT, started_at REAL NOT NULL, ended_at REAL,
                input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
                reasoning_tokens INTEGER DEFAULT 0, cwd TEXT, title TEXT,
                last_activity_at REAL, api_call_count INTEGER DEFAULT 0);
             CREATE TABLE session_model_usage (session_id TEXT NOT NULL, model TEXT NOT NULL,
                billing_provider TEXT NOT NULL DEFAULT '', billing_base_url TEXT NOT NULL DEFAULT '',
                billing_mode TEXT NOT NULL DEFAULT '', task TEXT NOT NULL DEFAULT '',
                api_call_count INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_tokens INTEGER NOT NULL DEFAULT 0, first_seen REAL, last_seen REAL,
                PRIMARY KEY (session_id, model, billing_provider, billing_base_url, billing_mode, task));
             INSERT INTO sessions VALUES ('s1', 'cli', 'anthropic/claude-opus-5-5',
                'secret system prompt', 1795000000.5, 1795003600.0, 0, 0, 0, 0, 0,
                '/nowhere/proj', 'secret title', 1795003600.0, 0);
             INSERT INTO session_model_usage VALUES ('s1', 'anthropic/claude-opus-5-5',
                'openrouter', '', '', '', 12, 3000, 900, 400000, 20000, 100, 1795000000.5, 1795003000.0);
             INSERT INTO session_model_usage VALUES ('s1', 'openai/gpt-6.2-astra',
                'openrouter', '', '', 'voice_chat', 2, 1000, 50, 0, 0, 0, 1795001000.0, 1795001100.0);",
        )
        .unwrap();
    }

    fn store(dir: &Path) -> Connection {
        drop(crate::store::Store::open(&dir.join("t.db")).unwrap());
        Connection::open(dir.join("t.db")).unwrap()
    }

    #[test]
    fn use_per_session_and_model_is_read_without_changing_hermes() {
        let dir = tmp("read");
        let home = dir.join("hermes");
        std::fs::create_dir_all(&home).unwrap();
        hermes_db(&home.join("state.db"));
        let before = std::fs::metadata(home.join("state.db")).unwrap();
        let mut conn = store(&dir);
        let dbs = databases(&home);
        assert_eq!(dbs.len(), 1);
        assert_eq!(scan(&mut conn, &dbs, 0).unwrap(), 2);
        let (n, input, cached, write, output): (i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT count(*), sum(input), sum(cached), sum(cache_write), sum(output)
                 FROM hermes_usage",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            (n, input, cached, write, output),
            (2, 4000, 400000, 20000, 950)
        );
        let (model, project, start, end): (String, String, i64, i64) = conn
            .query_row(
                "SELECT model, project, start_ms, end_ms FROM hermes_usage WHERE calls = 12",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (model.as_str(), project.as_str(), start, end),
            (
                "anthropic/claude-opus-5-5",
                "/nowhere/proj",
                1_795_000_000_500,
                1_795_003_000_000
            )
        );
        // Hermes's file is as it was, with no lock or log file beside it.
        let after = std::fs::metadata(home.join("state.db")).unwrap();
        assert_eq!(
            (before.len(), before.modified().unwrap()),
            (after.len(), after.modified().unwrap())
        );
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
        // Unchanged: skipped. No title or prompt is kept.
        assert_eq!(scan(&mut conn, &dbs, 0).unwrap(), 0);
        let dump: String = conn
            .query_row(
                "SELECT group_concat(model || project) FROM hermes_usage",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!dump.contains("secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
