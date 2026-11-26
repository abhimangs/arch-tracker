//! Browsers: which browser each piece of time belongs to, a browser's sites grouped by domain,
//! and a day's visits with what came right before and after them. Decided at query time from
//! the raw subjects, the browsers the extension connected from and the installed web apps, so
//! nothing here rewrites history.

use super::{
    BadRequest, Cache, DayRollup, Row, Session, SimpleRow, Times, TitleRow, each_day, host_key,
    parse_date,
};
use crate::config::Config;
use crate::desktop::is_web_app;
use crate::model::Subject;
use crate::store;
use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The time no browser could be told for: older versions filed some of it as "Browser".
pub const UNKNOWN: &str = "unknown";
pub const UNKNOWN_NAME: &str = "Unknown browser";
/// Every browser together.
pub const ALL: &str = "all";
/// The app older versions stored when they could not tell the browser.
const LEGACY_UNKNOWN: &str = "browser";
/// Days the menu orders browsers by.
const RECENT_DAYS: i64 = 30;

/// Which browser a subject is: browsers the extension linked or that have site data, and the
/// installed web apps they run.
#[derive(Clone, Debug, Default)]
pub struct BrowserMap {
    /// Browser app key -> name.
    names: HashMap<String, String>,
    /// Installed web app -> its browser.
    web_apps: HashMap<String, String>,
    /// Lowercase name -> browser key, for subjects named after their browser (the Plasma
    /// Browser Integration bridge's).
    by_name: HashMap<String, String>,
    /// Browser -> (first, last) time the extension connected from it.
    linked: HashMap<String, (i64, i64)>,
}

impl BrowserMap {
    pub fn load(conn: &Connection) -> Result<BrowserMap> {
        let mut m = BrowserMap {
            web_apps: store::web_apps(conn)?,
            ..Default::default()
        };
        for l in store::browser_links(conn)? {
            m.linked.insert(l.app.clone(), (l.first_ms, l.last_ms));
            m.add(&l.app, &l.name);
        }
        let mut stmt = conn.prepare_cached(
            "SELECT app, MAX(app_name) FROM subjects WHERE host != '' OR private = 1 GROUP BY app",
        )?;
        let seen = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        m.saw_all(&seen);
        Ok(m)
    }

    /// From the subjects at hand only (no database).
    #[cfg(test)]
    pub fn from_subjects<'a>(subjects: impl IntoIterator<Item = &'a Subject>) -> BrowserMap {
        let seen: Vec<(String, String)> = subjects
            .into_iter()
            .filter(|s| !s.host.is_empty() || s.private)
            .map(|s| (s.app.clone(), s.app_name.clone()))
            .collect();
        let mut m = BrowserMap::default();
        m.saw_all(&seen);
        m
    }

    /// Apps seen with site data: browsers, unless they are something that runs in one.
    fn saw_all(&mut self, seen: &[(String, String)]) {
        for (app, name) in seen {
            let other = app == LEGACY_UNKNOWN
                || app == "plasma-browser-integration"
                || app == "hidden"
                || self.web_apps.contains_key(app)
                || is_web_app(app, "");
            if !other && !self.names.contains_key(app) {
                self.add(app, name);
            }
        }
    }

    fn add(&mut self, app: &str, name: &str) {
        let name = if name.is_empty() { app } else { name };
        self.names.insert(app.to_string(), name.to_string());
        self.by_name
            .entry(name.to_lowercase())
            .or_insert_with(|| app.to_string());
    }

    /// The browser a subject's time belongs to, and whether it came through an installed web
    /// app. None for time that is not browsing.
    pub fn of(&self, s: &Subject) -> Option<(String, bool)> {
        if let Some(b) = self.web_apps.get(&s.app) {
            return Some((b.clone(), true));
        }
        if self.names.contains_key(&s.app) {
            return Some((s.app.clone(), false));
        }
        if s.host.is_empty() && !s.private {
            return None;
        }
        if s.app == LEGACY_UNKNOWN {
            return Some((UNKNOWN.into(), false));
        }
        if is_web_app(&s.app, "") {
            // From before web apps were recorded: Chromium names them after their browser
            // ("brave-<id>-Default"), which settles it when one browser fits.
            let first = s.app.split(['-', '_']).next().unwrap_or("");
            let mut fits = self
                .names
                .keys()
                .filter(|k| k.split(['-', '.', '_']).next() == Some(first));
            return match (fits.next(), fits.next()) {
                (Some(k), None) => Some((k.clone(), true)),
                _ => Some((UNKNOWN.into(), true)),
            };
        }
        let named = self.by_name.get(&s.app_name.to_lowercase());
        Some((named.cloned().unwrap_or_else(|| UNKNOWN.into()), false))
    }

    /// Whether a browser's time belongs in the view for id: that browser, or for ALL every
    /// browser the extension has connected from.
    pub fn wanted(&self, id: &str, key: &str) -> bool {
        if id == ALL {
            self.linked.contains_key(key)
        } else {
            key == id
        }
    }

    pub fn name(&self, key: &str) -> String {
        match self.names.get(key) {
            Some(n) => n.clone(),
            None if key == UNKNOWN => UNKNOWN_NAME.into(),
            None => key.into(),
        }
    }
}

