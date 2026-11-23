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
use std::collections::HashMap;

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
    pub hourly: Vec<Times>,
    pub timeline: Vec<Bucket>,
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
        let host = lc(&s.host);
        let host = host.split(':').next().unwrap_or(&host).to_string();
        let mut best: Option<(&Category, usize)> = None;
        for c in cats {
            for h in &c.hosts {
                let h = lc(h);
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
    ]
    .iter()
    .any(|b| app.contains(b));
    let fallback = if !s.host.is_empty() || s.private || browser {
        "Browsing"
    } else {
        "Other"
    };
    cats.iter().find(|c| c.name == fallback)
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
    let Some((ds, de)) = crate::util::date_bounds(date) else {
        anyhow::bail!("bad date {date:?}");
    };
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
    let mut stmt = conn.prepare_cached(
        "SELECT kind, subject_id, start_ms, end_ms FROM spans
         WHERE start_ms >= ?1 AND start_ms < ?2 AND end_ms > ?3 ORDER BY start_ms",
    )?;
    let rows = stmt.query_map(params![ds - pad - MAX_SPAN_MS, de + pad, ds - pad], |r| {
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
        } else if start < de && end > ds {
            out.push(Span {
                kind,
                subject,
                start: start.max(ds),
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
    let mut hourly = vec![Times::default(); 24];
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
                            hourly[(i / 3600).min(23)].add(*st, 1);
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
        timeline,
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DaySummary {
    pub date: String,
    pub totals: Totals,
    pub categories: Vec<Row>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RangeReport {
    pub from: String,
    pub to: String,
    pub days: Vec<DaySummary>,
    pub totals: Totals,
    pub apps: Vec<Row>,
    pub sites: Vec<Row>,
    pub categories: Vec<Row>,
    pub programs: Vec<Row>,
    pub agents: Vec<AgentRow>,
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

/// Inclusive date range (at most ~3 years).
pub fn range(conn: &Connection, cfg: &Config, from: &str, to: &str) -> Result<RangeReport> {
    let mut d: jiff::civil::Date = from.parse()?;
    let end: jiff::civil::Date = to.parse()?;
    let mut out = RangeReport {
        from: from.into(),
        to: to.into(),
        ..Default::default()
    };
    let (mut apps, mut sites, mut cats, mut progs) = (
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    );
    let mut agents: HashMap<(String, String), AgentRow> = HashMap::new();
    let mut guard = 0;
    while d <= end && guard < 1200 {
        guard += 1;
        let ds = d.to_string();
        let r = day_cached(conn, cfg, &ds)?;
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
        out.days.push(DaySummary {
            date: ds,
            totals: r.totals,
            categories: r.categories,
        });
        d = d.tomorrow()?;
    }
    out.apps = finish(apps, 10);
    out.sites = finish(sites, 10);
    out.categories = finish(cats, 0);
    out.programs = finish(progs, 5);
    let mut ag: Vec<AgentRow> = agents.into_values().collect();
    ag.sort_by_key(|x| std::cmp::Reverse(x.busy + x.focus));
    out.agents = ag;
    Ok(out)
}

/// Hash of everything that changes how a day is classified (for the rollup cache).
pub fn settings_hash(cfg: &Config) -> String {
    let v = serde_json::json!({
        "v": 1,
        "idle": cfg.idle,
        "categories": cfg.categories,
        "tools": cfg.agents.tools,
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

/// Day report, served from the rollup cache for days that are over.
pub fn day_cached(conn: &Connection, cfg: &Config, date: &str) -> Result<DayReport> {
    let (_, de) = crate::util::date_bounds(date).unwrap_or((0, i64::MAX));
    // A no-input gap that started before midnight is judged with the part after it.
    let pad = cfg.idle.threshold_secs.max(cfg.idle.call_threshold_secs) as i64 * 1000;
    let finished = crate::util::now_ms() > de.saturating_add(pad + 10 * 60_000);
    let hash = settings_hash(cfg);
    if finished {
        let cached: Option<String> = conn
            .query_row(
                "SELECT json FROM rollups WHERE day=?1 AND settings_hash=?2",
                params![date, hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(j) = cached
            && let Ok(r) = serde_json::from_str(&j)
        {
            return Ok(r);
        }
    }
    let r = build_day(conn, cfg, date)?;
    if finished {
        let _ = conn.execute(
            "INSERT OR REPLACE INTO rollups(day, settings_hash, json) VALUES (?1, ?2, ?3)",
            params![date, hash, serde_json::to_string(&r)?],
        );
        let _ = conn.execute(
            "DELETE FROM rollups WHERE settings_hash != ?1",
            params![hash],
        );
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
