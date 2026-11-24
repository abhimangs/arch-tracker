//! The tracker actor: owns the live picture of the desktop and turns it into spans.

use super::{Event, SharedRef};
use crate::config::Config;
use crate::desktop::DesktopIndex;
use crate::ipc::Request;
use crate::model::{Kind, Subject};
use crate::sources::browser::{self, ExtMsg, ExtTab, ExtWindow, pbi::PbiTab, snss};
use crate::sources::kwin::{self, Win};
use crate::sources::media::{self, Player};
use crate::sources::terminal::{self, TermInfo, agents, procfs::ProcTable, tools::Projects};
use crate::sources::{idle, session};
use crate::store::Store;
use crate::store::ledger::Ledger;
use crate::util::{self, now_ms};
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Instant;

/// What kind of app a window belongs to decides how it is enriched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKind {
    Browser,
    WebApp,
    Terminal,
    Other,
}

pub fn app_kind(app: &str, cls: &str) -> AppKind {
    let a = app.to_lowercase();
    let c = cls.to_lowercase();
    if c.starts_with("crx_") || (a.starts_with("brave-") && a.ends_with("-default") && a.len() > 40)
    {
        return AppKind::WebApp;
    }
    const BROWSERS: [&str; 22] = [
        "brave",
        "chrom",
        "vivaldi",
        "microsoft-edge",
        "opera",
        "firefox",
        "librewolf",
        "helium",
        "thorium",
        "floorp",
        "waterfox",
        "zen-browser",
        "qutebrowser",
        "falkon",
        "epiphany",
        "torbrowser",
        "tor-browser",
        "mullvad-browser",
        "midori",
        "palemoon",
        "seamonkey",
        "ladybird",
    ];
    // Short names that would match too much as substrings ("zen" in "zenity") must be whole words.
    const WORDS: [&str; 2] = ["zen", "app.zen_browser.zen"];
    let word = |s: &str| {
        WORDS
            .iter()
            .any(|w| s == *w || s.split(['-', '_', '.']).next() == Some(*w))
    };
    if BROWSERS.iter().any(|b| a.contains(b) || c.contains(b)) || word(&a) || word(&c) {
        return AppKind::Browser;
    }
    const TERMS: [&str; 20] = [
        "kitty",
        "ghostty",
        "alacritty",
        "foot",
        "konsole",
        "wezterm",
        "xterm",
        "terminator",
        "tilix",
        "gnome-terminal",
        "yakuake",
        "qterminal",
        "xfce4-terminal",
        "ptyxis",
        "org.gnome.console",
        "lxterminal",
        "mate-terminal",
        "terminology",
        "cool-retro-term",
        "contour",
    ];
    if TERMS.iter().any(|t| a.contains(t) || c.contains(t)) {
        return AppKind::Terminal;
    }
    AppKind::Other
}

/// "Instagram - Brave Origin" -> "Instagram" (also " - Work" profile suffixes).
pub fn strip_browser_suffix(caption: &str) -> String {
    const NAMES: [&str; 16] = [
        " - Brave Origin",
        " - Brave",
        " - Google Chrome",
        " - Chromium",
        " - Helium",
        " - Vivaldi",
        " - Microsoft Edge",
        " - Opera",
        " - Thorium",
        " — Mozilla Firefox",
        " - Mozilla Firefox",
        " — Zen Browser",
        " — Zen",
        " — LibreWolf",
        " — Floorp",
        " — Waterfox",
    ];
    let mut best: Option<usize> = None;
    for n in NAMES {
        if let Some(i) = caption.rfind(n) {
            best = Some(best.map_or(i, |b| b.min(i)));
        }
    }
    match best {
        Some(i) => caption[..i].trim().to_string(),
        None => caption.trim().to_string(),
    }
}

/// One connected browser instance (through our extension).
struct ExtConn {
    pid: u32,
    last: i64,
    /// When the last window snapshot arrived.
    snap_at: i64,
    windows: Vec<ExtWindow>,
}

struct PbiState {
    pid: u32,
    at: i64,
    tabs: Vec<PbiTab>,
}

/// What we know about the tab a browser window shows.
#[derive(Clone, Debug, Default)]
struct TabPick {
    host: String,
    title: String,
    private: bool,
    audible: bool,
    conn: Option<u64>,
    tab_id: Option<i64>,
    /// Other tabs shown in the same window (Chromium split view).
    split: Vec<(String, String)>,
}

/// Meta key holding an active pause: "indefinite" or the end in ms.
const PAUSE_KEY: &str = "paused_until";

/// Reads a persisted pause (dropping one that has already ended).
fn stored_pause(store: &Store, now: i64) -> Option<Option<i64>> {
    match store.meta(PAUSE_KEY)?.as_str() {
        "indefinite" => Some(None),
        v => v.parse::<i64>().ok().filter(|t| *t > now).map(Some),
    }
}

/// The KWin script re-sends every 60 s: a snapshot older than this is stale, and focus and
/// visibility are not recorded from it.
const KWIN_STALE_MS: i64 = 150_000;
const KWIN_RETRY_MIN_MS: i64 = 30_000;
const KWIN_RETRY_MAX_MS: i64 = 600_000;

/// Upper bounds for one Plasma Browser Integration query and one running-apps scan.
const PBI_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1_500);
const RUNNING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// Running apps are rescanned this often (not every tick).
const RUNNING_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// The status JSON is rebuilt at most this often.
const STATUS_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// A terminal window whose title keeps changing is inspected at most this often.
const TERM_MIN_GAP_MS: i64 = 2_000;

/// How long the extension may lag behind a KWin focus or caption change.
const FOCUS_GRACE_MS: i64 = 2_000;

/// The tab a browser window shows, from the windows the extension reports. focused: KWin
/// focuses this window; settled: the extension has reported since the focus or caption last
/// changed (or the grace period is over), so its focused window can be trusted.
fn pick_ext(
    wins: &[(u64, &ExtWindow)],
    focused: bool,
    settled: bool,
    kwin_same: usize,
    caption: &str,
    norm: &dyn Fn(&str) -> String,
) -> Option<TabPick> {
    let fits = |t: &ExtTab| {
        let n = norm(&t.title);
        caption.is_empty() || n == caption || util::titles_match(&n, caption)
    };
    let make = |cid: u64, x: &ExtWindow, t: &ExtTab| TabPick {
        host: t.host.clone(),
        title: norm(&t.title),
        private: x.incognito,
        audible: t.audible && !t.muted,
        conn: Some(cid),
        tab_id: Some(t.id),
        split: match t.split {
            Some(sid) => x
                .tabs
                .iter()
                .filter(|o| o.id != t.id && o.split == Some(sid))
                .map(|o| (o.host.clone(), norm(&o.title)))
                .collect(),
            None => vec![],
        },
    };
    let private = || TabPick {
        private: true,
        ..Default::default()
    };
    let devtools = || {
        caption.starts_with("DevTools").then(|| TabPick {
            host: "devtools".into(),
            title: caption.into(),
            ..Default::default()
        })
    };
    if focused {
        if let Some((cid, x)) = wins.iter().find(|(_, x)| x.focused) {
            if x.incognito {
                return Some(private());
            }
            if let Some(t) = x.active_tab()
                && fits(t)
            {
                return Some(make(*cid, x, t));
            }
        }
        if settled {
            // KWin focuses a window of this browser that the extension does not report as
            // focused with this title: one it cannot see, i.e. incognito. Matching the caption
            // against normal tabs would record a private window as a normal tab.
            return Some(devtools().unwrap_or_else(private));
        }
    }
    // Every KWin window showing this caption must be accounted for by a normal tab; otherwise
    // one of them is a window the extension cannot see (private), and the title proves nothing.
    let candidates: Vec<(u64, &ExtWindow, &ExtTab)> = wins
        .iter()
        .filter(|(_, x)| !x.incognito)
        .filter_map(|(cid, x)| x.active_tab().filter(|t| fits(t)).map(|t| (*cid, *x, t)))
        .collect();
    if candidates.len() >= kwin_same
        && let Some((cid, x, t)) = candidates.first()
    {
        return Some(make(*cid, x, t));
    }
    if let Some(d) = devtools() {
        return Some(d);
    }
    // Focused, and no window the extension can see shows this: incognito.
    focused.then(private)
}