/// One host in a browser's list.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SiteRow {
    /// Empty for tabs the browser showed but nothing could name (no extension).
    pub host: String,
    /// "site" (grouped by domain), "browser" (its own pages), "local" or "unknown". Filled in
    /// when a report is made, never cached.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub domain: String,
    pub category: String,
    pub color: String,
    #[serde(flatten)]
    pub times: Times,
    pub total: i64,
    pub visible: i64,
    /// The part of total spent in an installed web app.
    #[serde(default)]
    pub app_secs: i64,
    /// Seconds per hour of the local day (0-23).
    #[serde(default)]
    pub hours: Vec<i64>,
    #[serde(default)]
    pub titles: Vec<TitleRow>,
}

/// One browser's day (also what the rollup cache keeps for it).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BrowserDay {
    pub key: String,
    pub name: String,
    #[serde(flatten)]
    pub times: Times,
    pub total: i64,
    pub visible: i64,
    pub background_audio: i64,
    /// Time in private windows: counted, but never with a site or title.
    pub private: i64,
    pub hours: Vec<i64>,
    pub sites: Vec<SiteRow>,
    pub categories: Vec<Row>,
    /// Background audio by site (or by title, for media without one).
    pub audio: Vec<SimpleRow>,
}

fn add_hours(into: &mut [i64; 24], from: &[i64]) {
    for (a, b) in into.iter_mut().zip(from) {
        *a += b;
    }
}

#[derive(Default)]
struct SiteAcc {
    times: Times,
    visible: i64,
    app_secs: i64,
    hours: [i64; 24],
    titles: HashMap<String, i64>,
    category: (String, String),
}

/// Adds up one browser over a day (from spans) or a range (from days).
#[derive(Default)]
pub(super) struct BrowserAcc {
    name: String,
    times: Times,
    visible: i64,
    background_audio: i64,
    private: i64,
    hours: [i64; 24],
    sites: HashMap<String, SiteAcc>,
    categories: HashMap<String, (Times, (String, String))>,
    audio: HashMap<String, i64>,
}

impl BrowserAcc {
    pub(super) fn named(&mut self, name: impl FnOnce() -> String) -> &mut Self {
        if self.name.is_empty() {
            self.name = name();
        }
        self
    }

    /// Focused time; hours holds its seconds per hour of the day.
    pub(super) fn focus(
        &mut self,
        s: &Subject,
        web_app: bool,
        t: &Times,
        hours: &[i64; 24],
        cat: &(String, String),
    ) {
        self.times.merge(t);
        add_hours(&mut self.hours, hours);
        self.categories
            .entry(cat.0.clone())
            .or_insert_with(|| (Times::default(), cat.clone()))
            .0
            .merge(t);
        if s.private {
            self.private += t.total();
            return;
        }
        let site = self.sites.entry(s.host.clone()).or_default();
        site.times.merge(t);
        add_hours(&mut site.hours, hours);
        if web_app {
            site.app_secs += t.total();
        }
        if site.category.0.is_empty() {
            site.category = cat.clone();
        }
        if !s.title.is_empty() {
            *site.titles.entry(s.title.clone()).or_default() += t.total();
        }
    }

    /// On screen beside the focused window.
    pub(super) fn visible(&mut self, s: &Subject, v: i64, cat: impl FnOnce() -> (String, String)) {
        self.visible += v;
        if s.private {
            return;
        }
        let site = self.sites.entry(s.host.clone()).or_default();
        site.visible += v;
        if site.category.0.is_empty() {
            site.category = cat();
        }
    }

    pub(super) fn audio(&mut self, s: &Subject, secs: i64) {
        self.background_audio += secs;
        let what = if s.host.is_empty() { &s.title } else { &s.host };
        *self.audio.entry(what.clone()).or_default() += secs;
    }

