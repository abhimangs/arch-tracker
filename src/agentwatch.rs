//! "Agent needs you": a notice when an AI agent you aren't looking at finishes its turn, or waits
//! for your answer, so you can go back to it. Claude Code's transcripts, Codex's session logs and
//! opencode's database say when a turn ends; only the kind and time of their last entries are
//! read (never their text), and only when the file changed. Any other agent counts as done when
//! it goes quiet after working. Each turn is announced once, and never while you look at it.

use crate::alerts::Note;
use crate::config::Config;
use crate::sources::terminal::procfs;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// A turn that ended longer ago than this (before the tracker noticed) is not announced.
const STALE_MS: i64 = 10 * 60_000;
/// A finished turn waits this long for you to look before it is announced.
const GRACE_MS: i64 = 3_000;
/// How much of a log's end is read; a turn's last entries are near the end, after any large
/// request records.
const TAIL: u64 = 256 * 1024;
const TAIL_MORE: u64 = 4 * 1024 * 1024;

/// What an agent's own records say about its current turn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Turn {
    /// The turn ended at this time: it waits for your next message.
    Done(i64),
    /// It asked to run a tool at this time and has no answer yet (perhaps an approval).
    Asking(i64),
    /// You stopped the turn.
    Stopped,
    /// Still working, or a new turn began.
    Working,
}

/// An agent process as the live status lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct Live {
    pub pid: u32,
    pub busy: bool,
    pub subject: LiveSubject,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LiveSubject {
    pub app_name: String,
    pub program: String,
    pub project: String,
}

#[derive(Default)]
struct Inst {
    /// Time it worked since the last notice, in ms.
    work_ms: i64,
    quiet_since: Option<i64>,
    /// The last turn already announced (or seen by you).
    handled: Option<String>,
}

#[derive(Default)]
pub struct Watch {
    started: i64,
    last: i64,
    insts: HashMap<u32, Inst>,
    /// Per log file: its size and change time when read, and what it said.
    cache: HashMap<PathBuf, (String, Option<Turn>)>,
}

impl Watch {
    pub fn new(now: i64) -> Watch {
        Watch {
            started: now,
            last: now,
            ..Default::default()
        }
    }

    /// The notices due now. `looking`: the agent and project in front of you, if one is.
    pub fn check(
        &mut self,
        cfg: &Config,
        agents: &[Live],
        looking: Option<(&str, &str)>,
        now: i64,
    ) -> Vec<Note> {
        let step = (now - self.last).clamp(0, 30_000);
        self.last = now;
        let delay = cfg.notifications.agent_quiet_secs as i64 * 1000;
        self.insts
            .retain(|pid, _| agents.iter().any(|a| a.pid == *pid));
        let mut notes = vec![];
        for a in agents {
            let tool = a.subject.program.as_str();
            let (base, turn) = match self.turn(tool, a.pid) {
                Some((b, t)) => (b, Some(t)),
                None => (String::new(), None),
            };
            let inst = self.insts.entry(a.pid).or_default();
            if a.busy {
                inst.work_ms += step;
                inst.quiet_since = None;
            } else if inst.quiet_since.is_none() {
                inst.quiet_since = Some(now);
            }
            let quiet = inst.quiet_since.map_or(0, |q| now - q);
            let seen = looking == Some((tool, a.subject.project.as_str()));
            let mut announce = |inst: &mut Inst, key: String, what: What| {
                inst.handled = Some(key);
                if !seen {
                    notes.push(note(a, what, inst.work_ms));
                }
                inst.work_ms = 0;
            };
            match turn {
                Some(Turn::Done(at)) => {
                    let key = format!("{base}:done:{at}");
                    if inst.handled.as_deref() == Some(key.as_str()) {
                        continue;
                    }
                    if at < self.started || now - at > STALE_MS {
                        inst.handled = Some(key);
                    } else if seen || now - at >= GRACE_MS {
                        announce(inst, key, What::Done);
                    }
                }
                Some(Turn::Asking(at)) => {
                    let key = format!("{base}:ask:{at}");
                    if inst.handled.as_deref() == Some(key.as_str()) {
                        continue;
                    }
                    if at < self.started && quiet == 0 {
                        inst.handled = Some(key);
                    } else if seen || (!a.busy && quiet >= delay && now - at >= delay) {
                        announce(inst, key, What::Waiting);
                    }
                }
                Some(Turn::Stopped) => inst.work_ms = 0,
                Some(Turn::Working) | None => {
                    if inst.work_ms >= 10_000 && !a.busy && quiet >= delay {
                        let key = format!("{}:quiet:{}", a.pid, now - quiet);
                        let what = if turn.is_some() {
                            What::Waiting
                        } else {
                            What::Quiet
                        };
                        announce(inst, key, what);
                    }
                }
            }
        }
        notes
    }

