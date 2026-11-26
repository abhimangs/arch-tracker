//! AI agents (Claude Code, Codex, Hermes…): each one's time per project, hour and day, its
//! terminal sessions with what came right before and after, and the stretches it worked.

use super::browsers::{BrowserMap, Neighbor, neighbor};
use super::{AgentRow, BadRequest, Cache, DayRollup, Times, each_day, parse_date};
use crate::config::{Config, Tool};
use crate::model::Kind;
use crate::store;
use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Every agent together.
pub const ALL: &str = "all";
/// The agents that get a page in the sidebar (any configured tool is still recorded).
pub const LISTED: [&str; 3] = ["claude", "codex", "hermes"];
const RECENT_DAYS: i64 = 30;

/// One agent's day: your focus on its terminal and the time it worked, hour by hour.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AgentDay {
    pub tool: String,
    pub focus: i64,
    pub busy: i64,
    pub focus_hours: Vec<i64>,
    pub busy_hours: Vec<i64>,
}

impl AgentDay {
    pub fn new(tool: &str) -> AgentDay {
        AgentDay {
            tool: tool.to_string(),
            focus_hours: vec![0; 24],
            busy_hours: vec![0; 24],
            ..Default::default()
        }
    }

    pub(super) fn add_focus(&mut self, secs: i64, hours: &[i64; 24]) {
        self.focus += secs;
        for (a, b) in self.focus_hours.iter_mut().zip(hours) {
            *a += b;
        }
    }

    fn add(&mut self, o: &AgentDay) {
        self.focus += o.focus;
        self.busy += o.busy;
        for (a, b) in self.focus_hours.iter_mut().zip(&o.focus_hours) {
            *a += b;
        }
        for (a, b) in self.busy_hours.iter_mut().zip(&o.busy_hours) {
            *a += b;
        }
    }
}

fn tool_name(cfg: &Config, id: &str) -> String {
    if id == ALL {
        return "All agents".into();
    }
    tool(cfg, id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| id.to_string())
}

fn tool<'a>(cfg: &'a Config, id: &str) -> Option<&'a Tool> {
    cfg.agents.tools.iter().find(|t| t.id == id)
}

/// Installed: one of its programs is on PATH, or its own folder (~/.claude, ~/.codex…) exists.
fn installed(t: &Tool) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    let dirs: Vec<&str> = path.split(':').filter(|d| !d.is_empty()).collect();
    let home = crate::util::home();
    let own = home.join(format!(".{}", t.id)).is_dir()
        || (t.id == "codex" && crate::sources::codex::home().is_dir());
    own || t.matches.iter().any(|m| {
        !m.contains('/')
            && dirs
                .iter()
                .any(|d| std::path::Path::new(d).join(m).is_file())
    })
}

/// An agent for the dashboard's sidebar.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct AgentEntry {
    pub id: String,
    pub name: String,
    pub installed: bool,
    /// Today: your focus on it and the time it worked.
    pub today_focus: i64,
    pub today_busy: i64,
    /// Both over the last 30 days.
    pub recent: i64,
    /// Which cost report it has ("claude" or "codex"), if it is switched on.
    pub cost: Option<&'static str>,
}

