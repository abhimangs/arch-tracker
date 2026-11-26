//! Made-up activity for screenshots and for trying the dashboard: `arch-tracker demo --db FILE`
//! writes a fresh database with a week of plausible days. Every name and title is invented, and
//! it never writes into your own database.

use crate::model::{Kind, Subject};
use crate::store::Store;
use crate::util;
use anyhow::{Result, bail};
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;

const MIN: i64 = 60_000;
/// A stretch of the day (start and end, from midnight) and what fills it.
type Block = (i64, i64, &'static [(u32, Act)]);
const HOUR: i64 = 60 * MIN;

/// A tiny deterministic generator, so the screenshots come out the same every time.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// A whole number of minutes in [lo, hi].
    fn mins(&mut self, lo: i64, hi: i64) -> i64 {
        (lo + (self.next() % (hi - lo + 1) as u64) as i64) * MIN
    }
    fn pick<'a, T>(&mut self, items: &'a [(u32, T)]) -> &'a T {
        let total: u32 = items.iter().map(|(w, _)| w).sum();
        let mut r = (self.next() % total as u64) as u32;
        for (w, t) in items {
            if r < *w {
                return t;
            }
            r -= w;
        }
        &items[0].1
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Nvim(&'static str),
    Claude(&'static str),
    Codex(&'static str),
    Cargo(&'static str),
    Zed(&'static str, &'static str),
    Site(&'static str, &'static str),
    Video(&'static str, &'static str),
    /// A site in the second browser.
    Zen(&'static str, &'static str),
    Private,
    /// The installed web app (music in a browser's app window).
    Music,
    Slack,
    Notes,
    Files,
}

struct Writer<'a> {
    store: &'a Store,
    ids: HashMap<Subject, i64>,
    next: i64,
    next_request: i64,
}

impl Writer<'_> {
    fn subject(&mut self, s: Subject) -> Result<i64> {
        if let Some(id) = self.ids.get(&s) {
            return Ok(*id);
        }
        self.next += 1;
        self.store.conn.execute(
            "INSERT INTO subjects(id, app, app_name, title, host, private, program, project)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                self.next, s.app, s.app_name, s.title, s.host, s.private, s.program, s.project
            ],
        )?;
        self.ids.insert(s, self.next);
        Ok(self.next)
    }

    /// Made-up requests while an agent works: a few a minute, mostly read from the cache.
    fn requests(&mut self, tool: &str, project: &str, a: i64, b: i64, rng: &mut Rng) -> Result<()> {
        let folder = format!("~/code/{project}");
        let mut t = a;
        while t < b {
            self.next_request += 1;
            let id = self.next_request;
            let context = 40_000 + (rng.next() % 90_000) as i64;
            let fresh = 1_500 + (rng.next() % 6_000) as i64;
            let out = 300 + (rng.next() % 2_500) as i64;
            if tool == "claude" {
                let model = if rng.next().is_multiple_of(4) {
                    "claude-sonnet-5-5"
                } else {
                    "claude-opus-5-5"
                };
                self.store.conn.execute(
                    "INSERT INTO claude_usage(id, at_ms, model, project, input, output, cache_5m,
                         cache_1h, cache_read, web_search, fast, us_only)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, 0, 0, 0)",
                    params![id, t, model, folder, 6, out, fresh, context],
                )?;
            } else {
                self.store.conn.execute(
                    "INSERT INTO codex_usage(id, at_ms, model, project, input, cached, cache_write,
                         output, reasoning, fast)
                     VALUES (?1, ?2, 'gpt-6.2-astra', ?3, ?4, ?5, 0, ?6, ?7, 0)",
                    params![id, t, folder, context + fresh, context, out, out / 3],
                )?;
            }
            t += 25_000 + (rng.next() % 70_000) as i64;
        }
        Ok(())
    }

    fn span(&self, kind: Kind, subject: i64, a: i64, b: i64) -> Result<()> {
        if b > a {
            self.store.conn.execute(
                "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (?1, ?2, ?3, ?4)",
                params![kind as i64, subject, a, b],
            )?;
        }
        Ok(())
    }
}

fn app(app: &str, name: &str) -> Subject {
    Subject::app(app, name)
}

fn site(host: &str, title: &str) -> Subject {
    Subject {
        host: host.into(),
        title: title.into(),
        ..app("brave-browser", "Brave")
    }
}

/// An installed web app's id, the way Chromium names them (all made up).
const MUSIC_APP: &str = "brave-cfmgknoapdifhejmkaplbaogdbnilfjk-Default";

fn music() -> Subject {
    Subject {
        host: "music.youtube.com".into(),
        title: "Focus mix".into(),
        ..app(MUSIC_APP, "YouTube Music")
    }
}

fn term(program: &str, project: &str) -> Subject {
    Subject {
        program: program.into(),
        project: format!("~/code/{project}"),
        ..app("kitty", "kitty")
    }
}