    /// The agent's turn from its own records: the record's key and what it says.
    fn turn(&mut self, tool: &str, pid: u32) -> Option<(String, Turn)> {
        let cwd = procfs::cwd(pid)?;
        let (path, read): (PathBuf, fn(&Path) -> Option<Turn>) = match tool {
            "claude" => (claude_transcript(&cwd)?, |p| claude_turn(&tail(p)?)),
            "codex" => (codex_rollout(pid, &cwd)?, |p| codex_turn(&tail(p)?)),
            "opencode" => {
                let db = crate::sources::opencode::database(&crate::sources::opencode::home())?;
                let sig = crate::sources::hermes::signature(&db);
                let key = db.join(&cwd);
                if let Some((s, t)) = self.cache.get(&key)
                    && *s == sig
                {
                    return t.map(|t| (key.display().to_string(), t));
                }
                let t = opencode_turn(&db, &cwd);
                self.cache.insert(key.clone(), (sig, t));
                return t.map(|t| (key.display().to_string(), t));
            }
            _ => return None,
        };
        let sig = crate::sources::hermes::signature(&path);
        let turn = match self.cache.get(&path) {
            Some((s, t)) if *s == sig => *t,
            _ => {
                let t = read(&path);
                if self.cache.len() > 64 {
                    self.cache.clear();
                }
                self.cache.insert(path.clone(), (sig, t));
                t
            }
        };
        turn.map(|t| (path.display().to_string(), t))
    }
}

#[derive(Clone, Copy)]
enum What {
    Done,
    Waiting,
    Quiet,
}

fn note(a: &Live, what: What, work_ms: i64) -> Note {
    let name = if a.subject.app_name.is_empty() {
        a.subject.program.as_str()
    } else {
        a.subject.app_name.as_str()
    };
    let project = a
        .subject
        .project
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|p| !p.is_empty() && *p != "~")
        .unwrap_or("its terminal");
    let worked = if work_ms >= 60_000 {
        format!(", after {} of work", crate::tray::fmt_dur(work_ms / 1000))
    } else {
        String::new()
    };
    let (summary, body) = match what {
        What::Done => (
            format!("{name} finished"),
            format!("In {project}{worked}. It waits for your next message."),
        ),
        What::Waiting => (
            format!("{name} is waiting for you"),
            format!("In {project}{worked}. It needs your answer to go on."),
        ),
        What::Quiet => (
            format!("{name} stopped working"),
            format!("In {project}{worked}."),
        ),
    };
    Note {
        key: format!("agent:{}", a.pid),
        family: format!("agent:{}", a.pid),
        route: format!("focus:{}", a.pid),
        summary,
        body,
    }
}

/// The end of a log, whole lines only: more of it when the end holds only large records.
fn tail(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    for want in [TAIL, TAIL_MORE] {
        let from = len.saturating_sub(want);
        f.seek(SeekFrom::Start(from)).ok()?;
        let mut buf = Vec::with_capacity((len - from) as usize);
        f.by_ref().take(want).read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        let text = if from > 0 {
            text.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            &text
        };
        let relevant = [
            "\"type\":\"assistant\"",
            "\"turn_duration\"",
            "\"event_msg\"",
        ];
        if from == 0 || relevant.iter().any(|r| text.contains(r)) {
            return Some(text.to_string());
        }
    }
    None
}

fn millis(v: &serde_json::Value) -> i64 {
    v.as_str()
        .and_then(|s| s.parse::<jiff::Timestamp>().ok())
        .map_or(0, |t| t.as_millisecond())
}

/// Claude Code's transcript folder for a working directory: every character but letters and
/// digits becomes "-". The newest transcript in it is the session that runs there.
fn claude_transcript(cwd: &Path) -> Option<PathBuf> {
    let name: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    crate::sources::claude::roots()
        .into_iter()
        .filter_map(|r| newest(&r.join(&name), "jsonl"))
        .max_by_key(|(_, t)| *t)
        .map(|(p, _)| p)
}

fn newest(dir: &Path, ext: &str) -> Option<(PathBuf, std::time::SystemTime)> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == ext))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .max_by_key(|(_, t)| *t)
}

