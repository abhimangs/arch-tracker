//! Desktop notifications: a heads-up before, and a notice at, each category's daily limit and the
//! daily screen-time goal; a reminder to take a break after a long unbroken stretch; and a notice
//! when Codex's 5-hour or weekly plan limit passes the share you chose (once per limit window,
//! remembered in the database); and, from agentwatch, when an AI agent you aren't looking at
//! finishes or waits for you (a click takes you to its terminal). Each can play a sound.
//!
//! `[notifications] enabled = false` turns all of it off: no report is built, nothing is sent.
//! The numbers come from the same day report as Today, so a notice never disagrees with the page.
//! Each notice is sent once a day; what had already happened when the tracker started (or when
//! notifications were switched on) is not announced late.

use crate::config::Config;
use crate::daemon::SharedRef;
use crate::report::{self, DayReport};
use crate::web::Web;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zbus::wire::Variant;
use zbus::{Connection, MatchRule, MessageStream};

const BUS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// Agents are looked at more often: a finished turn should be announced within seconds.
const AGENTS_EVERY: Duration = Duration::from_secs(3);

/// Asks the notifier for something outside its schedule.
pub enum Cmd {
    /// Show a sample notice now; answers with why it could not be shown.
    Test(oneshot::Sender<Result<(), String>>),
}

/// A notice to show. `family` notices replace each other on screen (a heads-up by the notice that
/// the limit is reached, one break reminder by the next).
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub key: String,
    pub family: String,
    /// The dashboard page a click opens ("" for Today).
    pub route: String,
    pub summary: String,
    pub body: String,
}

/// What was sent today.
#[derive(Default)]
pub struct Book {
    day: String,
    sent: HashSet<String>,
    /// The first look after starting (or switching on) only records what already happened.
    primed: bool,
}

impl Book {
    fn reset(&mut self) {
        *self = Book::default();
    }
}

/// What the live status says right now.
#[derive(Default, Clone, Copy)]
pub struct Live {
    pub paused: bool,
    /// No keyboard or mouse input for a while (you may be on a break already).
    pub away: bool,
    /// A call (microphone open) or a video playing in a visible window: a break reminder waits.
    pub busy: bool,
}

pub fn spawn(
    conn: Connection,
    shared: SharedRef,
    web: Arc<Web>,
    db: PathBuf,
) -> mpsc::UnboundedSender<Cmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run(conn, shared, web, db, rx));
    tx
}