/// An extension connection is alive while it keeps sending (heartbeat every 20 s).
const EXT_ALIVE_MS: i64 = 60_000;

pub struct Tracker {
    store: Store,
    ledger: Ledger,
    cfg: Config,
    title_res: Vec<regex_lite::Regex>,
    shared: SharedRef,
    /// Session bus (None in tests).
    conn: Option<zbus::Connection>,
    desktop: DesktopIndex,
    desktop_loaded: Instant,
    started_ms: i64,
    /// Why config.toml failed to load (defaults or the last good settings are in effect).
    config_error: Option<String>,
    /// Some(None) = paused until resumed; Some(Some(t)) = paused until t.
    paused: Option<Option<i64>>,
    kwin: Option<kwin::Snapshot>,
    /// Indices of the snapshot's visible windows (see visible_wins).
    kwin_vis: Vec<usize>,
    kwin_last: i64,
    /// Next KWin script reload attempt while snapshots are missing, and the delay after it.
    kwin_retry_at: i64,
    kwin_backoff_ms: i64,
    /// KWin's focused window and its caption, and when either last changed.
    focus_key: Option<(String, String)>,
    focus_changed_at: i64,
    /// Reconcile again when the focus grace period ends.
    regrace: bool,
    /// The published status is out of date; when it was last published.
    status_dirty: bool,
    status_at: Option<Instant>,
    focus: Option<Subject>,
    /// When the focused subject, and the focused app, last changed (for the live timer).
    focus_since: i64,
    app_since: i64,
    /// When the current no-input gap started.
    idle_since: Option<i64>,
    /// Next time old history may be pruned (general.keep_days).
    prune_at: i64,
    visible: Vec<Subject>,
    media_visible: Vec<Subject>,
    bg_audio: Vec<Subject>,
    /// No keyboard/mouse input (inhibitors ignored).
    input_idle: bool,
    /// Idle as the compositor sees it (inhibitors respected).
    session_idle: bool,
    idle_version: u32,
    idle_error: Option<String>,
    screensaver_locked: bool,
    logind_locked: bool,
    suspended: bool,
    players: Vec<Player>,
    mic: bool,
    sys_conn: Option<zbus::Connection>,
    locked_hint: session::LockedHint,
    /// Per KWin window: (normalized caption when inspected, when, what runs inside).
    term_cache: HashMap<String, (String, i64, TermInfo)>,
    projects: Projects,
    agent_scanner: agents::Scanner,
    agents: Vec<agents::AgentState>,
    running: Vec<Subject>,
    /// When running apps were last scanned (they change slowly; see RUNNING_EVERY).
    running_at: Option<Instant>,
    ext: HashMap<u64, ExtConn>,
    pbi: Option<PbiState>,
    pbi_caption: String,
    snss: snss::Cache,
    snss_selected: std::collections::HashSet<i64>,
    last_flush: Instant,
    last_wall: i64,
    last_mono: Instant,
}

impl Tracker {
    pub fn new(store: Store, cfg: Config, shared: SharedRef, conn: zbus::Connection) -> Self {
        Self::build(store, cfg, shared, Some(conn))
    }

    fn build(store: Store, cfg: Config, shared: SharedRef, conn: Option<zbus::Connection>) -> Self {
        let now = now_ms();
        let paused = stored_pause(&store, now);
        if paused.is_some() {
            tracing::info!("still paused (until {:?})", paused.flatten());
        }
        Tracker {
            store,
            ledger: Ledger::default(),
            title_res: cfg.title_regexes(),
            cfg,
            shared,
            conn,
            desktop: DesktopIndex::load(),
            desktop_loaded: Instant::now(),
            started_ms: now,
            config_error: None,
            paused,
            kwin: None,
            kwin_vis: vec![],
            // Grace period: the first snapshot arrives right after the script loads.
            kwin_last: now,
            kwin_retry_at: 0,
            kwin_backoff_ms: KWIN_RETRY_MIN_MS,
            focus_key: None,
            focus_changed_at: 0,
            regrace: false,
            status_dirty: false,
            status_at: None,
            focus: None,
            focus_since: now,
            app_since: now,
            idle_since: None,
            prune_at: now + 60_000,
            visible: vec![],
            media_visible: vec![],
            bg_audio: vec![],
            input_idle: false,
            session_idle: false,
            idle_version: 0,
            idle_error: None,
            screensaver_locked: false,
            logind_locked: false,
            suspended: false,
            players: vec![],
            mic: false,
            sys_conn: None,
            locked_hint: Default::default(),
            term_cache: HashMap::new(),
            projects: Projects::default(),
            agent_scanner: agents::Scanner::default(),
            agents: vec![],
            running: vec![],
            running_at: None,
            ext: HashMap::new(),
            pbi: None,
            pbi_caption: String::new(),
            snss: snss::Cache::default(),
            snss_selected: Default::default(),
            last_flush: Instant::now(),
            last_wall: now,
            last_mono: Instant::now(),
        }
    }

    pub fn start(&mut self) {
        self.reconcile(now_ms());
    }

    pub async fn handle(&mut self, ev: Event) {
        let now = now_ms();
        match ev {
            Event::Control(req, reply) => {
                let resp = self.control(req, now);
                let _ = reply.send(resp);
            }
            Event::ConfigChanged(cfg) => {
                tracing::info!("config reloaded");
                self.config_error = None;
                self.set_config(*cfg);
            }
            Event::ConfigError(e) => self.config_error = Some(e),
            Event::Kwin(snap) => {
                let key = snap.focused().map(|w| (w.id.clone(), w.cap.clone()));
                if key != self.focus_key {
                    self.focus_key = key;
                    self.focus_changed_at = now;
                    self.regrace = true;
                }
                self.kwin = Some(*snap);
                self.update_visible();
                self.kwin_last = now;
                self.kwin_retry_at = 0;
                self.kwin_backoff_ms = KWIN_RETRY_MIN_MS;
                self.refresh_terminals(now, false, None).await;
                self.refresh_pbi(now, false).await;
            }
            Event::Idle {
                input,
                idle: is_idle,
                at,
            } => {
                if input {
                    self.input_idle = is_idle;
                    // The notification fires GAP_MS after the last input: start the gap there.
                    let start = if is_idle {
                        at - idle::GAP_MS as i64
                    } else {
                        at
                    };
                    self.ledger.set_flag(Kind::InputIdle, is_idle, start);
                    self.idle_since = is_idle.then_some(start);
                } else {
                    self.session_idle = is_idle;
                }
            }
            Event::IdleReady { version } => {
                self.idle_version = version;
                self.idle_error = None;
            }
            Event::IdleUnavailable(e) => {
                // The notifications are gone: whatever they said last is no longer known.
                self.idle_error = Some(e);
                self.idle_version = 0;
                self.input_idle = false;
                self.session_idle = false;
                self.ledger.set_flag(Kind::InputIdle, false, now);
            }
            Event::Locked(l) => {
                tracing::info!("screen {}", if l { "locked" } else { "unlocked" });
                self.screensaver_locked = l;
                if !l {
                    self.logind_locked = false;
                }
            }
            Event::Sleep { going, done } => {
                tracing::info!("{}", if going { "going to sleep" } else { "woke up" });
                self.suspended = going;
                if !going {
                    // Restart the clock-jump baseline so the sleep gap is not seen as a jump.
                    self.last_wall = now;
                    self.last_mono = Instant::now();
                }
                self.reconcile(now);
                if going {
                    self.flush(now);
                }
                if let Some(d) = done {
                    let _ = d.send(());
                }
                return;
            }
            Event::Media(players) => self.players = players,
            Event::BrowserExt { conn, msg } => {
                let c = self.ext.entry(conn).or_insert(ExtConn {
                    pid: 0,
                    last: now,
                    snap_at: 0,
                    windows: vec![],
                });
                c.last = now;
                match msg {
                    ExtMsg::Hello { browser_pid } => {
                        tracing::info!("browser extension connected (browser pid {browser_pid})");
                        c.pid = browser_pid;
                    }
                    ExtMsg::Snapshot { windows } => {
                        c.windows = windows;
                        c.snap_at = now;
                    }
                }
            }
            Event::BrowserGone(conn) => {
                if self.ext.remove(&conn).is_some() {
                    tracing::info!("browser extension disconnected");
                }
            }
        }
        self.reconcile(now);
    }