/// The turn in the end of a Claude Code transcript. Only each entry's kind, stop reason, kinds of
/// content and time are looked at.
pub fn claude_turn(text: &str) -> Option<Turn> {
    for line in text.lines().rev() {
        let kind = if line.contains("\"turn_duration\"") {
            "system"
        } else if line.contains("\"type\":\"assistant\"") {
            "assistant"
        } else if line.contains("\"type\":\"user\"") {
            "user"
        } else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["isSidechain"] == true || v["isMeta"] == true || v["type"] != kind {
            continue;
        }
        let at = millis(&v["timestamp"]);
        let m = &v["message"];
        return Some(match kind {
            "system" if v["subtype"] == "turn_duration" => Turn::Done(at),
            "system" => continue,
            "assistant" => {
                let tool = m["content"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|x| x["type"] == "tool_use"));
                match m["stop_reason"].as_str() {
                    _ if tool => Turn::Asking(at),
                    Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal") => Turn::Done(at),
                    _ => Turn::Working,
                }
            }
            _ => Turn::Working,
        });
    }
    None
}

/// The rollout a Codex process is writing: one it holds open, else the newest one started in
/// its folder today or yesterday.
fn codex_rollout(pid: u32, cwd: &Path) -> Option<PathBuf> {
    let table = procfs::ProcTable::read();
    let mut pids = vec![pid];
    pids.extend(procfs::descendants(pid, &table.children));
    let sessions = crate::sources::codex::home().join("sessions");
    for p in pids {
        let Ok(rd) = std::fs::read_dir(format!("/proc/{p}/fd")) else {
            continue;
        };
        for e in rd.flatten() {
            if let Ok(t) = std::fs::read_link(e.path())
                && t.starts_with(&sessions)
                && t.extension().is_some_and(|x| x == "jsonl")
            {
                return Some(t);
            }
        }
    }
    let now = crate::util::now_ms();
    let mut found: Vec<(PathBuf, std::time::SystemTime)> = vec![];
    for back in [0, 1] {
        let date = crate::util::local_date(now - back * 86_400_000);
        let dir = sessions.join(date.replace('-', "/"));
        if let Ok(rd) = std::fs::read_dir(dir) {
            found.extend(rd.flatten().filter_map(|e| {
                let p = e.path();
                (p.extension()? == "jsonl").then_some(())?;
                Some((p, e.metadata().ok()?.modified().ok()?))
            }));
        }
    }
    found.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    found.into_iter().take(8).map(|(p, _)| p).find(|p| {
        // The first line names the folder the session runs in.
        let mut first = String::new();
        let _ = std::io::BufRead::read_line(
            &mut std::io::BufReader::new(match std::fs::File::open(p) {
                Ok(f) => f.take(64 * 1024),
                Err(_) => return false,
            }),
            &mut first,
        );
        serde_json::from_str::<serde_json::Value>(&first)
            .is_ok_and(|v| v["payload"]["cwd"].as_str() == Some(&*cwd.to_string_lossy()))
    })
}

