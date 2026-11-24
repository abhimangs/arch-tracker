//! What Claude Code's tokens would cost at Anthropic's API prices. The prices are built in, so
//! this works offline; config.toml's [agents.prices] can add or correct a model.

use super::{BadRequest, ProjectRules, parse_date};
use crate::config::{Config, ModelPrice};
use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// When PRICES was copied from https://platform.claude.com/docs/en/about-claude/pricing
pub const PRICES_AS_OF: &str = "2026-11-24";

/// A model's prices in USD per million tokens. Cache writes cost 1.25x (5 minutes) and 2x
/// (1 hour) the input price, cache reads `read` times it; all of them follow fast mode and long
/// prompts too.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub read: f64,
    /// Fast mode's input and output prices.
    pub fast: Option<(f64, f64)>,
    /// Prompts over this many tokens (cache reads and writes included) cost these instead.
    pub long: Option<(u64, f64, f64)>,
}

const fn p(input: f64, output: f64, read: f64) -> Price {
    Price {
        input,
        output,
        read,
        fast: None,
        long: None,
    }
}

const fn fast(mut x: Price, i: f64, o: f64) -> Price {
    x.fast = Some((i, o));
    x
}

/// Anthropic's first-party prices. The longest matching name wins, so "claude-opus-4-5" is not
/// priced as "claude-opus-4".
const PRICES: &[(&str, Price)] = &[
    ("claude-fable-5-2", fast(p(8.0, 40.0, 0.025), 16.0, 80.0)),
    ("claude-mythos-5-2", fast(p(8.0, 40.0, 0.025), 16.0, 80.0)),
    ("claude-fable-5-1", p(10.0, 50.0, 0.025)),
    ("claude-mythos-5-1", p(10.0, 50.0, 0.025)),
    ("claude-fable-5", p(10.0, 50.0, 0.1)),
    ("claude-mythos-5", p(10.0, 50.0, 0.1)),
    ("claude-opus-5-5", fast(p(4.0, 20.0, 0.05), 8.0, 40.0)),
    ("claude-opus-5", fast(p(5.0, 25.0, 0.1), 10.0, 50.0)),
    ("claude-opus-4-8", fast(p(5.0, 25.0, 0.1), 10.0, 50.0)),
    ("claude-opus-4-7", p(5.0, 25.0, 0.1)),
    ("claude-opus-4-6", p(5.0, 25.0, 0.1)),
    ("claude-opus-4-5", p(5.0, 25.0, 0.1)),
    ("claude-opus-4-1", p(15.0, 75.0, 0.1)),
    ("claude-opus-4", p(15.0, 75.0, 0.1)),
    ("claude-sonnet-5-5", p(2.0, 10.0, 0.05)),
    ("claude-sonnet-5", p(2.0, 10.0, 0.1)),
    ("claude-sonnet-4-6", p(3.0, 15.0, 0.1)),
    ("claude-sonnet-4-5", p(3.0, 15.0, 0.1)),
    ("claude-sonnet-4", p(3.0, 15.0, 0.1)),
    (
        "claude-haiku-5-5",
        Price {
            input: 0.1,
            output: 0.5,
            read: 0.1,
            fast: None,
            long: Some((100_000, 0.5, 2.5)),
        },
    ),
    ("claude-haiku-4-5", p(1.0, 5.0, 0.1)),
];

/// US-only inference (inference_geo "us") costs 1.1x on every token.
const US_ONLY: f64 = 1.1;
/// Web search: $10 per 1,000 searches. Web fetch costs only its tokens.
const PER_SEARCH: f64 = 0.01;

/// "us.anthropic.claude-opus-4-5-20251101-v1:0" and "claude-opus-4-5@20251101" -> "claude-opus-4-5-…".
fn base_name(model: &str) -> String {
    let m = model.trim().to_lowercase();
    let m = m.find("claude-").map(|i| &m[i..]).unwrap_or(&m);
    let m = m.split(['@', ':']).next().unwrap_or(m);
    m.to_string()
}

fn matches(model: &str, name: &str) -> bool {
    model == name || model.strip_prefix(name).is_some_and(|r| r.starts_with('-'))
}