    fn locked(&self) -> bool {
        self.screensaver_locked || self.logind_locked
    }

    /// When the tracker wants to be woken without an event (see on_deadline).
    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        let now = tokio::time::Instant::now();
        // Re-decide a focused browser window once the extension's grace period is over.
        let grace = self.regrace.then(|| {
            let left = (self.focus_changed_at + FOCUS_GRACE_MS - now_ms()).clamp(0, FOCUS_GRACE_MS);
            now + std::time::Duration::from_millis(left as u64 + 50)
        });
        // Publish a status change that was held back by the once-a-second limit.
        let status = self.status_dirty.then(|| match self.status_at {
            Some(t) => tokio::time::Instant::from_std(t + STATUS_EVERY),
            None => now,
        });
        match (grace, status) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn on_deadline(&mut self) {
        let now = now_ms();
        let grace_over =
            now >= self.focus_changed_at + FOCUS_GRACE_MS || now < self.focus_changed_at;
        if self.regrace && grace_over {
            self.regrace = false;
            self.reconcile(now);
        }
        if self.status_dirty && self.status_due() {
            self.publish_status(now);
        }
    }

    /// Why config.toml could not be loaded, shown in the status until it loads again.
    pub fn set_config_error(&mut self, e: Option<String>) {
        self.config_error = e;
    }

    pub fn sample_interval_secs(&self) -> u64 {
        self.cfg.general.sample_interval_secs
    }

    fn set_config(&mut self, cfg: Config) {
        if cfg == self.cfg {
            return;
        }
        self.title_res = cfg.title_regexes();
        let vis_changed = cfg.visibility != self.cfg.visibility;
        self.cfg = cfg;
        if vis_changed {
            self.update_visible();
        }
        if let Ok(mut s) = self.shared.lock() {
            s.config = self.cfg.clone();
        }
    }

    /// Periodic work: pause expiry, clock jumps, sampling, flushing (the ledger splits spans at
    /// local midnight itself).
    pub async fn tick(&mut self) {
        let now = now_ms();
        let mono_elapsed = self.last_mono.elapsed().as_millis() as i64;
        let expected = self.last_wall + mono_elapsed;
        if (now - expected).abs() > 5_000 && !self.suspended {
            // Wall clock jumped (a suspend we missed, NTP, manual change): don't let open spans
            // stretch across the jump.
            tracing::info!("clock jump of {} s detected", (now - expected) / 1000);
            self.ledger.close_all(expected.min(now));
        }
        self.last_wall = now;
        self.last_mono = Instant::now();

        if let Some(Some(until)) = self.paused
            && now >= until
        {
            tracing::info!("pause expired");
            self.set_paused(None);
        }
        self.mic = media::mic_active();
        if self.sys_conn.is_none() {
            self.sys_conn = util::system_bus().await.ok();
        }
        if let Some(sys) = &self.sys_conn
            && let Some(hint) = self.locked_hint.get(sys).await
        {
            self.logind_locked = hint;
        }
        if now >= self.prune_at {
            self.prune_at = now + 3_600_000;
            let days = self.cfg.general.keep_days;
            if days > 0 {
                // Saturating: a corrupt value must not wrap the cutoff into the future.
                let cutoff = now.saturating_sub((days as i64).saturating_mul(86_400_000));
                match self.store.prune(cutoff, &self.ledger.subjects_in_use()) {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("deleted {n} intervals older than {days} days"),
                    Err(e) => tracing::warn!("could not delete old history: {e:#}"),
                }
            }
        }
        if self.desktop_loaded.elapsed().as_secs() > 600 {
            self.desktop = DesktopIndex::load();
            self.desktop_loaded = Instant::now();
        }
        // One read of /proc per tick, shared by terminal inspection and the agent scan.
        let procs = (self.paused.is_none() && !self.suspended).then(ProcTable::read);
        self.refresh_terminals(now, true, procs.as_ref()).await;
        self.refresh_pbi(now, true).await;
        self.ext.retain(|_, c| now - c.last < EXT_ALIVE_MS * 5);
        self.agents = match &procs {
            Some(p) => self
                .agent_scanner
                .scan(&self.cfg.agents, &mut self.projects, p),
            None => vec![],
        };
        let scan_due = self.running_at.is_none_or(|t| t.elapsed() >= RUNNING_EVERY);
        let tracking = self.cfg.general.track_running && self.paused.is_none() && !self.suspended;
        if !tracking {
            self.running.clear();
            self.running_at = None;
        } else if let Some(conn) = &self.conn
            && scan_due
            && self.kwin.is_some()
        {
            self.running_at = Some(Instant::now());
            let kwin = self
                .kwin
                .as_ref()
                .filter(|_| now - self.kwin_last <= KWIN_STALE_MS);
            let scan = crate::sources::running::scan(conn, &self.desktop, kwin);
            if let Ok(apps) = tokio::time::timeout(RUNNING_TIMEOUT, scan).await {
                self.running = apps
                    .into_iter()
                    .filter_map(|(k, n)| self.apply_privacy(Subject::app(&k, &n)))
                    .collect();
            }
        }
        // The KWin script re-sends every 60 s; silence means KWin restarted or dropped it.
        // Reload it, retrying with a growing delay until a snapshot arrives again.
        if now - self.kwin_last > KWIN_STALE_MS
            && now >= self.kwin_retry_at
            && let Some(conn) = self.conn.clone()
        {
            self.kwin_retry_at = now + self.kwin_backoff_ms;
            self.kwin_backoff_ms = (self.kwin_backoff_ms * 2).min(KWIN_RETRY_MAX_MS);
            tokio::spawn(async move {
                if let Err(e) = kwin::load_script(&conn).await {
                    tracing::warn!("KWin script reload failed: {e:#}");
                }
            });
        }
        self.reconcile(now);
        if self.last_flush.elapsed().as_secs() >= self.cfg.general.flush_interval_secs {
            self.flush(now);
        }
    }

    pub fn shutdown(&mut self) {
        let now = now_ms();
        self.ledger.close_all(now);
        self.flush(now);
    }

    fn flush(&mut self, now: i64) {
        if let Err(e) = self.store.flush(&mut self.ledger, now) {
            tracing::error!("database write failed: {e:#}");
        }
        self.last_flush = Instant::now();
    }

    /// Changes the pause state and persists it, so "pause until resumed" survives restarts.
    fn set_paused(&mut self, p: Option<Option<i64>>) {
        self.paused = p;
        let v = p.map(|u| u.map_or("indefinite".to_string(), |t| t.to_string()));
        if let Err(e) = self.store.set_meta(PAUSE_KEY, v.as_deref()) {
            tracing::warn!("could not save the pause state: {e:#}");
        }
    }

