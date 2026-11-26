//! Codex's token use and plan limits, read from the session logs it keeps under ~/.codex
//! (sessions/ and archived_sessions/, or $CODEX_HOME), so the dashboard can show what the work
//! would cost at OpenAI's API prices and how much of the plan's limits is used.
//!
//! The same rules as the Claude Code reader (sources::claude), whose helpers this shares:
//! - logs are opened with O_RDONLY | O_NOFOLLOW | O_NOATIME and only listed and read; symbolic
//!   links are skipped.
//! - from each response only its time, model, token counts, service tier and the folder Codex ran
//!   in are kept; from the limits only the plan, the share used and when it resets. Prompts,
//!   replies, tool calls and instructions are skipped unparsed. Response ids are kept only as a
//!   64-bit hash, so each response counts once.

use super::claude::{
    Found, MAX_LINE, OneLine, contains, hash, list_with, open_read_only, paused_spans, read_capped,
};
use crate::daemon::SharedRef;
use crate::sources::terminal::tools::Projects;
use anyhow::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bytes read per pass (a first import of a long history may take a few passes).
pub const BUDGET: u64 = 1 << 30;

/// Codex's home: $CODEX_HOME, else ~/.codex.
pub fn home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::util::home().join(".codex"))
}

/// Where Codex keeps its session logs.
pub fn roots() -> Vec<PathBuf> {
    let h = home();
    vec![h.join("sessions"), h.join("archived_sessions")]
}

/// Reads new responses every minute (and a few seconds after Codex writes) while
/// agents.codex_cost is on; never while paused.
pub fn spawn(shared: SharedRef, db: PathBuf) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(12)).await;
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let (wtx, mut wrx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut watcher: Option<notify::RecommendedWatcher> = None;
        let mut paused_seen: Option<i64> = None;
        let mut last = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                Some(()) = wrx.recv() => {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    while wrx.try_recv().is_ok() {}
                    tokio::time::sleep_until(last + Duration::from_secs(10)).await;
                    while wrx.try_recv().is_ok() {}
                }
            }
            last = tokio::time::Instant::now();
            let Ok((cfg, paused)) = shared.lock().map(|s| {
                (
                    s.config.clone(),
                    s.status["paused"].as_bool().unwrap_or(false),
                )
            }) else {
                continue;
            };
            if !cfg.agents.codex_cost {
                watcher = None;
                continue;
            }
            if watcher.is_none() {
                watcher = watch(wtx.clone());
            }
            let now = crate::util::now_ms();
            if paused {
                paused_seen = Some(now);
                continue;
            }
            let settle = (cfg.general.flush_interval_secs as i64 + 10) * 1000;
            if paused_seen.is_some_and(|t| now - t < settle) {
                continue;
            }
            let keep_from = match cfg.general.keep_days {
                0 => 0,
                d => now.saturating_sub(d as i64 * 86_400_000),
            };
            let db = db.clone();
            let pass = tokio::task::spawn_blocking(move || -> Result<Pass> {
                let mut conn = Connection::open_with_flags(
                    &db,
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                conn.busy_timeout(Duration::from_secs(5))?;
                let pass = scan(&mut conn, &roots(), BUDGET, keep_from);
                if let Err(e) = scan_threads(&mut conn, &home(), keep_from) {
                    tracing::debug!("codex: its conversation list was not read: {e:#}");
                }
                drop(conn);
                crate::util::trim_heap();
                pass
            })
            .await;
            match pass {
                Ok(Ok(p)) if p.files > 0 => {
                    tracing::debug!(
                        "codex: {} new responses from {} files",
                        p.responses,
                        p.files
                    )
                }
                Ok(Err(e)) => tracing::warn!("codex usage not read: {e:#}"),
                _ => {}
            }
        }
    });
}

fn watch(tx: tokio::sync::mpsc::UnboundedSender<()>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && (ev.kind.is_modify() || ev.kind.is_create())
            && ev
                .paths
                .iter()
                .any(|p| p.extension().is_some_and(|x| x == "jsonl"))
        {
            let _ = tx.send(());
        }
    })
    .ok()?;
    let mut any = false;
    for r in roots().iter().filter(|r| r.is_dir()) {
        any |= w.watch(r, notify::RecursiveMode::Recursive).is_ok();
    }
    any.then_some(w)
}

#[derive(Debug, Default, PartialEq)]
pub struct Pass {
    pub files: usize,
    pub bytes: u64,
    pub responses: usize,
    pub done: bool,
}