async fn run(
    conn: Connection,
    shared: SharedRef,
    web: Arc<Web>,
    db: PathBuf,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut book = Book::default();
    // Notification ids on screen: family -> id, and id -> the activation token its click brought.
    let mut shown: HashMap<String, u32> = HashMap::new();
    let mut tokens: HashMap<u32, String> = HashMap::new();
    // The page each notice on screen opens.
    let mut routes: HashMap<u32, String> = HashMap::new();
    let mut signals = signal_stream(&conn).await;
    let mut tick = tokio::time::interval(CHECK_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut agent_tick = tokio::time::interval(AGENTS_EVERY);
    agent_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut watch = Some(crate::agentwatch::Watch::new(crate::util::now_ms()));
    loop {
        tokio::select! {
            _ = agent_tick.tick() => {
                let (cfg, live) = snapshot(&shared);
                let n = &cfg.notifications;
                if !n.enabled || !n.agents || live.paused {
                    watch = Some(crate::agentwatch::Watch::new(crate::util::now_ms()));
                    continue;
                }
                let Some(mut w) = watch.take() else { continue };
                let (agents, focus) = agent_status(&shared);
                let sound = n.sound;
                let r = tokio::task::spawn_blocking(move || {
                    let looking = focus.as_ref().map(|(a, b)| (a.as_str(), b.as_str()));
                    let notes = w.check(&cfg, &agents, looking, crate::util::now_ms());
                    (w, notes)
                })
                .await;
                let Ok((w, notes)) = r else {
                    watch = Some(crate::agentwatch::Watch::new(crate::util::now_ms()));
                    continue;
                };
                watch = Some(w);
                for note in notes {
                    let replaces = shown.get(&note.family).copied().unwrap_or(0);
                    match notify(&conn, &note, replaces, sound).await {
                        Ok(id) => {
                            shown.insert(note.family.clone(), id);
                            routes.insert(id, note.route.clone());
                        }
                        Err(e) => tracing::warn!("notification not shown: {e}"),
                    }
                }
            }
            _ = tick.tick() => {
                let (cfg, live) = snapshot(&shared);
                if !cfg.notifications.enabled {
                    book.reset();
                    continue;
                }
                if live.paused {
                    continue;
                }
                let now = crate::util::now_ms();
                let mut notes = vec![];
                if wants_report(&cfg)
                    && let Some(day) = today(&db, &cfg).await
                {
                    if let Ok(mut s) = shared.lock() {
                        s.today_screen = Some((now, day.date.clone(), day.totals.screen));
                    }
                    notes.extend(due(&cfg, &day, now, live, &mut book));
                }
                notes.extend(lasting(&db, &cfg, now, live).await);
                for note in notes {
                    let replaces = shown.get(&note.family).copied().unwrap_or(0);
                    match notify(&conn, &note, replaces, cfg.notifications.sound).await {
                        Ok(id) => {
                            shown.insert(note.family.clone(), id);
                            routes.insert(id, note.route.clone());
                        }
                        Err(e) => tracing::warn!("notification not shown: {e}"),
                    }
                }
            }
            Some(cmd) = rx.recv() => match cmd {
                Cmd::Test(reply) => {
                    let note = Note {
                        key: "test".into(),
                        family: "test".into(),
                        route: String::new(),
                        summary: "Notifications work".into(),
                        body: "Limits, your daily goal and break reminders will show up like this.".into(),
                    };
                    let replaces = shown.get("test").copied().unwrap_or(0);
                    let sound = snapshot(&shared).0.notifications.sound;
                    let r = notify(&conn, &note, replaces, sound).await.map(|id| {
                        shown.insert("test".into(), id);
                    });
                    let _ = reply.send(r.map_err(|e| e.to_string()));
                }
            },
            Some(sig) = next_signal(&mut signals) => match sig {
                Signal::Token(id, t) => {
                    if shown.values().any(|v| *v == id) {
                        tokens.insert(id, t);
                    }
                }
                Signal::Clicked(id) => {
                    if shown.values().any(|v| *v == id) {
                        let route = routes.get(&id).filter(|r| !r.is_empty()).cloned();
                        let route = route.as_deref().unwrap_or("today");
                        let token = tokens.remove(&id);
                        // An agent's notice: its terminal, else its page in the dashboard.
                        if let Some(pid) = route.strip_prefix("focus:").and_then(|p| p.parse().ok()) {
                            if let Err(e) = crate::focus::agent(&conn, pid).await {
                                tracing::debug!("could not bring the agent forward: {e:#}");
                                crate::tray::open_dashboard(&conn, &web, "agent/all/1", token).await;
                            }
                        } else {
                            crate::tray::open_dashboard(&conn, &web, route, token).await;
                        }
                    }
                }
                Signal::Closed(id) => {
                    shown.retain(|_, v| *v != id);
                    tokens.remove(&id);
                    routes.remove(&id);
                }
            },
        }
    }
}

/// The agents running now and the agent and project in front of you, from the live status.
fn agent_status(shared: &SharedRef) -> (Vec<crate::agentwatch::Live>, Option<(String, String)>) {
    let Ok(s) = shared.lock() else {
        return (vec![], None);
    };
    let agents = serde_json::from_value(s.status["agents"].clone()).unwrap_or_default();
    let f = &s.status["focus"];
    let focus = f["program"].as_str().filter(|p| !p.is_empty()).map(|p| {
        (
            p.to_string(),
            f["project"].as_str().unwrap_or("").to_string(),
        )
    });
    (agents, focus)
}

fn snapshot(shared: &SharedRef) -> (Config, Live) {
    let Ok(s) = shared.lock() else {
        return (Config::default(), Live::default());
    };
    let st = &s.status;
    let truthy = |v: &serde_json::Value| match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
        serde_json::Value::Null => false,
        _ => true,
    };
    let live = Live {
        paused: st["paused"].as_bool().unwrap_or(false),
        away: st["idle"]["input_idle"].as_bool().unwrap_or(false)
            || st["locked"].as_bool().unwrap_or(false),
        busy: truthy(&st["mic"]) || truthy(&st["media_visible"]),
    };
    (s.config.clone(), live)
}