    fn control(&mut self, req: Request, now: i64) -> serde_json::Value {
        match req {
            Request::Status => self.publish_status(now),
            Request::Pause { until_ms } => {
                self.set_paused(Some(until_ms));
                tracing::info!("paused until {:?}", until_ms);
                json!({"ok": true, "paused_until": until_ms})
            }
            Request::Resume => {
                self.set_paused(None);
                tracing::info!("resumed");
                json!({"ok": true})
            }
            Request::Reload => match crate::config::Config::load() {
                Ok(c) => {
                    self.config_error = None;
                    self.set_config(c);
                    json!({"ok": true})
                }
                Err(e) => {
                    self.config_error = Some(format!("{e:#}"));
                    json!({"error": format!("{e:#}")})
                }
            },
        }
    }

    /// Looks inside focused/visible terminal windows. Cached per window; refreshed when the
    /// normalized caption changes (agents animate spinner glyphs in their titles several times
    /// a second), at most once per TERM_MIN_GAP_MS, and on the periodic tick for windows on
    /// screen. procs: this tick's /proc snapshot, if one was taken.
    async fn refresh_terminals(&mut self, now: i64, periodic: bool, procs: Option<&ProcTable>) {
        if now - self.kwin_last > KWIN_STALE_MS {
            return;
        }
        let Some(snap) = &self.kwin else { return };
        let focused_id = snap.focused().map(|w| w.id.clone());
        let mut targets: Vec<(String, String, i64, bool, String, String)> = vec![];
        let interval = self.cfg.general.sample_interval_secs as i64 * 1000;
        for w in snap.focused().into_iter().chain(self.visible_wins()) {
            let (app, _) = self.desktop.identify(&w.df, &w.cls, w.pid);
            if self.kind_of(&app, &w.cls) != AppKind::Terminal {
                continue;
            }
            let key = self.norm(&w.cap);
            let stale = match self.term_cache.get(&w.id) {
                None => true,
                Some((cap, at, _)) => {
                    let age = now - at;
                    (*cap != key && age >= TERM_MIN_GAP_MS) || (periodic && age >= interval - 500)
                }
            };
            if stale {
                let focused = focused_id.as_deref() == Some(w.id.as_str());
                targets.push((
                    w.id.clone(),
                    w.cls.clone(),
                    w.pid,
                    focused,
                    w.cap.clone(),
                    key,
                ));
            }
        }
        let alive: std::collections::HashSet<String> =
            snap.windows.iter().map(|w| w.id.clone()).collect();
        self.term_cache.retain(|id, _| alive.contains(id));
        for (id, cls, pid, focused, cap, key) in targets {
            let info = terminal::inspect(
                &cls,
                pid,
                focused,
                &cap,
                &self.cfg.agents.tools,
                &mut self.projects,
                procs,
            )
            .await
            .unwrap_or_default();
            self.term_cache.insert(id, (key, now, info));
        }
    }

    /// app_kind, plus the desktop entry's categories for apps the built-in lists do not name.
    fn kind_of(&self, app: &str, cls: &str) -> AppKind {
        match app_kind(app, cls) {
            AppKind::Other => match self
                .desktop
                .kind_hint(app)
                .or_else(|| self.desktop.kind_hint(cls))
            {
                Some((true, _)) => AppKind::Terminal,
                Some((_, true)) => AppKind::Browser,
                _ => AppKind::Other,
            },
            k => k,
        }
    }

    /// Visible (not focused) windows of the current snapshot, computed once per snapshot.
    fn visible_wins(&self) -> Vec<&Win> {
        let Some(k) = &self.kwin else { return vec![] };
        self.kwin_vis
            .iter()
            .filter_map(|i| k.windows.get(*i))
            .collect()
    }

    fn update_visible(&mut self) {
        let v = &self.cfg.visibility;
        self.kwin_vis = self
            .kwin
            .as_ref()
            .map(|k| k.visible_indices(v.min_fraction, v.min_pixels))
            .unwrap_or_default();
    }

    fn norm(&self, t: &str) -> String {
        util::normalize_title(t, &self.title_res, self.cfg.titles.max_len)
    }

    fn ext_conns(&self, pid: i64, now: i64) -> Vec<(u64, &ExtConn)> {
        self.ext
            .iter()
            .filter(|(_, c)| now - c.last < EXT_ALIVE_MS && (c.pid == 0 || c.pid as i64 == pid))
            .map(|(id, c)| (*id, c))
            .collect()
    }

    /// The tab a browser window shows. Exact with the extension; PBI + session file otherwise.
    fn browser_pick(&self, w: &Win, focused: bool, caption: &str, now: i64) -> Option<TabPick> {
        let want_app = w.cls.to_lowercase().starts_with("crx_");
        let conns = self.ext_conns(w.pid, now);
        if !conns.is_empty() {
            let wins: Vec<(u64, &ExtWindow)> = conns
                .iter()
                .flat_map(|(id, c)| c.windows.iter().map(move |x| (*id, x)))
                .filter(|(_, x)| (x.kind == "app") == want_app)
                .collect();
            // Right after KWin's focus or caption changed, the extension's report may lag.
            let settled = now - self.focus_changed_at >= FOCUS_GRACE_MS
                || conns
                    .iter()
                    .any(|(_, c)| c.snap_at >= self.focus_changed_at);
            // KWin windows of this browser (same kind) that show this caption, this one included.
            let kwin_same = self.kwin.as_ref().map_or(0, |k| {
                k.windows
                    .iter()
                    .filter(|o| {
                        o.pid == w.pid && o.cls.to_lowercase().starts_with("crx_") == want_app
                    })
                    .filter(|o| {
                        let n = self.norm(&strip_browser_suffix(&o.cap));
                        n == caption || util::titles_match(&n, caption)
                    })
                    .count()
            });
            return pick_ext(&wins, focused, settled, kwin_same.max(1), caption, &|t| {
                self.norm(t)
            });
        }
        let p = self.pbi.as_ref()?;
        if (p.pid != 0 && p.pid as i64 != w.pid) || now - p.at > 30_000 {
            return None;
        }
        let exact: Vec<&PbiTab> = p
            .tabs
            .iter()
            .filter(|t| self.norm(&t.title) == caption)
            .collect();
        let t = match exact.len() {
            0 => p
                .tabs
                .iter()
                .find(|t| util::titles_match(&self.norm(&t.title), caption))?,
            1 => exact[0],
            _ => exact
                .iter()
                .find(|t| self.snss_selected.contains(&t.id))
                .copied()
                .or_else(|| {
                    exact
                        .iter()
                        .all(|t| t.host == exact[0].host)
                        .then(|| exact[0])
                })?,
        };
        Some(TabPick {
            host: t.host.clone(),
            title: caption.to_string(),
            audible: t.audible,
            tab_id: Some(t.id),
            ..Default::default()
        })
    }