/// One response's usage, as stored. input counts every prompt token, cached and cache writes
/// included (as Codex reports it); output includes reasoning.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    pub id: i64,
    pub at_ms: i64,
    pub model: String,
    pub project: String,
    pub input: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    pub fast: bool,
    /// The conversation it belongs to (a hash of its id), 0 if unknown.
    pub thread: i64,
}

/// The plan's limits as Codex last reported them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Limits {
    pub at_ms: i64,
    pub plan: String,
    pub windows: Vec<LimitWindow>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LimitWindow {
    pub minutes: u64,
    pub used_percent: f64,
    /// When it resets (ms), if known.
    pub resets_ms: Option<i64>,
}

/// Where a file's reading left off: the model, folder and tier in effect, how usage is counted.
#[derive(Debug, Clone, Default)]
struct State {
    model: String,
    cwd: String,
    tier: String,
    /// 0 not known yet, 1 per-response records, 2 running token counts (older Codex).
    mode: i64,
    /// The running total last counted (mode 2), so a repeated count adds nothing.
    last_total: u64,
    /// The conversation (a hash of its id), from the log's first line.
    thread: i64,
}

/// Reads what is new in every log, up to budget bytes, and stores usage and the latest limits.
pub fn scan(conn: &mut Connection, roots: &[PathBuf], budget: u64, keep_from: i64) -> Result<Pass> {
    let paused = paused_spans(conn)?;
    let mut files: Vec<Found> = vec![];
    for r in roots {
        list_with(r, 0, &mut files, true);
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.mtime_ns));
    type Known = (i64, i64, i64, State);
    let known: HashMap<i64, Known> = {
        let mut st = conn.prepare(
            "SELECT key, size, mtime_ns, offset, model, cwd, tier, mode, last_total, thread
             FROM codex_files",
        )?;
        st.query_map([], |r| {
            Ok((
                r.get(0)?,
                (
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    State {
                        model: r.get(4)?,
                        cwd: r.get(5)?,
                        tier: r.get(6)?,
                        mode: r.get(7)?,
                        last_total: r.get::<_, i64>(8)?.max(0) as u64,
                        thread: r.get(9)?,
                    },
                ),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut pass = Pass {
        done: true,
        ..Default::default()
    };
    let mut seen = HashSet::new();
    let mut projects = Projects::default();
    let mut limits: Option<Limits> = None;
    for f in &files {
        let key = hash(&[f.path.as_os_str().as_bytes()]);
        seen.insert(key);
        let (size0, mtime0, off0, state0) =
            known
                .get(&key)
                .cloned()
                .unwrap_or((-1, -1, 0, State::default()));
        if size0 == f.size as i64 && mtime0 == f.mtime_ns {
            continue;
        }
        // A file that shrank was rewritten: read it again (each response still counts once).
        // A compressed (archived) log is read whole whenever it changes.
        let zst = f.path.extension().is_some_and(|x| x == "zst");
        let (start, state) = if !zst && (off0 as u64) <= f.size {
            (off0 as u64, state0)
        } else {
            (0, State::default())
        };
        let left = budget.saturating_sub(pass.bytes);
        if left == 0 {
            pass.done = false;
            break;
        }
        let read = if zst {
            read_zst(&f.path, f.size, state, &mut projects)
        } else {
            read_file(&f.path, start, left, state, &mut projects)
        };
        let got = match read {
            Ok(g) => g,
            Err(e) => {
                tracing::debug!("codex: skipped a log: {e}");
                continue;
            }
        };
        pass.bytes += got.end - start;
        pass.files += 1;
        if let Some(l) = got.limits
            && limits.as_ref().is_none_or(|x| l.at_ms > x.at_ms)
        {
            limits = Some(l);
        }
        let rows: Vec<&Usage> = got
            .rows
            .iter()
            .filter(|u| {
                u.at_ms >= keep_from && !paused.iter().any(|&(a, b)| u.at_ms >= a && u.at_ms < b)
            })
            .collect();
        pass.responses += rows.len();
        let size = if got.cut { -1 } else { f.size as i64 };
        let tx = conn.transaction()?;
        {
            // A response read again (after an upgrade) only fills in its conversation.
            let mut up = tx.prepare_cached(
                "INSERT INTO codex_usage(id, at_ms, model, project, input, cached,
                     cache_write, output, reasoning, fast, thread)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET thread = excluded.thread
                     WHERE codex_usage.thread = 0 AND excluded.thread != 0",
            )?;
            for u in rows {
                up.execute(params![
                    u.id,
                    u.at_ms,
                    u.model,
                    u.project,
                    u.input as i64,
                    u.cached as i64,
                    u.cache_write as i64,
                    u.output as i64,
                    u.reasoning as i64,
                    u.fast,
                    u.thread
                ])?;
            }
            let s = &got.state;
            tx.execute(
                "INSERT OR REPLACE INTO codex_files(key, size, mtime_ns, offset, model, cwd, tier,
                     mode, last_total, thread)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    key,
                    size,
                    f.mtime_ns,
                    got.end as i64,
                    s.model,
                    s.cwd,
                    s.tier,
                    s.mode,
                    s.last_total as i64,
                    s.thread
                ],
            )?;
        }
        tx.commit()?;
        if got.cut {
            pass.done = false;
            break;
        }
    }
    if let Some(l) = limits {
        save_limits(conn, &l)?;
    }
    if pass.done {
        let gone: Vec<i64> = known
            .keys()
            .filter(|k| !seen.contains(k))
            .copied()
            .collect();
        for k in gone {
            conn.execute("DELETE FROM codex_files WHERE key = ?1", [k])?;
        }
    }
    Ok(pass)
}

const LIMITS_KEY: &str = "codex_limits";

/// Keeps the newest limits Codex reported.
fn save_limits(conn: &Connection, l: &Limits) -> Result<()> {
    if load_limits(conn)?.is_some_and(|old| old.at_ms >= l.at_ms) {
        return Ok(());
    }
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        params![LIMITS_KEY, serde_json::to_string(l)?],
    )?;
    Ok(())
}

