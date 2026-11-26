//! Claude Code's token use, read from the transcripts it keeps under ~/.claude/projects (or
//! ~/.config/claude/projects), so the dashboard can show what the work would cost at Anthropic's
//! API prices.
//!
//! Strictly read-only:
//! - transcript files are opened with O_RDONLY | O_NOFOLLOW | O_NOATIME, so not even their access
//!   time changes; nothing there is ever written, created, renamed, truncated or locked, and
//!   folders are only listed. Symbolic links are skipped, so nothing outside those folders is read.
//! - from each reply only the model, its time, its token counts and the folder Claude Code ran in
//!   are kept. Prompts, replies, tool calls and file contents are parsed past, never copied or
//!   stored. The reply's ids are kept only as a 64-bit hash, so each reply is counted once.
//! - the dashboard shows only totals, and nothing leaves the computer.

use crate::daemon::SharedRef;
use crate::sources::terminal::tools::Projects;
use anyhow::Result;
use rusqlite::{Connection, OpenFlags, params};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bytes read per pass. Reading is quick (about 1 GB a second), so a first import of a long
/// history takes one pass; only a very long one is spread over a few minutes.
pub const BUDGET: u64 = 2 << 30;
/// Lines up to this long are read whole; longer ones are parsed as a stream.
pub(crate) const MAX_LINE: usize = 512 << 10;
/// projects/<folder>/<session>/subagents/workflows/<run>/<agent>.jsonl is five folders down;
/// a little room for deeper layouts. Links are never followed, so this is the only bound needed.
const MAX_DEPTH: usize = 8;

/// Where Claude Code keeps its transcripts.
pub fn roots() -> Vec<PathBuf> {
    let home = crate::util::home();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    vec![
        home.join(".claude/projects"),
        config.join("claude/projects"),
    ]
}

/// Reads new replies a few seconds after Claude Code writes them (inotify, which only watches)
/// and at least once a minute, while `agents.claude_cost` is on; never while paused.
pub fn spawn(shared: SharedRef, db: PathBuf) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(8)).await;
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
                    // A reply is often written as a few lines in a row: wait for the burst to end.
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    while wrx.try_recv().is_ok() {}
                    // While Claude Code works it writes every few seconds: at most one pass per 10 s.
                    tokio::time::sleep_until(last + Duration::from_secs(10)).await;
                    while wrx.try_recv().is_ok() {}
                }
            }
            last = tokio::time::Instant::now();
            let on = shared
                .lock()
                .map(|s| s.config.agents.claude_cost)
                .unwrap_or(false);
            if !on {
                watcher = None;
            } else if watcher.is_none() {
                watcher = watch(wtx.clone());
            }
            let Ok((cfg, paused)) = shared.lock().map(|s| {
                (
                    s.config.clone(),
                    s.status["paused"].as_bool().unwrap_or(false),
                )
            }) else {
                continue;
            };
            if !cfg.agents.claude_cost {
                continue;
            }
            let now = crate::util::now_ms();
            if paused {
                paused_seen = Some(now);
                continue;
            }
            // Just resumed: wait until the pause itself is in the database, so replies written
            // during it are recognised and left out.
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
                drop(conn);
                crate::util::trim_heap();
                pass
            })
            .await;
            if let Ok(Ok(p)) = &pass
                && let Ok(mut s) = shared.lock()
            {
                s.claude_scan = Some(Progress {
                    done: p.done,
                    left_bytes: p.left,
                });
            }
            match pass {
                Ok(Ok(p)) if p.files > 0 => tracing::debug!(
                    "claude code: {} new replies from {} files ({} bytes)",
                    p.messages,
                    p.files,
                    p.bytes
                ),
                Ok(Err(e)) => tracing::warn!("claude code usage not read: {e:#}"),
                _ => {}
            }
        }
    });
}

/// Watches the transcript folders for new lines. inotify only reports changes; it reads and
/// writes nothing in the folders.
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