/// Claude Code, Codex and Hermes, when installed or seen running, in that order.
pub fn agent_list(
    conn: &Connection,
    cfg: &Config,
    today: &str,
    cache: Cache,
) -> Result<Vec<AgentEntry>> {
    let map = BrowserMap::load(conn)?;
    let (end, _, _) = parse_date(today)?;
    let from = end
        .checked_sub(jiff::Span::new().days(RECENT_DAYS - 1))?
        .to_string();
    let mut time: HashMap<String, (i64, i64, i64)> = HashMap::new();
    each_day(
        conn,
        cfg,
        &map,
        &from,
        today,
        cache,
        |date, r: DayRollup| {
            for a in &r.agent_days {
                let e = time.entry(a.tool.clone()).or_default();
                e.2 += a.focus + a.busy;
                if date == today {
                    e.0 += a.focus;
                    e.1 += a.busy;
                }
            }
        },
    )?;
    let mut out = vec![];
    for id in LISTED {
        let Some(t) = tool(cfg, id) else { continue };
        let (today_focus, today_busy, recent) = time.get(id).copied().unwrap_or_default();
        let installed = installed(t);
        if !installed && recent == 0 {
            continue;
        }
        let cost = match id {
            "claude" if cfg.agents.claude_cost => Some("claude"),
            "codex" if cfg.agents.codex_cost => Some("codex"),
            _ => None,
        };
        out.push(AgentEntry {
            id: id.to_string(),
            name: t.name.clone(),
            installed,
            today_focus,
            today_busy,
            recent,
            cost,
        });
    }
    Ok(out)
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct DayAgent {
    pub date: String,
    pub focus: i64,
    pub busy: i64,
}

/// One agent (or all of them) over a range of days.
#[derive(Serialize, Clone, Debug, Default)]
pub struct AgentReport {
    pub id: String,
    pub name: String,
    pub from: String,
    pub to: String,
    pub focus: i64,
    pub busy: i64,
    pub days_used: usize,
    pub focus_hours: Vec<i64>,
    pub busy_hours: Vec<i64>,
    pub days: Vec<DayAgent>,
    /// Per project (and, for all agents, per agent): your focus and its work.
    pub projects: Vec<AgentRow>,
    /// Each agent's totals (for all agents).
    pub agents: Vec<AgentRow>,
}

pub fn agent_report(
    conn: &Connection,
    cfg: &Config,
    id: &str,
    from: &str,
    to: &str,
    cache: Cache,
) -> Result<AgentReport> {
    if id.is_empty() {
        return Err(BadRequest("no agent given".into()).into());
    }
    let map = BrowserMap::load(conn)?;
    let wanted = |t: &str| id == ALL || t == id;
    let mut sum = AgentDay::new(id);
    let mut days = vec![];
    let mut projects: HashMap<(String, String), AgentRow> = HashMap::new();
    let mut per_tool: HashMap<String, AgentRow> = HashMap::new();
    each_day(conn, cfg, &map, from, to, cache, |date, r: DayRollup| {
        let mut day = DayAgent {
            date: date.to_string(),
            ..Default::default()
        };
        for a in r.agent_days.iter().filter(|a| wanted(&a.tool)) {
            sum.add(a);
            day.focus += a.focus;
            day.busy += a.busy;
        }
        for a in r.agents.iter().filter(|a| wanted(&a.tool)) {
            let key = (
                if id == ALL {
                    a.tool.clone()
                } else {
                    String::new()
                },
                a.project.clone(),
            );
            let p = projects.entry(key).or_insert_with(|| AgentRow {
                tool: a.tool.clone(),
                name: a.name.clone(),
                project: a.project.clone(),
                ..Default::default()
            });
            p.focus += a.focus;
            p.busy += a.busy;
            let t = per_tool.entry(a.tool.clone()).or_insert_with(|| AgentRow {
                tool: a.tool.clone(),
                name: tool_name(cfg, &a.tool),
                ..Default::default()
            });
            t.focus += a.focus;
            t.busy += a.busy;
        }
        days.push(day);
    })?;
    let by_time = |a: &AgentRow, b: &AgentRow| {
        (b.busy + b.focus)
            .cmp(&(a.busy + a.focus))
            .then_with(|| a.project.cmp(&b.project))
    };
    let mut projects: Vec<AgentRow> = projects.into_values().collect();
    projects.sort_by(by_time);
    let mut agents: Vec<AgentRow> = per_tool.into_values().collect();
    agents.sort_by(by_time);
    Ok(AgentReport {
        id: id.to_string(),
        name: tool_name(cfg, id),
        from: from.to_string(),
        to: to.to_string(),
        focus: sum.focus,
        busy: sum.busy,
        days_used: days.iter().filter(|d| d.focus + d.busy > 0).count(),
        focus_hours: sum.focus_hours,
        busy_hours: sum.busy_hours,
        days,
        projects,
        agents,
    })
}

/// A stretch with the agent's terminal in front of you.
#[derive(Serialize, Clone, Debug, Default)]
pub struct AgentSession {
    pub start_ms: i64,
    pub end_ms: i64,
    pub tool: String,
    pub project: String,
    #[serde(flatten)]
    pub times: Times,
    pub counted: i64,
    pub before: Option<Neighbor>,
    pub after: Option<Neighbor>,
}

/// A stretch the agent worked (CPU or terminal output), wherever you were.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Stretch {
    pub start_ms: i64,
    pub end_ms: i64,
    pub tool: String,
    /// The folder it worked in.
    pub project: String,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct AgentDayReport {
    pub date: String,
    pub id: String,
    pub name: String,
    pub session_gap_ms: i64,
    pub sessions: Vec<AgentSession>,
    pub stretches: Vec<Stretch>,
    pub focus_hours: Vec<i64>,
    pub busy_hours: Vec<i64>,
}

/// Work this close together is one stretch.
const STRETCH_GAP_MS: i64 = 60_000;

/// One day of an agent (or all of them): sessions in its terminal and stretches of work.
pub fn agent_day(conn: &Connection, cfg: &Config, id: &str, date: &str) -> Result<AgentDayReport> {
    if id.is_empty() {
        return Err(BadRequest("no agent given".into()).into());
    }
    let (_, ds, de) = parse_date(date)?;
    let map = BrowserMap::load(conn)?;
    let r = super::build(conn, cfg, date, ds, de, &map)?;
    let tools: Vec<&str> = cfg.agents.tools.iter().map(|t| t.id.as_str()).collect();
    let wanted = |t: &str| (id == ALL && tools.contains(&t)) || t == id;
    let mut sessions = vec![];
    for (i, s) in r.sessions.iter().enumerate() {
        if s.kind != "terminal" || !wanted(&s.detail) {
            continue;
        }
        sessions.push(AgentSession {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            tool: s.detail.clone(),
            project: s.project.clone(),
            times: s.times,
            counted: s.counted,
            before: i
                .checked_sub(1)
                .and_then(|j| r.sessions.get(j))
                .map(|p| neighbor(p, s.start_ms - p.end_ms)),
            after: r
                .sessions
                .get(i + 1)
                .map(|n| neighbor(n, n.start_ms - s.end_ms)),
        });
    }
    // Stretches of work, from the raw spans: an agent works in the background too.
    let spans: Vec<_> = store::spans_in(conn, ds - super::MAX_SPAN_MS, de)?
        .into_iter()
        .filter(|s| s.kind == Kind::AgentBusy && s.end > ds)
        .collect();
    let ids: Vec<i64> = spans.iter().map(|s| s.subject).collect();
    let subjects = store::subjects_by_id(conn, &ids)?;
    let mut stretches: Vec<Stretch> = vec![];
    let mut work: Vec<(i64, i64, String, String)> = spans
        .iter()
        .filter_map(|s| {
            let subj = subjects.get(&s.subject)?;
            wanted(&subj.program).then(|| {
                (
                    s.start.max(ds),
                    s.end.min(de),
                    subj.program.clone(),
                    subj.project.clone(),
                )
            })
        })
        .filter(|(a, b, _, _)| b > a)
        .collect();
    work.sort();
    for (a, b, tool, project) in work {
        match stretches
            .iter_mut()
            .rev()
            .find(|x| x.tool == tool && x.project == project)
        {
            Some(x) if a - x.end_ms <= STRETCH_GAP_MS => x.end_ms = x.end_ms.max(b),
            _ => stretches.push(Stretch {
                start_ms: a,
                end_ms: b,
                tool,
                project,
            }),
        }
    }
    stretches.sort_by_key(|s| s.start_ms);
    let mut sum = AgentDay::new(id);
    for a in r.agent_days.iter().filter(|a| wanted(&a.tool)) {
        sum.add(a);
    }
    Ok(AgentDayReport {
        date: date.to_string(),
        id: id.to_string(),
        name: tool_name(cfg, id),
        session_gap_ms: cfg.insights.session_gap_secs as i64 * 1000,
        sessions,
        stretches,
        focus_hours: sum.focus_hours,
        busy_hours: sum.busy_hours,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn day(d: &str) -> i64 {
        crate::util::date_bounds(d).unwrap().0
    }

    const H: i64 = 3_600_000;
    const M: i64 = 60_000;

    fn sample() -> crate::store::Store {
        let dir = std::env::temp_dir().join(format!(
            "at-agents-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let st = crate::store::Store::open(&dir.join("t.db")).unwrap();
        let c = &st.conn;
        for (app, name, program, project) in [
            ("kitty", "kitty", "claude", "/nowhere/alpha"),
            ("agent:claude", "Claude Code", "claude", "/nowhere/alpha"),
            ("agent:codex", "Codex", "codex", "/nowhere/beta"),
            ("kitty", "kitty", "nvim", "/nowhere/alpha"),
        ] {
            c.execute(
                "INSERT INTO subjects(app, app_name, title, host, private, program, project)
                 VALUES (?1, ?2, '', '', 0, ?3, ?4)",
                params![app, name, program, project],
            )
            .unwrap();
        }
        let span = |kind: Kind, s: i64, a: i64, b: i64| {
            c.execute(
                "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (?1, ?2, ?3, ?4)",
                params![kind as i64, s, a, b],
            )
            .unwrap();
        };
        let d = day("2020-01-01");
        // 09:00-09:20 in Claude's terminal, then nvim; Claude works 09:05-09:50 with a short
        // pause, Codex 10:00-10:30.
        span(Kind::Focus, 1, d + 9 * H, d + 9 * H + 20 * M);
        span(Kind::Focus, 4, d + 9 * H + 20 * M, d + 9 * H + 40 * M);
        span(Kind::AgentBusy, 2, d + 9 * H + 5 * M, d + 9 * H + 30 * M);
        span(
            Kind::AgentBusy,
            2,
            d + 9 * H + 30 * M + 30_000,
            d + 9 * H + 50 * M,
        );
        span(Kind::AgentBusy, 3, d + 10 * H, d + 10 * H + 30 * M);
        st
    }

    #[test]
    fn an_agent_has_its_hours_projects_sessions_and_stretches() {
        let st = sample();
        let cfg = Config::default();
        let r = agent_report(
            &st.conn,
            &cfg,
            "claude",
            "2020-01-01",
            "2020-01-01",
            Cache::ReadOnly,
        )
        .unwrap();
        assert_eq!((r.focus, r.busy), (20 * 60, 45 * 60 - 30));
        assert_eq!(r.focus_hours.iter().sum::<i64>(), r.focus);
        assert_eq!(r.busy_hours.iter().sum::<i64>(), r.busy);
        assert_eq!(r.projects.len(), 1);
        assert_eq!(r.name, "Claude Code");
        let all = agent_report(
            &st.conn,
            &cfg,
            ALL,
            "2020-01-01",
            "2020-01-01",
            Cache::ReadOnly,
        )
        .unwrap();
        assert_eq!(all.busy, r.busy + 30 * 60);
        assert_eq!(all.agents.len(), 2);
        let d = agent_day(&st.conn, &cfg, "claude", "2020-01-01").unwrap();
        assert_eq!(d.sessions.len(), 1);
        let after = d.sessions[0].after.as_ref().unwrap();
        assert_eq!(
            (after.kind.as_str(), after.detail.as_str()),
            ("terminal", "nvim")
        );
        // Work with a 30 s pause is one stretch.
        assert_eq!(d.stretches.len(), 1);
        let s = &d.stretches[0];
        assert_eq!(s.end_ms - s.start_ms, 45 * M);
        let codex = agent_day(&st.conn, &cfg, "codex", "2020-01-01").unwrap();
        assert!(codex.sessions.is_empty());
        assert_eq!(codex.stretches[0].project, "/nowhere/beta");
    }

    #[test]
    fn the_sidebar_lists_agents_that_ran_or_are_installed() {
        let st = sample();
        let cfg = Config::default();
        let list = agent_list(&st.conn, &cfg, "2020-01-01", Cache::ReadOnly).unwrap();
        let ids: Vec<&str> = list.iter().map(|a| a.id.as_str()).collect();
        // Both ran that day; Hermes shows only where it is installed.
        assert_eq!(&ids[..2], ["claude", "codex"]);
        let hermes = tool(&cfg, "hermes").unwrap();
        assert_eq!(ids.contains(&"hermes"), installed(hermes));
        assert_eq!(list[0].today_busy, 45 * 60 - 30);
        assert_eq!(
            (list[0].cost, list[1].cost),
            (Some("claude"), Some("codex"))
        );
        let mut off = cfg.clone();
        off.agents.codex_cost = false;
        let list = agent_list(&st.conn, &off, "2020-01-01", Cache::ReadOnly).unwrap();
        assert_eq!(list[1].cost, None);
    }
}