    fn add_day(&mut self, d: &BrowserDay) {
        self.named(|| d.name.clone());
        self.times.merge(&d.times);
        self.visible += d.visible;
        self.background_audio += d.background_audio;
        self.private += d.private;
        add_hours(&mut self.hours, &d.hours);
        for s in &d.sites {
            let a = self.sites.entry(s.host.clone()).or_default();
            a.times.merge(&s.times);
            a.visible += s.visible;
            a.app_secs += s.app_secs;
            add_hours(&mut a.hours, &s.hours);
            if a.category.0.is_empty() {
                a.category = (s.category.clone(), s.color.clone());
            }
            for t in &s.titles {
                *a.titles.entry(t.title.clone()).or_default() += t.secs;
            }
        }
        for c in &d.categories {
            self.categories
                .entry(c.key.clone())
                .or_insert_with(|| (Times::default(), (c.category.clone(), c.color.clone())))
                .0
                .merge(&c.times);
        }
        for a in &d.audio {
            *self.audio.entry(a.name.clone()).or_default() += a.secs;
        }
    }

    pub(super) fn finish(self, key: String, max_titles: usize) -> BrowserDay {
        let mut sites: Vec<SiteRow> = self
            .sites
            .into_iter()
            .map(|(host, a)| {
                let mut titles: Vec<TitleRow> = a
                    .titles
                    .into_iter()
                    .filter(|(_, s)| *s > 0)
                    .map(|(title, secs)| TitleRow { title, secs })
                    .collect();
                titles.sort_by(|x, y| y.secs.cmp(&x.secs).then_with(|| x.title.cmp(&y.title)));
                titles.truncate(max_titles);
                SiteRow {
                    host,
                    category: a.category.0,
                    color: a.category.1,
                    total: a.times.total(),
                    times: a.times,
                    visible: a.visible,
                    app_secs: a.app_secs,
                    hours: a.hours.to_vec(),
                    titles,
                    ..Default::default()
                }
            })
            .filter(|s| s.total > 0 || s.visible > 0)
            .collect();
        sites.sort_by(|a, b| {
            (b.total, b.visible)
                .cmp(&(a.total, a.visible))
                .then_with(|| a.host.cmp(&b.host))
        });
        let mut categories: Vec<Row> = self
            .categories
            .into_iter()
            .map(|(name, (times, cat))| Row {
                key: name.clone(),
                name,
                category: cat.0,
                color: cat.1,
                total: times.total(),
                times,
                ..Default::default()
            })
            .filter(|r| r.total > 0)
            .collect();
        categories.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.key.cmp(&b.key)));
        let mut audio: Vec<SimpleRow> = self
            .audio
            .into_iter()
            .filter(|(_, s)| *s > 0)
            .map(|(name, secs)| SimpleRow {
                name,
                detail: String::new(),
                secs,
            })
            .collect();
        audio.sort_by(|a, b| b.secs.cmp(&a.secs).then_with(|| a.name.cmp(&b.name)));
        BrowserDay {
            key,
            name: self.name,
            total: self.times.total(),
            times: self.times,
            visible: self.visible,
            background_audio: self.background_audio,
            private: self.private,
            hours: self.hours.to_vec(),
            sites,
            categories,
            audio,
        }
    }
}

/// Fills in each site's group and domain.
fn place(sites: &mut [SiteRow]) {
    for s in sites {
        let (group, domain) = domain_of(&s.host);
        s.group = group.into();
        s.domain = domain;
    }
}

/// Where a host goes in a browser's site list: (group, domain). Sites group by registrable
/// domain (music.youtube.com and www.youtube.com under youtube.com, bbc.co.uk as a whole); the
/// browser's own pages, local servers and tabs nothing could name are groups of their own.
pub fn domain_of(host: &str) -> (&'static str, String) {
    let h = host.trim().to_lowercase();
    if h.is_empty() {
        return ("unknown", String::new());
    }
    if h == "file://" {
        return ("local", String::new());
    }
    if h.contains("://") || h.starts_with("about:") || h == "devtools" {
        return ("browser", String::new());
    }
    let name = host_key(&h);
    if let Ok(ip) = name.parse::<std::net::IpAddr>() {
        return if local_ip(&ip) {
            ("local", String::new())
        } else {
            ("site", name)
        };
    }
    const LOCAL: [&str; 7] = [
        "localhost",
        "local",
        "lan",
        "internal",
        "home.arpa",
        "localdomain",
        "test",
    ];
    if !name.contains('.')
        || LOCAL
            .iter()
            .any(|t| name == *t || name.ends_with(&format!(".{t}")))
    {
        return ("local", String::new());
    }
    let domain = psl::domain_str(&name).unwrap_or(&name).to_string();
    ("site", domain)
}

fn local_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified()
        }
        std::net::IpAddr::V6(v) => {
            let first = v.segments()[0];
            v.is_loopback()
                || v.is_unspecified()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80
        }
    }
}

/// A browser's time per day of a range.
#[derive(Serialize, Clone, Debug, Default)]
pub struct DayTotal {
    pub date: String,
    #[serde(flatten)]
    pub times: Times,
    pub total: i64,
}