/// Whether anything is switched on that needs today's numbers.
fn wants_report(cfg: &Config) -> bool {
    let n = &cfg.notifications;
    (n.goal && cfg.insights.daily_goal_mins > 0)
        || (n.limits && cfg.categories.iter().any(|c| c.goal_mins > 0))
        || (n.breaks && n.break_after_mins > 0)
}

async fn today(db: &std::path::Path, cfg: &Config) -> Option<DayReport> {
    let (db, cfg) = (db.to_path_buf(), cfg.clone());
    tokio::task::spawn_blocking(move || {
        let conn = crate::store::open_readonly(&db)?;
        let date = crate::util::local_date(crate::util::now_ms());
        let day = report::build_day(&conn, &cfg, &date);
        drop(conn);
        crate::util::trim_heap();
        day
    })
    .await
    .ok()?
    .map_err(|e| tracing::debug!("notifications: no day report: {e:#}"))
    .ok()
}

/// The notices due now, each at most once a day. Pure, for tests: the caller sends them.
pub fn due(cfg: &Config, d: &DayReport, now: i64, live: Live, book: &mut Book) -> Vec<Note> {
    if book.day != d.date {
        book.day = d.date.clone();
        book.sent.clear();
    }
    let n = &cfg.notifications;
    let warn = n.warn_mins as i64 * 60;
    let mut out = vec![];
    let goal = cfg.insights.daily_goal_mins as i64 * 60;
    if n.goal && goal > 0 {
        let used = d.totals.screen;
        let dur = crate::tray::fmt_dur;
        if used >= goal {
            out.push(Note {
                key: format!("goal:{goal}"),
                family: "goal".into(),
                route: String::new(),
                summary: "Daily screen-time goal reached".into(),
                body: format!(
                    "{} on screen today; your goal is to stay under {}.",
                    dur(used),
                    dur(goal)
                ),
            });
        } else if warn > 0 && goal > warn && used >= goal - warn {
            out.push(Note {
                key: format!("goal-warn:{goal}:{warn}"),
                family: "goal".into(),
                route: String::new(),
                summary: format!("{} left of today's screen-time goal", dur(goal - used)),
                body: format!(
                    "{} on screen today; your goal is to stay under {}.",
                    dur(used),
                    dur(goal)
                ),
            });
        }
    }
    if n.limits {
        for c in cfg.categories.iter().filter(|c| c.goal_mins > 0) {
            let lim = c.goal_mins as i64 * 60;
            let used = d
                .categories
                .iter()
                .find(|r| r.name == c.name)
                .map(|r| r.total)
                .unwrap_or(0);
            let dur = crate::tray::fmt_dur;
            if used >= lim {
                out.push(Note {
                    key: format!("limit:{}:{lim}", c.name),
                    family: format!("limit:{}", c.name),
                    route: String::new(),
                    summary: format!("{} limit reached", c.name),
                    body: format!(
                        "{} on {} today; your daily limit is {}.",
                        dur(used),
                        c.name,
                        dur(lim)
                    ),
                });
            } else if warn > 0 && lim > warn && used >= lim - warn {
                out.push(Note {
                    key: format!("limit-warn:{}:{lim}:{warn}", c.name),
                    family: format!("limit:{}", c.name),
                    route: String::new(),
                    summary: format!("{} left for {}", dur(lim - used), c.name),
                    body: format!("{} of your {} daily limit used.", dur(used), dur(lim)),
                });
            }
        }
    }
    let every = n.break_after_mins as i64 * 60_000;
    if n.breaks && every > 0 && !live.away {
        let gap = cfg.insights.session_gap_secs as i64 * 1000;
        // The day report lags the screen by up to one database write.
        let recent = cfg.general.flush_interval_secs as i64 * 1000 + 90_000;
        if let Some((start, end)) = stretch(d, gap)
            && now - end <= recent
        {
            let k = (end - start) / every;
            let key = format!("break:{start}:{k}");
            if k >= 1 && !(live.busy && book.primed) && !book.sent.contains(&key) {
                let step = cfg.idle.threshold_secs.max(cfg.insights.session_gap_secs) as i64;
                out.push(Note {
                    key,
                    family: "break".into(),
                    route: String::new(),
                    summary: "Time for a break".into(),
                    body: format!(
                        "{} on screen without a break. Lock the screen or step away for {} to start fresh.",
                        crate::tray::fmt_dur((end - start) / 1000),
                        crate::tray::fmt_dur(step)
                    ),
                });
            }
        }
    }
    out.retain(|x| !book.sent.contains(&x.key));
    for x in &out {
        book.sent.insert(x.key.clone());
    }
    if !book.primed {
        // What had already happened before we were watching is recorded, not announced.
        book.primed = true;
        out.clear();
    }
    out
}

