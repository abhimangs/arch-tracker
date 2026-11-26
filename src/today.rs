//! "arch-tracker today": a day in the terminal. Screen time against your goal, categories against
//! their limits, top apps and sites, projects, and what your agents did and would cost. Built from
//! the same day report as the dashboard's Today page, read straight from the database.

use crate::config::Config;
use crate::report::{self, DayReport};
use anyhow::{Result, bail};
use rusqlite::Connection;
use serde::Serialize;
use std::fmt::Write;

#[derive(Serialize, Debug, Default, PartialEq)]
pub struct Summary {
    pub date: String,
    /// Seconds, like every duration here.
    pub screen: i64,
    pub active: i64,
    pub watching: i64,
    pub calls: i64,
    pub idle_removed: i64,
    /// Your daily screen-time goal (0 = none).
    pub goal: i64,
    pub sessions: usize,
    pub longest: i64,
    pub deep_work: i64,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    pub categories: Vec<Line>,
    pub apps: Vec<Line>,
    pub sites: Vec<Line>,
    pub projects: Vec<Project>,
    pub agents: Vec<Agent>,
}

#[derive(Serialize, Debug, Default, PartialEq)]
pub struct Line {
    pub name: String,
    pub secs: i64,
    /// Its daily limit (categories only; 0 = none).
    pub limit: i64,
}

#[derive(Serialize, Debug, Default, PartialEq)]
pub struct Project {
    pub name: String,
    pub secs: i64,
    /// Time agents worked on it.
    pub agents: i64,
}

#[derive(Serialize, Debug, Default, PartialEq)]
pub struct Agent {
    pub name: String,
    pub worked: i64,
    pub watched: i64,
    /// At list prices, where its cost is read.
    pub cost: Option<f64>,
}

pub fn run(date: Option<String>, json: bool) -> Result<()> {
    let cfg = Config::load().unwrap_or_default();
    let today = crate::util::local_date(crate::util::now_ms());
    let date = match date.as_deref() {
        None | Some("today") => today,
        Some("yesterday") => {
            let (d, _, _) = report::parse_date(&today)?;
            d.yesterday()?.to_string()
        }
        Some(d) => {
            report::parse_date(d)?;
            d.to_string()
        }
    };
    let db = crate::util::db_path();
    if !db.exists() {
        bail!(
            "nothing recorded yet: start the tracker with systemctl --user enable --now arch-tracker"
        );
    }
    let conn = crate::store::open_readonly(&db)?;
    let day = report::build_day(&conn, &cfg, &date)?;
    let s = summarize(&conn, &cfg, &day);
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
    } else {
        print!("{}", render(&s, cfg.dashboard.time_format == "12h"));
    }
    Ok(())
}

/// An agent's cost report for a range of days.
type CostFn = fn(&Connection, &Config, &str, &str) -> Result<report::cost::CostReport>;

pub fn summarize(conn: &Connection, cfg: &Config, d: &DayReport) -> Summary {
    let t = &d.totals;
    let line = |r: &report::Row| Line {
        name: r.name.clone(),
        secs: r.total,
        limit: 0,
    };
    let top = |rows: &[report::Row]| -> Vec<Line> {
        rows.iter()
            .filter(|r| r.total > 0)
            .take(5)
            .map(line)
            .collect()
    };
    let categories = d
        .categories
        .iter()
        .filter(|r| r.total > 0)
        .take(6)
        .map(|r| Line {
            limit: cfg
                .categories
                .iter()
                .find(|c| c.name == r.name)
                .map_or(0, |c| c.goal_mins as i64 * 60),
            ..line(r)
        })
        .collect();
    let projects = d
        .projects
        .iter()
        .filter(|p| p.total + p.agent_busy > 0)
        .take(5)
        .map(|p| Project {
            name: p.name.clone(),
            secs: p.total,
            agents: p.agent_busy,
        })
        .collect();
    // Each agent's time, then its cost where it is read.
    let mut agents: Vec<(String, Agent)> = vec![];
    for a in &d.agents {
        let i = match agents.iter().position(|(t, _)| *t == a.tool) {
            Some(i) => i,
            None => {
                agents.push((
                    a.tool.clone(),
                    Agent {
                        name: a.name.clone(),
                        ..Default::default()
                    },
                ));
                agents.len() - 1
            }
        };
        agents[i].1.worked += a.busy;
        agents[i].1.watched += a.focus;
    }
    let on = &cfg.agents;
    let costs: [(&str, bool, CostFn); 4] = [
        ("claude", on.claude_cost, report::cost::claude),
        ("codex", on.codex_cost, report::cost::codex),
        ("hermes", on.hermes_cost, report::cost::hermes),
        ("opencode", on.opencode_cost, report::cost::opencode),
    ];
    for (id, enabled, cost) in costs {
        if !enabled {
            continue;
        }
        let Ok(r) = cost(conn, cfg, &d.date, &d.date) else {
            continue;
        };
        if r.cost <= 0.0 && r.requests == 0 && !agents.iter().any(|(t, _)| t == id) {
            continue;
        }
        match agents.iter_mut().find(|(t, _)| t == id) {
            Some((_, a)) => a.cost = Some(r.cost),
            None => agents.push((
                id.to_string(),
                Agent {
                    name: cfg
                        .agents
                        .tools
                        .iter()
                        .find(|t| t.id == id)
                        .map_or_else(|| id.to_string(), |t| t.name.clone()),
                    cost: Some(r.cost),
                    ..Default::default()
                },
            )),
        }
    }
    let mut agents: Vec<Agent> = agents.into_iter().map(|(_, a)| a).collect();
    agents.sort_by(|a, b| b.worked.cmp(&a.worked).then(b.watched.cmp(&a.watched)));
    Summary {
        date: d.date.clone(),
        screen: t.screen,
        active: t.times.active,
        watching: t.times.passive,
        calls: t.times.call,
        idle_removed: t.idle_removed,
        goal: cfg.insights.daily_goal_mins as i64 * 60,
        sessions: d.stats.sessions,
        longest: d.stats.longest,
        deep_work: d.stats.deep_work,
        first_ms: d.stats.first_ms,
        last_ms: d.stats.last_ms,
        categories,
        apps: top(&d.apps),
        sites: top(&d.sites),
        projects,
        agents,
    }
}