    /// Re-queries Plasma Browser Integration when a browser window on screen has no extension.
    async fn refresh_pbi(&mut self, now: i64, periodic: bool) {
        if now - self.kwin_last > KWIN_STALE_MS {
            return;
        }
        let Some(snap) = &self.kwin else { return };
        let mut need = None;
        for w in snap.focused().into_iter().chain(self.visible_wins()) {
            let (app, _) = self.desktop.identify(&w.df, &w.cls, w.pid);
            if self.kind_of(&app, &w.cls) == AppKind::Browser
                && self.ext_conns(w.pid, now).is_empty()
            {
                need = Some((w.pid, w.cap.clone()));
                break;
            }
        }
        let Some((pid, cap)) = need else { return };
        let age = self.pbi.as_ref().map(|p| now - p.at).unwrap_or(i64::MAX);
        let changed = cap != self.pbi_caption;
        if !(changed || periodic || age > 10_000) || age < 1_000 {
            return;
        }
        self.pbi_caption = cap;
        let Some(conn) = &self.conn else { return };
        // Bounded: a busy plasmashell must not hold up recording.
        let res = tokio::time::timeout(PBI_TIMEOUT, browser::pbi::query(conn))
            .await
            .ok()
            .flatten();
        match res {
            Some((bpid, tabs)) => {
                let mut seen = std::collections::HashSet::new();
                let dupes = tabs.iter().any(|t| !seen.insert(self.norm(&t.title)));
                if dupes
                    && let Some(path) =
                        snss::open_session_file(if bpid != 0 { bpid } else { pid as u32 })
                {
                    self.snss_selected = self
                        .snss
                        .get(&path)
                        .map(|s| s.selected.clone())
                        .unwrap_or_default();
                }
                self.pbi = Some(PbiState {
                    pid: bpid,
                    at: now,
                    tabs,
                });
            }
            None => self.pbi = None,
        }
    }

    /// Subject for a window: app identity + normalized title (+ browser/terminal details),
    /// after privacy rules. Also returns the browser tab it shows, if any.
    fn window_subject_ex(
        &self,
        w: &Win,
        focused: bool,
        now: i64,
    ) -> Option<(Subject, Option<TabPick>)> {
        let (app, app_name) = self.desktop.identify(&w.df, &w.cls, w.pid);
        let kind = self.kind_of(&app, &w.cls);
        let raw = match kind {
            AppKind::Browser => strip_browser_suffix(&w.cap),
            _ => w.cap.clone(),
        };
        let mut s = Subject {
            app,
            app_name,
            title: self.norm(&raw),
            ..Default::default()
        };
        let mut pick = None;
        match kind {
            AppKind::Browser | AppKind::WebApp => {
                let p = self.browser_pick(w, focused, &s.title, now);
                match &p {
                    Some(p) if p.private => {
                        s.private = true;
                        s.title.clear();
                    }
                    Some(p) => {
                        s.host = p.host.clone();
                        if !p.title.is_empty() {
                            s.title = p.title.clone();
                        }
                    }
                    // A tab we cannot attribute might be incognito: keep no title.
                    None if kind == AppKind::Browser => s.title.clear(),
                    None => {}
                }
                pick = p;
            }
            AppKind::Terminal => {
                // Shells and terminals put the running command line (with its arguments) in the
                // window title: terminal data is program + project only, never the caption.
                s.title.clear();
                if let Some((_, _, info)) = self.term_cache.get(&w.id) {
                    s.program = info.program.clone();
                    s.project = info.project.clone();
                }
            }
            AppKind::Other => {}
        }
        self.apply_privacy(s).map(|s| (s, pick))
    }

    /// Whether a live window survives the privacy rules (an excluded window is not listed).
    fn window_shown(&self, id: &str, now: i64) -> bool {
        self.kwin
            .as_ref()
            .and_then(|k| k.windows.iter().find(|w| w.id == id))
            .and_then(|w| self.window_subject_ex(w, false, now))
            .is_some_and(|(s, _)| s.app != "hidden")
    }

    /// The browser app identity for a browser process (for tabs that are not on screen).
    fn browser_app(&self, pid: u32) -> (String, String) {
        self.kwin
            .as_ref()
            .and_then(|k| {
                k.windows
                    .iter()
                    .find(|w| w.pid as u32 == pid && !w.cls.starts_with("crx_"))
            })
            .map(|w| self.desktop.identify(&w.df, &w.cls, w.pid))
            .unwrap_or_else(|| ("browser".into(), "Browser".into()))
    }

    /// Exclude list: "hide" keeps the time as "Hidden" with no details, "drop" records nothing.
    fn apply_privacy(&self, s: Subject) -> Option<Subject> {
        let p = &self.cfg.privacy;
        let app_hit = p.exclude_apps.iter().any(|x| {
            let x = x.to_lowercase();
            x == s.app.to_lowercase() || x == s.app_name.to_lowercase()
        });
        let host = s.host.to_lowercase();
        let host_hit = !host.is_empty()
            && p.exclude_hosts.iter().any(|h| {
                let h = h.trim().trim_start_matches("*.").to_lowercase();
                !h.is_empty() && (host == h || host.ends_with(&format!(".{h}")))
            });
        if !(app_hit || host_hit) {
            return Some(s);
        }
        match p.exclude_mode.as_str() {
            "drop" => None,
            _ => Some(Subject::app("hidden", "Hidden")),
        }
    }

    fn ids(&mut self, subjects: &[Subject]) -> HashSet<i64> {
        let mut out = HashSet::new();
        for s in subjects {
            match self.store.subject_id(s) {
                Ok(id) => {
                    out.insert(id);
                }
                Err(e) => tracing::error!("subject insert failed: {e:#}"),
            }
        }
        out
    }

    /// Subject for media that plays without being on screen.
    fn player_subject(&self, p: &Player, now: i64) -> Option<Subject> {
        let win = self.kwin.as_ref().and_then(|k| {
            k.windows
                .iter()
                .find(|w| w.pid as u32 == p.pid && p.pid != 0)
        });
        let (app, app_name) = match win {
            Some(w) => self.desktop.identify(&w.df, &w.cls, w.pid),
            None if !p.identity.is_empty() => (
                p.bus
                    .trim_start_matches("org.mpris.MediaPlayer2.")
                    .to_string(),
                p.identity.clone(),
            ),
            None => (p.bus.clone(), p.bus.clone()),
        };
        let browser = self.browser_player(p) || self.kind_of(&app, "") == AppKind::Browser;
        // A browser's own MPRIS player also plays media of private windows, and nothing tells
        // which: keep its details only when Plasma Browser Integration (which never sees
        // private windows) knows the title. The PBI bridge itself only reports normal windows.
        let known = !browser || p.is_browser_bridge() || self.pbi_knows(&p.title, now);
        let s = Subject {
            host: if browser && known {
                media::url_host(&p.url)
            } else {
                String::new()
            },
            title: if known {
                self.norm(&p.title)
            } else {
                String::new()
            },
            app,
            app_name,
            ..Default::default()
        };
        self.apply_privacy(s)
    }

    /// A player run by a browser: the PBI bridge or the browser's own MPRIS service
    /// ("org.mpris.MediaPlayer2.brave.instance1234").
    fn browser_player(&self, p: &Player) -> bool {
        if p.is_browser_bridge() {
            return true;
        }
        let name = p.bus.trim_start_matches("org.mpris.MediaPlayer2.");
        if app_kind(name, "") == AppKind::Browser || app_kind(&p.identity, "") == AppKind::Browser {
            return true;
        }
        p.pid != 0
            && self.kwin.as_ref().is_some_and(|k| {
                k.windows.iter().any(|w| {
                    w.pid as u32 == p.pid
                        && matches!(
                            self.kind_of(&self.desktop.identify(&w.df, &w.cls, w.pid).0, &w.cls),
                            AppKind::Browser | AppKind::WebApp
                        )
                })
            })
    }

    /// True if a fresh Plasma Browser Integration tab list has a tab with this title.
    fn pbi_knows(&self, title: &str, now: i64) -> bool {
        let t = self.norm(title);
        self.pbi.as_ref().is_some_and(|p| {
            now - p.at <= 30_000
                && p.tabs
                    .iter()
                    .any(|x| util::titles_match(&self.norm(&x.title), &t))
        })
    }