/// Notices that are not about one day (Codex's plan limits), each sent once and remembered in
/// the database, so a restart does not repeat them.
async fn lasting(db: &std::path::Path, cfg: &Config, now: i64, live: Live) -> Vec<Note> {
    let n = &cfg.notifications;
    if !n.plan_limits && !(n.weekly && !live.away) {
        return vec![];
    }
    let (db, cfg) = (db.to_path_buf(), cfg.clone());
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Note>> {
        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mut sent = load_sent(&conn);
        let before = sent.len();
        let mut out = vec![];
        if cfg.notifications.plan_limits
            && let Some(l) = crate::sources::codex::load_limits(&conn)?
        {
            out.extend(due_limits(&cfg, &l, now, &mut sent));
        }
        if cfg.notifications.weekly
            && !live.away
            && let Some(last) = weekly_due(&cfg, now, &sent)
        {
            let today = crate::util::local_date(now);
            let w = report::week::week(&conn, &cfg, &last, &today, report::Cache::ReadWrite)?;
            sent.insert(format!("weekly:{last}"), now);
            if w.this.screen > 0 {
                out.push(weekly_note(&w));
            }
        }
        // Forget what is more than ten days old.
        sent.retain(|_, at| now - *at < 10 * 86_400_000);
        if !out.is_empty() || sent.len() != before {
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
                rusqlite::params![SENT_KEY, serde_json::to_string(&sent)?],
            )?;
        }
        Ok(out)
    })
    .await
    .ok()
    .and_then(|r| r.map_err(|e| tracing::debug!("notifications: {e:#}")).ok())
    .unwrap_or_default()
}

const SENT_KEY: &str = "notices_sent";

/// The lasting notices sent: key -> when (ms).
fn load_sent(conn: &rusqlite::Connection) -> HashMap<String, i64> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [SENT_KEY], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|v| serde_json::from_str(&v).ok())
    .unwrap_or_default()
}

/// The week to sum up, if it is time: from 9:00 on the first day of the week on, once per week.
fn weekly_due(cfg: &Config, now: i64, sent: &HashMap<String, i64>) -> Option<String> {
    let t = jiff::Timestamp::from_millisecond(now)
        .ok()?
        .to_zoned_datetime(jiff::tz::TimeZone::system());
    let today = t.date().to_string();
    let this = report::week::week_of(&today, &cfg.general.week_start).ok()?;
    if today == this && t.hour() < 9 {
        return None;
    }
    let (d, _, _) = report::parse_date(&this).ok()?;
    let last = d.checked_sub(jiff::Span::new().days(7)).ok()?.to_string();
    (!sent.contains_key(&format!("weekly:{last}"))).then_some(last)
}

