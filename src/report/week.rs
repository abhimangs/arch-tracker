//! A week against the one before: screen time per day, categories, apps, sites and agents, and
//! what the agents would cost at API prices. A week in progress is compared with the same days of
//! the week before, so a Wednesday is not measured against a whole week.

use super::{Cache, Row, parse_date, range};
use crate::config::Config;
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize, Clone, Debug, Default)]
pub struct WeekSide {
    pub from: String,
    pub to: String,
    pub screen: i64,
    /// Screen time per day, from the first day of the week.
    pub days: Vec<i64>,
    /// The time agents worked, and the time you watched them.
    pub agent_busy: i64,
    pub agent_focus: i64,
    /// What the agents' requests would cost at API prices.
    pub cost: f64,
}

/// One row (a category, app, site or agent) in both weeks.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Compare {
    pub key: String,
    pub name: String,
    pub color: String,
    pub cur: i64,
    pub prev: i64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct WeekReport {
    /// The week's first day, and its last (the week runs on past today until it ends).
    pub start: String,
    pub end: String,
    /// The week is not over yet.
    pub partial: bool,
    pub week_start: String,
    pub this: WeekSide,
    pub prev: WeekSide,
    pub categories: Vec<Compare>,
    pub apps: Vec<Compare>,
    pub sites: Vec<Compare>,
    pub agents: Vec<Compare>,
}

/// The first day of the week holding date (weeks start as general.week_start says).
pub fn week_of(date: &str, week_start: &str) -> Result<String> {
    let (d, _, _) = parse_date(date)?;
    let back = if week_start == "sunday" {
        d.weekday().to_sunday_zero_offset()
    } else {
        d.weekday().to_monday_zero_offset()
    };
    Ok(d.checked_sub(jiff::Span::new().days(back))?.to_string())
}

fn add_days(date: &str, n: i64) -> Result<String> {
    let (d, _, _) = parse_date(date)?;
    Ok(d.checked_add(jiff::Span::new().days(n))?.to_string())
}

fn compare(cur: &[Row], prev: &[Row], top: usize) -> Vec<Compare> {
    let mut by: HashMap<&str, Compare> = HashMap::new();
    for (rows, this) in [(cur, true), (prev, false)] {
        for r in rows {
            let c = by.entry(r.key.as_str()).or_insert_with(|| Compare {
                key: r.key.clone(),
                name: r.name.clone(),
                color: r.color.clone(),
                ..Default::default()
            });
            if this {
                c.cur += r.total;
            } else {
                c.prev += r.total;
            }
        }
    }
    let mut v: Vec<Compare> = by.into_values().filter(|c| c.cur + c.prev > 0).collect();
    v.sort_by(|a, b| {
        b.cur
            .cmp(&a.cur)
            .then(b.prev.cmp(&a.prev))
            .then(a.name.cmp(&b.name))
    });
    v.truncate(top);
    v
}

fn cost(conn: &Connection, cfg: &Config, from: &str, to: &str) -> f64 {
    let a = &cfg.agents;
    let mut c = 0.0;
    if a.claude_cost {
        c += super::cost::claude(conn, cfg, from, to).map_or(0.0, |r| r.cost);
    }
    if a.codex_cost {
        c += super::cost::codex(conn, cfg, from, to).map_or(0.0, |r| r.cost);
    }
    if a.hermes_cost {
        c += super::cost::hermes(conn, cfg, from, to).map_or(0.0, |r| r.cost);
    }
    if a.opencode_cost {
        c += super::cost::opencode(conn, cfg, from, to).map_or(0.0, |r| r.cost);
    }
    c
}