/// The plan's limits as Codex last reported them, if it ever did.
pub fn load_limits(conn: &Connection) -> Result<Option<Limits>> {
    let v: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [LIMITS_KEY], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(v.and_then(|v| serde_json::from_str(&v).ok()))
}

/// A conversation's key: a hash of its id, so the id itself is not kept.
pub fn thread_key(id: &str) -> i64 {
    hash(&[b"codex-thread", id.as_bytes()])
}

/// The newest version of one of Codex's databases in its home ("state_" finds state_5.sqlite).
fn newest_db(home: &Path, prefix: &str) -> Option<PathBuf> {
    std::fs::read_dir(home)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let n = name
                .to_str()?
                .strip_prefix(prefix)?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            Some((n, e.path()))
        })
        .filter(|(_, p)| p.symlink_metadata().is_ok_and(|m| m.file_type().is_file()))
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Opens another program's database (Codex's, Hermes's) without changing it: read-only, and
/// while that program is not using it (no write-ahead log beside it) as immutable, so not even a
/// lock or index file is made.
pub(crate) fn open_foreign_db(path: &Path) -> rusqlite::Result<Connection> {
    let mut uri = String::from("file:");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/._-~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri.push_str(if with_suffix(path, "-wal").exists() {
        "?mode=ro"
    } else {
        "?mode=ro&immutable=1"
    });
    let c = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    c.busy_timeout(Duration::from_secs(2))?;
    Ok(c)
}

fn columns(conn: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    let mut st = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    st.query_map([table], |r| r.get(0))?.collect()
}

/// Size and change time of a database and its write-ahead log, to skip one that has not changed.
fn signature(paths: &[&Path]) -> String {
    let mut s = String::new();
    for p in paths {
        for f in [p.to_path_buf(), with_suffix(p, "-wal")] {
            if let Ok(m) = f.metadata() {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                s.push_str(&format!("{}:{t};", m.len()));
            } else {
                s.push_str("-;");
            }
        }
    }
    s
}

const THREADS_KEY: &str = "codex_threads_seen";

#[derive(Debug, Default, PartialEq)]
pub struct ThreadPass {
    pub threads: usize,
    pub turns: usize,
}

/// Seconds or milliseconds since 1970, as milliseconds.
fn to_ms(t: i64) -> i64 {
    if t < 100_000_000_000 { t * 1000 } else { t }
}