/// The weekly summary: screen time against the week before, where it went, and the agents.
pub fn weekly_note(w: &report::week::WeekReport) -> Note {
    let dur = crate::tray::fmt_dur;
    let (cur, prev) = (w.this.screen, w.prev.screen);
    let mut body = vec![];
    if prev > 0 {
        let p = ((cur - prev) as f64 / prev as f64 * 100.0).round() as i64;
        body.push(match p {
            0 => "About the same as the week before.".to_string(),
            p if p > 0 => format!("{p}% more than the week before."),
            p => format!("{}% less than the week before.", -p),
        });
    }
    let top: Vec<String> = w
        .apps
        .iter()
        .filter(|a| a.cur > 0)
        .take(2)
        .map(|a| format!("{} ({})", a.name, dur(a.cur)))
        .collect();
    if !top.is_empty() {
        body.push(format!("Most of it in {}.", top.join(" and ")));
    }
    if w.this.agent_busy > 0 {
        let cost = if w.this.cost >= 0.01 {
            format!("; ${:.2} at API prices", w.this.cost)
        } else {
            String::new()
        };
        body.push(format!("Agents worked {}{cost}.", dur(w.this.agent_busy)));
    }
    Note {
        key: format!("weekly:{}", w.start),
        family: "weekly".into(),
        route: format!("week/{}", w.start),
        summary: format!("Last week: {} on screen", dur(cur)),
        body: body.join(" "),
    }
}

/// A time as the dashboard shows it: "20:52" today, else "Nov 29, 18:38".
fn when(ms: i64, now: i64, h12: bool) -> String {
    let tz = jiff::tz::TimeZone::system();
    let (Ok(t), Ok(n)) = (
        jiff::Timestamp::from_millisecond(ms),
        jiff::Timestamp::from_millisecond(now),
    ) else {
        return String::new();
    };
    let (t, n) = (t.to_zoned_datetime(tz.clone()), n.to_zoned_datetime(tz));
    let clock = if h12 {
        t.strftime("%-I:%M %P").to_string()
    } else {
        t.strftime("%H:%M").to_string()
    };
    if t.date() == n.date() {
        format!("at {clock}")
    } else {
        format!("{}, {clock}", t.strftime("%b %-d"))
    }
}

/// Codex's plan limits past the level you chose, once per limit window. Pure, for tests.
pub fn due_limits(
    cfg: &Config,
    l: &crate::sources::codex::Limits,
    now: i64,
    sent: &mut HashMap<String, i64>,
) -> Vec<Note> {
    let n = &cfg.notifications;
    let mut out = vec![];
    for w in &l.windows {
        let (name, level) = match w.minutes {
            300 => ("5-hour".to_string(), n.limit_5h_percent),
            10080 => ("weekly".to_string(), n.limit_week_percent),
            m => (format!("{}-hour", m / 60), n.limit_5h_percent),
        };
        if level == 0 || w.used_percent < level as f64 {
            continue;
        }
        // A limit that has reset since Codex last reported starts again from nothing, and a
        // report older than the limit's window says nothing about now.
        let window_ms = w.minutes as i64 * 60_000;
        if w.resets_ms.is_some_and(|r| r <= now) || now - l.at_ms > window_ms {
            continue;
        }
        let window = w.resets_ms.unwrap_or(l.at_ms / window_ms.max(1));
        let key = format!("plan:{}:{window}:{level}", w.minutes);
        if sent.contains_key(&key) {
            continue;
        }
        sent.insert(key.clone(), now);
        let h12 = cfg.dashboard.time_format == "12h";
        out.push(Note {
            key,
            family: format!("plan:{}", w.minutes),
            route: "agent/codex/1".into(),
            summary: format!(
                "Codex has used {}% of its {name} limit",
                w.used_percent.round()
            ),
            body: match w.resets_ms {
                Some(r) => format!(
                    "It resets {}. You get this notice at {level}%; change that in Settings.",
                    when(r, now, h12)
                ),
                None => format!("You get this notice at {level}%; change that in Settings."),
            },
        });
    }
    out
}

/// The unbroken stretch of screen time that ends last today: (start, end) in ms. Sessions closer
/// together than the session break belong to one stretch, as in Today's stats.
fn stretch(d: &DayReport, gap: i64) -> Option<(i64, i64)> {
    let mut cur: Option<(i64, i64)> = None;
    for s in d.sessions.iter().filter(|s| s.counted > 0) {
        cur = match cur {
            Some((a, b)) if s.start_ms - b <= gap => Some((a, b.max(s.end_ms))),
            _ => Some((s.start_ms, s.end_ms)),
        };
    }
    cur
}