    /// Splits playing media into "in a window you can see" and "background".
    fn split_media(
        &self,
        shown: &[(&Win, Subject, Option<TabPick>)],
        now: i64,
    ) -> (Vec<Subject>, Vec<Subject>) {
        let mut vis = vec![];
        let mut bg = vec![];
        // Audible tabs the extension reports.
        for (_, s, p) in shown {
            if p.as_ref().map(|p| p.audible).unwrap_or(false) {
                vis.push(s.clone());
            }
        }
        let shown_tabs: Vec<(u64, i64)> = shown
            .iter()
            .filter_map(|(_, _, p)| p.as_ref().and_then(|p| Some((p.conn?, p.tab_id?))))
            .collect();
        for (cid, c) in self.ext.iter().filter(|(_, c)| now - c.last < EXT_ALIVE_MS) {
            let (app, app_name) = self.browser_app(c.pid);
            // Private windows count as time only: their audio is not recorded at all.
            for x in c.windows.iter().filter(|x| !x.incognito) {
                for t in x.tabs.iter().filter(|t| t.audible && !t.muted) {
                    if shown_tabs.contains(&(*cid, t.id)) {
                        continue;
                    }
                    let s = Subject {
                        app: app.clone(),
                        app_name: app_name.clone(),
                        host: t.host.clone(),
                        title: self.norm(&t.title),
                        ..Default::default()
                    };
                    bg.extend(self.apply_privacy(s));
                }
            }
        }
        // MPRIS players (non-browser apps, and browsers without the extension).
        for p in self.players.iter().filter(|p| p.playing) {
            // The extension already reported this browser's audible tabs (shown or not); its
            // own MPRIS player and the PBI bridge would count the same audio a second time.
            if self.browser_player(p) && !self.ext_conns(p.pid as i64, now).is_empty() {
                continue;
            }
            let hit = shown.iter().find(|(w, s, _)| {
                if w.pid as u32 != p.pid || p.pid == 0 {
                    return false;
                }
                if p.is_browser_bridge() {
                    // The bridge reports the playing tab; only a window's active tab counts.
                    let t = self.norm(&p.title);
                    return util::titles_match(&t, &s.title)
                        || (!s.host.is_empty() && media::url_host(&p.url) == s.host);
                }
                true
            });
            match hit {
                Some((_, s, _)) => vis.push(s.clone()),
                None => bg.extend(self.player_subject(p, now)),
            }
        }
        vis.dedup();
        bg.dedup();
        (vis, bg)
    }

    /// Input is idle but the compositor's idle clock is not: an inhibitor (a video, a call) is
    /// holding the screen awake. Only protocol v2 reports the two clocks apart; with v1 the one
    /// notification respects inhibitors, so an input gap would look inhibited.
    fn inhibited(&self) -> bool {
        self.idle_version >= 2 && self.input_idle && !self.session_idle
    }

    /// Recomputes which spans should be open right now.
    fn reconcile(&mut self, now: i64) {
        let paused = self.paused.is_some();
        let locked = self.locked();
        let suspended = self.suspended;
        let blocked = paused || locked || suspended;
        self.ledger.set_flag(Kind::Paused, paused, now);
        self.ledger
            .set_flag(Kind::Locked, locked && !suspended, now);
        self.ledger.set_flag(Kind::Suspended, suspended, now);
        self.ledger
            .set_flag(Kind::Inhibited, self.inhibited() && !suspended, now);
        self.ledger
            .set_flag(Kind::Mic, self.mic && !suspended && !paused, now);

        let mut shown: Vec<(&Win, Subject, Option<TabPick>)> = vec![];
        let mut focus = None;
        // A stale snapshot no longer says what is on screen: record no focus rather than piling
        // time onto the last known window.
        let kwin_fresh = now - self.kwin_last <= KWIN_STALE_MS;
        if let (Some(snap), false, true) = (&self.kwin, blocked, kwin_fresh) {
            if let Some(w) = snap.focused()
                && let Some((s, p)) = self.window_subject_ex(w, true, now)
            {
                focus = Some(s.clone());
                shown.push((w, s, p));
            }
            for w in self.visible_wins() {
                if let Some((s, p)) = self.window_subject_ex(w, false, now) {
                    shown.push((w, s, p));
                }
            }
        }
        let mut visible: Vec<Subject> = shown
            .iter()
            .skip(usize::from(focus.is_some()))
            .map(|(_, s, _)| s.clone())
            .collect();
        // Split view: the other tab(s) of a shown window are visible too.
        for (_, s, p) in &shown {
            for (host, title) in p.iter().flat_map(|p| p.split.iter()) {
                let mut o = s.clone();
                o.host = host.clone();
                o.title = title.clone();
                visible.extend(self.apply_privacy(o));
            }
        }
        visible.retain(|v| Some(v) != focus.as_ref());
        visible.dedup();
        let (media_visible, bg_audio) = if paused || suspended {
            (vec![], vec![])
        } else {
            self.split_media(&shown, now)
        };

        let focus_ids = self.ids(focus.as_slice());
        let visible_ids = self.ids(&visible);
        let media_ids = self.ids(&media_visible);
        let bg_ids = self.ids(&bg_audio);
        self.ledger.reconcile(Kind::Focus, &focus_ids, now);
        self.ledger.reconcile(Kind::Visible, &visible_ids, now);
        self.ledger.reconcile(Kind::MediaVisible, &media_ids, now);
        self.ledger.reconcile(Kind::BgAudio, &bg_ids, now);
        // Nothing works or runs while the machine sleeps, and a pause stops all recording.
        let busy: Vec<Subject> = if paused || suspended {
            vec![]
        } else {
            self.agents
                .iter()
                .filter(|a| a.busy)
                .filter_map(|a| self.apply_privacy(a.subject.clone()))
                .collect()
        };
        let busy_ids = self.ids(&busy);
        self.ledger.reconcile(Kind::AgentBusy, &busy_ids, now);
        let running = if paused || suspended {
            vec![]
        } else {
            self.running.clone()
        };
        let running_ids = self.ids(&running);
        self.ledger.reconcile(Kind::Running, &running_ids, now);
        let focus_changed = self.focus != focus;
        if focus_changed {
            if self.focus.as_ref().map(|f| &f.app) != focus.as_ref().map(|f| &f.app) {
                self.app_since = now;
            }
            self.focus_since = now;
        }
        self.focus = focus;
        self.visible = visible;
        self.media_visible = media_visible;
        self.bg_audio = bg_audio;

        // The status is for people (dashboard, CLI): rebuilt at most once a second; a change
        // in between is published by on_deadline.
        self.status_dirty = true;
        // A focus change is shown at once (the dashboard's live view); the rest at most 1/s.
        if focus_changed || self.status_due() {
            self.publish_status(now);
        }
    }

    fn status_due(&self) -> bool {
        self.status_at.is_none_or(|t| t.elapsed() >= STATUS_EVERY)
    }

    /// Rebuilds the status for the dashboard; returns it.
    fn publish_status(&mut self, now: i64) -> serde_json::Value {
        let status = self.status_json(now);
        if let Ok(mut s) = self.shared.lock() {
            s.status = status.clone();
            s.live.send_modify(|v| *v = v.wrapping_add(1));
        }
        self.status_at = Some(Instant::now());
        self.status_dirty = false;
        status
    }