/// The turn in the end of a Codex rollout, from its task events.
pub fn codex_turn(text: &str) -> Option<Turn> {
    for line in text.lines().rev() {
        if !line.contains("\"event_msg\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let at = millis(&v["timestamp"]);
        return Some(match v["payload"]["type"].as_str().unwrap_or("") {
            "task_complete" => Turn::Done(at),
            "turn_aborted" => Turn::Stopped,
            "task_started" | "user_message" => Turn::Working,
            "exec_approval_request" | "apply_patch_approval_request" | "request_user_input" => {
                Turn::Asking(at)
            }
            _ => continue,
        });
    }
    None
}

/// The turn of opencode's newest session in this folder, from its last message.
fn opencode_turn(db: &Path, cwd: &Path) -> Option<Turn> {
    let conn = crate::sources::codex::open_foreign_db(db).ok()?;
    let row = conn.query_row(
        "SELECT json_extract(data, '$.role'), json_extract(data, '$.finish'),
                json_extract(data, '$.time.completed'), json_extract(data, '$.error.name')
         FROM message
         WHERE session_id = (SELECT id FROM session WHERE directory = ?1 AND parent_id IS NULL
                             ORDER BY time_updated DESC LIMIT 1)
           AND json_valid(data)
         ORDER BY time_created DESC LIMIT 1",
        [cwd.to_string_lossy()],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        },
    );
    let (role, finish, completed, error) = row.ok()?;
    Some(match (role.as_deref(), completed) {
        _ if error.as_deref() == Some("MessageAbortedError") => Turn::Stopped,
        (Some("assistant"), Some(at)) if finish.as_deref() != Some("tool-calls") => Turn::Done(at),
        _ => Turn::Working,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(pid: u32, tool: &str, busy: bool) -> Live {
        Live {
            pid,
            busy,
            subject: LiveSubject {
                app_name: "Gemini CLI".into(),
                program: tool.into(),
                project: "~/code/atlas".into(),
            },
        }
    }

    /// Run with: AT_PID=<agent pid> AT_TOOL=claude cargo test real_turn -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_turn() {
        let pid: u32 = std::env::var("AT_PID").unwrap().parse().unwrap();
        let tool = std::env::var("AT_TOOL").unwrap();
        let mut w = Watch::new(0);
        println!("{tool} {pid}: {:?}", w.turn(&tool, pid).map(|(_, t)| t));
    }

    #[test]
    fn a_claude_transcript_tells_a_finished_turn_from_a_pending_tool() {
        let user = r#"{"type":"user","timestamp":"2026-11-26T10:00:00Z","message":{"role":"user","content":"x"}}"#;
        let tool = r#"{"type":"assistant","timestamp":"2026-11-26T10:00:05Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use"}]}}"#;
        let result = r#"{"type":"user","timestamp":"2026-11-26T10:00:07Z","message":{"content":[{"type":"tool_result"}]}}"#;
        let reply = r#"{"type":"assistant","timestamp":"2026-11-26T10:00:09Z","message":{"stop_reason":"end_turn","content":[{"type":"text"}]}}"#;
        let duration =
            r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-11-26T10:00:10Z"}"#;
        let noise = r#"{"type":"last-prompt","lastPrompt":"\"type\":\"assistant\""}"#;
        let side = r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-11-26T10:00:11Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use"}]}}"#;
        let t0 = "2026-11-26T10:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond();
        let j = |l: &[&str]| l.join("\n");
        assert_eq!(claude_turn(&j(&[user])), Some(Turn::Working));
        assert_eq!(
            claude_turn(&j(&[user, tool])),
            Some(Turn::Asking(t0 + 5000))
        );
        assert_eq!(claude_turn(&j(&[user, tool, result])), Some(Turn::Working));
        assert_eq!(
            claude_turn(&j(&[user, tool, result, reply])),
            Some(Turn::Done(t0 + 9000))
        );
        assert_eq!(
            claude_turn(&j(&[user, tool, result, reply, duration, noise, side])),
            Some(Turn::Done(t0 + 10_000))
        );
        assert_eq!(claude_turn("not json\n{}"), None);
    }

    #[test]
    fn a_codex_rollout_tells_its_turn_from_task_events() {
        let ev = |t: &str, ts: &str| {
            format!(r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"{t}"}}}}"#)
        };
        let item = r#"{"timestamp":"2026-11-26T10:00:09Z","type":"response_item","payload":{"type":"message"}}"#;
        let t0 = "2026-11-26T10:00:08Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond();
        let started = ev("task_started", "2026-11-26T10:00:00Z");
        let done = ev("task_complete", "2026-11-26T10:00:08Z");
        let tokens = ev("token_count", "2026-11-26T10:00:09Z");
        assert_eq!(codex_turn(&started), Some(Turn::Working));
        assert_eq!(
            codex_turn(&[started.as_str(), &done, &tokens, item].join("\n")),
            Some(Turn::Done(t0))
        );
        let aborted = ev("turn_aborted", "2026-11-26T10:00:09Z");
        assert_eq!(
            codex_turn(&[started, aborted].join("\n")),
            Some(Turn::Stopped)
        );
    }

    #[test]
    fn an_agent_without_records_is_announced_once_when_it_goes_quiet_unless_you_look() {
        let mut cfg = Config::default();
        cfg.notifications.agent_quiet_secs = 20;
        let s = 1000;
        let mut w = Watch::new(0);
        // An agent with no records of its own (gemini): busy for a minute, then quiet.
        let mut t = 0;
        while t <= 60 * s {
            assert!(
                w.check(&cfg, &[live(42, "gemini", true)], None, t)
                    .is_empty()
            );
            t += 5 * s;
        }
        let quiet = |w: &mut Watch, t: i64, looking| {
            w.check(&cfg, &[live(42, "gemini", false)], looking, t)
        };
        assert!(quiet(&mut w, 65 * s, None).is_empty());
        assert!(quiet(&mut w, 80 * s, None).is_empty());
        let n = quiet(&mut w, 86 * s, None);
        assert_eq!(n.len(), 1, "{n:?}");
        assert_eq!(n[0].summary, "Gemini CLI stopped working");
        assert_eq!(n[0].body, "In atlas, after 1m of work.");
        assert_eq!(n[0].route, "focus:42");
        // Once.
        assert!(quiet(&mut w, 120 * s, None).is_empty());
        // Works again, then goes quiet while you watch: nothing.
        for k in 0..5 {
            w.check(&cfg, &[live(42, "gemini", true)], None, 130 * s + k * 5 * s);
        }
        let you = Some(("gemini", "~/code/atlas"));
        assert!(quiet(&mut w, 160 * s, you).is_empty());
        assert!(quiet(&mut w, 190 * s, you).is_empty());
        assert!(quiet(&mut w, 220 * s, None).is_empty());
        // A process that ended is forgotten.
        assert!(w.check(&cfg, &[], None, 230 * s).is_empty());
        assert!(w.insts.is_empty());
    }
}
