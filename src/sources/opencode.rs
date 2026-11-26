//! opencode's token use, read from the database it keeps ($XDG_DATA_HOME/opencode/opencode.db),
//! so the dashboard can show what its work would cost at list prices. opencode records each
//! response's tokens (fresh input, output, reasoning, cache reads and writes), its model and the
//! folder it ran in, and that is all that is kept. Only the assistant rows of its `message`
//! table are read, and only those fields of them: prompts, replies, titles and keys are not.
//!
//! The database is opened read-only (immutable while opencode is not using it), only when it
//! changed, and never while tracking is paused; each read starts from the responses changed
//! since the one before. Rows are only added or raised, never removed, so use stays counted when
//! opencode later deletes a session.

use super::claude::{hash, paused_spans};
use super::codex::open_foreign_db;
use super::hermes::{Usage, columns, signature, store_usage};
use crate::daemon::SharedRef;
use crate::sources::terminal::tools::Projects;
use anyhow::Result;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// opencode's data folder: $XDG_DATA_HOME/opencode, else ~/.local/share/opencode.
pub fn home() -> PathBuf {
    crate::util::xdg("XDG_DATA_HOME", ".local/share").join("opencode")
}

/// Its database, if it has one.
pub fn database(home: &Path) -> Option<PathBuf> {
    let p = home.join("opencode.db");
    p.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_file())
        .then_some(p)
}

/// Reads new use every minute while agents.opencode_cost is on; never while paused.
pub fn spawn(shared: SharedRef, db: PathBuf) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
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
            if !cfg.agents.opencode_cost || paused {
                continue;
            }
            let Some(theirs) = database(&home()) else {
                continue;
            };
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
                let n = scan(&mut conn, &theirs, keep_from);
                drop(conn);
                crate::util::trim_heap();
                n
            })
            .await;
            match r {
                Ok(Ok(n)) if n > 0 => tracing::debug!("opencode: {n} usage rows"),
                Ok(Err(e)) => tracing::debug!("opencode usage not read: {e:#}"),
                _ => {}
            }
        }
    });
}

/// Responses changed up to this long before the newest one read last time are read again:
/// opencode fills a response's tokens in as it completes.
const OVERLAP_MS: i64 = 3_600_000;

/// Reads opencode's database if it changed and stores its use. Returns the rows written.
pub fn scan(conn: &mut Connection, path: &Path, keep_from: i64) -> Result<usize> {
    let id = hash(&[path.as_os_str().as_bytes()]);
    let (seen_key, upto_key) = (format!("opencode_seen:{id}"), format!("opencode_upto:{id}"));
    let meta = |conn: &Connection, k: &str| -> rusqlite::Result<Option<String>> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [k], |r| r.get(0))
            .optional()
    };
    let sig = signature(path);
    if meta(conn, &seen_key)?.as_deref() == Some(sig.as_str()) {
        return Ok(0);
    }
    let upto: i64 = meta(conn, &upto_key)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let paused = paused_spans(conn)?;
    let mut projects = Projects::default();
    let (rows, newest) = read(path, (upto - OVERLAP_MS).max(0), &mut projects)?;
    let tx = conn.transaction()?;
    let n = store_usage(&tx, "opencode_usage", &rows, &paused, keep_from)?;
    tx.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        params![seen_key, sig],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        params![upto_key, newest.max(upto).to_string()],
    )?;
    tx.commit()?;
    Ok(n)
}

/// A JSON number (opencode writes integers, but a float would do) as a count.
fn count(v: ValueRef) -> i64 {
    match v {
        ValueRef::Integer(n) => n.max(0),
        ValueRef::Real(f) if f > 0.0 => f as i64,
        _ => 0,
    }
}