/// The app's own icon when the package installed it, else a theme icon every desktop has.
fn app_icon() -> &'static str {
    static ICON: std::sync::OnceLock<&str> = std::sync::OnceLock::new();
    ICON.get_or_init(|| {
        let installed = ["/usr/share", "/usr/local/share"].iter().any(|d| {
            Path::new(d)
                .join("icons/hicolor/scalable/apps/arch-tracker.svg")
                .exists()
        });
        if installed {
            "arch-tracker"
        } else {
            "appointment-soon"
        }
    })
}

async fn notify(conn: &Connection, n: &Note, replaces: u32, sound: bool) -> zbus::Result<u32> {
    let mut hints: HashMap<&str, Variant<'_>> = HashMap::new();
    hints.insert("urgency", Variant::from(1u8));
    if sound {
        hints.insert("sound-name", Variant::from("message-new-instant"));
    }
    let open = match n.route.as_str() {
        "" | "today" => "Open Today",
        r if r.starts_with("agent/") => "Open the agent",
        r if r.starts_with("focus:") => "Go to it",
        r if r.starts_with("week") => "Open the week",
        _ => "Open",
    };
    let reply = conn
        .call_method(
            Some(BUS),
            PATH,
            Some(BUS),
            "Notify",
            &(
                "arch-tracker",
                replaces,
                app_icon(),
                n.summary.as_str(),
                n.body.as_str(),
                vec!["default", open],
                hints,
                -1i32,
            ),
        )
        .await?;
    reply.body().deserialize::<u32>()
}

enum Signal {
    Token(u32, String),
    Clicked(u32),
    Closed(u32),
}

async fn signal_stream(conn: &Connection) -> Option<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(BUS)
        .path(PATH)
        .build()
        .ok()?;
    MessageStream::for_match_rule(rule, conn, None).await.ok()
}