fn subject_of(a: Act) -> Subject {
    match a {
        Act::Nvim(p) => term("nvim", p),
        Act::Claude(p) => term("claude", p),
        Act::Codex(p) => term("codex", p),
        Act::Cargo(p) => term("cargo", p),
        Act::Zed(p, file) => Subject {
            title: format!("{p} — {file}"),
            ..app("dev.zed.Zed", "Zed")
        },
        Act::Site(host, title) | Act::Video(host, title) => site(host, title),
        Act::Zen(host, title) => Subject {
            host: host.into(),
            title: title.into(),
            ..app("zen", "Zen")
        },
        Act::Private => Subject {
            private: true,
            ..app("brave-browser", "Brave")
        },
        Act::Music => music(),
        Act::Slack => Subject {
            title: "#release-train".into(),
            ..app("slack", "Slack")
        },
        Act::Notes => Subject {
            title: "Weekly review".into(),
            ..app("md.obsidian.Obsidian", "Obsidian")
        },
        Act::Files => Subject {
            title: "Downloads".into(),
            ..app("org.kde.dolphin", "Dolphin")
        },
    }
}

const WORK: &[(u32, Act)] = &[
    (14, Act::Nvim("atlas")),
    (10, Act::Claude("atlas")),
    (6, Act::Cargo("atlas")),
    (12, Act::Zed("atlas", "src/report.rs")),
    (6, Act::Zed("ember", "src/lib.rs")),
    (5, Act::Nvim("ember")),
    (4, Act::Claude("ember")),
    (5, Act::Codex("ember")),
    (3, Act::Codex("atlas")),
    (4, Act::Zed("notes-site", "content/now.md")),
    (
        7,
        Act::Site("github.com", "Pull request #142: Parse snapshots lazily"),
    ),
    (5, Act::Site("docs.rs", "tokio::sync::watch - Rust")),
    (3, Act::Site("claude.ai", "Designing a retry policy")),
    (3, Act::Site("linear.app", "ATL-88 Session timeline")),
    (2, Act::Site("gist.github.com", "retry.rs")),
    (2, Act::Site("localhost:5173", "Atlas dev server")),
    (1, Act::Music),
    (4, Act::Slack),
    (2, Act::Notes),
    (1, Act::Files),
];

const EVENING: &[(u32, Act)] = &[
    (
        5,
        Act::Video("www.youtube.com", "Rust in production, a field report"),
    ),
    (4, Act::Site("news.ycombinator.com", "Hacker News")),
    (3, Act::Site("www.reddit.com", "r/rust")),
    (
        3,
        Act::Zen("en.wikipedia.org", "Rust (programming language)"),
    ),
    (2, Act::Zen("lobste.rs", "Lobsters")),
    (1, Act::Private),
    (2, Act::Slack),
    (3, Act::Nvim("notes-site")),
];