/// What one pass did.
#[derive(Debug, Default, PartialEq)]
pub struct Pass {
    pub files: usize,
    pub bytes: u64,
    pub messages: usize,
    /// False when the byte budget ran out before every file was read.
    pub done: bool,
    /// Bytes still to read after this pass.
    pub left: u64,
}

/// How far the transcripts have been read, for the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Progress {
    pub done: bool,
    pub left_bytes: u64,
}

/// One reply's usage, as stored.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    pub id: i64,
    pub at_ms: i64,
    pub model: String,
    pub project: String,
    pub input: u64,
    pub output: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub web_search: u64,
    pub fast: bool,
    pub us_only: bool,
}

pub(crate) struct Found {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) mtime_ns: i64,
}

/// Reads what is new in every transcript, up to `budget` bytes, and stores the replies' usage.
/// `keep_from`: replies older than this (ms) are skipped (general.keep_days).
pub fn scan(conn: &mut Connection, roots: &[PathBuf], budget: u64, keep_from: i64) -> Result<Pass> {
    // Line totals came later than the replies: read every transcript once more so they cover
    // all of them. Replies are stored by id with max(), so a second read changes no count.
    let complete: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'claude_lines'",
            [],
            |r| r.get(0),
        )
        .ok();
    if complete.is_none() {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM claude_files", [])?;
        tx.execute("DELETE FROM claude_lines", [])?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('claude_lines', '1')",
            [],
        )?;
        tx.commit()?;
    }
    let paused = paused_spans(conn)?;
    let mut files = vec![];
    for r in roots {
        list(r, 0, &mut files);
    }
    // Newest first, so today's numbers are there before an old history is.
    files.sort_by_key(|f| std::cmp::Reverse(f.mtime_ns));
    let known: HashMap<i64, (i64, i64, i64)> = {
        let mut st = conn.prepare("SELECT key, size, mtime_ns, offset FROM claude_files")?;
        st.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut pass = Pass {
        done: true,
        ..Default::default()
    };
    let mut seen = HashSet::new();
    let mut projects = Projects::default();
    let mut stopped = false;
    for f in &files {
        let key = hash(&[f.path.as_os_str().as_bytes()]);
        seen.insert(key);
        let (size0, mtime0, off0) = known.get(&key).copied().unwrap_or((-1, -1, 0));
        if size0 == f.size as i64 && mtime0 == f.mtime_ns {
            continue;
        }
        // A file that shrank was rewritten: read it again (each reply still counts once).
        let start = if (off0 as u64) <= f.size {
            off0 as u64
        } else {
            0
        };
        let left = budget.saturating_sub(pass.bytes);
        if stopped || left == 0 {
            // Out of budget: count what is left for the next pass.
            stopped = true;
            pass.done = false;
            pass.left += f.size - start;
            continue;
        }
        let got = match read_file(&f.path, start, left, &paused, keep_from, &mut projects) {
            Ok(g) => g,
            Err(e) => {
                tracing::debug!("claude code: skipped a transcript: {e}");
                continue;
            }
        };
        pass.bytes += got.end - start;
        pass.files += 1;
        if got.cut {
            pass.left += f.size.saturating_sub(got.end);
        }
        pass.messages += got.rows.len();
        // Cut short by the budget: no size, so the next pass goes on from `end`.
        let size = if got.cut { -1 } else { f.size as i64 };
        let tx = conn.transaction()?;
        {
            let mut up = tx.prepare_cached(
                "INSERT INTO claude_usage(id, at_ms, model, project, input, output, cache_5m,
                     cache_1h, cache_read, web_search, fast, us_only)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(id) DO UPDATE SET
                     input = max(input, excluded.input), output = max(output, excluded.output),
                     cache_5m = max(cache_5m, excluded.cache_5m),
                     cache_1h = max(cache_1h, excluded.cache_1h),
                     cache_read = max(cache_read, excluded.cache_read),
                     web_search = max(web_search, excluded.web_search)",
            )?;
            for u in &got.rows {
                up.execute(params![
                    u.id,
                    u.at_ms,
                    u.model,
                    u.project,
                    u.input as i64,
                    u.output as i64,
                    u.cache_5m as i64,
                    u.cache_1h as i64,
                    u.cache_read as i64,
                    u.web_search as i64,
                    u.fast,
                    u.us_only
                ])?;
            }
            for (model, tokens) in &got.lines {
                tx.execute(
                    "INSERT INTO claude_lines(model, tokens) VALUES (?1, ?2)
                     ON CONFLICT(model) DO UPDATE SET tokens = tokens + excluded.tokens",
                    params![model, *tokens as i64],
                )?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO claude_files(key, size, mtime_ns, offset) VALUES (?1, ?2, ?3, ?4)",
                params![key, size, f.mtime_ns, got.end as i64],
            )?;
        }
        tx.commit()?;
        if got.cut {
            pass.done = false;
            stopped = true;
        }
    }
    if pass.done {
        // Forget transcripts that are gone (their replies stay counted).
        let gone: Vec<i64> = known
            .keys()
            .filter(|k| !seen.contains(k))
            .copied()
            .collect();
        for k in gone {
            conn.execute("DELETE FROM claude_files WHERE key = ?1", [k])?;
        }
    }
    Ok(pass)
}

