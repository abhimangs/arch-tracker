//! Turns raw spans into screen time, using the *current* settings. Nothing here is stored as a
//! decision: change a threshold and every day is re-classified.
//!
//! Per second of the day:
//!   excluded  locked / suspended / paused
//!   idle      inside a no-input gap >= idle threshold, with no visible media and no qualifying call
//!   passive   inside such a gap while media plays in a visible window
//!   call      microphone open (and, inside a gap, the gap is < the call threshold)
//!   active    everything else

use crate::config::{Category, Config};
use crate::model::{Kind, Span, Subject};
use crate::store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const BUCKET_SECS: usize = 300;

/// No stored span is longer than this: the ledger splits spans at local midnight (a DST day has
/// 25 hours). Bounds how far back a day query looks for spans that reach into the day.
const MAX_SPAN_MS: i64 = 26 * 3_600_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum State {
    Active = 1,
    Passive = 2,
    Call = 3,
    Idle = 4,
    Excluded = 5,
}

pub struct Rules {
    pub idle_ms: i64,
    pub call_ms: i64,
    pub media_keeps_active: bool,
    pub inhibited_passive: bool,
}

impl Rules {
    pub fn from(cfg: &Config) -> Rules {
        Rules {
            idle_ms: cfg.idle.threshold_secs as i64 * 1000,
            call_ms: cfg.idle.call_threshold_secs as i64 * 1000,
            media_keeps_active: cfg.idle.media_keeps_active,
            inhibited_passive: cfg.idle.count_inhibited_as_passive,
        }
    }
}

#[derive(Default, Clone, Copy, Serialize, Deserialize, Debug, PartialEq)]
pub struct Times {
    pub active: i64,
    pub passive: i64,
    pub call: i64,
}