/// "6h 05m", "42m", "35s".
fn dur(secs: i64) -> String {
    crate::tray::fmt_dur(secs)
}

fn clock(ms: i64, h12: bool) -> String {
    let Ok(ts) = jiff::Timestamp::from_millisecond(ms) else {
        return "?".into();
    };
    let t = ts.to_zoned_datetime(jiff::tz::TimeZone::system()).time();
    if h12 {
        let h = match t.hour() % 12 {
            0 => 12,
            h => h,
        };
        format!(
            "{h}:{:02} {}",
            t.minute(),
            if t.hour() < 12 { "AM" } else { "PM" }
        )
    } else {
        format!("{:02}:{:02}", t.hour(), t.minute())
    }
}

/// A bar of up to 16 blocks for a share of the largest.
fn bar(v: i64, max: i64) -> String {
    let n = if max > 0 { (v * 16 + max - 1) / max } else { 0 };
    "\u{2588}".repeat(n.clamp(0, 16) as usize)
}

pub fn render(s: &Summary, h12: bool) -> String {
    let mut o = String::new();
    let heading = match report::parse_date(&s.date) {
        Ok((d, _, _)) => format!("{:?}, {} {}", d.weekday(), d.day(), month(d.month())),
        Err(_) => s.date.clone(),
    };
    let _ = writeln!(o, "{heading}");
    let mut line = format!("  Screen time  {}", dur(s.screen));
    if s.goal > 0 {
        if s.screen <= s.goal {
            let _ = write!(
                line,
                ", {} left of your {} goal",
                dur(s.goal - s.screen),
                dur(s.goal)
            );
        } else {
            let _ = write!(
                line,
                ", {} over your {} goal",
                dur(s.screen - s.goal),
                dur(s.goal)
            );
        }
    }
    let _ = writeln!(o, "{line}");
    if s.screen == 0 {
        let _ = writeln!(o, "  Nothing recorded on this day.");
        return o;
    }
    let mut parts = vec![format!("active {}", dur(s.active))];
    if s.watching > 0 {
        parts.push(format!("watching {}", dur(s.watching)));
    }
    if s.calls > 0 {
        parts.push(format!("calls {}", dur(s.calls)));
    }
    if s.idle_removed > 0 {
        parts.push(format!("{} idle left out", dur(s.idle_removed)));
    }
    let _ = writeln!(o, "  {}", parts.join(" · "));
    let mut parts = vec![format!(
        "{} session{}",
        s.sessions,
        if s.sessions == 1 { "" } else { "s" }
    )];
    if s.longest > 0 {
        parts.push(format!("longest {}", dur(s.longest)));
    }
    if s.deep_work > 0 {
        parts.push(format!("deep work {}", dur(s.deep_work)));
    }
    if let (Some(a), Some(b)) = (s.first_ms, s.last_ms) {
        parts.push(format!("{} to {}", clock(a, h12), clock(b, h12)));
    }
    let _ = writeln!(o, "  {}", parts.join(" · "));
    let table = |o: &mut String, title: &str, rows: Vec<(String, i64, String)>| {
        if rows.is_empty() {
            return;
        }
        let _ = writeln!(o, "\n{title}");
        let w = rows
            .iter()
            .map(|r| r.0.chars().count())
            .max()
            .unwrap_or(0)
            .min(32);
        let max = rows.iter().map(|r| r.1).max().unwrap_or(0);
        for (name, secs, note) in rows {
            let name: String = name.chars().take(32).collect();
            let row = format!(
                "  {name:<w$}  {:>7}  {:<16}{}",
                dur(secs),
                bar(secs, max),
                if note.is_empty() {
                    String::new()
                } else {
                    format!("  {note}")
                }
            );
            let _ = writeln!(o, "{}", row.trim_end());
        }
    };
    table(
        &mut o,
        "Categories",
        s.categories
            .iter()
            .map(|c| {
                let note = match c.limit {
                    0 => String::new(),
                    l if c.secs > l => format!("{} over its {} limit", dur(c.secs - l), dur(l)),
                    l => format!("{} left of {}", dur(l - c.secs), dur(l)),
                };
                (c.name.clone(), c.secs, note)
            })
            .collect(),
    );
    let simple = |v: &[Line]| {
        v.iter()
            .map(|l| (l.name.clone(), l.secs, String::new()))
            .collect()
    };
    table(&mut o, "Apps", simple(&s.apps));
    table(&mut o, "Sites", simple(&s.sites));
    table(
        &mut o,
        "Projects",
        s.projects
            .iter()
            .map(|p| {
                let note = if p.agents > 0 {
                    format!("agents worked {}", dur(p.agents))
                } else {
                    String::new()
                };
                (p.name.clone(), p.secs, note)
            })
            .collect(),
    );
    if !s.agents.is_empty() {
        let _ = writeln!(o, "\nAgents");
        let w = s
            .agents
            .iter()
            .map(|a| a.name.chars().count())
            .max()
            .unwrap_or(0);
        for a in &s.agents {
            let mut parts = vec![format!("worked {}", dur(a.worked))];
            if a.watched > 0 {
                parts.push(format!("you watched {}", dur(a.watched)));
            }
            if let Some(c) = a.cost {
                parts.push(format!("{} at API prices", money(c)));
            }
            let _ = writeln!(o, "  {:<w$}  {}", a.name, parts.join(" · "));
        }
    }
    o
}