pub(crate) fn paused_spans(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let kind = crate::model::Kind::Paused as i64;
    let mut st = conn.prepare("SELECT start_ms, end_ms FROM spans WHERE kind = ?1")?;
    Ok(st
        .query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Transcript files under dir, without following symbolic links.
pub(crate) fn list(dir: &Path, depth: usize, out: &mut Vec<Found>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        // DirEntry's type and metadata describe the link itself, never its target.
        let Ok(t) = e.file_type() else { continue };
        let path = e.path();
        if t.is_dir() && depth < MAX_DEPTH {
            list(&path, depth + 1, out);
        } else if t.is_file()
            && path.extension().is_some_and(|x| x == "jsonl")
            && let Ok(m) = e.metadata()
        {
            out.push(Found {
                path,
                size: m.len(),
                mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            });
        }
    }
}

/// Opens a transcript read-only: no writing, no following a symbolic link, and (where the
/// kernel allows it, as it does for your own files) no access-time update.
pub fn open_read_only(path: &Path) -> io::Result<File> {
    let open = |extra: i32| {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | extra)
            .open(path)
    };
    match open(libc::O_NOATIME) {
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => open(0),
        r => r,
    }
}

struct Read_ {
    rows: Vec<Usage>,
    /// Tokens per model summed over every line, before each reply is counted once. Claude
    /// Code's own statistics count this way; the ratio undoes it for older history.
    lines: HashMap<String, u64>,
    /// Where the next pass starts: after the last complete line read.
    end: u64,
    /// Stopped because the budget ran out.
    cut: bool,
}

fn read_file(
    path: &Path,
    start: u64,
    budget: u64,
    paused: &[(i64, i64)],
    keep_from: i64,
    projects: &mut Projects,
) -> io::Result<Read_> {
    let mut r = BufReader::with_capacity(64 << 10, open_read_only(path)?);
    r.seek(SeekFrom::Start(start))?;
    let mut out = Read_ {
        rows: vec![],
        lines: HashMap::new(),
        end: start,
        cut: false,
    };
    let mut buf = Vec::new();
    loop {
        if out.end - start >= budget {
            out.cut = true;
            break;
        }
        buf.clear();
        let (n, newline) = read_capped(&mut r, &mut buf, MAX_LINE)?;
        let entry = if n == 0 {
            break;
        } else if newline {
            // A whole line. Only replies carry "usage"; most lines are skipped unparsed.
            if contains(&buf, b"\"usage\"") {
                serde_json::from_slice::<Entry>(&buf).ok()
            } else {
                None
            }
        } else if buf.len() < MAX_LINE {
            // The end of the file without a newline: Claude Code is still writing this line.
            break;
        } else {
            // A very long line: parse the rest as a stream (a reply) or skip it (anything else).
            let mut rest = OneLine::new(&mut r);
            let e = if contains(&buf, b"\"role\":\"assistant\"") {
                serde_json::from_reader::<_, Entry>(io::Read::chain(&buf[..], &mut rest)).ok()
            } else {
                None
            };
            rest.drain()?;
            if !rest.ended {
                break;
            }
            out.end += rest.read as u64;
            e
        };
        out.end += n as u64;
        if let Some(u) = entry.and_then(|e| e.usage(projects))
            && u.at_ms >= keep_from
            && !paused.iter().any(|&(a, b)| u.at_ms >= a && u.at_ms < b)
        {
            *out.lines.entry(u.model.clone()).or_default() +=
                u.input + u.output + u.cache_5m + u.cache_1h + u.cache_read;
            out.rows.push(u);
        }
    }
    Ok(out)
}