async fn next_signal(s: &mut Option<MessageStream>) -> Option<Signal> {
    let Some(stream) = s.as_mut() else {
        return std::future::pending().await;
    };
    loop {
        let msg = match stream.next().await {
            Some(Ok(m)) => m,
            Some(Err(_)) => continue,
            None => return std::future::pending().await,
        };
        let member = msg
            .header()
            .member()
            .map(|m| m.to_string())
            .unwrap_or_default();
        let body = msg.body();
        let sig = match member.as_str() {
            "ActionInvoked" => body
                .deserialize::<(u32, String)>()
                .ok()
                .filter(|(_, k)| k == "default")
                .map(|(id, _)| Signal::Clicked(id)),
            "ActivationToken" => body
                .deserialize::<(u32, String)>()
                .ok()
                .map(|(id, t)| Signal::Token(id, t)),
            "NotificationClosed" => body
                .deserialize::<(u32, u32)>()
                .ok()
                .map(|(id, _)| Signal::Closed(id)),
            _ => None,
        };
        if sig.is_some() {
            return sig;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Row, Session};

    const M: i64 = 60_000;

    fn cfg() -> Config {
        let mut c = Config::default();
        c.notifications.enabled = true;
        c.notifications.warn_mins = 5;
        c.notifications.break_after_mins = 50;
        c.insights.daily_goal_mins = 120;
        c.insights.session_gap_secs = 60;
        for cat in c.categories.iter_mut().filter(|x| x.name == "Social") {
            cat.goal_mins = 30;
        }
        c
    }

    fn day(screen: i64, social: i64, sessions: &[(i64, i64)]) -> DayReport {
        DayReport {
            date: "2026-11-24".into(),
            totals: report::Totals {
                screen,
                ..Default::default()
            },
            categories: vec![Row {
                name: "Social".into(),
                total: social,
                ..Default::default()
            }],
            sessions: sessions
                .iter()
                .map(|&(a, b)| Session {
                    start_ms: a,
                    end_ms: b,
                    counted: (b - a) / 1000,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn keys(v: &[Note]) -> Vec<String> {
        v.iter()
            .map(|n| n.key.split(':').next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_weekly_note_sums_up_the_week() {
        use report::week::{Compare, WeekReport, WeekSide};
        let w = WeekReport {
            start: "2026-11-16".into(),
            this: WeekSide {
                screen: 9 * 3600,
                agent_busy: 2 * 3600,
                cost: 12.5,
                ..Default::default()
            },
            prev: WeekSide {
                screen: 10 * 3600,
                ..Default::default()
            },
            apps: vec![
                Compare {
                    name: "Zed".into(),
                    cur: 4 * 3600,
                    ..Default::default()
                },
                Compare {
                    name: "Brave".into(),
                    cur: 3 * 3600,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let n = weekly_note(&w);
        assert_eq!(n.route, "week/2026-11-16");
        assert!(n.summary.starts_with("Last week: 9"), "{}", n.summary);
        assert!(
            n.body.contains("10% less than the week before"),
            "{}",
            n.body
        );
        assert!(
            n.body.contains("Zed") && n.body.contains("Brave"),
            "{}",
            n.body
        );
        assert!(n.body.contains("$12.50 at API prices"), "{}", n.body);
        // Once per week: a week already summed up is not due again.
        let mut sent = HashMap::new();
        let c = cfg();
        let now = 1_795_700_000_000; // a Thursday, well past 9:00 on Monday
        let last = weekly_due(&c, now, &sent).unwrap();
        sent.insert(format!("weekly:{last}"), now);
        assert!(weekly_due(&c, now, &sent).is_none());
    }

    #[test]
    fn plan_limit_notices_come_once_per_limit_window() {
        use crate::sources::codex::{LimitWindow, Limits};
        let mut c = cfg();
        c.notifications.limit_week_percent = 80;
        let now = 1_795_000_000_000;
        let w = |minutes: u64, used: f64, resets: i64| LimitWindow {
            minutes,
            used_percent: used,
            resets_ms: Some(resets),
        };
        let l = Limits {
            at_ms: now - 60_000,
            plan: "plus".into(),
            windows: vec![
                w(300, 74.0, now + 3_600_000),
                w(10080, 72.0, now + 86_400_000),
            ],
        };
        let mut sent = HashMap::new();
        let v = due_limits(&c, &l, now, &mut sent);
        assert_eq!(
            v.len(),
            1,
            "the 5-hour limit passed 70%; the weekly one is under 80%"
        );
        assert_eq!(v[0].route, "agent/codex/1");
        assert!(
            v[0].summary.contains("74% of its 5-hour limit"),
            "{}",
            v[0].summary
        );
        assert!(
            due_limits(&c, &l, now + 60_000, &mut sent).is_empty(),
            "once per window"
        );
        // The next window, after a reset, can tell again.
        let next = Limits {
            at_ms: now + 4_000_000,
            windows: vec![w(300, 90.0, now + 3_600_000 + 18_000_000)],
            ..l.clone()
        };
        assert_eq!(due_limits(&c, &next, now + 4_100_000, &mut sent).len(), 1);
        // A limit that has reset since the report, or a report too old, says nothing.
        let mut fresh = HashMap::new();
        assert!(due_limits(&c, &l, now + 3_700_000, &mut fresh).is_empty());
        c.notifications.limit_5h_percent = 50;
        let old = Limits {
            at_ms: now - 6 * 3_600_000,
            ..l.clone()
        };
        assert!(due_limits(&c, &old, now, &mut fresh).is_empty());
    }

    #[test]
    fn what_happened_before_starting_is_not_announced() {
        let c = cfg();
        let mut b = Book::default();
        // Already over the goal and the limit when the tracker starts.
        assert!(due(&c, &day(3 * 3600, 40 * 60, &[]), 0, Live::default(), &mut b).is_empty());
        assert!(
            due(
                &c,
                &day(3 * 3600 + 60, 41 * 60, &[]),
                0,
                Live::default(),
                &mut b
            )
            .is_empty()
        );
    }

    #[test]
    fn heads_up_then_reached_once_each() {
        let c = cfg();
        let mut b = Book::default();
        assert!(due(&c, &day(60, 60, &[]), 0, Live::default(), &mut b).is_empty());
        let warn = due(&c, &day(116 * 60, 26 * 60, &[]), 0, Live::default(), &mut b);
        assert_eq!(keys(&warn), ["goal-warn", "limit-warn"]);
        assert_eq!(warn[1].summary, "4m left for Social");
        assert!(
            due(&c, &day(117 * 60, 27 * 60, &[]), 0, Live::default(), &mut b).is_empty(),
            "once"
        );
        let hit = due(&c, &day(121 * 60, 31 * 60, &[]), 0, Live::default(), &mut b);
        assert_eq!(keys(&hit), ["goal", "limit"]);
        assert_eq!(
            hit[1].family, warn[1].family,
            "replaces the heads-up on screen"
        );
        assert!(due(&c, &day(200 * 60, 50 * 60, &[]), 0, Live::default(), &mut b).is_empty());
        // A new day starts over.
        let mut next = day(60, 60, &[]);
        next.date = "2026-11-25".into();
        assert!(due(&c, &next, 0, Live::default(), &mut b).is_empty());
    }

    #[test]
    fn switched_off_parts_stay_quiet() {
        let mut c = cfg();
        c.notifications.limits = false;
        c.notifications.goal = false;
        c.notifications.warn_mins = 0;
        let mut b = Book::default();
        due(&c, &day(0, 0, &[]), 0, Live::default(), &mut b);
        assert!(
            due(
                &c,
                &day(300 * 60, 300 * 60, &[]),
                0,
                Live::default(),
                &mut b
            )
            .is_empty()
        );
    }

    #[test]
    fn a_break_reminder_after_each_long_unbroken_stretch() {
        let c = cfg();
        let mut b = Book::default();
        due(&c, &day(0, 0, &[]), 0, Live::default(), &mut b);
        // Editor and terminal back and forth for 40 minutes: not yet.
        let s: Vec<(i64, i64)> = (0..8).map(|i| (i * 5 * M, (i + 1) * 5 * M)).collect();
        assert!(due(&c, &day(0, 0, &s), 40 * M, Live::default(), &mut b).is_empty());
        let mut s2 = s.clone();
        s2.push((40 * M + 30_000, 52 * M));
        let n = due(&c, &day(0, 0, &s2), 52 * M, Live::default(), &mut b);
        assert_eq!(keys(&n), ["break"]);
        assert!(
            n[0].body.starts_with("52m on screen without a break"),
            "{}",
            n[0].body
        );
        assert!(
            due(&c, &day(0, 0, &s2), 53 * M, Live::default(), &mut b).is_empty(),
            "once"
        );
        // After a real break the clock starts over.
        let mut s3 = s2.clone();
        s3.push((60 * M, 100 * M));
        assert!(due(&c, &day(0, 0, &s3), 100 * M, Live::default(), &mut b).is_empty());
        s3.push((100 * M, 111 * M));
        assert_eq!(
            keys(&due(&c, &day(0, 0, &s3), 111 * M, Live::default(), &mut b)),
            ["break"]
        );
    }

    #[test]
    fn a_break_reminder_waits_for_calls_videos_and_your_return() {
        let c = cfg();
        let mut b = Book::default();
        due(&c, &day(0, 0, &[]), 0, Live::default(), &mut b);
        let s = [(0, 55 * M)];
        let busy = Live {
            busy: true,
            ..Default::default()
        };
        assert!(due(&c, &day(0, 0, &s), 55 * M, busy, &mut b).is_empty());
        let away = Live {
            away: true,
            ..Default::default()
        };
        assert!(due(&c, &day(0, 0, &s), 55 * M, away, &mut b).is_empty());
        // Long gone: the stretch is over, nothing to remind.
        assert!(due(&c, &day(0, 0, &s), 70 * M, Live::default(), &mut b).is_empty());
        assert_eq!(
            keys(&due(&c, &day(0, 0, &s), 56 * M, Live::default(), &mut b)),
            ["break"]
        );
    }
}