/// Copies what Codex's own databases say about each conversation: its token total (prompts,
/// cached or not, and replies), model and folder from state_N.sqlite, and when its turns ran
/// from thread_history_N.sqlite. Codex keeps these after it removes a conversation's log, so
/// that use still counts. Titles, prompts and replies are not read. Rows are only added or
/// raised, never removed, and nothing is taken from while tracking was paused.
pub fn scan_threads(conn: &mut Connection, home: &Path, keep_from: i64) -> Result<ThreadPass> {
    let Some(state) = newest_db(home, "state_") else {
        return Ok(ThreadPass::default());
    };
    let history = newest_db(home, "thread_history_");
    let sig = match &history {
        Some(h) => signature(&[&state, h]),
        None => signature(&[&state]),
    };
    let seen: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [THREADS_KEY],
            |r| r.get(0),
        )
        .optional()?;
    if seen.as_deref() == Some(sig.as_str()) {
        return Ok(ThreadPass::default());
    }
    let paused = paused_spans(conn)?;
    let in_pause = |t: i64| paused.iter().any(|&(a, b)| t >= a && t < b);
    let mut projects = Projects::default();
    // (key, created, updated, model, project, tokens)
    let mut threads: Vec<(i64, i64, i64, String, String, i64)> = vec![];
    {
        let src = open_foreign_db(&state)?;
        let cols = columns(&src, "threads")?;
        if !["id", "created_at", "updated_at", "cwd", "tokens_used"]
            .iter()
            .all(|c| cols.contains(*c))
        {
            return Ok(ThreadPass::default());
        }
        let pick = |ms: &str, s: &str| {
            if cols.contains(ms) {
                format!("coalesce({ms}, {s} * 1000)")
            } else {
                format!("{s} * 1000")
            }
        };
        let model = if cols.contains("model") {
            "coalesce(model, '')"
        } else {
            "''"
        };
        let sql = format!(
            "SELECT id, {}, {}, {model}, cwd, tokens_used FROM threads",
            pick("created_at_ms", "created_at"),
            pick("updated_at_ms", "updated_at"),
        );
        let mut st = src.prepare(&sql)?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        for row in rows {
            let (id, created, updated, model, cwd, tokens) = row?;
            if updated < keep_from || tokens <= 0 {
                continue;
            }
            let project = if cwd.starts_with('/') {
                projects.of(Path::new(&cwd))
            } else {
                String::new()
            };
            threads.push((thread_key(&id), created, updated, model, project, tokens));
        }
    }
    // When each turn ran; the total per conversation counts paused turns too, so that their
    // share of the tokens is left out rather than moved to other turns.
    let mut turns: Vec<(i64, i64, i64)> = vec![];
    let mut turn_ms: HashMap<i64, i64> = HashMap::new();
    if let Some(h) = &history
        && let Ok(src) = open_foreign_db(h)
        && let Ok(cols) = columns(&src, "thread_turns")
        && ["thread_id", "started_at", "completed_at", "duration_ms"]
            .iter()
            .all(|c| cols.contains(*c))
    {
        let mut st = src.prepare(
            "SELECT thread_id, started_at, completed_at, duration_ms FROM thread_turns
             WHERE started_at IS NOT NULL",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, started, completed, dur) = row?;
            let start = to_ms(started);
            let end = match (dur, completed) {
                (Some(d), _) if d > 0 => start + d,
                (_, Some(c)) => to_ms(c),
                _ => continue,
            };
            if end <= start {
                continue;
            }
            let key = thread_key(&id);
            *turn_ms.entry(key).or_default() += end - start;
            if end >= keep_from && !in_pause(start) {
                turns.push((key, start, end));
            }
        }
    }
    let mut pass = ThreadPass::default();
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare(
            "INSERT INTO codex_threads(key, created_ms, updated_ms, model, project, tokens, turn_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(key) DO UPDATE SET
                 updated_ms = max(updated_ms, excluded.updated_ms),
                 model = CASE WHEN excluded.model != '' THEN excluded.model ELSE model END,
                 project = excluded.project,
                 tokens = max(tokens, excluded.tokens),
                 turn_ms = max(turn_ms, excluded.turn_ms)",
        )?;
        for (key, created, updated, model, project, tokens) in &threads {
            let ms = turn_ms.get(key).copied().unwrap_or(0);
            // A conversation without turns is placed when it last changed: not if that was paused.
            if ms == 0 && in_pause(*updated) {
                continue;
            }
            up.execute(params![key, created, updated, model, project, tokens, ms])?;
            pass.threads += 1;
        }
        let mut ins = tx.prepare(
            "INSERT OR IGNORE INTO codex_turns(thread, start_ms, end_ms) VALUES (?1, ?2, ?3)",
        )?;
        for (key, start, end) in &turns {
            pass.turns += ins.execute(params![key, start, end])?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            params![THREADS_KEY, sig],
        )?;
    }
    tx.commit()?;
    Ok(pass)
}