/// Reads one line into buf, keeping at most cap bytes. Returns (bytes consumed, ended with a
/// newline). With cap reached and no newline yet, the reader is left inside the line.
pub(crate) fn read_capped<R: BufRead>(
    r: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<(usize, bool)> {
    let mut n = 0;
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            return Ok((n, false));
        }
        let room = cap - buf.len();
        let look = &avail[..avail.len().min(room)];
        if let Some(i) = look.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&look[..i]);
            r.consume(i + 1);
            return Ok((n + i + 1, true));
        }
        let k = look.len();
        buf.extend_from_slice(look);
        r.consume(k);
        n += k;
        if buf.len() >= cap {
            return Ok((n, false));
        }
    }
}

/// The rest of the current line as a reader, stopping before the newline.
pub(crate) struct OneLine<'a, R: BufRead> {
    r: &'a mut R,
    done: bool,
    /// The line ended with a newline (not the end of the file).
    pub(crate) ended: bool,
    /// Bytes consumed, the newline included.
    pub(crate) read: usize,
}

impl<'a, R: BufRead> OneLine<'a, R> {
    pub(crate) fn new(r: &'a mut R) -> Self {
        OneLine {
            r,
            done: false,
            ended: false,
            read: 0,
        }
    }

    /// Consumes what is left of the line without keeping it.
    pub(crate) fn drain(&mut self) -> io::Result<()> {
        while !self.done {
            let avail = self.r.fill_buf()?;
            if avail.is_empty() {
                self.done = true;
                break;
            }
            match avail.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    self.r.consume(i + 1);
                    self.read += i + 1;
                    self.done = true;
                    self.ended = true;
                }
                None => {
                    let k = avail.len();
                    self.r.consume(k);
                    self.read += k;
                }
            }
        }
        Ok(())
    }
}

impl<R: BufRead> Read for OneLine<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        let avail = self.r.fill_buf()?;
        if avail.is_empty() {
            self.done = true;
            return Ok(0);
        }
        let k = avail.len().min(out.len());
        match avail[..k].iter().position(|&b| b == b'\n') {
            Some(i) => {
                out[..i].copy_from_slice(&avail[..i]);
                self.r.consume(i + 1);
                self.read += i + 1;
                self.done = true;
                self.ended = true;
                Ok(i)
            }
            None => {
                out[..k].copy_from_slice(&avail[..k]);
                self.r.consume(k);
                self.read += k;
                Ok(k)
            }
        }
    }
}

pub(crate) fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// FNV-1a: a stable 64-bit hash (std's hasher may change between Rust releases).
pub(crate) fn hash(parts: &[&[u8]]) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in *p {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h as i64
}

// Only these fields are read from a transcript line; serde skips everything else (the prompt,
// the reply, tool calls, file contents) without keeping a copy.

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    cwd: Option<String>,
    message: Option<Msg>,
}

#[derive(Deserialize)]
struct Msg {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Counts>,
}

#[derive(Deserialize)]
struct Counts {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation: Option<CacheWrites>,
    server_tool_use: Option<ServerTools>,
    speed: Option<String>,
    inference_geo: Option<String>,
}