/// The price of a model: your own from config.toml first, then the built-in table.
pub fn price_for(model: &str, own: &BTreeMap<String, ModelPrice>) -> Option<Price> {
    let m = base_name(model);
    let mine = own
        .iter()
        .map(|(k, v)| (base_name(k), v))
        .filter(|(k, _)| matches(&m, k))
        .max_by_key(|(k, _)| k.len());
    if let Some((_, v)) = mine {
        return Some(Price {
            input: v.input,
            output: v.output,
            read: match v.cache_read {
                Some(r) if v.input > 0.0 => r / v.input,
                _ => 0.1,
            },
            fast: None,
            long: None,
        });
    }
    PRICES
        .iter()
        .filter(|(k, _)| matches(&m, k))
        .max_by_key(|(k, _)| k.len())
        .map(|(_, p)| *p)
}

/// One reply's tokens, as stored by sources::claude.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tokens4 {
    pub input: u64,
    pub output: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub web_search: u64,
    pub fast: bool,
    pub us_only: bool,
}

/// What one reply cost, in USD.
#[cfg(test)]
pub fn cost(p: &Price, t: &Tokens4) -> f64 {
    cost_parts(p, t).total()
}

/// What one reply cost, in USD, per kind of token.
pub fn cost_parts(p: &Price, t: &Tokens4) -> CostBy {
    let prompt = t.input + t.cache_5m + t.cache_1h + t.cache_read;
    let (mut i, mut o) = (p.input, p.output);
    if let Some((over, li, lo)) = p.long
        && prompt > over
    {
        (i, o) = (li, lo);
    }
    if t.fast
        && let Some((fi, fo)) = p.fast
    {
        (i, o) = (fi, fo);
    }
    let per = if t.us_only { US_ONLY } else { 1.0 } / 1e6;
    CostBy {
        input: t.input as f64 * i * per,
        output: t.output as f64 * o * per,
        cache_write: (t.cache_5m as f64 * 1.25 + t.cache_1h as f64 * 2.0) * i * per,
        cache_read: t.cache_read as f64 * i * p.read * per,
        web_search: t.web_search as f64 * PER_SEARCH,
    }
}

/// Cost in USD per kind of token.
#[derive(Serialize, Default, Clone, Copy, Debug, PartialEq)]
pub struct CostBy {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub web_search: f64,
}

impl CostBy {
    pub fn total(&self) -> f64 {
        self.input + self.output + self.cache_write + self.cache_read + self.web_search
    }
    fn add(&mut self, o: &CostBy) {
        self.input += o.input;
        self.output += o.output;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.web_search += o.web_search;
    }
}