/// One browser's part of the All browsers view.
#[derive(Serialize, Clone, Debug, Default)]
pub struct BrowserShare {
    pub id: String,
    pub name: String,
    pub total: i64,
    pub visible: i64,
}

/// A browser (or all of them) over a range of days.
#[derive(Serialize, Clone, Debug, Default)]
pub struct BrowserReport {
    pub id: String,
    pub name: String,
    pub from: String,
    pub to: String,
    #[serde(flatten)]
    pub times: Times,
    pub total: i64,
    pub visible: i64,
    pub background_audio: i64,
    pub private: i64,
    pub days_used: usize,
    /// Seconds per hour of the day, over the whole range.
    pub hours: Vec<i64>,
    pub days: Vec<DayTotal>,
    pub sites: Vec<SiteRow>,
    pub categories: Vec<Row>,
    pub audio: Vec<SimpleRow>,
    /// Every browser's total (for All browsers).
    pub browsers: Vec<BrowserShare>,
}

fn display(cfg: &Config, key: &str, name: String) -> String {
    cfg.browsers
        .name_for(key)
        .map(str::to_string)
        .unwrap_or(name)
}

/// A browser (or ALL) over an inclusive range, from the day rollups.
pub fn browser_report(
    conn: &Connection,
    cfg: &Config,
    id: &str,
    from: &str,
    to: &str,
    cache: Cache,
) -> Result<BrowserReport> {
    if id.is_empty() {
        return Err(BadRequest("no browser given".into()).into());
    }
    let map = BrowserMap::load(conn)?;
    let wanted = |key: &str| map.wanted(id, key);
    let mut acc = BrowserAcc::default();
    let mut shares: HashMap<String, BrowserShare> = HashMap::new();
    let mut days = vec![];
    each_day(conn, cfg, &map, from, to, cache, |date, r: DayRollup| {
        let mut day = Times::default();
        for b in r.browsers.iter().filter(|b| wanted(&b.key)) {
            acc.add_day(b);
            day.merge(&b.times);
            let s = shares.entry(b.key.clone()).or_insert_with(|| BrowserShare {
                id: b.key.clone(),
                name: b.name.clone(),
                ..Default::default()
            });
            s.total += b.total;
            s.visible += b.visible;
        }
        days.push(DayTotal {
            date: date.to_string(),
            total: day.total(),
            times: day,
        });
    })?;
    let name = if id == ALL {
        "All browsers".to_string()
    } else {
        display(cfg, id, map.name(id))
    };
    let mut d = acc.finish(id.to_string(), 10);
    place(&mut d.sites);
    let mut browsers: Vec<BrowserShare> = shares
        .into_values()
        .map(|mut s| {
            s.name = display(cfg, &s.id, map.name(&s.id));
            s
        })
        .collect();
    browsers.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.name.cmp(&b.name)));
    Ok(BrowserReport {
        id: id.to_string(),
        name,
        from: from.to_string(),
        to: to.to_string(),
        times: d.times,
        total: d.total,
        visible: d.visible,
        background_audio: d.background_audio,
        private: d.private,
        days_used: days.iter().filter(|x| x.total > 0).count(),
        hours: d.hours,
        days,
        sites: d.sites,
        categories: d.categories,
        audio: d.audio,
        browsers,
    })
}

/// The app, site or program on either side of a visit.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Neighbor {
    /// "site", "terminal", "editor" or "app".
    pub kind: String,
    pub app_name: String,
    /// Site host, terminal program, or empty.
    pub detail: String,
    pub project: String,
    pub title: String,
    pub browser: String,
    /// Time between the two (away, idle or locked).
    pub gap_ms: i64,
}

/// One stretch on one site (a session of the Timeline, in a browser).
#[derive(Serialize, Clone, Debug, Default)]
pub struct Visit {
    pub start_ms: i64,
    pub end_ms: i64,
    pub host: String,
    pub group: String,
    pub domain: String,
    pub title: String,
    pub category: String,
    pub color: String,
    #[serde(flatten)]
    pub times: Times,
    pub counted: i64,
    pub browser: String,
    pub web_app: bool,
    pub private: bool,
    pub before: Option<Neighbor>,
    pub after: Option<Neighbor>,
}

/// A browser's (or every browser's) visits on one day.
#[derive(Serialize, Clone, Debug, Default)]
pub struct BrowserDayReport {
    pub date: String,
    pub id: String,
    pub name: String,
    /// A break longer than this ends a session ("after 12 min away").
    pub session_gap_ms: i64,
    pub visits: Vec<Visit>,
    /// The day's sites, each with its seconds per hour.
    pub sites: Vec<SiteRow>,
    pub hours: Vec<i64>,
}