#[derive(Deserialize)]
struct CacheWrites {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ServerTools {
    web_search_requests: Option<u64>,
}

impl Entry {
    fn usage(self, projects: &mut Projects) -> Option<Usage> {
        if self.kind.as_deref() != Some("assistant") {
            return None;
        }
        let msg = self.message?;
        let c = msg.usage?;
        let model = msg.model.filter(|m| !m.is_empty() && !m.starts_with('<'))?;
        let id = msg.id.filter(|i| !i.is_empty())?;
        let at_ms = self
            .timestamp?
            .parse::<jiff::Timestamp>()
            .ok()?
            .as_millisecond();
        let writes = c.cache_creation_input_tokens.unwrap_or(0);
        let (cache_5m, cache_1h) = match c.cache_creation {
            Some(w) => (
                w.ephemeral_5m_input_tokens.unwrap_or(0),
                w.ephemeral_1h_input_tokens.unwrap_or(0),
            ),
            None => (writes, 0),
        };
        let project = self
            .cwd
            .filter(|d| d.starts_with('/'))
            .map(|d| projects.of(Path::new(&d)))
            .unwrap_or_default();
        Some(Usage {
            id: hash(&[
                id.as_bytes(),
                self.request_id.unwrap_or_default().as_bytes(),
            ]),
            at_ms,
            model,
            project,
            input: c.input_tokens.unwrap_or(0),
            output: c.output_tokens.unwrap_or(0),
            cache_5m,
            cache_1h,
            cache_read: c.cache_read_input_tokens.unwrap_or(0),
            web_search: c
                .server_tool_use
                .and_then(|s| s.web_search_requests)
                .unwrap_or(0),
            fast: c.speed.as_deref() == Some("fast"),
            us_only: c.inference_geo.as_deref() == Some("us"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("at-claude-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn db(dir: &Path) -> Connection {
        let p = dir.join("t.db");
        drop(crate::store::Store::open(&p).unwrap());
        Connection::open(&p).unwrap()
    }

    fn reply(id: &str, req: &str, at: &str, model: &str, out: u64, geo: &str) -> String {
        let geo = if geo.is_empty() { "global" } else { geo };
        format!(
            r#"{{"parentUuid":"x","isSidechain":false,"cwd":"/nonexistent/demo","sessionId":"s","message":{{"model":"{model}","id":"{id}","type":"message","role":"assistant","content":[{{"type":"text","text":"a reply with \"usage\": inside it"}}],"usage":{{"input_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":{out},"server_tool_use":{{"web_search_requests":1,"web_fetch_requests":0}},"service_tier":"standard","cache_creation":{{"ephemeral_1h_input_tokens":60,"ephemeral_5m_input_tokens":40}},"inference_geo":"{geo}","speed":"standard"}}}},"requestId":"{req}","type":"assistant","uuid":"u","timestamp":"{at}"}}"#
        )
    }

    fn user(text_len: usize) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{}"}},"timestamp":"2026-11-20T10:00:00Z"}}"#,
            "y".repeat(text_len)
        )
    }

    /// Everything about a file that reading could change: a hash of its bytes (read without
    /// touching the access time), its size, its three times and its mode.
    fn fingerprint(p: &Path) -> [i64; 9] {
        let mut bytes = vec![];
        open_read_only(p).unwrap().read_to_end(&mut bytes).unwrap();
        let m = std::fs::symlink_metadata(p).unwrap();
        [
            hash(&[&bytes]),
            m.len() as i64,
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
            m.atime(),
            m.atime_nsec(),
            m.mode() as i64,
        ]
    }

    #[test]
    fn replies_are_counted_once_and_nothing_else_is_kept() {
        let dir = tmp("count");
        let root = dir.join("projects");
        let proj = root.join("-demo");
        std::fs::create_dir_all(proj.join("sess/subagents/workflows/run1")).unwrap();
        let t = "2026-11-20T10:00:00.000Z";
        let main = [
            user(50),
            // One reply written as two lines (one per content block): counted once.
            reply("msg_a", "req_a", t, "claude-opus-5-5", 5, ""),
            reply("msg_a", "req_a", t, "claude-opus-5-5", 7, ""),
            "not json at all".to_string(),
            reply("msg_s", "req_s", t, "<synthetic>", 0, ""),
            reply(
                "msg_b",
                "req_b",
                "2026-11-20T11:00:00Z",
                "claude-sonnet-5-5",
                3,
                "us",
            ),
        ]
        .join("\n")
            + "\n";
        std::fs::write(proj.join("s1.jsonl"), &main).unwrap();
        std::fs::write(
            // A workflow agent, five folders down.
            proj.join("sess/subagents/workflows/run1/agent-1.jsonl"),
            reply("msg_c", "req_c", t, "claude-haiku-5-5", 2, "") + "\n",
        )
        .unwrap();
        let mut conn = db(&dir);
        let p = scan(&mut conn, std::slice::from_ref(&root), BUDGET, 0).unwrap();
        assert_eq!((p.files, p.messages, p.done), (2, 4, true));
        let rows: Vec<(String, i64, i64, i64, i64, bool)> = conn
            .prepare("SELECT model, output, cache_5m, cache_1h, web_search, us_only FROM claude_usage ORDER BY model")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("claude-haiku-5-5".into(), 2, 40, 60, 1, false),
                ("claude-opus-5-5".into(), 7, 40, 60, 1, false),
                ("claude-sonnet-5-5".into(), 3, 40, 60, 1, true),
            ]
        );
        // Nothing but numbers, a model and a folder is stored.
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('claude_usage')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            cols,
            [
                "id",
                "at_ms",
                "model",
                "project",
                "input",
                "output",
                "cache_5m",
                "cache_1h",
                "cache_read",
                "web_search",
                "fast",
                "us_only"
            ]
        );
        // A second pass finds nothing new.
        assert_eq!(
            scan(&mut conn, std::slice::from_ref(&root), BUDGET, 0)
                .unwrap()
                .messages,
            0
        );
        // An install from before line totals reads everything once more, counting nothing twice.
        conn.execute("DELETE FROM meta WHERE key = 'claude_lines'", [])
            .unwrap();
        assert_eq!(scan(&mut conn, &[root], BUDGET, 0).unwrap().files, 2);
        let n: i64 = conn
            .query_row("SELECT count(*) FROM claude_usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        let lines: i64 = conn
            .query_row("SELECT sum(tokens) FROM claude_lines", [], |r| r.get(0))
            .unwrap();
        assert_eq!(lines, 4 * 1110 + 5 + 7 + 3 + 2, "each line once, not twice");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcripts_are_never_changed_not_even_their_access_time() {
        let dir = tmp("ro");
        let root = dir.join("projects");
        std::fs::create_dir_all(root.join("-demo")).unwrap();
        let f = root.join("-demo/s.jsonl");
        let mut body = String::new();
        for i in 0..50 {
            body += &reply(
                &format!("m{i}"),
                "r",
                "2026-11-20T10:00:00Z",
                "claude-opus-5-5",
                1,
                "",
            );
            body.push('\n');
            body += &user(2000);
            body.push('\n');
        }
        std::fs::write(&f, &body).unwrap();
        // An old access time, so a normal read would have to move it (relatime).
        let old = libc::timespec {
            tv_sec: 1_000_000_000,
            tv_nsec: 0,
        };
        let times = [
            old,
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
        ];
        let c = std::ffi::CString::new(f.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) },
            0
        );
        std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o400)).unwrap();
        let before = fingerprint(&f);
        let dir_before = std::fs::symlink_metadata(root.join("-demo"))
            .unwrap()
            .mtime_nsec();
        let mut conn = db(&dir);
        assert_eq!(
            scan(&mut conn, std::slice::from_ref(&root), BUDGET, 0)
                .unwrap()
                .messages,
            50
        );
        assert_eq!(
            fingerprint(&f),
            before,
            "bytes, size, times and mode unchanged"
        );
        assert_eq!(
            std::fs::symlink_metadata(root.join("-demo"))
                .unwrap()
                .mtime_nsec(),
            dir_before
        );
        let names: Vec<_> = std::fs::read_dir(root.join("-demo"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, ["s.jsonl"], "no file created beside it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn links_are_not_followed_and_a_half_written_line_waits() {
        let dir = tmp("links");
        let root = dir.join("projects");
        std::fs::create_dir_all(root.join("-demo")).unwrap();
        let outside = dir.join("outside.jsonl");
        std::fs::write(
            &outside,
            reply("m1", "r", "2026-11-20T10:00:00Z", "claude-opus-5-5", 1, "") + "\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, root.join("-demo/link.jsonl")).unwrap();
        std::os::unix::fs::symlink(&dir, root.join("-loop")).unwrap();
        assert!(
            open_read_only(&root.join("-demo/link.jsonl")).is_err(),
            "O_NOFOLLOW"
        );
        let f = root.join("-demo/s.jsonl");
        let line = reply("m2", "r", "2026-11-20T10:00:00Z", "claude-opus-5-5", 1, "");
        std::fs::write(&f, &line[..line.len() / 2]).unwrap();
        let mut conn = db(&dir);
        assert_eq!(
            scan(&mut conn, std::slice::from_ref(&root), BUDGET, 0)
                .unwrap()
                .messages,
            0
        );
        // Claude Code finishes the line: now it counts.
        let mut w = OpenOptions::new().append(true).open(&f).unwrap();
        w.write_all(&line.as_bytes()[line.len() / 2..]).unwrap();
        w.write_all(b"\n").unwrap();
        drop(w);
        assert_eq!(scan(&mut conn, &[root], BUDGET, 0).unwrap().messages, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_lines_budget_pauses_and_retention() {
        let dir = tmp("long");
        let root = dir.join("projects");
        std::fs::create_dir_all(root.join("-demo")).unwrap();
        let big_reply = reply("big", "r", "2026-11-20T10:00:00Z", "claude-opus-5-5", 9, "")
            .replace("a reply with", &"z".repeat(MAX_LINE * 2));
        let body = [
            user(MAX_LINE * 3),
            big_reply,
            reply(
                "paused",
                "r",
                "2026-11-20T12:30:00Z",
                "claude-opus-5-5",
                1,
                "",
            ),
            reply("old", "r", "2020-01-01T00:00:00Z", "claude-opus-5-5", 1, ""),
            reply(
                "late",
                "r",
                "2026-11-20T13:00:00Z",
                "claude-opus-5-5",
                1,
                "",
            ),
        ]
        .join("\n")
            + "\n";
        std::fs::write(root.join("-demo/s.jsonl"), &body).unwrap();
        let mut conn = db(&dir);
        let at = |s: &str| s.parse::<jiff::Timestamp>().unwrap().as_millisecond();
        conn.execute(
            "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (?1, 0, ?2, ?3)",
            params![
                crate::model::Kind::Paused as i64,
                at("2026-11-20T12:00:00Z"),
                at("2026-11-20T12:45:00Z")
            ],
        )
        .unwrap();
        // A small budget reads part of the file per pass and picks up where it stopped.
        let mut total = 0;
        for _ in 0..20 {
            let p = scan(
                &mut conn,
                std::slice::from_ref(&root),
                400_000,
                at("2025-01-01T00:00:00Z"),
            )
            .unwrap();
            total += p.messages;
            assert_eq!(
                p.done,
                p.left == 0,
                "what is left is counted until it is read"
            );
            if p.done {
                break;
            }
        }
        let ids: Vec<i64> = conn
            .prepare("SELECT output FROM claude_usage ORDER BY at_ms")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!(
            ids,
            [9, 1],
            "the long reply and the late one; not the paused or the old one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