pub fn run(db: &Path, days: u32) -> Result<()> {
    // Only ever a new file: an existing database (your own included) is never touched.
    if db.exists() {
        bail!("{} already exists; pass a new file", db.display());
    }
    let store = Store::open(db)?;
    let mut w = Writer {
        store: &store,
        ids: HashMap::new(),
        next: 0,
        next_request: 0,
    };
    let now = util::now_ms();
    let flag = 0;
    let spotify = w.subject(Subject {
        title: "Deep Focus".into(),
        ..app("spotify", "Spotify")
    })?;
    let running: Vec<i64> = [
        ("spotify", "Spotify"),
        ("slack", "Slack"),
        ("discord", "Discord"),
    ]
    .iter()
    .map(|(a, n)| w.subject(app(a, n)))
    .collect::<Result<_>>()?;
    // The extension connected from both browsers; the web app runs in Brave.
    let first = now - days as i64 * 24 * HOUR;
    store.note_browser("brave-browser", "Brave", first)?;
    store.note_browser("zen", "Zen", first + 3 * 24 * HOUR)?;
    store.note_browser("brave-browser", "Brave", now)?;
    store.note_web_app(MUSIC_APP, "brave-browser")?;
    let tunes = w.subject(music())?;
    store.conn.execute_batch("BEGIN")?;
    for back in (0..days as i64).rev() {
        let date = util::local_date(now - back * 24 * HOUR);
        let (_, ds, _) = crate::report::parse_date(&date)?;
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (ds as u64));
        let weekend = back % 7 == 2 || back % 7 == 3;
        let blocks: &[Block] = if weekend {
            &[
                (10 * HOUR + 20 * MIN, 12 * HOUR + 10 * MIN, WORK),
                (19 * HOUR, 22 * HOUR + 30 * MIN, EVENING),
            ]
        } else {
            &[
                (8 * HOUR + 40 * MIN, 12 * HOUR + 30 * MIN, WORK),
                (13 * HOUR + 20 * MIN, 18 * HOUR + 20 * MIN, WORK),
                (20 * HOUR + 30 * MIN, 22 * HOUR + 30 * MIN, EVENING),
            ]
        };
        let day_end = (now - 2 * MIN).min(ds + 23 * HOUR);
        for (a, b) in [(ds + 8 * HOUR + 30 * MIN, ds + 23 * HOUR)] {
            for r in &running {
                w.span(Kind::Running, *r, a, b.min(day_end))?;
            }
        }
        if !weekend {
            w.span(
                Kind::Locked,
                flag,
                ds + 12 * HOUR + 30 * MIN,
                (ds + 13 * HOUR + 20 * MIN).min(day_end),
            )?;
        }
        for (from, to, pool) in blocks {
            let (mut t, end) = (ds + from, (ds + to).min(day_end));
            let mut last = None;
            while t < end {
                let mut act = *rng.pick(pool);
                if Some(act) == last {
                    act = *rng.pick(pool);
                }
                last = Some(act);
                let len = match act {
                    Act::Nvim(_) | Act::Zed(..) => rng.mins(8, 34),
                    Act::Claude(_) | Act::Codex(_) => rng.mins(5, 22),
                    Act::Video(..) => rng.mins(12, 32),
                    Act::Cargo(_) | Act::Files => rng.mins(1, 3),
                    Act::Slack => rng.mins(1, 6),
                    _ => rng.mins(2, 11),
                };
                // Real stretches don't start on the minute.
                t += (rng.next() % 59_000) as i64;
                let stop = (t + len + (rng.next() % 59_000) as i64).min(end);
                let id = w.subject(subject_of(act))?;
                w.span(Kind::Focus, id, t, stop)?;
                match act {
                    Act::Claude(p) | Act::Codex(p) => {
                        // The agent keeps working after you look away.
                        let (tool, name) = match act {
                            Act::Claude(_) => ("claude", "Claude Code"),
                            _ => ("codex", "Codex"),
                        };
                        let busy = w.subject(Subject {
                            program: tool.into(),
                            project: format!("~/code/{p}"),
                            ..app(&format!("agent:{tool}"), name)
                        })?;
                        let until = (stop + rng.mins(4, 18)).min(day_end);
                        w.span(Kind::AgentBusy, busy, t + MIN, until)?;
                        w.requests(tool, p, t + MIN, until, &mut rng)?;
                    }
                    Act::Video(..) => {
                        w.span(Kind::MediaVisible, id, t, stop)?;
                        w.span(Kind::InputIdle, flag, t + 2 * MIN, stop - MIN)?;
                    }
                    Act::Zed(..) => {
                        let docs = w.subject(site("docs.rs", "tokio::sync::watch - Rust"))?;
                        w.span(Kind::Visible, docs, t, stop)?;
                        w.span(Kind::BgAudio, spotify, t, stop)?;
                    }
                    // Music plays from Spotify, or from the web app in the background.
                    Act::Nvim(_) if rng.next().is_multiple_of(3) => {
                        w.span(Kind::BgAudio, tunes, t, stop)?
                    }
                    Act::Nvim(_) => w.span(Kind::BgAudio, spotify, t, stop)?,
                    _ => {}
                }
                // A short break now and then (still at the desk).
                t = stop
                    + if rng.next().is_multiple_of(5) {
                        rng.mins(1, 6)
                    } else {
                        0
                    };
            }
        }
    }
    store.conn.execute_batch("COMMIT")?;
    // Codex's plan limits, as it would report them.
    let limits = serde_json::json!({
        "at_ms": now - 20 * MIN, "plan": "plus",
        "windows": [
            {"minutes": 300, "used_percent": 38.0, "resets_ms": now + 2 * HOUR + 14 * MIN},
            {"minutes": 10080, "used_percent": 21.0, "resets_ms": now + 3 * 24 * HOUR},
        ],
    });
    store.set_meta("codex_limits", Some(&limits.to_string()))?;
    // An older Codex conversation whose log Codex has since removed: only its total is known.
    let at = now - 2 * 24 * HOUR - 5 * HOUR;
    store.conn.execute(
        "INSERT INTO codex_threads(key, created_ms, updated_ms, model, project, tokens, turn_ms)
         VALUES (1, ?1, ?2, 'gpt-6.1-sol', '~/code/atlas', 6400000, ?3)",
        params![at, at + 50 * MIN, 40 * MIN],
    )?;
    for (a, b) in [(at, at + 25 * MIN), (at + 35 * MIN, at + 50 * MIN)] {
        store.conn.execute(
            "INSERT INTO codex_turns(thread, start_ms, end_ms) VALUES (1, ?1, ?2)",
            params![a, b],
        )?;
    }
    println!("Wrote {days} days of made-up activity to {}", db.display());
    Ok(())
}