fn neighbor(s: &Session, gap_ms: i64) -> Neighbor {
    Neighbor {
        kind: s.kind.clone(),
        app_name: s.app_name.clone(),
        detail: s.detail.clone(),
        project: s.project.clone(),
        title: s.title.clone(),
        browser: s.browser.clone(),
        gap_ms: gap_ms.max(0),
    }
}

/// Visits from a day's sessions: those in this browser (or any, for ALL), each with the
/// session before and after it, whatever it was.
pub fn visits(sessions: &[Session], id: &str) -> Vec<Visit> {
    let mut out = vec![];
    for (i, s) in sessions.iter().enumerate() {
        if s.browser.is_empty() || (id != ALL && s.browser != id) {
            continue;
        }
        let private = s.kind == "site" && s.detail == super::PRIVATE_DETAIL;
        let host = if s.kind == "site" && !private {
            s.detail.clone()
        } else {
            String::new()
        };
        let (group, domain) = if private {
            ("private", String::new())
        } else {
            domain_of(&host)
        };
        let before = i
            .checked_sub(1)
            .and_then(|j| sessions.get(j))
            .map(|p| neighbor(p, s.start_ms - p.end_ms));
        let after = sessions
            .get(i + 1)
            .map(|n| neighbor(n, n.start_ms - s.end_ms));
        out.push(Visit {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            host,
            group: group.into(),
            domain,
            title: s.title.clone(),
            category: s.category.clone(),
            color: s.color.clone(),
            times: s.times,
            counted: s.counted,
            browser: s.browser.clone(),
            web_app: s.web_app,
            private,
            before,
            after,
        });
    }
    out
}

/// One day of a browser (or ALL): its visits and its sites hour by hour.
pub fn browser_day(
    conn: &Connection,
    cfg: &Config,
    id: &str,
    date: &str,
) -> Result<BrowserDayReport> {
    if id.is_empty() {
        return Err(BadRequest("no browser given".into()).into());
    }
    let (_, ds, de) = parse_date(date)?;
    let map = BrowserMap::load(conn)?;
    let r = super::build(conn, cfg, date, ds, de, &map)?;
    let mut acc = BrowserAcc::default();
    for b in r.browsers.iter().filter(|b| map.wanted(id, &b.key)) {
        acc.add_day(b);
    }
    let mut d = acc.finish(id.to_string(), 10);
    place(&mut d.sites);
    Ok(BrowserDayReport {
        date: date.to_string(),
        id: id.to_string(),
        name: if id == ALL {
            "All browsers".to_string()
        } else {
            display(cfg, id, map.name(id))
        },
        session_gap_ms: cfg.insights.session_gap_secs as i64 * 1000,
        visits: visits(&r.sessions, id)
            .into_iter()
            .filter(|v| map.wanted(id, &v.browser))
            .collect(),
        sites: d.sites,
        hours: d.hours,
    })
}

/// A browser for the dashboard's menu.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct BrowserEntry {
    pub id: String,
    /// The name to show (renamed in Settings, or the browser's own).
    pub name: String,
    pub orig_name: String,
    pub hidden: bool,
    /// The extension has connected from it.
    pub linked: bool,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    /// Screen time today, and over the last 30 days (the menu's order).
    pub today: i64,
    pub recent: i64,
}