struct Got {
    rows: Vec<Usage>,
    limits: Option<Limits>,
    state: State,
    end: u64,
    cut: bool,
}

fn read_file(
    path: &Path,
    start: u64,
    budget: u64,
    state: State,
    projects: &mut Projects,
) -> io::Result<Got> {
    let mut r = BufReader::with_capacity(64 << 10, open_read_only(path)?);
    r.seek(SeekFrom::Start(start))?;
    let key = path.as_os_str().as_bytes();
    read_lines(&mut r, start, budget, state, key, projects, false)
}

/// An archived log, compressed with zstd: read whole, through a streaming decoder. Its offset is
/// its compressed size, so an unchanged file is not read again.
fn read_zst(path: &Path, size: u64, state: State, projects: &mut Projects) -> io::Result<Got> {
    let dec = ruzstd::decoding::StreamingDecoder::new(open_read_only(path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut r = BufReader::with_capacity(64 << 10, dec);
    let key = path.as_os_str().as_bytes();
    let mut got = read_lines(&mut r, 0, u64::MAX, state, key, projects, true)?;
    got.end = size;
    Ok(got)
}

/// Reads log lines from start on. complete: the log is finished, so a last line without a
/// newline is whole (else it is still being written and waits for the next pass).
fn read_lines<R: io::BufRead>(
    r: &mut R,
    start: u64,
    budget: u64,
    state: State,
    file_key: &[u8],
    projects: &mut Projects,
    complete: bool,
) -> io::Result<Got> {
    let mut out = Got {
        rows: vec![],
        limits: None,
        state,
        end: start,
        cut: false,
    };
    // Running counts (older Codex) are kept apart until it is clear the file has no records.
    let mut counted: Vec<Usage> = vec![];
    let mut buf = Vec::new();
    loop {
        if out.end - start >= budget {
            out.cut = true;
            break;
        }
        buf.clear();
        let (n, newline) = read_capped(r, &mut buf, MAX_LINE)?;
        if n == 0 {
            break;
        }
        if !newline && !(complete && buf.len() < MAX_LINE) {
            if buf.len() < MAX_LINE {
                // Codex is still writing this line.
                break;
            }
            // A very long line (a reply or tool output): never one of ours. Skip it.
            let mut rest = OneLine::new(r);
            rest.drain()?;
            if !rest.ended {
                break;
            }
            out.end += (n + rest.read) as u64;
            continue;
        }
        out.end += n as u64;
        let ours = [
            &b"\"turn_context\""[..],
            b"\"token_usage_record\"",
            b"\"token_count\"",
            b"\"session_meta\"",
            b"\"service_tier\"",
        ];
        if !ours.iter().any(|k| contains(&buf, k)) {
            continue;
        }
        let Ok(line) = serde_json::from_slice::<Line>(&buf) else {
            continue;
        };
        line.apply(&mut out, &mut counted, file_key, projects);
    }
    if out.state.mode == 0 && !counted.is_empty() {
        out.state.mode = 2;
    }
    if out.state.mode == 2 {
        out.rows.append(&mut counted);
    }
    Ok(out)
}

// Only these fields are read from a log line; serde skips the rest without keeping a copy.

#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize, Default)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// The conversation's id (session_meta).
    id: Option<String>,
    thread_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    service_tier: Option<String>,
    response_id: Option<String>,
    usage: Option<Counts>,
    info: Option<Info>,
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct Counts {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

#[derive(Deserialize)]
struct Info {
    last_token_usage: Option<Counts>,
    total_token_usage: Option<Counts>,
}

#[derive(Deserialize)]
struct RateLimits {
    plan_type: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    used_percent: Option<f64>,
    window_minutes: Option<u64>,
    resets_at: Option<i64>,
}

impl Line {
    fn apply(self, out: &mut Got, counted: &mut Vec<Usage>, file: &[u8], projects: &mut Projects) {
        let Some(p) = self.payload else { return };
        let at_ms = self
            .timestamp
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .map(|t| t.as_millisecond());
        if let Some(t) = p.service_tier.as_deref() {
            out.state.tier = t.to_string();
        }
        let thread = p.thread_id.as_deref().map(thread_key);
        let usage = |c: Counts, id: i64, at_ms: i64, st: &State, projects: &mut Projects| Usage {
            id,
            at_ms,
            model: st.model.clone(),
            project: if st.cwd.starts_with('/') {
                projects.of(Path::new(&st.cwd))
            } else {
                String::new()
            },
            input: c.input_tokens,
            cached: c.cached_input_tokens,
            cache_write: c.cache_write_input_tokens,
            output: c.output_tokens,
            reasoning: c.reasoning_output_tokens,
            fast: matches!(st.tier.as_str(), "fast" | "priority"),
            thread: thread.unwrap_or(st.thread),
        };
        match self.kind.as_deref() {
            Some("session_meta") | Some("turn_context") => {
                if self.kind.as_deref() == Some("session_meta")
                    && out.state.thread == 0
                    && let Some(id) = p.id.as_deref().filter(|i| !i.is_empty())
                {
                    out.state.thread = thread_key(id);
                }
                if let Some(c) = p.cwd.filter(|c| !c.is_empty()) {
                    out.state.cwd = c;
                }
                if let Some(m) = p.model.filter(|m| !m.is_empty()) {
                    out.state.model = m;
                }
            }
            Some("token_usage_record") => {
                let (Some(id), Some(c), Some(at)) = (p.response_id, p.usage, at_ms) else {
                    return;
                };
                // Per-response records: these alone count, and running counts no longer do.
                out.state.mode = 1;
                counted.clear();
                let u = usage(
                    c,
                    hash(&[b"codex", id.as_bytes()]),
                    at,
                    &out.state,
                    projects,
                );
                out.rows.push(u);
            }
            Some("event_msg") if p.kind.as_deref() == Some("token_count") => {
                if let (Some(rl), Some(at)) = (p.rate_limits, at_ms) {
                    let windows: Vec<LimitWindow> = [rl.primary, rl.secondary]
                        .into_iter()
                        .flatten()
                        .filter_map(|w| {
                            Some(LimitWindow {
                                minutes: w.window_minutes?,
                                used_percent: w.used_percent?,
                                resets_ms: w.resets_at.map(|s| s * 1000),
                            })
                        })
                        .collect();
                    if !windows.is_empty() {
                        out.limits = Some(Limits {
                            at_ms: at,
                            plan: rl.plan_type.unwrap_or_default(),
                            windows,
                        });
                    }
                }
                // Older Codex wrote no per-response records: count from the running total,
                // which a repeated report does not move.
                let (Some(info), Some(at)) = (p.info, at_ms) else {
                    return;
                };
                if out.state.mode == 1 {
                    return;
                }
                let (Some(last), Some(total)) = (info.last_token_usage, info.total_token_usage)
                else {
                    return;
                };
                if total.total_tokens <= out.state.last_total {
                    return;
                }
                out.state.last_total = total.total_tokens;
                let id = hash(&[b"codex-count", file, &total.total_tokens.to_le_bytes()]);
                let u = usage(last, id, at, &out.state, projects);
                counted.push(u);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("at-codex-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn db(dir: &Path) -> Connection {
        let st = crate::store::Store::open(&dir.join("t.db")).unwrap();
        let path = dir.join("t.db");
        drop(st);
        Connection::open(path).unwrap()
    }

    fn sums(conn: &Connection) -> (i64, i64, i64, i64) {
        conn.query_row(
            "SELECT count(*), coalesce(sum(input), 0), coalesce(sum(cached), 0), coalesce(sum(output), 0) FROM codex_usage",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    }

    const META: &str = r#"{"timestamp":"2026-11-20T10:00:00.000Z","type":"session_meta","payload":{"id":"t-one","cwd":"/nowhere/proj","base_instructions":"secret instructions"}}"#;
    const TURN: &str = r#"{"timestamp":"2026-11-20T10:00:01.000Z","type":"turn_context","payload":{"model":"gpt-6.2-astra","cwd":"/nowhere/proj"}}"#;

    fn record(id: &str, input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"timestamp":"2026-11-20T10:00:05.000Z","type":"token_usage_record","payload":{{"response_id":"{id}","usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{}}}}}}}"#,
            input + output
        )
    }

    fn count(total: u64, last_in: u64, last_out: u64, used: f64) -> String {
        format!(
            r#"{{"timestamp":"2026-11-20T10:00:06.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{last_in},"output_tokens":{last_out},"total_tokens":{}}},"total_token_usage":{{"total_tokens":{total}}}}},"rate_limits":{{"plan_type":"plus","primary":{{"used_percent":{used},"window_minutes":300,"resets_at":1795519969}},"secondary":{{"used_percent":25.0,"window_minutes":10080,"resets_at":1795665556}}}}}}}}"#,
            last_in + last_out
        )
    }

    #[test]
    fn records_count_once_and_limits_are_kept() {
        let dir = tmp("records");
        let logs = dir.join("sessions/2026/11/20");
        std::fs::create_dir_all(&logs).unwrap();
        let f = logs.join("rollout-a.jsonl");
        let mut w = std::fs::File::create(&f).unwrap();
        for l in [
            META.to_string(),
            TURN.to_string(),
            record("r1", 1000, 400, 50),
            count(1050, 1000, 50, 54.0),
        ] {
            writeln!(w, "{l}").unwrap();
        }
        drop(w);
        let mut conn = db(&dir);
        let roots = vec![dir.join("sessions")];
        let p = scan(&mut conn, &roots, BUDGET, 0).unwrap();
        assert_eq!(p.responses, 1);
        assert_eq!(sums(&conn), (1, 1000, 400, 50));
        let (model, project): (String, String) = conn
            .query_row("SELECT model, project FROM codex_usage", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(
            (model.as_str(), project.as_str()),
            ("gpt-6.2-astra", "/nowhere/proj")
        );
        let thread: i64 = conn
            .query_row("SELECT thread FROM codex_usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            thread,
            thread_key("t-one"),
            "a response knows its conversation"
        );
        let l = load_limits(&conn).unwrap().unwrap();
        assert_eq!(l.plan, "plus");
        assert_eq!(l.windows[0].minutes, 300);
        assert_eq!(l.windows[0].resets_ms, Some(1_795_519_969_000));
        // More lines, then a second pass: only the new response is added; a repeat adds nothing.
        let mut w = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
        writeln!(w, "{}", record("r2", 2000, 1500, 70)).unwrap();
        writeln!(w, "{}", record("r2", 2000, 1500, 70)).unwrap();
        drop(w);
        scan(&mut conn, &roots, BUDGET, 0).unwrap();
        assert_eq!(sums(&conn), (2, 3000, 1900, 120));
        // Nothing of the instructions or prompts is stored.
        let all: String = conn
            .query_row(
                "SELECT group_concat(model || project) FROM codex_usage",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!all.contains("secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archived_logs_compressed_with_zstd_are_read_once() {
        let dir = tmp("zst");
        let arch = dir.join("archived_sessions");
        std::fs::create_dir_all(&arch).unwrap();
        // The last line has no newline: an archived log is finished, so it counts.
        let text = format!("{META}\n{TURN}\n{}", record("z1", 1000, 600, 40));
        let packed = ruzstd::encoding::compress_to_vec(
            text.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        std::fs::write(arch.join("rollout-z.jsonl.zst"), packed).unwrap();
        let mut conn = db(&dir);
        let roots = vec![dir.join("sessions"), arch];
        let p = scan(&mut conn, &roots, BUDGET, 0).unwrap();
        assert_eq!((p.files, p.responses), (1, 1));
        assert_eq!(sums(&conn), (1, 1000, 600, 40));
        let thread: i64 = conn
            .query_row("SELECT thread FROM codex_usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(thread, thread_key("t-one"));
        // Unchanged: not read again.
        assert_eq!(scan(&mut conn, &roots, BUDGET, 0).unwrap().files, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Codex's own databases, as small as the reader needs them.
    fn codex_home(dir: &Path, threads: &[(&str, i64, &str, i64)], turns: &[(&str, i64, i64)]) {
        let s = Connection::open(dir.join("state_5.sqlite")).unwrap();
        s.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, cwd TEXT NOT NULL,
                title TEXT NOT NULL, tokens_used INTEGER NOT NULL DEFAULT 0, model TEXT,
                first_user_message TEXT NOT NULL DEFAULT '', created_at_ms INTEGER,
                updated_at_ms INTEGER)",
        )
        .unwrap();
        for (id, at, model, tokens) in threads {
            s.execute(
                "INSERT OR REPLACE INTO threads VALUES (?1, '/gone.jsonl', ?2, ?2, '/nowhere/proj',
                    'secret title', ?3, ?4, 'secret prompt', NULL, NULL)",
                params![id, at, tokens, model],
            )
            .unwrap();
        }
        let h = Connection::open(dir.join("thread_history_1.sqlite")).unwrap();
        h.execute_batch(
            "CREATE TABLE thread_turns (thread_id TEXT NOT NULL, turn_id TEXT NOT NULL,
                started_at INTEGER, completed_at INTEGER, duration_ms INTEGER,
                PRIMARY KEY (thread_id, turn_id))",
        )
        .unwrap();
        for (i, (id, start, dur)) in turns.iter().enumerate() {
            h.execute(
                "INSERT INTO thread_turns VALUES (?1, ?2, ?3, ?3 + ?4 / 1000, ?4)",
                params![id, format!("turn{i}"), start, dur],
            )
            .unwrap();
        }
    }

    fn files_of(dir: &Path) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().unwrap().len(),
                )
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn conversations_are_read_from_codex_database_without_changing_it() {
        let dir = tmp("threads");
        let home = dir.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        let t0 = 1_795_000_000; // seconds, as Codex stores them
        codex_home(
            &home,
            &[("a", t0, "gpt-6.1-sol", 5_000_000), ("b", t0 + 60, "", 0)],
            &[("a", t0, 30_000), ("a", t0 + 100, 90_000)],
        );
        let before = files_of(&home);
        let mut conn = db(&dir);
        let p = scan_threads(&mut conn, &home, 0).unwrap();
        assert_eq!(
            p,
            ThreadPass {
                threads: 1,
                turns: 2
            },
            "an empty conversation is skipped"
        );
        let (tokens, turn_ms, model, project): (i64, i64, String, String) = conn
            .query_row(
                "SELECT tokens, turn_ms, model, project FROM codex_threads WHERE key = ?1",
                [thread_key("a")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((tokens, turn_ms), (5_000_000, 120_000));
        assert_eq!(
            (model.as_str(), project.as_str()),
            ("gpt-6.1-sol", "/nowhere/proj")
        );
        // Codex's files are exactly as they were: no lock, log or index file was made.
        assert_eq!(files_of(&home), before);
        // Unchanged: skipped. Then Codex drops the conversation: ours is kept.
        assert_eq!(
            scan_threads(&mut conn, &home, 0).unwrap(),
            ThreadPass::default()
        );
        Connection::open(home.join("state_5.sqlite"))
            .unwrap()
            .execute("DELETE FROM threads", [])
            .unwrap();
        scan_threads(&mut conn, &home, 0).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM codex_threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        // No title or prompt is stored anywhere.
        let dump: String = conn
            .query_row(
                "SELECT group_concat(model || project) FROM codex_threads",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!dump.contains("secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A check against real data, run by hand on a copy of tracker.db:
    /// AT_DB=/path/copy.db cargo test real_codex -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_codex() {
        let Some(path) = std::env::var_os("AT_DB").map(PathBuf::from) else {
            return;
        };
        drop(crate::store::Store::open(&path).unwrap());
        let mut conn = Connection::open(&path).unwrap();
        println!("logs: {:?}", scan(&mut conn, &roots(), BUDGET, 0).unwrap());
        println!(
            "threads: {:?}",
            scan_threads(&mut conn, &home(), 0).unwrap()
        );
        let to = crate::util::local_date(crate::util::now_ms());
        let cfg = crate::config::Config::default();
        let a = crate::report::agents::agent_report(
            &conn,
            &cfg,
            "codex",
            "2026-07-01",
            &to,
            crate::report::Cache::ReadWrite,
        )
        .unwrap();
        println!(
            "turns: {:?} s, seen working {} s",
            a.turns.map(|t| t.total),
            a.busy
        );
        for from in ["2026-01-01", to.as_str()] {
            let r = crate::report::cost::codex(&conn, &Default::default(), from, &to).unwrap();
            println!(
                "{from}..{to}: ${:.2}, {} requests, tokens {:?}, estimated {:?}",
                r.cost, r.requests, r.tokens, r.estimated
            );
            for m in &r.models {
                println!("  {} ${:.2} {} requests", m.key, m.cost, m.requests);
            }
        }
    }

    #[test]
    fn older_logs_count_from_the_running_total() {
        let dir = tmp("counts");
        let logs = dir.join("sessions");
        std::fs::create_dir_all(&logs).unwrap();
        let f = logs.join("rollout-b.jsonl");
        let mut w = std::fs::File::create(&f).unwrap();
        // The same report twice (a limits refresh), then a new response.
        for l in [
            TURN.to_string(),
            count(1050, 1000, 50, 10.0),
            count(1050, 1000, 50, 11.0),
            count(3120, 2000, 70, 12.0),
        ] {
            writeln!(w, "{l}").unwrap();
        }
        drop(w);
        let mut conn = db(&dir);
        scan(&mut conn, &[logs], BUDGET, 0).unwrap();
        assert_eq!(sums(&conn), (2, 3000, 0, 120));
        assert_eq!(
            load_limits(&conn).unwrap().unwrap().windows[0].used_percent,
            12.0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