/// The responses changed since `since` (ms), and the newest change seen.
fn read(path: &Path, since: i64, projects: &mut Projects) -> Result<(Vec<Usage>, i64)> {
    let src = open_foreign_db(path)?;
    let cols = columns(&src, "message")?;
    if !["id", "time_created", "time_updated", "data"]
        .iter()
        .all(|c| cols.contains(*c))
    {
        return Ok((vec![], 0));
    }
    let mut st = src.prepare(
        "SELECT id, time_created, time_updated,
                json_extract(data, '$.time.created'), json_extract(data, '$.time.completed'),
                json_extract(data, '$.modelID'), json_extract(data, '$.path.cwd'),
                json_extract(data, '$.tokens.input'), json_extract(data, '$.tokens.output'),
                json_extract(data, '$.tokens.reasoning'), json_extract(data, '$.tokens.cache.read'),
                json_extract(data, '$.tokens.cache.write')
         FROM message
         WHERE time_updated >= ?1 AND json_valid(data)
           AND json_extract(data, '$.role') = 'assistant'",
    )?;
    let rows = st.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            count(r.get_ref(1)?),
            count(r.get_ref(2)?),
            count(r.get_ref(3)?),
            count(r.get_ref(4)?),
            r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            [7, 8, 9, 10, 11].map(|i| r.get_ref(i).map(count).unwrap_or(0)),
        ))
    })?;
    let db_key = path.as_os_str().as_bytes();
    let (mut out, mut newest) = (vec![], 0);
    for row in rows {
        let (id, row_created, updated, created, completed, model, cwd, tokens) = row?;
        newest = newest.max(updated);
        let [input, output, reasoning, cached, write] = tokens;
        let start = if created > 0 { created } else { row_created };
        if input + output + reasoning + cached + write == 0 || start <= 0 {
            continue;
        }
        out.push(Usage {
            key: hash(&[b"opencode", db_key, id.as_bytes()]),
            start_ms: start,
            end_ms: completed.max(start),
            model,
            project: if cwd.starts_with('/') {
                projects.of(Path::new(&cwd))
            } else {
                String::new()
            },
            calls: 1,
            input,
            cached,
            cache_write: write,
            output,
            reasoning,
        });
    }
    Ok((out, newest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("at-opencode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// An opencode database as small as the reader needs it (made-up responses).
    fn opencode_db(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, data TEXT);
             INSERT INTO message VALUES ('m1', 's1', 1795000000000, 1795000000000,
                '{\"role\":\"user\",\"time\":{\"created\":1795000000000}}');
             INSERT INTO message VALUES ('m2', 's1', 1795000001000, 1795000030000,
                '{\"role\":\"assistant\",\"modelID\":\"deepseek/deepseek-v4-flash\",
                  \"providerID\":\"openrouter\",\"path\":{\"cwd\":\"/nowhere/proj\",
                  \"root\":\"/nowhere/proj\"},\"cost\":0.01,\"time\":{\"created\":1795000001000,
                  \"completed\":1795000030000},\"tokens\":{\"input\":12000,\"output\":800,
                  \"reasoning\":200,\"cache\":{\"read\":90000,\"write\":0}}}');
             INSERT INTO message VALUES ('m3', 's1', 1795000040000, 1795000040000,
                '{\"role\":\"assistant\",\"modelID\":\"stealth/made-up-alpha\",
                  \"time\":{\"created\":1795000040000},
                  \"tokens\":{\"input\":0,\"output\":0,\"reasoning\":0,
                  \"cache\":{\"read\":0,\"write\":0}}}');
             INSERT INTO message VALUES ('m4', 's1', 1795000050000, 1795000050000, 'not json');
             INSERT INTO part VALUES ('p1', 'm2', '{\"text\":\"secret reply\"}');",
        )
        .unwrap();
    }

    fn store(dir: &Path) -> Connection {
        drop(crate::store::Store::open(&dir.join("t.db")).unwrap());
        Connection::open(dir.join("t.db")).unwrap()
    }

    /// Run with: cargo test real_opencode -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_opencode() {
        let dir = tmp("real");
        let theirs = database(&home()).expect("no opencode database");
        let mut conn = store(&dir);
        let n = scan(&mut conn, &theirs, 0).unwrap();
        let r =
            crate::report::cost::opencode(&conn, &Default::default(), "2026-01-01", "2026-12-31")
                .unwrap();
        println!(
            "{n} rows, {} requests, ${:.2}, unpriced {:?}",
            r.requests, r.cost, r.unpriced
        );
        for m in &r.models {
            let t = &m.tokens;
            println!(
                "{:40} {:5} req {:>12} tok ${:.3} free={}",
                m.key,
                m.requests,
                t.input + t.output + t.cache_read + t.cache_write,
                m.cost,
                m.free
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn responses_are_read_without_changing_opencode() {
        let dir = tmp("read");
        let home = dir.join("opencode");
        std::fs::create_dir_all(&home).unwrap();
        let theirs = home.join("opencode.db");
        opencode_db(&theirs);
        assert_eq!(database(&home).as_deref(), Some(theirs.as_path()));
        let before = std::fs::metadata(&theirs).unwrap();
        let mut conn = store(&dir);
        // The finished response; the one still running (no tokens yet) and a broken row are not.
        assert_eq!(scan(&mut conn, &theirs, 0).unwrap(), 1);
        let row: (String, String, i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT model, project, start_ms, end_ms, input, cached, output, reasoning
                 FROM opencode_usage",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "deepseek/deepseek-v4-flash".into(),
                "/nowhere/proj".into(),
                1_795_000_001_000,
                1_795_000_030_000,
                12000,
                90000,
                800,
                200
            )
        );
        // Its file is as it was, with nothing beside it.
        let after = std::fs::metadata(&theirs).unwrap();
        assert_eq!(
            (before.len(), before.modified().unwrap()),
            (after.len(), after.modified().unwrap())
        );
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
        // Unchanged: skipped.
        assert_eq!(scan(&mut conn, &theirs, 0).unwrap(), 0);
        // The running response finishes: read on the next pass, counted once.
        Connection::open(&theirs)
            .unwrap()
            .execute_batch(
                "UPDATE message SET time_updated = 1795000090000, data = json_set(data,
                    '$.tokens.input', 3000, '$.tokens.output', 400, '$.time.completed',
                    1795000090000) WHERE id = 'm3';",
            )
            .unwrap();
        assert_eq!(scan(&mut conn, &theirs, 0).unwrap(), 2);
        let (n, dump): (i64, String) = conn
            .query_row(
                "SELECT count(*), group_concat(model || project) FROM opencode_usage",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 2);
        assert!(!dump.contains("secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