/// Every browser the extension has connected from, most used first.
pub fn browser_list(
    conn: &Connection,
    cfg: &Config,
    today: &str,
    cache: Cache,
) -> Result<Vec<BrowserEntry>> {
    let map = BrowserMap::load(conn)?;
    let (end, _, _) = parse_date(today)?;
    let from = end
        .checked_sub(jiff::Span::new().days(RECENT_DAYS - 1))?
        .to_string();
    let mut time: HashMap<String, (i64, i64)> = HashMap::new();
    each_day(
        conn,
        cfg,
        &map,
        &from,
        today,
        cache,
        |date, r: DayRollup| {
            for b in &r.browsers {
                let e = time.entry(b.key.clone()).or_default();
                e.1 += b.total;
                if date == today {
                    e.0 += b.total;
                }
            }
        },
    )?;
    // Only browsers the extension has connected from: others have no exact tabs to show.
    let keys: Vec<String> = map.linked.keys().cloned().collect();
    let mut out: Vec<BrowserEntry> = keys
        .into_iter()
        .map(|id| {
            let orig = map.name(&id);
            let (today, recent) = time.get(&id).copied().unwrap_or_default();
            let link = map.linked.get(&id);
            BrowserEntry {
                name: display(cfg, &id, orig.clone()),
                orig_name: orig,
                hidden: cfg.browsers.is_hidden(&id),
                linked: link.is_some(),
                first_ms: link.map(|l| l.0),
                last_ms: link.map(|l| l.1),
                today,
                recent,
                id,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.recent
            .cmp(&a.recent)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Span};
    use rusqlite::params;

    const DS: i64 = 1_000_000_000_000;
    const M: i64 = 60_000;

    fn site(app: &str, name: &str, host: &str, title: &str) -> Subject {
        Subject {
            app: app.into(),
            app_name: name.into(),
            host: host.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    fn sp(kind: Kind, subject: i64, a: i64, b: i64) -> Span {
        Span {
            kind,
            subject,
            start: DS + a,
            end: DS + b,
        }
    }

    #[test]
    fn hosts_group_by_domain() {
        let d = |h: &str| domain_of(h);
        assert_eq!(d("music.youtube.com"), ("site", "youtube.com".into()));
        assert_eq!(d("www.youtube.com"), ("site", "youtube.com".into()));
        assert_eq!(d("news.bbc.co.uk"), ("site", "bbc.co.uk".into()));
        assert_eq!(d("someone.github.io"), ("site", "someone.github.io".into()));
        assert_eq!(d("example.org:8443"), ("site", "example.org".into()));
        assert_eq!(d("93.184.216.34"), ("site", "93.184.216.34".into()));
        for local in [
            "localhost:3000",
            "127.0.0.1:8000",
            "[::1]:5173",
            "192.168.1.20",
            "router",
            "nas.local",
            "app.localhost",
            "file://",
        ] {
            assert_eq!(d(local).0, "local", "{local}");
        }
        for own in [
            "brave://settings",
            "about:newtab",
            "chrome-extension://abc",
            "devtools",
        ] {
            assert_eq!(d(own).0, "browser", "{own}");
        }
        assert_eq!(d("").0, "unknown");
    }

    #[test]
    fn subjects_find_their_browser() {
        let a = site("browser-a", "Browser A", "a.example.org", "A");
        let legacy = site("browser", "Browser", "b.example.org", "B");
        let bridge = site(
            "plasma-browser-integration",
            "Browser A",
            "c.example.org",
            "C",
        );
        let app = site(
            "browser-abcdefghijklmnopabcdefghijklmnop-Default",
            "Music",
            "music.example.org",
            "M",
        );
        let mut m = BrowserMap::from_subjects([&a, &legacy, &bridge, &app]);
        assert_eq!(m.of(&a), Some(("browser-a".into(), false)));
        assert_eq!(m.of(&legacy), Some((UNKNOWN.into(), false)));
        assert_eq!(
            m.of(&bridge),
            Some(("browser-a".into(), false)),
            "by its name"
        );
        assert_eq!(m.of(&app), Some(("browser-a".into(), true)), "by its id");
        // A browser window without a tab anyone could name is still that browser's time.
        let untold = Subject::app("browser-a", "Browser A");
        assert_eq!(m.of(&untold), Some(("browser-a".into(), false)));
        assert_eq!(m.of(&Subject::app("editor", "Editor")), None);
        // A recorded web app wins over guesses.
        m.web_apps.insert(app.app.clone(), "browser-b".into());
        assert_eq!(m.of(&app), Some(("browser-b".into(), true)));
        assert_eq!(m.name(UNKNOWN), UNKNOWN_NAME);
        assert_eq!(m.name("browser-a"), "Browser A");
    }

    /// Two browsers, a terminal in between, a private window, a local server and audio from a
    /// tab an older version could not place.
    fn sample() -> (HashMap<i64, Subject>, Vec<Span>) {
        let mut subjects = HashMap::new();
        subjects.insert(
            1,
            site("browser-a", "Browser A", "music.example.org", "Song"),
        );
        subjects.insert(
            2,
            site("browser-a", "Browser A", "www.example.org", "Video"),
        );
        subjects.insert(
            3,
            site("browser-b", "Browser B", "docs.example.net", "Docs"),
        );
        subjects.insert(
            4,
            Subject {
                private: true,
                ..Subject::app("browser-a", "Browser A")
            },
        );
        subjects.insert(
            5,
            Subject {
                program: "claude".into(),
                project: "~/p/x".into(),
                ..Subject::app("kitty", "kitty")
            },
        );
        subjects.insert(6, site("browser", "Browser", "music.example.org", "Song"));
        subjects.insert(7, site("browser-a", "Browser A", "localhost:3000", "Dev"));
        let spans = vec![
            sp(Kind::Focus, 1, 0, 10 * M),
            sp(Kind::Focus, 5, 10 * M, 20 * M),
            sp(Kind::Focus, 3, 20 * M, 30 * M),
            sp(Kind::Focus, 2, 30 * M, 35 * M),
            sp(Kind::Visible, 3, 30 * M, 35 * M),
            sp(Kind::Focus, 4, 35 * M, 40 * M),
            sp(Kind::BgAudio, 6, 0, 15 * M),
            // Back after a long break.
            sp(Kind::Focus, 7, 50 * M, 55 * M),
        ];
        (subjects, spans)
    }

    #[test]
    fn a_day_splits_into_browsers() {
        let (subjects, spans) = sample();
        let r = super::super::compute(
            &Config::default(),
            "x",
            DS,
            DS + 3_600_000,
            &spans,
            &subjects,
        );
        let get = |k: &str| r.browsers.iter().find(|b| b.key == k).unwrap();
        let a = get("browser-a");
        assert_eq!(a.total, 25 * 60);
        assert_eq!(a.private, 5 * 60);
        assert_eq!(a.hours.iter().sum::<i64>(), a.total);
        let hosts: Vec<(&str, i64)> = a.sites.iter().map(|s| (s.host.as_str(), s.total)).collect();
        assert_eq!(
            hosts,
            [
                ("music.example.org", 600),
                ("localhost:3000", 300),
                ("www.example.org", 300)
            ]
        );
        assert_eq!(a.sites[0].titles[0].title, "Song");
        assert_eq!(a.sites[0].hours.iter().sum::<i64>(), 600);
        let b = get("browser-b");
        assert_eq!((b.total, b.visible), (600, 300));
        let unknown = get(UNKNOWN);
        assert_eq!((unknown.total, unknown.background_audio), (0, 15 * 60));
        assert_eq!(unknown.audio[0].name, "music.example.org");
        // The combined Sites list says which browsers a site's time came from.
        let music = r
            .sites
            .iter()
            .find(|s| s.key == "music.example.org")
            .unwrap();
        assert_eq!(music.browsers, ["browser-a"]);
        // Sessions know their browser; the terminal's doesn't.
        assert_eq!(r.sessions[0].browser, "browser-a");
        assert_eq!(r.sessions[1].browser, "");
    }

    #[test]
    fn visits_know_what_came_before_and_after() {
        let (subjects, spans) = sample();
        let r = super::super::compute(
            &Config::default(),
            "x",
            DS,
            DS + 3_600_000,
            &spans,
            &subjects,
        );
        let v = visits(&r.sessions, "browser-a");
        let hosts: Vec<&str> = v.iter().map(|x| x.host.as_str()).collect();
        assert_eq!(
            hosts,
            ["music.example.org", "www.example.org", "", "localhost:3000"]
        );
        assert_eq!(v[0].before, None);
        let after = v[0].after.as_ref().unwrap();
        assert_eq!(
            (after.kind.as_str(), after.detail.as_str(), after.gap_ms),
            ("terminal", "claude", 0)
        );
        assert_eq!(v[1].before.as_ref().unwrap().detail, "docs.example.net");
        assert_eq!(v[1].before.as_ref().unwrap().browser, "browser-b");
        assert!(v[2].private && v[2].title.is_empty());
        assert_eq!(
            v[3].before.as_ref().unwrap().gap_ms,
            10 * M,
            "after ten minutes away"
        );
        assert_eq!((v[3].group.as_str(), v[3].domain.as_str()), ("local", ""));
        assert_eq!(v[0].domain, "example.org");
        assert_eq!(visits(&r.sessions, ALL).len(), 5);
    }

    fn day(d: &str) -> i64 {
        crate::util::date_bounds(d).unwrap().0
    }

    fn sample_db() -> crate::store::Store {
        let dir = std::env::temp_dir().join(format!(
            "at-browsers-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let st = crate::store::Store::open(&dir.join("t.db")).unwrap();
        let c = &st.conn;
        for (app, name, host, title) in [
            ("browser-a", "Browser A", "music.example.org", "Song"),
            ("browser-a", "Browser A", "www.example.org", "Video"),
            ("browser-b", "Browser B", "docs.example.net", "Docs"),
        ] {
            c.execute(
                "INSERT INTO subjects(app, app_name, title, host, private, program, project)
                 VALUES (?1, ?2, ?3, ?4, 0, '', '')",
                params![app, name, title, host],
            )
            .unwrap();
        }
        let span = |s: i64, a: i64, b: i64| {
            c.execute(
                "INSERT INTO spans(kind, subject_id, start_ms, end_ms) VALUES (1, ?1, ?2, ?3)",
                params![s, a, b],
            )
            .unwrap();
        };
        let h = 3_600_000;
        span(1, day("2020-01-01") + h, day("2020-01-01") + 2 * h);
        span(3, day("2020-01-01") + 2 * h, day("2020-01-01") + 3 * h);
        span(
            2,
            day("2020-01-01") + 3 * h,
            day("2020-01-01") + 3 * h + 30 * M,
        );
        span(1, day("2020-01-03") + h, day("2020-01-03") + h + 20 * M);
        st.note_browser("browser-a", "Browser A", day("2020-01-01"))
            .unwrap();
        st.note_browser("browser-b", "Browser B", day("2020-01-01"))
            .unwrap();
        st.note_browser("browser-c", "Browser C", day("2020-01-02"))
            .unwrap();
        st
    }

    #[test]
    fn ranges_add_up_a_browser_and_all_of_them() {
        let st = sample_db();
        let c = &st.conn;
        let mut cfg = Config::default();
        cfg.browsers.names = vec!["browser-a=A".into()];
        let r = browser_report(
            c,
            &cfg,
            "browser-a",
            "2020-01-01",
            "2020-01-03",
            Cache::ReadWrite,
        )
        .unwrap();
        assert_eq!(r.name, "A", "renamed in the settings");
        assert_eq!(r.total, 3600 + 1800 + 1200);
        assert_eq!((r.days.len(), r.days_used), (3, 2));
        assert_eq!(r.days[2].total, 1200);
        let music = &r.sites[0];
        assert_eq!(
            (music.host.as_str(), music.total),
            ("music.example.org", 4800)
        );
        assert_eq!(
            (music.group.as_str(), music.domain.as_str()),
            ("site", "example.org")
        );
        // The cached days give the same answer.
        let again = browser_report(
            c,
            &cfg,
            "browser-a",
            "2020-01-01",
            "2020-01-03",
            Cache::ReadWrite,
        )
        .unwrap();
        assert_eq!(again.total, r.total);
        assert_eq!(again.sites[0].titles[0].title, "Song");
        let all =
            browser_report(c, &cfg, ALL, "2020-01-01", "2020-01-03", Cache::ReadOnly).unwrap();
        assert_eq!(all.total, r.total + 3600);
        let shares: Vec<(&str, i64)> = all
            .browsers
            .iter()
            .map(|b| (b.name.as_str(), b.total))
            .collect();
        assert_eq!(shares, [("A", 6600), ("Browser B", 3600)]);
        assert!(browser_report(c, &cfg, "", "2020-01-01", "2020-01-01", Cache::ReadOnly).is_err());
    }

    #[test]
    fn the_menu_lists_only_browsers_the_extension_connected_from() {
        let st = sample_db();
        st.conn
            .execute("DELETE FROM browsers WHERE app = 'browser-b'", [])
            .unwrap();
        let mut cfg = Config::default();
        cfg.browsers.hidden = vec!["browser-c".into()];
        let list = browser_list(&st.conn, &cfg, "2020-01-03", Cache::ReadOnly).unwrap();
        let ids: Vec<&str> = list.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["browser-a", "browser-c"], "no history-only browser");
        assert_eq!((list[0].today, list[0].recent), (1200, 6600));
        let c = &list[1];
        assert!(
            c.linked && c.hidden && c.recent == 0,
            "linked, no browsing yet"
        );
        assert_eq!(c.first_ms, Some(day("2020-01-02")));
        // All browsers covers only the linked ones.
        let all = browser_report(
            &st.conn,
            &cfg,
            ALL,
            "2020-01-01",
            "2020-01-03",
            Cache::ReadOnly,
        );
        assert_eq!(all.unwrap().total, 6600);
    }

    #[test]
    fn a_browser_day_lists_its_visits() {
        let st = sample_db();
        let r = browser_day(&st.conn, &Config::default(), "browser-a", "2020-01-01").unwrap();
        assert_eq!(r.visits.len(), 2);
        assert_eq!(
            r.visits[1].before.as_ref().unwrap().detail,
            "docs.example.net"
        );
        assert_eq!(r.sites.len(), 2);
        assert_eq!(r.hours.iter().sum::<i64>(), 3600 + 1800);
        assert!(browser_day(&st.conn, &Config::default(), "browser-a", "nope").is_err());
    }

    #[test]
    fn browser_settings_are_cleaned_up() {
        let mut cfg = Config::default();
        cfg.browsers.hidden = vec![" a ".into(), "a".into(), "".into()];
        cfg.browsers.names = vec!["a = A ".into(), "b=".into(), "a=AA".into(), "junk".into()];
        let b = cfg.sanitized().browsers;
        assert_eq!(b.hidden, ["a"]);
        assert_eq!(b.names, ["a=AA"]);
        assert_eq!(b.name_for("a"), Some("AA"));
        assert_eq!(b.name_for("b"), None);
        assert!(b.is_hidden("a") && !b.is_hidden("b"));
    }
}