#[derive(Serialize, Default, Clone, Debug, PartialEq)]
pub struct TokenSums {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl TokenSums {
    fn add(&mut self, t: &Tokens4) {
        self.input += t.input;
        self.output += t.output;
        self.cache_write += t.cache_5m + t.cache_1h;
        self.cache_read += t.cache_read;
    }
}

#[derive(Serialize, Default, Clone, Debug)]
pub struct CostRow {
    pub key: String,
    pub name: String,
    pub cost: f64,
    pub requests: u64,
    pub tokens: TokenSums,
    /// False when some of its replies had no known price (their tokens are counted, not priced).
    pub priced: bool,
}

#[derive(Serialize, Default, Clone, Debug)]
pub struct DayCost {
    pub date: String,
    pub cost: f64,
    pub requests: u64,
}

#[derive(Serialize, Default, Clone, Debug)]
pub struct ClaudeReport {
    pub from: String,
    pub to: String,
    pub prices_as_of: &'static str,
    pub cost: f64,
    /// The same cost split by kind of token.
    pub cost_by: CostBy,
    pub requests: u64,
    pub tokens: TokenSums,
    pub web_searches: u64,
    pub models: Vec<CostRow>,
    pub projects: Vec<CostRow>,
    pub days: Vec<DayCost>,
    /// Models with no price, built in or your own.
    pub unpriced: Vec<String>,
    /// For a single day: cost per local hour (hours[i] starts i hours after midnight).
    pub hourly: Vec<f64>,
    /// The transcripts are still being read for the first time: totals will grow.
    pub reading: bool,
    /// Use from before the oldest transcript, estimated from Claude Code's own statistics,
    /// when the range reaches back that far.
    pub earlier: Option<Earlier>,
}

/// Claude Code removes session transcripts after cleanupPeriodDays (30 by default), but keeps
/// statistics in ~/.claude/stats-cache.json: totals per model since it was first started, and
/// tokens per model per day. Those count a reply once per transcript line it fills (2 to 2.5
/// lines each), so the estimate divides by the ratio measured on the transcripts still on disk.
#[derive(Serialize, Default, Clone, Debug)]
pub struct Earlier {
    pub from: String,
    pub until: String,
    pub cost: f64,
    pub tokens: u64,
    pub models: Vec<CostRow>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Stats {
    #[serde(default)]
    model_usage: HashMap<String, StatModel>,
    #[serde(default)]
    daily_model_tokens: Vec<StatDay>,
    first_session_date: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct StatModel {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    web_search_requests: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatDay {
    date: String,
    #[serde(default)]
    tokens_by_model: HashMap<String, u64>,
}

/// Where Claude Code keeps its statistics, beside its transcripts.
pub fn stats_paths() -> Vec<PathBuf> {
    crate::sources::claude::roots()
        .iter()
        .filter_map(|r| r.parent().map(|p| p.join("stats-cache.json")))
        .collect()
}

fn read_stats(paths: &[PathBuf]) -> Option<Stats> {
    use std::io::Read;
    for p in paths {
        // Read-only, like the transcripts; a few hundred KB at most.
        let Ok(f) = crate::sources::claude::open_read_only(p) else {
            continue;
        };
        let mut s = String::new();
        if f.take(64 << 20).read_to_string(&mut s).is_ok()
            && let Ok(v) = serde_json::from_str::<Stats>(&s)
        {
            return Some(v);
        }
    }
    None
}

/// Claude Code's use before the oldest transcript still on disk, priced like the rest.
pub fn earlier(conn: &Connection, cfg: &Config, paths: &[PathBuf]) -> Result<Option<Earlier>> {
    let Some(stats) = read_stats(paths) else {
        return Ok(None);
    };
    // The first day with transcripts; everything before it only Claude Code's statistics know.
    let first: Option<i64> = conn
        .query_row("SELECT min(at_ms) FROM claude_usage", [], |r| r.get(0))
        .unwrap_or(None);
    let first_day = first
        .map(crate::util::local_date)
        .unwrap_or_else(|| "9999-12-31".into());
    // Lines per reply, per model and overall, from the transcripts read so far.
    let mut ratio: HashMap<String, f64> = HashMap::new();
    let (mut all_lines, mut all_unique) = (0f64, 0f64);
    let (mut w5, mut w1) = (0f64, 0f64);
    {
        let mut st = conn.prepare(
            "SELECT u.model, sum(u.input + u.output + u.cache_5m + u.cache_1h + u.cache_read),
                    coalesce(l.tokens, 0), sum(u.cache_5m), sum(u.cache_1h)
             FROM claude_usage u LEFT JOIN claude_lines l ON l.model = u.model GROUP BY u.model",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as f64,
                r.get::<_, i64>(2)? as f64,
                r.get::<_, i64>(3)? as f64,
                r.get::<_, i64>(4)? as f64,
            ))
        })?;
        for row in rows {
            let (model, unique, lines, c5, c1) = row?;
            w5 += c5;
            w1 += c1;
            if unique > 0.0 && lines >= unique {
                ratio.insert(base_name(&model), lines / unique);
                all_lines += lines;
                all_unique += unique;
            }
        }
    }
    // Without transcripts to measure, two lines per reply is what Claude Code writes most.
    let overall = if all_unique > 0.0 {
        all_lines / all_unique
    } else {
        2.0
    };
    let hour_share = if w5 + w1 > 0.0 { w1 / (w5 + w1) } else { 0.0 };
    let mut out = Earlier {
        from: stats
            .first_session_date
            .as_deref()
            .and_then(|d| d.get(..10))
            .unwrap_or("")
            .to_string(),
        until: parse_date(&first_day)
            .ok()
            .and_then(|(d, _, _)| d.yesterday().ok())
            .map(|d| d.to_string())
            .unwrap_or_default(),
        ..Default::default()
    };
    for (model, m) in &stats.model_usage {
        let life = m.input_tokens
            + m.output_tokens
            + m.cache_read_input_tokens
            + m.cache_creation_input_tokens;
        let since: u64 = stats
            .daily_model_tokens
            .iter()
            .filter(|d| d.date.as_str() >= first_day.as_str())
            .filter_map(|d| d.tokens_by_model.get(model))
            .sum();
        let before = life.saturating_sub(since);
        if life == 0 || before == 0 {
            continue;
        }
        let f = ratio
            .get(&base_name(model))
            .copied()
            .unwrap_or(overall)
            .clamp(1.0, 5.0);
        let share = before as f64 / life as f64 / f;
        let writes = m.cache_creation_input_tokens as f64;
        let t = Tokens4 {
            input: m.input_tokens,
            output: m.output_tokens,
            cache_5m: (writes * (1.0 - hour_share)) as u64,
            cache_1h: (writes * hour_share) as u64,
            cache_read: m.cache_read_input_tokens,
            web_search: m.web_search_requests,
            ..Default::default()
        };
        // Totals, not single requests: long-prompt prices can't be told apart here.
        let price = price_for(model, &cfg.agents.prices).map(|p| Price { long: None, ..p });
        let cost = price
            .map(|p| cost_parts(&p, &t).total() * share)
            .unwrap_or(0.0);
        let key = base_name(model);
        out.cost += cost;
        let tokens = (before as f64 / f) as u64;
        out.tokens += tokens;
        out.models.push(CostRow {
            name: model_label(&key),
            key,
            cost,
            requests: 0,
            tokens: TokenSums {
                input: (m.input_tokens as f64 * share) as u64,
                output: (m.output_tokens as f64 * share) as u64,
                cache_write: (writes * share) as u64,
                cache_read: (m.cache_read_input_tokens as f64 * share) as u64,
            },
            priced: price.is_some(),
        });
    }
    if out.models.is_empty() {
        return Ok(None);
    }
    out.models.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    Ok(Some(out))
}

/// "claude-opus-5-5" -> "Opus 5.5", "claude-sonnet-4-5-20250929" -> "Sonnet 4.5".
pub fn model_label(model: &str) -> String {
    let m = base_name(model);
    let Some(rest) = m.strip_prefix("claude-") else {
        return model.to_string();
    };
    let mut parts = rest.split('-');
    let family = parts.next().unwrap_or(rest);
    let version: Vec<&str> = parts
        .take_while(|x| x.len() <= 2 && x.chars().all(|c| c.is_ascii_digit()))
        .collect();
    let mut f = family.chars();
    let family = f
        .next()
        .map(|c| c.to_uppercase().chain(f).collect::<String>())
        .unwrap_or_default();
    if version.is_empty() {
        family
    } else {
        format!("{family} {}", version.join("."))
    }
}

/// Claude Code's usage between two local dates (both included), priced.
pub fn claude(conn: &Connection, cfg: &Config, from: &str, to: &str) -> Result<ClaudeReport> {
    let (start, ds, _) = parse_date(from)?;
    let (end, de_from, de) = parse_date(to)?;
    if end < start {
        return Err(BadRequest("'to' must not be before 'from'".into()).into());
    }
    if start.until(end)?.get_days() >= super::MAX_RANGE_DAYS {
        return Err(BadRequest(format!(
            "a range can be at most {} days",
            super::MAX_RANGE_DAYS
        ))
        .into());
    }
    let mut out = ClaudeReport {
        from: from.into(),
        to: to.into(),
        prices_as_of: PRICES_AS_OF,
        ..Default::default()
    };
    // A database from before this feature (arch-tracker serve on an old copy) has no table.
    let has_table: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='claude_usage'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    if !has_table {
        return Ok(out);
    }
    let mut st = conn.prepare(
        "SELECT model, project, at_ms, input, output, cache_5m, cache_1h, cache_read, web_search,
                fast, us_only
         FROM claude_usage WHERE at_ms >= ?1 AND at_ms < ?2 ORDER BY at_ms",
    )?;
    let rows = st.query_map([ds, de], |r| {
        let n = |i: usize| r.get::<_, i64>(i).map(|v| v.max(0) as u64);
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            Tokens4 {
                input: n(3)?,
                output: n(4)?,
                cache_5m: n(5)?,
                cache_1h: n(6)?,
                cache_read: n(7)?,
                web_search: n(8)?,
                fast: r.get(9)?,
                us_only: r.get(10)?,
            },
        ))
    })?;
    let rules = ProjectRules::new(&cfg.projects);
    let mut prices: HashMap<String, Option<Price>> = HashMap::new();
    let mut names: HashMap<String, (String, String)> = HashMap::new();
    let mut models: HashMap<String, CostRow> = HashMap::new();
    let mut projects: HashMap<String, CostRow> = HashMap::new();
    let mut days: Vec<DayCost> = vec![];
    // Local day boundaries, so each reply's date needs no time-zone lookup.
    let mut day_end = i64::MIN;
    if start == end {
        out.hourly = vec![0.0; ((de - de_from + 3_599_999) / 3_600_000).max(1) as usize];
    }
    for row in rows {
        let (model, folder, at, t) = row?;
        let price = *prices
            .entry(model.clone())
            .or_insert_with(|| price_for(&model, &cfg.agents.prices));
        let parts = price.map(|p| cost_parts(&p, &t)).unwrap_or_default();
        let c = parts.total();
        out.cost += c;
        out.cost_by.add(&parts);
        out.requests += 1;
        out.web_searches += t.web_search;
        out.tokens.add(&t);
        if let Some(h) = out.hourly.get_mut(((at - ds) / 3_600_000).max(0) as usize) {
            *h += c;
        }
        let mkey = base_name(&model);
        let m = models.entry(mkey.clone()).or_insert_with(|| CostRow {
            name: model_label(&mkey),
            key: mkey,
            priced: true,
            ..Default::default()
        });
        m.cost += c;
        m.requests += 1;
        m.tokens.add(&t);
        m.priced &= price.is_some();
        let (pkey, pname) = names
            .entry(folder.clone())
            .or_insert_with(|| {
                rules
                    .of_path(&folder)
                    .unwrap_or_else(|| (String::new(), "No project".into()))
            })
            .clone();
        let pr = projects.entry(pkey.clone()).or_insert_with(|| CostRow {
            key: pkey,
            name: pname,
            priced: true,
            ..Default::default()
        });
        pr.cost += c;
        pr.requests += 1;
        pr.tokens.add(&t);
        pr.priced &= price.is_some();
        if at >= day_end {
            let date = crate::util::local_date(at);
            day_end = parse_date(&date).map(|(_, _, e)| e).unwrap_or(i64::MAX);
            days.push(DayCost {
                date,
                cost: 0.0,
                requests: 0,
            });
        }
        if let Some(d) = days.last_mut() {
            d.cost += c;
            d.requests += 1;
        }
    }
    out.unpriced = models
        .values()
        .filter(|m| !m.priced)
        .map(|m| m.key.clone())
        .collect();
    out.unpriced.sort();
    let by_cost =
        |a: &CostRow, b: &CostRow| b.cost.total_cmp(&a.cost).then(b.requests.cmp(&a.requests));
    out.models = models.into_values().collect();
    out.models.sort_by(by_cost);
    out.projects = projects.into_values().collect();
    out.projects.sort_by(by_cost);
    out.days = days;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(input: u64, output: u64, c5: u64, c1: u64, read: u64) -> Tokens4 {
        Tokens4 {
            input,
            output,
            cache_5m: c5,
            cache_1h: c1,
            cache_read: read,
            ..Default::default()
        }
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn prices_follow_the_published_table() {
        let none = BTreeMap::new();
        let opus = price_for("claude-opus-5-5", &none).unwrap();
        // 1M of each kind: $4 in, $20 out, $5 5-minute writes, $8 1-hour writes, $0.20 reads.
        let one = 1_000_000;
        assert!(near(cost(&opus, &t(one, 0, 0, 0, 0)), 4.0));
        assert!(near(cost(&opus, &t(0, one, 0, 0, 0)), 20.0));
        assert!(near(cost(&opus, &t(0, 0, one, 0, 0)), 5.0));
        assert!(near(cost(&opus, &t(0, 0, 0, one, 0)), 8.0));
        assert!(near(cost(&opus, &t(0, 0, 0, 0, one)), 0.2));
        let fable = price_for("claude-fable-5-2", &none).unwrap();
        assert!(near(cost(&fable, &t(0, 0, 0, 0, one)), 0.2), "0.025x reads");
        assert!(
            near(cost(&fable, &t(0, 0, one, one, 0)), 26.0),
            "$10 + $16 writes"
        );
        let sonnet = price_for("claude-sonnet-5-5", &none).unwrap();
        assert!(near(cost(&sonnet, &t(0, 0, 0, 0, one)), 0.1));
        assert!(near(
            cost(
                &price_for("claude-sonnet-5", &none).unwrap(),
                &t(0, 0, 0, 0, one)
            ),
            0.2
        ));
        // Longest name wins: Opus 4.5 is not Opus 4.
        assert!(near(
            cost(
                &price_for("claude-opus-4-5-20251101", &none).unwrap(),
                &t(one, 0, 0, 0, 0)
            ),
            5.0
        ));
        assert!(near(
            cost(
                &price_for("claude-opus-4-20250514", &none).unwrap(),
                &t(one, 0, 0, 0, 0)
            ),
            15.0
        ));
        // Cloud spellings.
        assert!(price_for("us.anthropic.claude-sonnet-4-5-20250929-v1:0", &none).is_some());
        assert!(price_for("claude-opus-4-5@20251101", &none).is_some());
        assert!(price_for("gpt-5", &none).is_none());
    }

    #[test]
    fn haiku_long_prompts_fast_mode_us_only_and_searches() {
        let none = BTreeMap::new();
        let haiku = price_for("claude-haiku-5-5", &none).unwrap();
        // 100,000 tokens of prompt is still the short price; one more is the long one.
        assert!(near(cost(&haiku, &t(100_000, 0, 0, 0, 0)), 0.01));
        let long = t(1, 1_000_000, 0, 0, 100_000);
        assert!(near(
            cost(&haiku, &long),
            (0.5 + 100_000.0 * 0.05 + 2.5e6) / 1e6
        ));
        let opus = price_for("claude-opus-5-5", &none).unwrap();
        let mut f = t(1_000_000, 1_000_000, 0, 0, 1_000_000);
        f.fast = true;
        assert!(
            near(cost(&opus, &f), 8.0 + 40.0 + 0.4),
            "fast mode with cache multipliers on top"
        );
        let mut us = t(1_000_000, 0, 0, 0, 0);
        us.us_only = true;
        us.web_search = 3;
        assert!(near(cost(&opus, &us), 4.4 + 0.03));
    }

    #[test]
    fn your_own_prices_come_first() {
        let mut own = BTreeMap::new();
        own.insert(
            "claude-new-model".to_string(),
            ModelPrice {
                input: 3.0,
                output: 15.0,
                cache_read: None,
            },
        );
        own.insert(
            "claude-opus-5-5".to_string(),
            ModelPrice {
                input: 1.0,
                output: 2.0,
                cache_read: Some(0.5),
            },
        );
        let p = price_for("claude-new-model-20270101", &own).unwrap();
        assert!(near(cost(&p, &t(0, 0, 0, 0, 1_000_000)), 0.3));
        let o = price_for("claude-opus-5-5", &own).unwrap();
        assert!(near(cost(&o, &t(1_000_000, 0, 0, 0, 1_000_000)), 1.5));
    }

    #[test]
    fn earlier_use_is_estimated_from_claude_codes_statistics() {
        let dir = std::env::temp_dir().join(format!("at-earlier-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        drop(crate::store::Store::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        let (_, ds, _) = parse_date("2026-09-10").unwrap();
        // On disk: one Opus 5.5 reply of 1M cache reads, written on 2 lines.
        conn.execute(
            "INSERT INTO claude_usage VALUES (1, ?1, 'claude-opus-5-5', '', 0, 0, 0, 0, 1000000, 0, 0, 0)",
            [ds + 3_600_000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO claude_lines VALUES ('claude-opus-5-5', 2000000)",
            [],
        )
        .unwrap();
        // Claude Code's statistics: 6M lines-tokens of Opus 5.5 reads in all, 2M of them on or
        // after the first transcript day; and 4M of a model with no transcripts left.
        let stats = r#"{"version":5,"firstSessionDate":"2026-05-28T06:35:21.547Z",
            "modelUsage":{"claude-opus-5-5":{"inputTokens":0,"outputTokens":0,"cacheReadInputTokens":6000000,"cacheCreationInputTokens":0},
                          "claude-sonnet-4-6":{"inputTokens":0,"outputTokens":0,"cacheReadInputTokens":4000000,"cacheCreationInputTokens":0}},
            "dailyModelTokens":[{"date":"2026-08-01","tokensByModel":{"claude-opus-5-5":4000000}},
                                {"date":"2026-09-10","tokensByModel":{"claude-opus-5-5":2000000}}]}"#;
        let sp = dir.join("stats-cache.json");
        std::fs::write(&sp, stats).unwrap();
        let e = earlier(&conn, &Config::default(), &[sp]).unwrap().unwrap();
        assert_eq!(
            (e.from.as_str(), e.until.as_str()),
            ("2026-05-28", "2026-09-09")
        );
        // Opus 5.5: 4M lines-tokens before, 2 lines per reply -> 2M reads at $0.20 = $0.40.
        // Sonnet 4.6: 4M, the overall ratio (2) -> 2M reads at $0.30 = $0.60.
        assert!(near(e.cost, 1.0), "{}", e.cost);
        assert_eq!(e.tokens, 4_000_000);
        assert_eq!(e.models[0].name, "Sonnet 4.6");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_labels() {
        assert_eq!(model_label("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_label("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(model_label("claude-opus-5"), "Opus 5");
        assert_eq!(
            model_label("us.anthropic.claude-haiku-4-5-20251001-v1:0"),
            "Haiku 4.5"
        );
    }

    #[test]
    fn a_report_prices_groups_and_splits_by_day() {
        let dir = std::env::temp_dir().join(format!("at-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        drop(crate::store::Store::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        let (_, ds, _) = parse_date("2026-11-20").unwrap();
        let add = |id: i64, at: i64, model: &str, project: &str, input: i64| {
            conn.execute(
                "INSERT INTO claude_usage VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0, 0, 0, 0, 0)",
                rusqlite::params![id, at, model, project, input],
            )
            .unwrap();
        };
        add(
            1,
            ds + 3_600_000,
            "claude-opus-5-5",
            "/nonexistent/a",
            1_000_000,
        );
        add(
            2,
            ds + 7_200_000,
            "claude-opus-5-5",
            "/nonexistent/a",
            1_000_000,
        );
        add(3, ds + 30 * 3_600_000, "claude-mystery-1", "", 500);
        add(4, ds - 1, "claude-opus-5-5", "", 1_000_000);
        let r = claude(&conn, &Config::default(), "2026-11-20", "2026-11-21").unwrap();
        assert_eq!(r.requests, 3, "only replies inside the range");
        assert!(near(r.cost, 8.0));
        assert_eq!(r.unpriced, ["claude-mystery-1"]);
        assert_eq!(r.models[0].name, "Opus 5.5");
        assert!(!r.models[1].priced);
        assert_eq!(r.days.len(), 2);
        assert!(near(r.days[0].cost, 8.0));
        assert_eq!(r.tokens.input, 2_000_500);
        assert!(near(r.cost_by.input, 8.0) && near(r.cost_by.total(), r.cost));
        assert!(r.hourly.is_empty(), "hours only for a single day");
        let one = claude(&conn, &Config::default(), "2026-11-20", "2026-11-20").unwrap();
        assert!(near(one.hourly[1], 4.0) && near(one.hourly[2], 4.0));
        assert_eq!(one.days[0].requests, 2);
        assert!(r.projects.iter().any(|p| p.name == "No project"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