impl Times {
    pub fn total(&self) -> i64 {
        self.active + self.passive + self.call
    }
    fn add(&mut self, st: State, n: i64) {
        match st {
            State::Active => self.active += n,
            State::Passive => self.passive += n,
            State::Call => self.call += n,
            _ => {}
        }
    }
    fn merge(&mut self, o: &Times) {
        self.active += o.active;
        self.passive += o.passive;
        self.call += o.call;
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TitleRow {
    pub title: String,
    pub secs: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Row {
    pub key: String,
    pub name: String,
    pub category: String,
    pub color: String,
    #[serde(flatten)]
    pub times: Times,
    pub total: i64,
    pub visible: i64,
    #[serde(default)]
    pub titles: Vec<TitleRow>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AgentRow {
    pub tool: String,
    pub name: String,
    pub project: String,
    /// You had this agent's terminal focused.
    pub focus: i64,
    /// The agent was working (CPU / output), whether you watched or not.
    pub busy: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SimpleRow {
    pub name: String,
    pub detail: String,
    pub secs: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Bucket {
    pub start_ms: i64,
    /// Dominant state: "active", "passive", "call", "idle", "excluded" or "" (nothing).
    pub state: String,
    pub app: String,
    pub detail: String,
    pub category: String,
    pub color: String,
    pub counted: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Totals {
    #[serde(flatten)]
    pub times: Times,
    pub screen: i64,
    pub idle_removed: i64,
    pub visible: i64,
    pub background_audio: i64,
    pub agent_busy: i64,
}

/// One hour of the local clock. A day has 24, or 23 / 25 when the clocks change.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Hour {
    pub start_ms: i64,
    /// Local time it starts at, "HH:MM".
    pub label: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DayReport {
    pub date: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub totals: Totals,
    pub apps: Vec<Row>,
    pub sites: Vec<Row>,
    pub categories: Vec<Row>,
    pub programs: Vec<Row>,
    pub agents: Vec<AgentRow>,
    pub background_audio: Vec<SimpleRow>,
    pub running: Vec<SimpleRow>,
    /// Time per local hour; hourly[i] belongs to hours[i].
    pub hourly: Vec<Times>,
    pub hours: Vec<Hour>,
    pub timeline: Vec<Bucket>,
    /// Stretches on one app, site or project, to the millisecond.
    pub sessions: Vec<Session>,
    pub stats: DayStats,
    /// Coding time per project: terminals, editors and agents.
    pub projects: Vec<ProjectRow>,
}

/// A stretch of focus on one thing (an app; a site in a browser; a program and project in a
/// terminal; a project in an editor). Breaks shorter than insights.session_gap_secs don't end it.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Session {
    pub start_ms: i64,
    pub end_ms: i64,
    pub app: String,
    pub app_name: String,
    /// "site", "terminal", "editor" or "app".
    pub kind: String,
    /// Site host, terminal program, or empty.
    pub detail: String,
    pub project: String,
    /// The title seen longest during the session (empty for private windows and terminals).
    pub title: String,
    pub category: String,
    pub color: String,
    #[serde(flatten)]
    pub times: Times,
    pub counted: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DayStats {
    pub sessions: usize,
    /// Times the focused app changed (between counted sessions).
    pub switches: usize,
    pub longest: i64,
    pub longest_app: String,
    pub avg_session: i64,
    /// Time in sessions of at least insights.deep_work_mins, and how many.
    pub deep_work: i64,
    pub deep_sessions: usize,
    /// First and last counted moment of the day.
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    pub peak_hour: String,
    pub goal_secs: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ProjectRow {
    pub key: String,
    pub name: String,
    /// Focused time in its terminals and editors.
    pub total: i64,
    pub terminal: i64,
    pub editor: i64,
    /// Time an agent worked on it, wherever you were.
    pub agent_busy: i64,
    /// Time you had an agent's terminal in front of you (part of terminal).
    pub agent_focus: i64,
    pub programs: Vec<TitleRow>,
    pub editors: Vec<TitleRow>,
}

/// Project grouping rules from the config: aliases, ignored folders, editor apps.
struct ProjectRules<'a> {
    cfg: &'a crate::config::Projects,
    /// Whether a folder is a project (checked once per report).
    seen: std::cell::RefCell<HashMap<String, bool>>,
}

/// A terminal folder is a project when it is a git repository. Folders that still exist but
/// aren't one (home, ~/projects, /tmp) are not; a folder that is gone (another machine, deleted
/// since) keeps counting, since the tracker records the git root when there is one.
fn is_project_dir(path: &str) -> bool {
    let p = match path.strip_prefix('~') {
        Some(rest) => crate::util::home().join(rest.trim_start_matches('/')),
        None => std::path::PathBuf::from(path),
    };
    !p.is_dir() || p.join(".git").exists()
}

/// A path in the form the tracker stores: "~" for home, no trailing slash.
fn norm_path(p: &str) -> String {
    let p = p.trim().trim_end_matches('/');
    // Stored paths write the home folder as "~", so an absolute path is abbreviated the same way.
    let path = match p.strip_prefix('~') {
        Some(rest) => crate::util::home().join(rest.trim_start_matches('/')),
        None => std::path::PathBuf::from(p),
    };
    crate::util::abbreviate_home(&path)
}

impl<'a> ProjectRules<'a> {
    fn new(cfg: &'a crate::config::Projects) -> Self {
        ProjectRules {
            cfg,
            seen: Default::default(),
        }
    }

    fn is_editor(&self, s: &Subject) -> bool {
        self.cfg
            .editors
            .iter()
            .any(|e| e.eq_ignore_ascii_case(&s.app) || e.eq_ignore_ascii_case(&s.app_name))
    }

    /// (key, display name) for a terminal or agent folder; None when it isn't a project.
    fn of_path(&self, path: &str) -> Option<(String, String)> {
        let path = path.trim().trim_end_matches('/');
        if path.is_empty() {
            return None;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        // A rename always counts (you named it); otherwise ignored and non-git folders don't.
        if let Some(hit) = self.aliased(name, path) {
            return Some(hit);
        }
        if self
            .cfg
            .ignore
            .iter()
            .any(|i| norm_path(i) == norm_path(path))
        {
            return None;
        }
        // Inside a project folder the top-level subfolder is the project, git or not.
        if let Some(child) = self.folder_child(path) {
            let cname = child.rsplit('/').next().unwrap_or(&child).to_string();
            if let Some(hit) = self.aliased(&cname, &child) {
                return Some(hit);
            }
            if self.cfg.ignore.iter().any(|i| norm_path(i) == child) {
                return None;
            }
            return Some((cname.to_lowercase(), cname));
        }
        let project = *self
            .seen
            .borrow_mut()
            .entry(path.to_string())
            .or_insert_with(|| is_project_dir(path));
        project.then(|| (name.to_lowercase(), name.to_string()))
    }

    /// The project folder a path is strictly inside: its top-level subfolder, e.g.
    /// "~/projects/foo" for "~/projects/foo/src". None for the folder itself and other paths.
    fn folder_child(&self, path: &str) -> Option<String> {
        let p = norm_path(path);
        self.cfg.folders.iter().find_map(|f| {
            let f = norm_path(f);
            if f.is_empty() {
                return None;
            }
            let rest = p.strip_prefix(&f)?.strip_prefix('/')?;
            let first = rest.split('/').next().filter(|s| !s.is_empty())?;
            Some(format!("{f}/{first}"))
        })
    }

    /// (key, display name) for a bare project name (from an editor title).
    fn of_name(&self, name: &str) -> Option<(String, String)> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        Some(self.alias(name, name))
    }

    fn alias(&self, name: &str, path: &str) -> (String, String) {
        self.aliased(name, path)
            .unwrap_or_else(|| (name.to_lowercase(), name.to_string()))
    }

    fn aliased(&self, name: &str, path: &str) -> Option<(String, String)> {
        self.cfg.aliases.iter().find_map(|line| {
            let (from, to) = line.split_once('=')?;
            let from = from.trim().trim_end_matches('/');
            (norm_path(from) == norm_path(path) || from.eq_ignore_ascii_case(name))
                .then(|| (to.trim().to_lowercase(), to.trim().to_string()))
        })
    }
}

/// The project an editor window shows, from its title: "main.rs - arch-tracker - Visual Studio
/// Code", "arch-tracker – main.rs" (JetBrains), "arch-tracker — src/main.rs" (Zed). A segment
/// that names a project seen in a terminal wins; otherwise the segment next to a trailing brand,
/// else the last segment that is neither the editor's name nor a file.
const EDITOR_BRANDS: [&str; 16] = [
    "visual studio code",
    "visual studio code - insiders",
    "code - oss",
    "code - insiders",
    "vscodium",
    "cursor",
    "windsurf",
    "zed",
    "kate",
    "sublime text",
    "intellij idea",
    "pycharm",
    "clion",
    "rustrover",
    "webstorm",
    "goland",
];

pub fn editor_project(title: &str, app_name: &str, known: &HashSet<String>) -> String {
    // A brand at the end of the title ("main.rs - my-app - Code - OSS") is removed first, so its
    // words cannot be read as a project. Only whole suffixes count: "Code - OSS" is not "OSS".
    let mut t = title.trim_start_matches(['●', '•', '*', ' ']).trim_end();
    let mut brands: Vec<String> = EDITOR_BRANDS.iter().map(|b| b.to_string()).collect();
    brands.push(app_name.to_lowercase());
    brands.retain(|b| !b.is_empty());
    brands.sort_by_key(|b| std::cmp::Reverse(b.len()));
    // The title is only the editor's name: no folder or file is open.
    if brands.iter().any(|b| t.to_lowercase() == *b) {
        return String::new();
    }
    let mut stripped = false;
    'brand: for b in &brands {
        for sep in [" - ", " — ", " – ", " | "] {
            let suffix = format!("{sep}{b}");
            let Some(cut) = t.len().checked_sub(suffix.len()) else {
                continue;
            };
            if t.get(cut..).is_some_and(|s| s.to_lowercase() == suffix) {
                t = &t[..cut];
                stripped = true;
                break 'brand;
            }
        }
    }
    let mut parts: Vec<&str> = vec![t];
    for sep in [" - ", " — ", " – ", " | "] {
        parts = parts.iter().flat_map(|p| p.split(sep)).collect();
    }
    let app = app_name.to_lowercase();
    // Brands inside the title must match a whole segment: "cursor-tools" is a project.
    let brand = |s: &str| {
        let l = s.to_lowercase();
        l == app || EDITOR_BRANDS.contains(&l.as_str())
    };
    let looks_like_file = |s: &str| {
        s.contains('/')
            || s.rsplit_once('.').is_some_and(|(stem, ext)| {
                !stem.is_empty()
                    && (1..=6).contains(&ext.len())
                    && ext.chars().all(|c| c.is_ascii_alphanumeric())
            })
    };
    let segs: Vec<&str> = parts
        .iter()
        .map(|p| p.trim().trim_start_matches(['●', '•', '*', ' ']).trim())
        .filter(|p| !p.is_empty() && !brand(p))
        .collect();
    if let Some(k) = segs.iter().find(|s| known.contains(&s.to_lowercase())) {
        return k.to_string();
    }
    // The segment next to a stripped brand is the project ("Makefile - example.com - VS Code").
    if stripped
        && segs.len() >= 2
        && let Some(last) = segs.last().filter(|s| !s.contains('/'))
    {
        return last.to_string();
    }
    segs.iter()
        .rev()
        .find(|s| !looks_like_file(s) && s.chars().count() <= 80)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// The local clock hours in [ds, de). Steps to the next hour boundary on the local clock, so a
/// DST change skips or repeats an hour (and a half-hour shift yields a short hour).
pub fn hour_marks(ds: i64, de: i64, tz: &jiff::tz::TimeZone) -> Vec<Hour> {
    let mut out = Vec::new();
    let mut t = ds;
    while t < de && out.len() < 50 {
        let Ok(ts) = jiff::Timestamp::from_millisecond(t) else {
            break;
        };
        let dt = tz.to_datetime(ts);
        out.push(Hour {
            start_ms: t,
            label: format!("{:02}:{:02}", dt.hour(), dt.minute()),
        });
        let into_hour =
            (dt.minute() as i64 * 60 + dt.second() as i64) * 1000 + dt.millisecond() as i64;
        t += 3_600_000 - into_hour;
    }
    out
}

/// A request the caller got wrong (a bad date, a reversed range): the dashboard answers 400.
#[derive(Debug)]
pub struct BadRequest(pub String);

impl std::fmt::Display for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadRequest {}

/// Parses a local date written exactly like 2026-11-22; returns it and its [start, end) in ms.
pub fn parse_date(date: &str) -> Result<(jiff::civil::Date, i64, i64)> {
    let shaped = date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    let parsed = shaped
        .then(|| date.parse::<jiff::civil::Date>().ok())
        .flatten()
        .and_then(|d| crate::util::date_bounds(date).map(|(s, e)| (d, s, e)));
    parsed.ok_or_else(|| BadRequest(format!("{date:?} is not a date like 2026-11-22")).into())
}

/// Category of a subject: program rules, then the most specific host rule, then app rules.
pub fn categorize<'a>(cats: &'a [Category], s: &Subject) -> Option<&'a Category> {
    let lc = |x: &str| x.to_lowercase();
    if !s.program.is_empty()
        && let Some(c) = cats
            .iter()
            .find(|c| c.programs.iter().any(|p| lc(p) == lc(&s.program)))
    {
        return Some(c);
    }
    if !s.host.is_empty() {
        let host = host_key(&s.host);
        let mut best: Option<(&Category, usize)> = None;
        for c in cats {
            for h in &c.hosts {
                let h = host_key(h);
                if (host == h || host.ends_with(&format!(".{h}")))
                    && best.map(|(_, l)| h.len() > l).unwrap_or(true)
                {
                    best = Some((c, h.len()));
                }
            }
        }
        if let Some((c, _)) = best {
            return Some(c);
        }
    }
    let app = lc(&s.app);
    let name = lc(&s.app_name);
    if let Some(c) = cats
        .iter()
        .find(|c| c.apps.iter().any(|a| lc(a) == app || lc(a) == name))
    {
        return Some(c);
    }
    let browser = [
        "brave",
        "chrom",
        "firefox",
        "vivaldi",
        "edge",
        "opera",
        "librewolf",
        "helium",
        "thorium",
        "floorp",
        "waterfox",
        "zen-browser",
        "zen_browser",
    ]
    .iter()
    .any(|b| app.contains(b))
        || app == "zen";
    let fallback = if !s.host.is_empty() || s.private || browser {
        "Browsing"
    } else {
        "Other"
    };
    cats.iter().find(|c| c.name == fallback)
}

/// A host as rules compare it: lowercase, with only a trailing ":port" removed, so
/// "localhost:3000" is "localhost" while "brave://settings" and IPv6 addresses stay whole
/// ("[::1]:8080" becomes "::1").
fn host_key(host: &str) -> String {
    let h = host.trim().to_lowercase();
    if let Some(rest) = h.strip_prefix('[')
        && let Some((addr, tail)) = rest.split_once(']')
        && (tail.is_empty() || tail.strip_prefix(':').is_some_and(is_port))
    {
        return addr.to_string();
    }
    match h.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && is_port(port) => name.to_string(),
        _ => h,
    }
}

fn is_port(s: &str) -> bool {
    !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit())
}

fn cat_of(cats: &[Category], s: &Subject) -> (String, String) {
    categorize(cats, s)
        .map(|c| (c.name.clone(), c.color.clone()))
        .unwrap_or_else(|| ("Other".into(), "#64748b".into()))
}

/// Classifies every second of [ds, de).
pub fn classify(spans: &[Span], ds: i64, de: i64, r: &Rules) -> Vec<State> {
    let n = ((de - ds) / 1000).max(0) as usize;
    let sec = |ms: i64| (((ms - ds) / 1000).clamp(0, n as i64)) as usize;
    let mut media = vec![false; n];
    let mut mic = vec![false; n];
    let mut inhib = vec![false; n];
    let mut state = vec![State::Active; n];
    let mut gaps: Vec<(i64, i64)> = vec![];
    for s in spans {
        let (a, b) = (sec(s.start), sec(s.end));
        match s.kind {
            Kind::MediaVisible => media[a..b].fill(true),
            Kind::Mic => mic[a..b].fill(true),
            Kind::Inhibited => inhib[a..b].fill(true),
            Kind::InputIdle => gaps.push((s.start, s.end)),
            _ => {}
        }
    }
    for (i, m) in mic.iter().enumerate() {
        if *m {
            state[i] = State::Call;
        }
    }
    for (gs, ge) in join_gaps(gaps) {
        let len = ge - gs;
        if len < r.idle_ms {
            continue; // short pause: still active
        }
        for i in sec(gs)..sec(ge) {
            state[i] = if r.media_keeps_active && media[i] {
                State::Passive
            } else if mic[i] && len < r.call_ms {
                State::Call
            } else if r.inhibited_passive && inhib[i] {
                State::Passive
            } else {
                State::Idle
            };
        }
    }
    for s in spans {
        if matches!(s.kind, Kind::Locked | Kind::Suspended | Kind::Paused) {
            state[sec(s.start)..sec(s.end)].fill(State::Excluded);
        }
    }
    state
}

/// Joins no-input pieces that touch or overlap: the ledger cuts a gap in two at midnight (and a
/// restart can do the same), but the threshold applies to the whole gap.
fn join_gaps(mut gaps: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    gaps.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(gaps.len());
    for (s, e) in gaps {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

#[derive(Default)]
struct Acc {
    times: Times,
    visible: i64,
    titles: HashMap<String, i64>,
    name: String,
    category: (String, String),
}

fn rows(map: HashMap<String, Acc>, max_titles: usize) -> Vec<Row> {
    let mut out: Vec<Row> = map
        .into_iter()
        .map(|(key, a)| {
            let mut titles: Vec<TitleRow> = a
                .titles
                .into_iter()
                .filter(|(t, s)| !t.is_empty() && *s > 0)
                .map(|(title, secs)| TitleRow { title, secs })
                .collect();
            titles.sort_by_key(|x| std::cmp::Reverse(x.secs));
            titles.truncate(max_titles);
            Row {
                key,
                name: a.name,
                category: a.category.0,
                color: a.category.1,
                total: a.times.total(),
                times: a.times,
                visible: a.visible,
                titles,
            }
        })
        .filter(|r| r.total > 0 || r.visible > 0)
        .collect();
    out.sort_by(|a, b| b.total.cmp(&a.total).then(b.visible.cmp(&a.visible)));
    out
}

pub fn build_day(conn: &Connection, cfg: &Config, date: &str) -> Result<DayReport> {
    let (_, ds, de) = parse_date(date)?;
    build(conn, cfg, date, ds, de)
}

fn build(conn: &Connection, cfg: &Config, date: &str, ds: i64, de: i64) -> Result<DayReport> {
    let rules = Rules::from(cfg);
    let spans = day_spans(conn, ds, de, rules.idle_ms.max(rules.call_ms))?;
    let ids: Vec<i64> = spans.iter().map(|s| s.subject).collect();
    let subjects = store::subjects_by_id(conn, &ids)?;
    Ok(compute(cfg, date, ds, de, &spans, &subjects))
}

/// Every span that overlaps [ds, de), clipped to it, plus the no-input pieces up to pad beyond
/// either edge (unclipped): a gap that runs over midnight is judged by its whole length, and pad
/// (the longest threshold) is as far as that length can matter.
fn day_spans(conn: &Connection, ds: i64, de: i64, pad: i64) -> Result<Vec<Span>> {
    let mut out = store::spans_in(conn, ds, de)?;
    // Before the day: spans that reach into it, and the tail of a gap that ends at midnight.
    // After it: the head of a gap that started before midnight.
    let mut stmt = conn.prepare_cached(
        "SELECT kind, subject_id, start_ms, end_ms FROM spans
         WHERE (start_ms >= ?1 AND start_ms < ?2 AND end_ms > ?3)
            OR (kind = ?4 AND start_ms >= ?5 AND start_ms < ?6)",
    )?;
    let idle = Kind::InputIdle as i64;
    let args = params![ds - pad - MAX_SPAN_MS, ds, ds - pad, idle, de, de + pad];
    let rows = stmt.query_map(args, |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (k, subject, start, end) = row?;
        let Some(kind) = Kind::from_i64(k) else {
            continue;
        };
        if kind == Kind::InputIdle {
            out.push(Span {
                kind,
                subject,
                start,
                end,
            });
        } else if start < ds && end > ds {
            out.push(Span {
                kind,
                subject,
                start: ds,
                end: end.min(de),
            });
        }
    }
    Ok(out)
}

pub fn compute(
    cfg: &Config,
    date: &str,
    ds: i64,
    de: i64,
    spans: &[Span],
    subjects: &HashMap<i64, Subject>,
) -> DayReport {
    let tz = jiff::tz::TimeZone::system();
    compute_in(cfg, date, ds, de, spans, subjects, &tz)
}

fn compute_in(
    cfg: &Config,
    date: &str,
    ds: i64,
    de: i64,
    spans: &[Span],
    subjects: &HashMap<i64, Subject>,
    tz: &jiff::tz::TimeZone,
) -> DayReport {
    let rules = Rules::from(cfg);
    let state = classify(spans, ds, de, &rules);
    let n = state.len();
    let sec = |ms: i64| (((ms - ds) / 1000).clamp(0, n as i64)) as usize;
    let cats = &cfg.categories;
    let tool_ids: Vec<&str> = cfg.agents.tools.iter().map(|t| t.id.as_str()).collect();
    let tool_name = |id: &str| {
        cfg.agents
            .tools
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.name.clone())
            .unwrap_or(id.to_string())
    };

    let mut totals = Totals::default();
    let mut apps: HashMap<String, Acc> = HashMap::new();
    let mut sites: HashMap<String, Acc> = HashMap::new();
    let mut categories: HashMap<String, Acc> = HashMap::new();
    let mut programs: HashMap<String, Acc> = HashMap::new();
    let mut agents: HashMap<(String, String), AgentRow> = HashMap::new();
    let mut bg: HashMap<(String, String), i64> = HashMap::new();
    let mut running: HashMap<String, i64> = HashMap::new();
    let hours = hour_marks(ds, de, tz);
    let hour_secs: Vec<usize> = hours
        .iter()
        .map(|h| ((h.start_ms - ds) / 1000) as usize)
        .collect();
    let hour_of = |i: usize| hour_secs.partition_point(|&s| s <= i).saturating_sub(1);
    let mut hourly = vec![Times::default(); hours.len().max(1)];
    // timeline: per bucket, counted seconds per subject and state counts
    let nb = n.div_ceil(BUCKET_SECS);
    let mut bucket_subj: Vec<HashMap<i64, i64>> = vec![HashMap::new(); nb];
    let mut bucket_state: Vec<[i64; 6]> = vec![[0; 6]; nb];
    let mut focus_cover = vec![false; n];

    for s in spans {
        let Some(subj) = subjects.get(&s.subject) else {
            if s.kind != Kind::Focus {
                continue;
            }
            continue;
        };
        let (a, b) = (sec(s.start), sec(s.end));
        match s.kind {
            Kind::Focus => {
                let mut t = Times::default();
                for (i, st) in state.iter().enumerate().take(b).skip(a) {
                    focus_cover[i] = true;
                    t.add(*st, 1);
                    match st {
                        State::Idle => totals.idle_removed += 1,
                        State::Active | State::Passive | State::Call => {
                            hourly[hour_of(i)].add(*st, 1);
                            *bucket_subj[i / BUCKET_SECS].entry(s.subject).or_default() += 1;
                        }
                        _ => {}
                    }
                }
                totals.times.merge(&t);
                let cat = cat_of(cats, subj);
                let title = if subj.private {
                    String::new()
                } else {
                    subj.title.clone()
                };
                let app = apps.entry(subj.app.clone()).or_default();
                app.name = subj.app_name.clone();
                if app.category.0.is_empty() || !subj.host.is_empty() {
                    app.category = cat_of(cats, &Subject::app(&subj.app, &subj.app_name));
                }
                app.times.merge(&t);
                *app.titles.entry(title.clone()).or_default() += t.total();
                if !subj.host.is_empty() || subj.private {
                    let key = if subj.private {
                        "(private)".to_string()
                    } else {
                        subj.host.clone()
                    };
                    let site = sites.entry(key.clone()).or_default();
                    site.name = if subj.private {
                        "Private browsing".into()
                    } else {
                        key
                    };
                    site.category = cat.clone();
                    site.times.merge(&t);
                    *site.titles.entry(title.clone()).or_default() += t.total();
                }
                let c = categories.entry(cat.0.clone()).or_default();
                c.name = cat.0.clone();
                c.category = cat.clone();
                c.times.merge(&t);
                if !subj.program.is_empty() {
                    let key = format!("{}\u{1f}{}", subj.program, subj.project);
                    let p = programs.entry(key).or_default();
                    p.name = subj.program.clone();
                    p.category = cat.clone();
                    p.times.merge(&t);
                    *p.titles.entry(subj.project.clone()).or_default() += t.total();
                    if tool_ids.contains(&subj.program.as_str()) {
                        let ag = agents
                            .entry((subj.program.clone(), subj.project.clone()))
                            .or_insert_with(|| AgentRow {
                                tool: subj.program.clone(),
                                name: tool_name(&subj.program),
                                project: subj.project.clone(),
                                ..Default::default()
                            });
                        ag.focus += t.total();
                    }
                }
            }
            Kind::Visible => {
                let v = state[a..b]
                    .iter()
                    .filter(|s| matches!(s, State::Active | State::Passive | State::Call))
                    .count() as i64;
                totals.visible += v;
                apps.entry(subj.app.clone()).or_default().visible += v;
                if let Some(app) = apps.get_mut(&subj.app)
                    && app.name.is_empty()
                {
                    app.name = subj.app_name.clone();
                    app.category = cat_of(cats, &Subject::app(&subj.app, &subj.app_name));
                }
                if !subj.host.is_empty() {
                    let site = sites.entry(subj.host.clone()).or_default();
                    if site.name.is_empty() {
                        site.name = subj.host.clone();
                        site.category = cat_of(cats, subj);
                    }
                    site.visible += v;
                }
            }
            Kind::BgAudio => {
                let secs = state[a..b]
                    .iter()
                    .filter(|s| **s != State::Excluded)
                    .count() as i64;
                totals.background_audio += secs;
                let detail = if subj.host.is_empty() {
                    subj.title.clone()
                } else {
                    subj.host.clone()
                };
                *bg.entry((subj.app_name.clone(), detail)).or_default() += secs;
            }
            Kind::AgentBusy => {
                let secs = (b - a) as i64;
                totals.agent_busy += secs;
                let ag = agents
                    .entry((subj.program.clone(), subj.project.clone()))
                    .or_insert_with(|| AgentRow {
                        tool: subj.program.clone(),
                        name: subj.app_name.clone(),
                        project: subj.project.clone(),
                        ..Default::default()
                    });
                ag.busy += secs;
            }
            Kind::Running => {
                *running.entry(subj.app_name.clone()).or_default() += (b - a) as i64;
            }
            _ => {}
        }
    }
    totals.screen = totals.times.total();

    for (i, st) in state.iter().enumerate() {
        let b = i / BUCKET_SECS;
        let k = if focus_cover[i] || *st == State::Excluded {
            *st as usize
        } else {
            0
        };
        bucket_state[b][k] += 1;
    }
    let timeline = (0..nb)
        .map(|b| {
            let counts = bucket_state[b];
            let names = ["", "active", "passive", "call", "idle", "excluded"];
            let (si, _) = counts
                .iter()
                .enumerate()
                .skip(1)
                .max_by_key(|(_, c)| **c)
                .unwrap_or((0, &0));
            let state_name = if counts[si] == 0 { "" } else { names[si] };
            let top = bucket_subj[b]
                .iter()
                .max_by_key(|(_, s)| **s)
                .map(|(id, s)| (*id, *s));
            let (app, detail, cat, counted) =
                match top.and_then(|(id, s)| subjects.get(&id).map(|x| (x, s))) {
                    Some((s, secs)) => {
                        let d = if !s.host.is_empty() {
                            s.host.clone()
                        } else if !s.program.is_empty() {
                            s.program.clone()
                        } else {
                            String::new()
                        };
                        (
                            s.app_name.clone(),
                            d,
                            cat_of(cats, s),
                            bucket_subj[b].values().sum::<i64>().max(secs),
                        )
                    }
                    None => (
                        String::new(),
                        String::new(),
                        (String::new(), String::new()),
                        0,
                    ),
                };
            Bucket {
                start_ms: ds + (b * BUCKET_SECS * 1000) as i64,
                state: state_name.into(),
                app,
                detail,
                category: cat.0,
                color: cat.1,
                counted,
            }
        })
        .collect();

    let (sessions, stats) = sessions_and_stats(cfg, ds, spans, subjects, &state, &hourly, &hours);
    let projects = projects_of(cfg, ds, spans, subjects, &state);

    let mut agents: Vec<AgentRow> = agents.into_values().collect();
    agents.sort_by_key(|x| std::cmp::Reverse(x.busy + x.focus));
    let mut background_audio: Vec<SimpleRow> = bg
        .into_iter()
        .map(|((name, detail), secs)| SimpleRow { name, detail, secs })
        .collect();
    background_audio.sort_by_key(|x| std::cmp::Reverse(x.secs));
    let mut running: Vec<SimpleRow> = running
        .into_iter()
        .map(|(name, secs)| SimpleRow {
            name,
            detail: String::new(),
            secs,
        })
        .collect();
    running.sort_by_key(|x| std::cmp::Reverse(x.secs));

    DayReport {
        date: date.to_string(),
        start_ms: ds,
        end_ms: de,
        totals,
        apps: rows(apps, 15),
        sites: rows(sites, 10),
        categories: rows(categories, 0),
        programs: rows(programs, 5),
        agents,
        background_audio,
        running,
        hourly,
        hours,
        timeline,
        sessions,
        stats,
        projects,
    }
}

/// Counted seconds of a focus span: (times, first counted ms, end of last counted ms).
fn counted_in(state: &[State], ds: i64, s: &Span) -> (Times, Option<(i64, i64)>) {
    let n = state.len() as i64;
    let a = ((s.start - ds) / 1000).clamp(0, n) as usize;
    // Floor, like compute_in: a partial last second is not counted, so adjacent spans never share one.
    let b = ((s.end - ds) / 1000).clamp(0, n) as usize;
    let mut t = Times::default();
    let (mut first, mut last) = (None, None);
    for (i, st) in state.iter().enumerate().take(b).skip(a) {
        if matches!(st, State::Active | State::Passive | State::Call) {
            t.add(*st, 1);
            first.get_or_insert(i);
            last = Some(i);
        }
    }
    let edges = first.zip(last).map(|(f, l)| {
        // Exact milliseconds where the span's own edges are counted, else second boundaries.
        let start = (ds + f as i64 * 1000).max(s.start);
        let end = (ds + (l as i64 + 1) * 1000).min(s.end);
        (start, end.max(start))
    });
    (t, edges)
}

fn sessions_and_stats(
    cfg: &Config,
    ds: i64,
    spans: &[Span],
    subjects: &HashMap<i64, Subject>,
    state: &[State],
    hourly: &[Times],
    hours: &[Hour],
) -> (Vec<Session>, DayStats) {
    let rules = ProjectRules::new(&cfg.projects);
    let known = known_projects(&rules, spans, subjects);
    let gap = cfg.insights.session_gap_secs as i64 * 1000;
    let mut focus: Vec<&Span> = spans.iter().filter(|s| s.kind == Kind::Focus).collect();
    focus.sort_by_key(|s| s.start);
    let mut out: Vec<Session> = vec![];
    let mut titles: Vec<HashMap<String, i64>> = vec![];
    for s in focus {
        let Some(subj) = subjects.get(&s.subject) else {
            continue;
        };
        let (t, Some((start, end))) = counted_in(state, ds, s) else {
            continue;
        };
        let project = if !subj.program.is_empty() {
            rules
                .of_path(&subj.project)
                .map(|p| p.1)
                .unwrap_or_default()
        } else if rules.is_editor(subj) {
            let p = editor_project(&subj.title, &subj.app_name, &known);
            rules.of_name(&p).map(|p| p.1).unwrap_or_default()
        } else {
            String::new()
        };
        let kind = if subj.private || !subj.host.is_empty() {
            "site"
        } else if !subj.program.is_empty() {
            "terminal"
        } else if rules.is_editor(subj) {
            "editor"
        } else {
            "app"
        };
        let detail = if subj.private {
            "Private window".to_string()
        } else if !subj.host.is_empty() {
            subj.host.clone()
        } else {
            subj.program.clone()
        };
        let same = out.last().is_some_and(|l| {
            l.app == subj.app
                && l.detail == detail
                && l.project == project
                && start - l.end_ms <= gap
        });
        if !same {
            let (category, color) = cat_of(&cfg.categories, subj);
            out.push(Session {
                start_ms: start,
                end_ms: end,
                app: subj.app.clone(),
                app_name: subj.app_name.clone(),
                kind: kind.into(),
                detail,
                project,
                category,
                color,
                ..Default::default()
            });
            titles.push(HashMap::new());
        }
        let l = out.last_mut().expect("just pushed");
        l.end_ms = l.end_ms.max(end);
        l.times.merge(&t);
        l.counted += t.total();
        if !subj.private && subj.program.is_empty() && !subj.title.is_empty() {
            *titles
                .last_mut()
                .expect("pushed with session")
                .entry(subj.title.clone())
                .or_default() += t.total();
        }
    }
    for (s, t) in out.iter_mut().zip(titles) {
        s.title = t
            .into_iter()
            .max_by_key(|(_, secs)| *secs)
            .map(|(title, _)| title)
            .unwrap_or_default();
    }
    let deep = cfg.insights.deep_work_mins as i64 * 60;
    let mut st = DayStats {
        sessions: out.len(),
        switches: out.windows(2).filter(|w| w[0].app != w[1].app).count(),
        first_ms: out.first().map(|s| s.start_ms),
        last_ms: out.last().map(|s| s.end_ms),
        goal_secs: cfg.insights.daily_goal_mins as i64 * 60,
        ..Default::default()
    };
    for s in &out {
        if s.counted > st.longest {
            st.longest = s.counted;
            st.longest_app = if s.project.is_empty() {
                s.app_name.clone()
            } else {
                format!("{} ({})", s.app_name, s.project)
            };
        }
        if s.counted >= deep {
            st.deep_work += s.counted;
            st.deep_sessions += 1;
        }
    }
    let total: i64 = out.iter().map(|s| s.counted).sum();
    st.avg_session = if out.is_empty() {
        0
    } else {
        total / out.len() as i64
    };
    st.peak_hour = hourly
        .iter()
        .enumerate()
        .filter(|(_, t)| t.total() > 0)
        .max_by_key(|(_, t)| t.total())
        .and_then(|(i, _)| hours.get(i).map(|h| h.label.clone()))
        .unwrap_or_default();
    (out, st)
}

/// Project names seen in terminals and agents today (lowercase), to recognise editor titles.
fn known_projects(
    rules: &ProjectRules,
    spans: &[Span],
    subjects: &HashMap<i64, Subject>,
) -> HashSet<String> {
    let mut seen: HashSet<i64> = HashSet::new();
    let mut out = HashSet::new();
    for s in spans {
        if !seen.insert(s.subject) {
            continue;
        }
        if let Some(subj) = subjects.get(&s.subject)
            && !subj.project.is_empty()
            && let Some((_, name)) = rules.of_path(&subj.project)
        {
            out.insert(name.to_lowercase());
        }
    }
    out
}

fn projects_of(
    cfg: &Config,
    ds: i64,
    spans: &[Span],
    subjects: &HashMap<i64, Subject>,
    state: &[State],
) -> Vec<ProjectRow> {
    let rules = ProjectRules::new(&cfg.projects);
    let known = known_projects(&rules, spans, subjects);
    let tools: HashSet<&str> = cfg.agents.tools.iter().map(|t| t.id.as_str()).collect();
    let mut rows: HashMap<String, ProjectRow> = HashMap::new();
    let mut programs: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut editors: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let n = state.len() as i64;
    for s in spans {
        let Some(subj) = subjects.get(&s.subject) else {
            continue;
        };
        match s.kind {
            Kind::Focus => {
                let (key, name, terminal) = if !subj.program.is_empty() {
                    match rules.of_path(&subj.project) {
                        Some((k, n)) => (k, n, true),
                        None => continue,
                    }
                } else if rules.is_editor(subj) {
                    let p = editor_project(&subj.title, &subj.app_name, &known);
                    match rules.of_name(&p) {
                        Some((k, n)) => (k, n, false),
                        None => continue,
                    }
                } else {
                    continue;
                };
                let (t, _) = counted_in(state, ds, s);
                let secs = t.total();
                if secs == 0 {
                    continue;
                }
                let r = rows.entry(key.clone()).or_insert_with(|| ProjectRow {
                    key: key.clone(),
                    name,
                    ..Default::default()
                });
                r.total += secs;
                if terminal {
                    r.terminal += secs;
                    if tools.contains(subj.program.as_str()) {
                        r.agent_focus += secs;
                    }
                    *programs
                        .entry(key)
                        .or_default()
                        .entry(subj.program.clone())
                        .or_default() += secs;
                } else {
                    r.editor += secs;
                    *editors
                        .entry(key)
                        .or_default()
                        .entry(subj.app_name.clone())
                        .or_default() += secs;
                }
            }
            Kind::AgentBusy => {
                if let Some((key, name)) = rules.of_path(&subj.project) {
                    let a = ((s.start - ds) / 1000).clamp(0, n);
                    let b = ((s.end - ds) / 1000).clamp(0, n);
                    let r = rows.entry(key.clone()).or_insert_with(|| ProjectRow {
                        key,
                        name,
                        ..Default::default()
                    });
                    r.agent_busy += b - a;
                }
            }
            _ => {}
        }
    }
    let top = |m: Option<HashMap<String, i64>>| {
        let mut v: Vec<TitleRow> = m
            .unwrap_or_default()
            .into_iter()
            .map(|(title, secs)| TitleRow { title, secs })
            .collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.secs));
        v.truncate(6);
        v
    };
    let mut out: Vec<ProjectRow> = rows
        .into_values()
        .map(|mut r| {
            r.programs = top(programs.remove(&r.key));
            r.editors = top(editors.remove(&r.key));
            r
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.total + r.agent_busy));
    out
}

/// Adds one day's project rows into a range's.
fn merge_projects(into: &mut HashMap<String, ProjectRow>, rows: &[ProjectRow]) {
    let add = |list: &mut Vec<TitleRow>, more: &[TitleRow]| {
        for t in more {
            match list.iter_mut().find(|x| x.title == t.title) {
                Some(x) => x.secs += t.secs,
                None => list.push(t.clone()),
            }
        }
    };
    for r in rows {
        let e = into.entry(r.key.clone()).or_insert_with(|| ProjectRow {
            key: r.key.clone(),
            name: r.name.clone(),
            ..Default::default()
        });
        e.total += r.total;
        e.terminal += r.terminal;
        e.editor += r.editor;
        e.agent_busy += r.agent_busy;
        e.agent_focus += r.agent_focus;
        add(&mut e.programs, &r.programs);
        add(&mut e.editors, &r.editors);
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DaySummary {
    pub date: String,
    pub totals: Totals,
    pub categories: Vec<Row>,
    /// Focused coding time (all projects).
    pub coding: i64,
    pub projects: Vec<ProjectRow>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RangeReport {
    pub from: String,
    pub to: String,
    /// "monday" or "sunday": how the page lays out weeks.
    pub week_start: String,
    pub days: Vec<DaySummary>,
    pub totals: Totals,
    pub apps: Vec<Row>,
    pub sites: Vec<Row>,
    pub categories: Vec<Row>,
    pub programs: Vec<Row>,
    pub agents: Vec<AgentRow>,
    pub projects: Vec<ProjectRow>,
}

fn merge_rows(into: &mut HashMap<String, Row>, rows: &[Row]) {
    for r in rows {
        let e = into.entry(r.key.clone()).or_insert_with(|| Row {
            key: r.key.clone(),
            name: r.name.clone(),
            category: r.category.clone(),
            color: r.color.clone(),
            ..Default::default()
        });
        e.times.merge(&r.times);
        e.total += r.total;
        e.visible += r.visible;
        for t in &r.titles {
            match e.titles.iter_mut().find(|x| x.title == t.title) {
                Some(x) => x.secs += t.secs,
                None => e.titles.push(t.clone()),
            }
        }
    }
}

fn finish(map: HashMap<String, Row>, max_titles: usize) -> Vec<Row> {
    let mut v: Vec<Row> = map.into_values().collect();
    for r in &mut v {
        r.titles.sort_by_key(|x| std::cmp::Reverse(x.secs));
        r.titles.truncate(max_titles);
    }
    v.sort_by_key(|x| std::cmp::Reverse(x.total));
    v
}

/// What a range needs from one day. The rollup cache keeps this rather than the whole day
/// report (288 timeline buckets, hourly totals, running apps), so a year of Trends stays small.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct DayRollup {
    totals: Totals,
    apps: Vec<Row>,
    sites: Vec<Row>,
    categories: Vec<Row>,
    programs: Vec<Row>,
    agents: Vec<AgentRow>,
    #[serde(default)]
    projects: Vec<ProjectRow>,
}

impl DayRollup {
    fn from_report(r: DayReport) -> DayRollup {
        let trim = |mut rows: Vec<Row>, n: usize| {
            for r in &mut rows {
                r.titles.truncate(n);
            }
            rows
        };
        DayRollup {
            totals: r.totals,
            apps: trim(r.apps, 10),
            sites: trim(r.sites, 10),
            categories: r.categories,
            programs: trim(r.programs, 5),
            agents: r.agents,
            projects: r.projects,
        }
    }
}

/// Whether range() may store the day rollups it computes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cache {
    ReadWrite,
    /// Read-only viewers (arch-tracker serve) never write to the database.
    ReadOnly,
}

/// Longest range, in days (about three years).
const MAX_RANGE_DAYS: i32 = 1200;

/// Inclusive date range (at most ~3 years).
pub fn range(
    conn: &Connection,
    cfg: &Config,
    from: &str,
    to: &str,
    cache: Cache,
) -> Result<RangeReport> {
    let (mut d, _, _) = parse_date(from)?;
    let (end, _, _) = parse_date(to)?;
    if end < d {
        return Err(BadRequest("'to' must not be before 'from'".into()).into());
    }
    if d.until(end)?.get_days() >= MAX_RANGE_DAYS {
        return Err(BadRequest(format!("a range can be at most {MAX_RANGE_DAYS} days")).into());
    }
    let mut out = RangeReport {
        from: from.into(),
        to: to.into(),
        week_start: cfg.general.week_start.clone(),
        ..Default::default()
    };
    let (mut apps, mut sites, mut cats, mut progs) = (
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    );
    let mut agents: HashMap<(String, String), AgentRow> = HashMap::new();
    let mut projects: HashMap<String, ProjectRow> = HashMap::new();
    let hash = settings_hash(cfg);
    // A no-input gap that started before midnight is judged with the part after it, so a day
    // is final once the longest threshold (and a flush) has passed.
    let pad = cfg.idle.threshold_secs.max(cfg.idle.call_threshold_secs) as i64 * 1000;
    let now = crate::util::now_ms();
    let mut fresh: Vec<(String, String)> = Vec::new();
    while d <= end {
        let date = d.to_string();
        let (_, ds, de) = parse_date(&date)?;
        let finished = now > de.saturating_add(pad + 10 * 60_000);
        let r = day_rollup(conn, cfg, &date, (ds, de), &hash, finished, &mut fresh)?;
        out.totals.times.merge(&r.totals.times);
        out.totals.screen += r.totals.screen;
        out.totals.idle_removed += r.totals.idle_removed;
        out.totals.visible += r.totals.visible;
        out.totals.background_audio += r.totals.background_audio;
        out.totals.agent_busy += r.totals.agent_busy;
        merge_rows(&mut apps, &r.apps);
        merge_rows(&mut sites, &r.sites);
        merge_rows(&mut cats, &r.categories);
        merge_rows(&mut progs, &r.programs);
        for a in &r.agents {
            let e = agents
                .entry((a.tool.clone(), a.project.clone()))
                .or_insert_with(|| AgentRow {
                    tool: a.tool.clone(),
                    name: a.name.clone(),
                    project: a.project.clone(),
                    ..Default::default()
                });
            e.focus += a.focus;
            e.busy += a.busy;
        }
        merge_projects(&mut projects, &r.projects);
        out.days.push(DaySummary {
            date,
            totals: r.totals,
            categories: r.categories,
            coding: r.projects.iter().map(|p| p.total).sum(),
            projects: r.projects,
        });
        d = d.tomorrow()?;
    }
    if cache == Cache::ReadWrite
        && !fresh.is_empty()
        && let Err(e) = store_rollups(conn, &hash, &fresh)
    {
        tracing::debug!("could not store day rollups: {e:#}");
    }
    out.apps = finish(apps, 10);
    out.sites = finish(sites, 10);
    out.categories = finish(cats, 0);
    out.programs = finish(progs, 5);
    let mut ag: Vec<AgentRow> = agents.into_values().collect();
    ag.sort_by_key(|x| std::cmp::Reverse(x.busy + x.focus));
    out.agents = ag;
    let mut pr: Vec<ProjectRow> = projects.into_values().collect();
    pr.sort_by_key(|r| std::cmp::Reverse(r.total + r.agent_busy));
    out.projects = pr;
    Ok(out)
}

/// One day of a range: nothing for a day without spans, the cached rollup for a finished day,
/// computed otherwise (finished days are queued in fresh for the cache).
fn day_rollup(
    conn: &Connection,
    cfg: &Config,
    date: &str,
    (ds, de): (i64, i64),
    hash: &str,
    finished: bool,
    fresh: &mut Vec<(String, String)>,
) -> Result<DayRollup> {
    let any: bool = conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM spans WHERE start_ms >= ?1 AND start_ms < ?2 AND end_ms > ?3)",
        )?
        .query_row(params![ds - MAX_SPAN_MS, de, ds], |r| r.get(0))?;
    if !any {
        return Ok(DayRollup::default());
    }
    if finished {
        let cached: Option<String> = conn
            .prepare_cached("SELECT json FROM rollups WHERE day=?1 AND settings_hash=?2")?
            .query_row(params![date, hash], |r| r.get(0))
            .optional()?;
        if let Some(r) = cached.and_then(|j| serde_json::from_str(&j).ok()) {
            return Ok(r);
        }
    }
    let r = DayRollup::from_report(build(conn, cfg, date, ds, de)?);
    if finished {
        fresh.push((date.to_string(), serde_json::to_string(&r)?));
    }
    Ok(r)
}

/// Stores rollups in one transaction and drops those made under other settings or versions.
fn store_rollups(conn: &Connection, hash: &str, rows: &[(String, String)]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM rollups WHERE settings_hash != ?1",
        params![hash],
    )?;
    {
        let mut ins = tx.prepare_cached(
            "INSERT OR REPLACE INTO rollups(day, settings_hash, json) VALUES (?1, ?2, ?3)",
        )?;
        for (day, json) in rows {
            ins.execute(params![day, hash, json])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Bump when DayRollup or the classification changes shape.
const ROLLUP_SCHEMA: u32 = 4;

/// Hash of everything that changes how a day is classified (for the rollup cache). The app
/// version is part of it: a new release may count differently, so it starts a fresh cache.
pub fn settings_hash(cfg: &Config) -> String {
    // Day bounds and hour marks follow the system zone, so a zone change makes cached days stale.
    let tz = jiff::tz::TimeZone::system();
    let zone = match tz.iana_name() {
        Some(name) => name.to_string(),
        None => format!("offset {}", tz.to_offset(jiff::Timestamp::now()).seconds()),
    };
    let v = serde_json::json!({
        "v": ROLLUP_SCHEMA,
        "app": env!("CARGO_PKG_VERSION"),
        "tz": zone,
        "idle": cfg.idle,
        "categories": cfg.categories,
        "tools": cfg.agents.tools,
        "projects": cfg.projects,
    });
    let s = v.to_string();
    // FNV-1a: stable across Rust versions (unlike DefaultHasher).
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_git_folders_are_projects() {
        let dir = std::env::temp_dir().join(format!("at-proj-{}", std::process::id()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let cfg = crate::config::Projects::default();
        let rules = ProjectRules::new(&cfg);
        let p = |x: &std::path::Path| x.to_string_lossy().into_owned();
        assert_eq!(
            rules.of_path(&p(&repo)).map(|x| x.1).as_deref(),
            Some("repo")
        );
        assert_eq!(
            rules.of_path(&p(&dir)),
            None,
            "a plain folder is not a project"
        );
        assert!(
            rules.of_path("/no/such/place/elsewhere").is_some(),
            "gone folders still count"
        );
        assert_eq!(rules.of_path("~"), None);
        let named = crate::config::Projects {
            aliases: vec![format!("{}=Scratch", p(&dir))],
            ..Default::default()
        };
        let rules = ProjectRules::new(&named);
        assert_eq!(
            rules.of_path(&p(&dir)).map(|x| x.1).as_deref(),
            Some("Scratch")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_folders_make_their_subfolders_projects() {
        let dir = std::env::temp_dir().join(format!("at-folders-{}", std::process::id()));
        let folder = dir.join("projects");
        std::fs::create_dir_all(folder.join("app/.git")).unwrap();
        std::fs::create_dir_all(folder.join("notes/src/bin")).unwrap();
        std::fs::create_dir_all(dir.join("loose")).unwrap();
        let p = |x: &std::path::Path| x.to_string_lossy().into_owned();
        let name = |r: &ProjectRules, x: &str| r.of_path(x).map(|x| x.1);
        let cfg = crate::config::Projects {
            folders: vec![p(&folder)],
            ..Default::default()
        };
        let rules = ProjectRules::new(&cfg);
        assert_eq!(
            name(&rules, &p(&folder.join("notes"))).as_deref(),
            Some("notes")
        );
        assert_eq!(
            name(&rules, &p(&folder.join("notes/src/bin"))).as_deref(),
            Some("notes"),
            "a deeper path maps to its top-level subfolder"
        );
        assert_eq!(
            name(&rules, &p(&folder.join("app/src"))).as_deref(),
            Some("app"),
            "a git repository inside is still the top-level subfolder"
        );
        assert_eq!(
            name(&rules, &p(&folder)),
            None,
            "the folder itself is not a project"
        );
        assert_eq!(
            name(&rules, &p(&dir.join("loose"))),
            None,
            "outside stays as before"
        );
        let ignored = crate::config::Projects {
            folders: vec![p(&folder)],
            ignore: vec![p(&folder.join("notes"))],
            ..Default::default()
        };
        assert_eq!(
            name(&ProjectRules::new(&ignored), &p(&folder.join("notes/src"))),
            None
        );
        let named = crate::config::Projects {
            folders: vec![p(&folder)],
            aliases: vec![format!("{}=Notes Book", p(&folder.join("notes")))],
            ..Default::default()
        };
        assert_eq!(
            name(&ProjectRules::new(&named), &p(&folder.join("notes/src"))).as_deref(),
            Some("Notes Book")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignored_and_renamed_folders_match_any_spelling_of_their_path() {
        let home = crate::util::home();
        let p = |x: &std::path::Path| x.to_string_lossy().into_owned();
        let ignored = crate::config::Projects {
            ignore: vec![p(&home.join("x"))],
            ..Default::default()
        };
        assert_eq!(ProjectRules::new(&ignored).of_path("~/x"), None);
        let renamed = crate::config::Projects {
            aliases: vec![format!("{}=Why", p(&home.join("y")))],
            ..Default::default()
        };
        assert_eq!(
            ProjectRules::new(&renamed)
                .of_path("~/y")
                .map(|x| x.1)
                .as_deref(),
            Some("Why")
        );
    }

    #[test]
    fn editor_titles_name_projects() {
        let none = HashSet::new();
        assert_eq!(
            editor_project(
                "main.rs - arch-tracker - Visual Studio Code",
                "Visual Studio Code",
                &none
            ),
            "arch-tracker"
        );
        assert_eq!(
            editor_project("● lib.rs - site - Cursor", "Cursor", &none),
            "site"
        );
        assert_eq!(
            editor_project("arch-tracker – main.rs", "RustRover", &none),
            "arch-tracker"
        );
        assert_eq!(
            editor_project("Welcome - Visual Studio Code", "Visual Studio Code", &none),
            "Welcome"
        );
        let known: HashSet<String> = ["api".to_string()].into();
        assert_eq!(editor_project("api — src/main.rs", "Zed", &known), "api");
        assert_eq!(editor_project("notes.md", "Kate", &none), "");
    }

    #[test]
    fn editor_brands_end_the_title() {
        let none = HashSet::new();
        let f = |t: &str, app: &str| editor_project(t, app, &none);
        assert_eq!(f("main.rs - my-app - Code - OSS", "Code - OSS"), "my-app");
        assert_eq!(
            f("main.rs - cursor-tools - Cursor", "Cursor"),
            "cursor-tools"
        );
        assert_eq!(
            f(
                "Makefile - example.com - Visual Studio Code",
                "Visual Studio Code"
            ),
            "example.com"
        );
        assert_eq!(f("main.rs - Visual Studio Code", "Visual Studio Code"), "");
        assert_eq!(f("Code - OSS", "Code - OSS"), "");
    }

    const DS: i64 = 1_000_000_000_000;
    const H: i64 = 3_600_000;
    const M: i64 = 60_000;

    fn sp(kind: Kind, subject: i64, a: i64, b: i64) -> Span {
        Span {
            kind,
            subject,
            start: DS + a,
            end: DS + b,
        }
    }

    fn rules() -> Rules {
        Rules {
            idle_ms: 3 * M,
            call_ms: 5 * M,
            media_keeps_active: true,
            inhibited_passive: false,
        }
    }

    fn count(st: &[State], want: State) -> i64 {
        st.iter().filter(|s| **s == want).count() as i64
    }

    #[test]
    fn short_gaps_stay_active_long_gaps_are_idle() {
        let spans = vec![
            sp(Kind::InputIdle, 0, 0, 2 * M),
            sp(Kind::InputIdle, 0, 10 * M, 20 * M),
        ];
        let st = classify(&spans, DS, DS + H, &rules());
        assert_eq!(count(&st, State::Idle), 600);
        assert_eq!(count(&st, State::Active), 3600 - 600);
    }

    #[test]
    fn visible_media_keeps_counting_as_passive() {
        let spans = vec![
            sp(Kind::InputIdle, 0, 0, 30 * M),
            sp(Kind::MediaVisible, 7, 0, 20 * M),
        ];
        let st = classify(&spans, DS, DS + H, &rules());
        assert_eq!(count(&st, State::Passive), 20 * 60);
        assert_eq!(count(&st, State::Idle), 10 * 60);
    }

    #[test]
    fn calls_tolerate_five_minutes_then_stop() {
        let spans = vec![
            sp(Kind::Mic, 0, 0, H),
            sp(Kind::InputIdle, 0, 0, 4 * M), // 4 min no input: still a call
            sp(Kind::InputIdle, 0, 10 * M, 16 * M), // 6 min no input: not counted
        ];
        let st = classify(&spans, DS, DS + H, &rules());
        assert_eq!(count(&st, State::Idle), 6 * 60);
        assert_eq!(count(&st, State::Call), 3600 - 6 * 60);
    }

    #[test]
    fn lock_and_suspend_are_excluded() {
        let spans = vec![
            sp(Kind::Locked, 0, 0, 10 * M),
            sp(Kind::Suspended, 0, 30 * M, 40 * M),
        ];
        let st = classify(&spans, DS, DS + H, &rules());
        assert_eq!(count(&st, State::Excluded), 20 * 60);
    }

    #[test]
    fn changing_threshold_reclassifies() {
        let spans = vec![sp(Kind::InputIdle, 0, 0, 2 * M)];
        let mut r = rules();
        assert_eq!(count(&classify(&spans, DS, DS + H, &r), State::Idle), 0);
        r.idle_ms = M;
        assert_eq!(count(&classify(&spans, DS, DS + H, &r), State::Idle), 120);
    }

    #[test]
    fn idle_pieces_that_touch_are_one_gap() {
        // 23:58-00:00 stored on one day, 00:00-00:03 on the next: one 5-minute gap.
        let spans = vec![
            sp(Kind::InputIdle, 0, H - 2 * M, H),
            sp(Kind::InputIdle, 0, H, H + 3 * M),
        ];
        let st = classify(&spans, DS, DS + H, &rules());
        assert_eq!(count(&st, State::Idle), 2 * 60);
        // The other side of midnight: 2 minutes before the day plus 2 inside it.
        let spans = vec![
            sp(Kind::InputIdle, 0, -2 * M, 0),
            sp(Kind::InputIdle, 0, 0, 2 * M),
        ];
        let st = classify(&spans, DS, DS + H, &rules());
        assert_eq!(count(&st, State::Idle), 2 * 60);
        // A real input between two short gaps keeps both active.
        let spans = vec![
            sp(Kind::InputIdle, 0, 0, 2 * M),
            sp(Kind::InputIdle, 0, 2 * M + 1000, 4 * M),
        ];
        assert_eq!(
            count(&classify(&spans, DS, DS + H, &rules()), State::Idle),
            0
        );
    }

    fn temp_store() -> crate::store::Store {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("at-report-{}-{nanos}", std::process::id()));
        crate::store::Store::open(&dir.join("t.db")).unwrap()
    }

    fn insert(conn: &Connection, kind: Kind, subject: i64, a: i64, b: i64) {
        conn.execute(
            "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (?1, ?2, ?3, ?4)",
            params![kind as i64, subject, DS + a, DS + b],
        )
        .unwrap();
    }

    #[test]
    fn gap_split_at_midnight_is_idle_on_both_days() {
        let st = temp_store();
        let c = &st.conn;
        c.execute(
            "INSERT INTO subjects(app, app_name, title, host, private, program, project)
             VALUES ('editor', 'Editor', '', '', 0, '', '')",
            [],
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.idle.threshold_secs = 180;
        // Day one is [0, H), day two [H, 2H); the gap runs 23:58-00:03 and the ledger split it.
        insert(c, Kind::Focus, 1, 0, H);
        insert(c, Kind::Focus, 1, H, 2 * H);
        insert(c, Kind::InputIdle, 0, H - 2 * M, H);
        insert(c, Kind::InputIdle, 0, H, H + 3 * M);
        let one = build(c, &cfg, "one", DS, DS + H).unwrap();
        assert_eq!(one.totals.idle_removed, 2 * 60);
        assert_eq!(one.totals.screen, 3600 - 2 * 60);
        let two = build(c, &cfg, "two", DS + H, DS + 2 * H).unwrap();
        assert_eq!(two.totals.idle_removed, 3 * 60);
        // A span that started before the day (not split) still counts inside it.
        insert(c, Kind::Focus, 1, 2 * H, 3 * H);
        insert(c, Kind::Locked, 0, 2 * H - M, 2 * H + 10 * M);
        let three = build(c, &cfg, "three", DS + 2 * H, DS + 3 * H).unwrap();
        assert_eq!(three.totals.screen, 3600 - 10 * 60);
    }

    #[test]
    fn day_report_sums_apps_sites_and_agents() {
        let cfg = Config::default();
        let mut subjects = HashMap::new();
        subjects.insert(
            1,
            Subject {
                app: "brave-origin-beta".into(),
                app_name: "Brave".into(),
                host: "www.youtube.com".into(),
                title: "Video".into(),
                ..Default::default()
            },
        );
        subjects.insert(
            2,
            Subject {
                app: "kitty".into(),
                app_name: "kitty".into(),
                program: "claude".into(),
                project: "~/p/x".into(),
                ..Default::default()
            },
        );
        subjects.insert(
            3,
            Subject {
                app: "agent:claude".into(),
                app_name: "Claude Code".into(),
                program: "claude".into(),
                project: "~/p/x".into(),
                ..Default::default()
            },
        );
        let spans = vec![
            sp(Kind::Focus, 1, 0, 30 * M),
            sp(Kind::Focus, 2, 30 * M, H),
            sp(Kind::AgentBusy, 3, 0, 45 * M),
            sp(Kind::InputIdle, 0, 0, 20 * M),
            sp(Kind::MediaVisible, 1, 0, 20 * M),
        ];
        let r = compute(&cfg, "x", DS, DS + H, &spans, &subjects);
        assert_eq!(r.totals.screen, 3600);
        assert_eq!(r.totals.times.passive, 20 * 60);
        assert_eq!(r.sites[0].key, "www.youtube.com");
        assert_eq!(r.sites[0].category, "Video");
        let kitty = r.apps.iter().find(|a| a.key == "kitty").unwrap();
        assert_eq!(kitty.total, 30 * 60);
        let ag = &r.agents[0];
        assert_eq!(
            (ag.tool.as_str(), ag.focus, ag.busy),
            ("claude", 30 * 60, 45 * 60)
        );
        assert!(r.categories.iter().any(|c| c.name == "AI"));
        assert_eq!(r.timeline.len(), 12);
    }

    #[test]
    fn most_specific_host_rule_wins() {
        let cats = crate::config::default_categories();
        let s = Subject {
            host: "music.youtube.com".into(),
            ..Default::default()
        };
        assert_eq!(categorize(&cats, &s).unwrap().name, "Music");
        let s = Subject {
            host: "www.youtube.com".into(),
            ..Default::default()
        };
        assert_eq!(categorize(&cats, &s).unwrap().name, "Video");
        let s = Subject {
            host: "localhost:3000".into(),
            ..Default::default()
        };
        assert_eq!(categorize(&cats, &s).unwrap().name, "Development");
    }

    #[test]
    fn sessions_and_apps_count_the_same_seconds() {
        // Span edges fall between whole seconds; each span counts only the seconds it covers in full.
        let mut subjects = HashMap::new();
        for id in [1, 2] {
            subjects.insert(
                id,
                Subject {
                    app: format!("app{id}"),
                    app_name: format!("App {id}"),
                    ..Default::default()
                },
            );
        }
        let spans = vec![
            sp(Kind::Focus, 1, 0, 1_500),
            sp(Kind::Focus, 2, 1_500, 3_200),
            sp(Kind::Focus, 1, 3_200, 4_900),
        ];
        let r = compute(&Config::default(), "x", DS, DS + H, &spans, &subjects);
        let apps: i64 = r.apps.iter().map(|a| a.total).sum();
        let sessions: i64 = r.sessions.iter().map(|s| s.counted).sum();
        assert_eq!(apps, 4);
        assert_eq!(r.totals.screen, apps);
        assert_eq!(sessions, apps);
    }

    fn local_day(tz: &jiff::tz::TimeZone, y: i16, m: i8, d: i8) -> (i64, i64) {
        let day = jiff::civil::date(y, m, d);
        let ms = |d: jiff::civil::Date| {
            d.to_zoned_datetime(tz.clone())
                .unwrap()
                .timestamp()
                .as_millisecond()
        };
        (ms(day), ms(day.tomorrow().unwrap()))
    }

    #[test]
    fn hours_follow_the_local_clock_on_dst_days() {
        let tz = jiff::tz::TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        let labels = |(ds, de): (i64, i64)| -> Vec<String> {
            hour_marks(ds, de, &tz)
                .into_iter()
                .map(|h| h.label)
                .collect()
        };
        let normal = labels(local_day(&tz, 2026, 6, 1));
        assert_eq!(normal.len(), 24);
        assert_eq!(
            (normal[0].as_str(), normal[23].as_str()),
            ("00:00", "23:00")
        );
        let spring = labels(local_day(&tz, 2026, 3, 29));
        assert_eq!(spring.len(), 23);
        assert_eq!(&spring[..3], ["00:00", "01:00", "03:00"]);
        let autumn = labels(local_day(&tz, 2026, 10, 25));
        assert_eq!(autumn.len(), 25);
        assert_eq!(&autumn[1..5], ["01:00", "02:00", "02:00", "03:00"]);

        // Focus from 03:00 to 04:00 on the short day lands in the "03:00" hour.
        let (ds, de) = local_day(&tz, 2026, 3, 29);
        let mut subjects = HashMap::new();
        subjects.insert(1, Subject::app("editor", "Editor"));
        let three = hour_marks(ds, de, &tz)[2].start_ms;
        let spans = vec![Span {
            kind: Kind::Focus,
            subject: 1,
            start: three,
            end: three + H,
        }];
        let r = compute_in(&Config::default(), "x", ds, de, &spans, &subjects, &tz);
        assert_eq!(r.hourly.len(), 23);
        assert_eq!(r.hours[2].label, "03:00");
        assert_eq!(r.hourly[2].active, 3600);
        assert_eq!(r.hourly.iter().map(|t| t.total()).sum::<i64>(), 3600);
    }

    #[test]
    fn bad_dates_are_rejected_not_panics() {
        let st = temp_store();
        let cfg = Config::default();
        for bad in [
            "",
            "yesterday",
            "2026-13-01",
            "2026-02-30",
            "99999-01-01",
            "2026-1-1",
        ] {
            let e = build_day(&st.conn, &cfg, bad).unwrap_err();
            assert!(e.downcast_ref::<BadRequest>().is_some(), "{bad}");
        }
        let rw = Cache::ReadWrite;
        assert!(range(&st.conn, &cfg, "2026-11-02", "2026-11-01", rw).is_err());
        assert!(range(&st.conn, &cfg, "2020-01-01", "2026-01-01", rw).is_err());
        assert!(build_day(&st.conn, &cfg, "2026-11-01").is_ok());
    }

    #[test]
    fn ranges_cache_small_rollups_for_days_with_data() {
        let st = temp_store();
        let c = &st.conn;
        c.execute(
            "INSERT INTO subjects(app, app_name, title, host, private, program, project)
             VALUES ('editor', 'Editor', 'notes', '', 0, '', '')",
            [],
        )
        .unwrap();
        let day = |d: &str| crate::util::date_bounds(d).unwrap().0;
        for d in ["2020-01-01", "2020-01-03"] {
            c.execute(
                "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (1, 1, ?1, ?2)",
                params![day(d) + H, day(d) + 2 * H],
            )
            .unwrap();
        }
        let cfg = Config::default();
        let count = |hash: &str| -> i64 {
            c.query_row(
                "SELECT COUNT(*) FROM rollups WHERE settings_hash=?1",
                params![hash],
                |r| r.get(0),
            )
            .unwrap()
        };
        let r = range(c, &cfg, "2020-01-01", "2020-01-03", Cache::ReadWrite).unwrap();
        assert_eq!(r.days.len(), 3);
        assert_eq!(r.totals.screen, 2 * 3600);
        assert_eq!(r.days[1].totals.screen, 0);
        let hash = settings_hash(&cfg);
        assert_eq!(count(&hash), 2, "the empty day is not cached");
        let biggest: i64 = c
            .query_row("SELECT MAX(LENGTH(json)) FROM rollups", [], |r| r.get(0))
            .unwrap();
        assert!(biggest < 2_000, "rollup is {biggest} bytes");
        // Served from the cache the second time.
        c.execute(
            "UPDATE rollups SET json = replace(json, '\"screen\":3600', '\"screen\":7')",
            [],
        )
        .unwrap();
        let again = range(c, &cfg, "2020-01-01", "2020-01-03", Cache::ReadWrite).unwrap();
        assert_eq!(again.totals.screen, 14);
        // Read-only callers compute but never write.
        let mut other = cfg.clone();
        other.idle.threshold_secs = 600;
        range(c, &other, "2020-01-01", "2020-01-03", Cache::ReadOnly).unwrap();
        assert_eq!(count(&settings_hash(&other)), 0);
        assert_eq!(count(&hash), 2);
    }

    #[test]
    fn only_a_trailing_port_is_stripped() {
        assert_eq!(host_key("LocalHost:3000"), "localhost");
        assert_eq!(host_key("brave://settings"), "brave://settings");
        assert_eq!(host_key("[::1]:8080"), "::1");
        assert_eq!(host_key("[::1]"), "::1");
        assert_eq!(host_key("fe80::1"), "fe80::1");
        assert_eq!(host_key("example.com:abc"), "example.com:abc");
        let cats = crate::config::default_categories();
        let host = |h: &str| Subject {
            app: "brave-origin-beta".into(),
            host: h.into(),
            ..Default::default()
        };
        assert_eq!(
            categorize(&cats, &host("brave://settings")).unwrap().name,
            "System"
        );
        assert_eq!(
            categorize(&cats, &host("127.0.0.1:8000")).unwrap().name,
            "Development"
        );
        let mut cats = cats;
        cats[1].hosts.push("[::1]".into());
        assert_eq!(
            categorize(&cats, &host("[::1]:5173")).unwrap().name,
            "Development"
        );
    }
}