/// The week holding date, against the week before (the same days of it while this one runs).
pub fn week(
    conn: &Connection,
    cfg: &Config,
    date: &str,
    today: &str,
    cache: Cache,
) -> Result<WeekReport> {
    let ws = cfg.general.week_start.as_str();
    let start = week_of(date, ws)?;
    let end = add_days(&start, 6)?;
    let last = if end.as_str() > today {
        today.to_string()
    } else {
        end.clone()
    };
    let n = {
        let (_, a, _) = parse_date(&start)?;
        let (_, b, _) = parse_date(&last)?;
        ((b - a) as f64 / 86_400_000.0).round() as i64
    };
    let prev_from = add_days(&start, -7)?;
    let prev_to = add_days(&prev_from, n)?;
    let side = |from: &str, to: &str| -> Result<(WeekSide, super::RangeReport)> {
        let r = range(conn, cfg, from, to, cache)?;
        let agents = r
            .agents
            .iter()
            .fold((0, 0), |(b, f), a| (b + a.busy, f + a.focus));
        Ok((
            WeekSide {
                from: from.to_string(),
                to: to.to_string(),
                screen: r.totals.screen,
                days: r.days.iter().map(|d| d.totals.screen).collect(),
                agent_busy: agents.0,
                agent_focus: agents.1,
                cost: cost(conn, cfg, from, to),
            },
            r,
        ))
    };
    let (this, cur) = if start.as_str() <= today {
        side(&start, &last)?
    } else {
        Default::default()
    };
    let (prev, before) = side(&prev_from, &prev_to)?;
    let agent_rows = |r: &super::RangeReport| -> Vec<Row> {
        let mut by: HashMap<String, Row> = HashMap::new();
        for a in &r.agents {
            let e = by.entry(a.tool.clone()).or_insert_with(|| Row {
                key: a.tool.clone(),
                name: a.name.clone(),
                ..Default::default()
            });
            e.total += a.busy;
        }
        by.into_values().collect()
    };
    Ok(WeekReport {
        partial: end.as_str() > today,
        week_start: ws.to_string(),
        categories: compare(&cur.categories, &before.categories, 12),
        apps: compare(&cur.apps, &before.apps, 8),
        sites: compare(&cur.sites, &before.sites, 8),
        agents: compare(&agent_rows(&cur), &agent_rows(&before), 8),
        start,
        end,
        this,
        prev,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weeks_start_on_the_chosen_day() {
        // 2026-11-26 is a Thursday.
        assert_eq!(week_of("2026-11-26", "monday").unwrap(), "2026-11-23");
        assert_eq!(week_of("2026-11-26", "sunday").unwrap(), "2026-11-22");
        assert_eq!(week_of("2026-11-23", "monday").unwrap(), "2026-11-23");
        assert_eq!(week_of("2026-11-22", "monday").unwrap(), "2026-11-16");
    }

    #[test]
    fn rows_are_compared_across_the_two_weeks() {
        let row = |k: &str, t: i64| Row {
            key: k.into(),
            name: k.into(),
            total: t,
            ..Default::default()
        };
        let v = compare(
            &[row("a", 50), row("b", 10)],
            &[row("b", 30), row("c", 5)],
            8,
        );
        let got: Vec<(&str, i64, i64)> =
            v.iter().map(|c| (c.key.as_str(), c.cur, c.prev)).collect();
        assert_eq!(got, vec![("a", 50, 0), ("b", 10, 30), ("c", 0, 5)]);
    }

    #[test]
    fn a_week_in_progress_is_compared_with_the_same_days() {
        let dir = std::env::temp_dir().join(format!("at-week-{}", std::process::id()));
        let st = crate::store::Store::open(&dir.join("t.db")).unwrap();
        let cfg = Config::default();
        let w = week(&st.conn, &cfg, "2026-11-26", "2026-11-26", Cache::ReadOnly).unwrap();
        assert_eq!(
            (w.start.as_str(), w.end.as_str(), w.partial),
            ("2026-11-23", "2026-11-29", true)
        );
        assert_eq!(
            (w.this.days.len(), w.prev.from.as_str(), w.prev.to.as_str()),
            (4, "2026-11-16", "2026-11-19")
        );
        let done = week(&st.conn, &cfg, "2026-11-20", "2026-11-26", Cache::ReadOnly).unwrap();
        assert!(!done.partial);
        assert_eq!((done.this.days.len(), done.prev.days.len()), (7, 7));
        drop(st);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