fn money(x: f64) -> String {
    if x <= 0.0 {
        "$0".into()
    } else if x < 0.01 {
        "<$0.01".into()
    } else {
        format!("${x:.2}")
    }
}

fn month(m: i8) -> &'static str {
    const M: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    M[(m.clamp(1, 12) - 1) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_day_reads_as_lines_with_goal_limits_and_agent_cost() {
        let s = Summary {
            date: "2026-11-26".into(),
            screen: 6 * 3600 + 300,
            active: 5 * 3600,
            watching: 3900,
            calls: 600,
            idle_removed: 1200,
            goal: 7 * 3600,
            sessions: 14,
            longest: 4320,
            deep_work: 3 * 3600,
            categories: vec![
                Line {
                    name: "Development".into(),
                    secs: 3 * 3600,
                    limit: 0,
                },
                Line {
                    name: "Social".into(),
                    secs: 2520,
                    limit: 1800,
                },
            ],
            apps: vec![Line {
                name: "kitty".into(),
                secs: 7200,
                limit: 0,
            }],
            projects: vec![Project {
                name: "atlas".into(),
                secs: 7000,
                agents: 9000,
            }],
            agents: vec![Agent {
                name: "Claude Code".into(),
                worked: 9000,
                watched: 3600,
                cost: Some(12.5),
            }],
            ..Default::default()
        };
        let text = render(&s, false);
        assert!(text.starts_with("Thursday, 26 November\n"), "{text}");
        assert!(
            text.contains("Screen time  6h 05m, 55m left of your 7h 00m goal"),
            "{text}"
        );
        assert!(
            text.contains("active 5h 00m · watching 1h 05m · calls 10m · 20m idle left out"),
            "{text}"
        );
        assert!(
            text.contains("14 sessions · longest 1h 12m · deep work 3h 00m"),
            "{text}"
        );
        let social = text.lines().find(|l| l.starts_with("  Social")).unwrap();
        assert!(
            social.contains(" 42m  ") && social.ends_with("12m over its 30m limit"),
            "{text}"
        );
        assert!(text.contains("agents worked 2h 30m"), "{text}");
        assert!(
            text.contains("Claude Code  worked 2h 30m · you watched 1h 00m · $12.50 at API prices"),
            "{text}"
        );
        assert!(!text.contains("Sites"), "no empty sections: {text}");
        let empty = render(
            &Summary {
                date: "2026-11-26".into(),
                ..Default::default()
            },
            false,
        );
        assert!(empty.contains("Nothing recorded on this day."));
    }
}