    fn status_json(&self, now: i64) -> serde_json::Value {
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "now_ms": now,
            "started_ms": self.started_ms,
            "config_error": self.config_error,
            "paused": self.paused.is_some(),
            "paused_until_ms": self.paused.flatten(),
            "focus": self.focus,
            "focus_since_ms": self.focus_since,
            "app_since_ms": self.app_since,
            "idle_since_ms": self.idle_since,
            "visible": self.visible,
            "media_visible": self.media_visible,
            "background_audio": self.bg_audio,
            "idle": {"input_idle": self.input_idle, "session_idle": self.session_idle,
                     "inhibited": self.inhibited()},
            "locked": self.locked(),
            "suspended": self.suspended,
            "mic": self.mic,
            "agents": self.agents.iter()
                .filter(|a| self.apply_privacy(a.subject.clone()).is_some_and(|s| s.app != "hidden"))
                .collect::<Vec<_>>(),
            "running": self.running.iter().map(|s| &s.app_name).collect::<Vec<_>>(),
            "terminals": self.term_cache.iter()
                .filter(|(id, _)| self.window_shown(id, now))
                .map(|(_, (_, _, i))| i)
                .collect::<Vec<_>>(),
            // Private-window titles are dropped by player_subject. An excluded player is listed
            // as "Hidden", without its identity.
            "players": self.players.iter().filter_map(|p| {
                let s = self.player_subject(p, now)?;
                let name = if s.app == "hidden" {
                    "Hidden"
                } else if p.identity.is_empty() {
                    p.bus.as_str()
                } else {
                    p.identity.as_str()
                };
                Some(json!({"name": name,
                    "playing": p.playing, "pid": p.pid, "host": s.host, "title": s.title}))
            }).collect::<Vec<_>>(),
            "open_spans": self.ledger.open_count(),
            "sources": {
                "kwin": {"ok": now - self.kwin_last <= KWIN_STALE_MS, "last_ms": self.kwin_last,
                         "windows": self.kwin.as_ref().map(|k| k.windows.len()).unwrap_or(0)},
                "idle": {"ok": self.idle_version > 0, "protocol_version": self.idle_version, "error": self.idle_error},
                "logind": {"ok": self.sys_conn.is_some()},
                "browser_extension": self.ext.values().map(|c| json!({"browser_pid": c.pid,
                    "alive": now - c.last < EXT_ALIVE_MS, "last_ms": c.last, "windows": c.windows.len(),
                    "tabs": c.windows.iter().map(|w| w.tabs.len()).sum::<usize>()})).collect::<Vec<_>>(),
                "plasma_browser_integration": self.pbi.as_ref().map(|p| json!({"browser_pid": p.pid, "tabs": p.tabs.len(), "at_ms": p.at})),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::kwin::{Screen, Snapshot};

    /// A tracker without D-Bus, on a scratch database.
    fn test_tracker() -> Tracker {
        let dir = std::env::temp_dir().join(format!(
            "at-tracker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&dir.join("t.db")).unwrap();
        Tracker::build(store, Config::default(), Default::default(), None)
    }

    fn one_window(cls: &str, cap: &str) -> Snapshot {
        Snapshot {
            active: Some("w1".into()),
            screens: vec![Screen {
                x: 0,
                y: 0,
                w: 1920,
                h: 1200,
            }],
            windows: vec![Win {
                id: "w1".into(),
                cls: cls.into(),
                cap: cap.into(),
                w: 1920,
                h: 1200,
                normal: true,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn kinds_in_db(t: &Tracker, kind: Kind) -> Vec<(i64, i64)> {
        crate::store::spans_in(&t.store.conn, 0, i64::MAX)
            .unwrap()
            .into_iter()
            .filter(|s| s.kind == kind)
            .map(|s| (s.start, s.end))
            .collect()
    }

    #[test]
    fn sleep_and_pause_end_agent_and_running_spans() {
        let mut t = test_tracker();
        t.agents = vec![agents::AgentState {
            subject: Subject::app("agent:test", "Test agent"),
            pid: 1,
            busy: true,
            cpu_percent: 50.0,
        }];
        t.running = vec![Subject::app("editor", "Editor")];
        t.reconcile(1_000);
        t.suspended = true;
        t.reconcile(10_000);
        t.suspended = false;
        t.reconcile(20_000);
        t.paused = Some(None);
        t.reconcile(30_000);
        t.flush(40_000);
        let want = vec![(1_000, 10_000), (20_000, 30_000)];
        assert_eq!(kinds_in_db(&t, Kind::AgentBusy), want);
        assert_eq!(kinds_in_db(&t, Kind::Running), want);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_errors_reset_idle_state() {
        let mut t = test_tracker();
        let at = now_ms();
        for input in [true, false] {
            t.handle(Event::Idle {
                input,
                idle: true,
                at,
            })
            .await;
        }
        assert!(t.input_idle && t.session_idle);
        t.handle(Event::IdleUnavailable("gone".into())).await;
        assert!(!t.input_idle && !t.session_idle);
        t.flush(now_ms() + 1_000);
        let idle = kinds_in_db(&t, Kind::InputIdle);
        assert_eq!(idle.len(), 1);
        assert!(idle[0].1 <= now_ms(), "the idle span was closed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v1_idle_gaps_are_not_inhibited() {
        let mut t = test_tracker();
        t.idle_version = 1;
        let at = now_ms();
        t.handle(Event::Idle {
            input: true,
            idle: true,
            at,
        })
        .await;
        t.flush(now_ms() + 1_000);
        assert_eq!(kinds_in_db(&t, Kind::InputIdle).len(), 1);
        assert!(kinds_in_db(&t, Kind::Inhibited).is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_kwin_snapshot_records_no_focus() {
        let mut t = test_tracker();
        t.handle(Event::Kwin(Box::new(one_window(
            "org.example.Editor",
            "Notes",
        ))))
        .await;
        assert!(t.focus.is_some());
        let later = t.kwin_last + KWIN_STALE_MS + 1;
        t.reconcile(later);
        assert!(t.focus.is_none());
        assert!(t.visible.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_inspection_ignores_spinners_and_is_rate_limited() {
        let mut t = test_tracker();
        let now = t.kwin_last;
        let at = |t: &Tracker| t.term_cache["w1"].1;
        t.kwin = Some(one_window("foot", "⠂ Working"));
        t.refresh_terminals(now, false, None).await;
        assert_eq!(at(&t), now);
        t.kwin = Some(one_window("foot", "⠐ Working"));
        t.refresh_terminals(now + 3_000, false, None).await;
        assert_eq!(at(&t), now, "only the spinner changed");
        t.kwin = Some(one_window("foot", "Other"));
        t.refresh_terminals(now + 3_500, false, None).await;
        assert_eq!(at(&t), now + 3_500);
        t.kwin = Some(one_window("foot", "Third"));
        t.refresh_terminals(now + 4_000, false, None).await;
        assert_eq!(at(&t), now + 3_500, "at most once per 2 s");
        t.refresh_terminals(now + 5_600, false, None).await;
        assert_eq!(at(&t), now + 5_600, "the pending change is picked up later");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn config_errors_show_in_the_status() {
        let mut t = test_tracker();
        t.set_config_error(Some("parsing config.toml: bad".into()));
        assert_eq!(t.status_json(0)["config_error"], "parsing config.toml: bad");
        t.handle(Event::ConfigChanged(Box::default())).await;
        assert!(t.status_json(0)["config_error"].is_null());
        t.handle(Event::ConfigError("again".into())).await;
        assert_eq!(t.status_json(0)["config_error"], "again");
    }

    #[test]
    fn status_is_rebuilt_at_most_once_a_second() {
        let mut t = test_tracker();
        let published = |t: &Tracker| t.shared.lock().unwrap().status["now_ms"].as_i64();
        t.reconcile(1_000);
        assert_eq!(published(&t), Some(1_000));
        t.reconcile(2_000);
        assert_eq!(published(&t), Some(1_000), "held back");
        assert!(t.deadline().is_some());
        t.status_at = Instant::now().checked_sub(STATUS_EVERY * 2);
        t.on_deadline();
        assert_ne!(published(&t), Some(1_000));
        assert!(t.deadline().is_none());
        // A control request always gets (and publishes) a fresh status.
        let v = t.control(Request::Status, 3_000);
        assert_eq!(v["now_ms"], 3_000);
        assert_eq!(published(&t), Some(3_000));
    }

    #[test]
    fn excluded_players_are_listed_as_hidden() {
        let mut t = test_tracker();
        t.cfg.privacy.exclude_apps = vec!["spotify".into()];
        t.players = vec![Player {
            bus: "org.mpris.MediaPlayer2.spotify".into(),
            identity: "Spotify".into(),
            pid: 0,
            playing: true,
            url: String::new(),
            title: "Song".into(),
        }];
        let status = t.status_json(now_ms()).to_string();
        assert!(!status.contains("Spotify"), "{status}");
        assert_eq!(t.status_json(now_ms())["players"][0]["name"], "Hidden");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_titles_are_never_stored() {
        let mut t = test_tracker();
        let cap = "sudo sed -i s/old/new/ /etc/example.conf";
        t.handle(Event::Kwin(Box::new(one_window("foot", cap))))
            .await;
        let focus = t.focus.clone().expect("terminal focused");
        assert_eq!(focus.app, "foot");
        assert!(focus.title.is_empty());
        let status = t.status_json(now_ms()).to_string();
        assert!(!status.contains("sed -i"), "{status}");
        t.shutdown();
        let n: i64 = t
            .store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM subjects WHERE title LIKE '%sed%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn midnight_is_in_the_future_and_within_a_day() {
        let now = now_ms();
        let m = util::next_local_midnight(now);
        assert!(m > now);
        assert!(m - now <= 25 * 3_600_000);
    }

    #[test]
    fn pause_survives_restart() {
        let dir =
            std::env::temp_dir().join(format!("at-pause-{}-{}", std::process::id(), now_ms()));
        let st = Store::open(&dir.join("t.db")).unwrap();
        assert_eq!(stored_pause(&st, 1_000), None);
        st.set_meta(PAUSE_KEY, Some("indefinite")).unwrap();
        assert_eq!(stored_pause(&st, 1_000), Some(None));
        st.set_meta(PAUSE_KEY, Some("5000")).unwrap();
        assert_eq!(stored_pause(&st, 1_000), Some(Some(5_000)));
        assert_eq!(stored_pause(&st, 6_000), None, "an ended pause is dropped");
        st.set_meta(PAUSE_KEY, None).unwrap();
        assert_eq!(stored_pause(&st, 1_000), None);
    }

    fn ext_win(id: i64, focused: bool, incognito: bool, host: &str, title: &str) -> ExtWindow {
        ExtWindow {
            id,
            focused,
            incognito,
            kind: "normal".into(),
            tabs: vec![ExtTab {
                id: id * 10,
                active: true,
                host: host.into(),
                title: title.into(),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn unseen_focused_window_is_private() {
        let norm = |t: &str| t.to_string();
        let normal = ext_win(1, false, false, "docs.example.org", "Getting started");
        let wins = vec![(1u64, &normal)];
        // An incognito window (invisible to the extension) shows the same title as a normal
        // window's active tab, and the extension reports no focused window.
        let p = pick_ext(&wins, true, true, 1, "Getting started", &norm).unwrap();
        assert!(p.private && p.host.is_empty() && p.title.is_empty());
        // While the extension may still lag, the caption match is used.
        let p = pick_ext(&wins, true, false, 1, "Getting started", &norm).unwrap();
        assert_eq!(p.host, "docs.example.org");
        // An unfocused visible window is matched by caption.
        let p = pick_ext(&wins, false, true, 1, "Getting started", &norm).unwrap();
        assert_eq!(p.host, "docs.example.org");
        // Nothing matches: private if focused, unknown otherwise.
        assert!(
            pick_ext(&wins, true, false, 1, "Other", &norm)
                .unwrap()
                .private
        );
        assert!(pick_ext(&wins, false, true, 1, "Other", &norm).is_none());
        let dt = pick_ext(&wins, true, true, 1, "DevTools - example", &norm).unwrap();
        assert_eq!(dt.host, "devtools");
    }

    #[test]
    fn focused_window_reported_by_the_extension() {
        let norm = |t: &str| t.to_string();
        let a = ext_win(1, true, false, "a.example.org", "Alpha page");
        let b = ext_win(2, false, false, "b.example.org", "Beta page");
        let wins = vec![(1u64, &a), (1u64, &b)];
        let p = pick_ext(&wins, true, true, 1, "Alpha page", &norm).unwrap();
        assert_eq!((p.host.as_str(), p.private), ("a.example.org", false));
        // The extension says a different window is focused than the one KWin shows.
        assert!(
            pick_ext(&wins, true, true, 1, "Beta page", &norm)
                .unwrap()
                .private
        );
        let a = ext_win(1, false, false, "a.example.org", "Alpha page");
        let inc = ext_win(3, true, true, "", "");
        let wins = vec![(1u64, &a), (1u64, &inc)];
        assert!(
            pick_ext(&wins, true, false, 1, "Alpha page", &norm)
                .unwrap()
                .private
        );
    }

    #[test]
    fn unfocused_window_with_a_private_twin_is_not_attributed() {
        let norm = |t: &str| t.to_string();
        let a = ext_win(1, false, false, "a.example.org", "Alpha page");
        let wins = vec![(1u64, &a)];
        // Two KWin windows show "Alpha page", but the extension sees only one of them: the
        // other is private, so the visible one cannot be matched by its title.
        assert!(pick_ext(&wins, false, true, 2, "Alpha page", &norm).is_none());
        let p = pick_ext(&wins, false, true, 1, "Alpha page", &norm).unwrap();
        assert_eq!((p.host.as_str(), p.private), ("a.example.org", false));
    }

    #[test]
    fn browser_suffix() {
        assert_eq!(
            strip_browser_suffix("Instagram - Brave Origin"),
            "Instagram"
        );
        assert_eq!(strip_browser_suffix("A - B - Brave Origin - Work"), "A - B");
        assert_eq!(strip_browser_suffix("plain"), "plain");
        assert_eq!(strip_browser_suffix("Docs — Zen Browser"), "Docs");
        assert_eq!(strip_browser_suffix("Docs - Helium"), "Docs");
    }

    #[test]
    fn kinds() {
        assert_eq!(
            app_kind("brave-origin-beta", "brave-origin-beta"),
            AppKind::Browser
        );
        assert_eq!(
            app_kind(
                "brave-kjgfgldnnfoeklkmfkjfagphfepbbdan-Default",
                "crx_kjgfgldnnfoeklkmfkjfagphfepbbdan"
            ),
            AppKind::WebApp
        );
        assert_eq!(app_kind("kitty", "kitty"), AppKind::Terminal);
        assert_eq!(app_kind("helium-browser", "helium"), AppKind::Browser);
        assert_eq!(app_kind("zen", "zen"), AppKind::Browser);
        assert_eq!(app_kind("app.zen_browser.zen", ""), AppKind::Browser);
        assert_eq!(app_kind("zenity", "zenity"), AppKind::Other);
        assert_eq!(app_kind("yakuake", "yakuake"), AppKind::Terminal);
        assert_eq!(app_kind("org.gnome.Epiphany", ""), AppKind::Browser);
        assert_eq!(
            app_kind("org.qutebrowser.qutebrowser", ""),
            AppKind::Browser
        );
        assert_eq!(app_kind("net.mullvad.vpn", ""), AppKind::Other);
        assert_eq!(
            app_kind("com.mitchellh.ghostty", "com.mitchellh.ghostty"),
            AppKind::Terminal
        );
        assert_eq!(
            app_kind("com.notion.app.desktop.notion", "notion"),
            AppKind::Other
        );
    }
}
