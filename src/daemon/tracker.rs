//! The tracker actor: owns the live picture of the desktop and turns it into spans.

use super::{Event, SharedRef};
use crate::config::Config;
use crate::desktop::DesktopIndex;
use crate::ipc::Request;
use crate::model::{Kind, Subject};
use crate::sources::browser::{self, ExtMsg, ExtTab, ExtWindow, pbi::PbiTab, snss};
use crate::sources::kwin::{self, Win};
use crate::sources::media::{self, Player};
use crate::sources::terminal::{self, TermInfo, agents, tools::Projects};
use crate::sources::{idle, session};
use crate::store::Store;
use crate::store::ledger::Ledger;
use crate::util::{self, now_ms};
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Instant;

pub use crate::util::next_local_midnight;

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
    const BROWSERS: [&str; 7] = [
        "brave",
        "chrom",
        "vivaldi",
        "microsoft-edge",
        "opera",
        "firefox",
        "librewolf",
    ];
    if BROWSERS.iter().any(|b| a.contains(b) || c.contains(b)) {
        return AppKind::Browser;
    }
    const TERMS: [&str; 10] = [
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
    ];
    if TERMS.iter().any(|t| a.contains(t) || c.contains(t)) {
        return AppKind::Terminal;
    }
    AppKind::Other
}

/// "Instagram - Brave Origin" -> "Instagram" (also " - Work" profile suffixes).
pub fn strip_browser_suffix(caption: &str) -> String {
    const NAMES: [&str; 9] = [
        " - Brave Origin",
        " - Brave",
        " - Google Chrome",
        " - Chromium",
        " - Vivaldi",
        " - Microsoft Edge",
        " - Opera",
        " — Mozilla Firefox",
        " - Mozilla Firefox",
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

/// An extension connection is alive while it keeps sending (heartbeat every 20 s).
const EXT_ALIVE_MS: i64 = 60_000;

pub struct Tracker {
    store: Store,
    ledger: Ledger,
    cfg: Config,
    title_res: Vec<regex_lite::Regex>,
    shared: SharedRef,
    conn: zbus::Connection,
    desktop: DesktopIndex,
    desktop_loaded: Instant,
    started_ms: i64,
    /// Some(None) = paused until resumed; Some(Some(t)) = paused until t.
    paused: Option<Option<i64>>,
    kwin: Option<kwin::Snapshot>,
    kwin_last: i64,
    kwin_reloading: bool,
    focus: Option<Subject>,
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
    /// Per KWin window: (caption when inspected, when, what runs inside).
    term_cache: HashMap<String, (String, i64, TermInfo)>,
    projects: Projects,
    agent_scanner: agents::Scanner,
    agents: Vec<agents::AgentState>,
    running: Vec<Subject>,
    ext: HashMap<u64, ExtConn>,
    pbi: Option<PbiState>,
    pbi_caption: String,
    snss: snss::Cache,
    snss_selected: std::collections::HashSet<i64>,
    last_flush: Instant,
    last_wall: i64,
    last_mono: Instant,
    next_midnight: i64,
}

impl Tracker {
    pub fn new(store: Store, cfg: Config, shared: SharedRef, conn: zbus::Connection) -> Self {
        let now = now_ms();
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
            paused: None,
            kwin: None,
            // Grace period: the first snapshot arrives right after the script loads.
            kwin_last: now,
            kwin_reloading: false,
            focus: None,
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
            term_cache: HashMap::new(),
            projects: Projects::default(),
            agent_scanner: agents::Scanner::default(),
            agents: vec![],
            running: vec![],
            ext: HashMap::new(),
            pbi: None,
            pbi_caption: String::new(),
            snss: snss::Cache::default(),
            snss_selected: Default::default(),
            last_flush: Instant::now(),
            last_wall: now,
            last_mono: Instant::now(),
            next_midnight: next_local_midnight(now),
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
                self.set_config(*cfg);
            }
            Event::Kwin(snap) => {
                self.kwin = Some(*snap);
                self.kwin_last = now;
                self.kwin_reloading = false;
                self.refresh_terminals(now, false).await;
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
                } else {
                    self.session_idle = is_idle;
                }
            }
            Event::IdleReady { version } => {
                self.idle_version = version;
                self.idle_error = None;
            }
            Event::IdleUnavailable(e) => self.idle_error = Some(e),
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
                    windows: vec![],
                });
                c.last = now;
                match msg {
                    ExtMsg::Hello { browser_pid } => {
                        tracing::info!("browser extension connected (browser pid {browser_pid})");
                        c.pid = browser_pid;
                    }
                    ExtMsg::Snapshot { windows } => c.windows = windows,
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

    pub fn sample_interval_secs(&self) -> u64 {
        self.cfg.general.sample_interval_secs
    }

    fn set_config(&mut self, cfg: Config) {
        if cfg == self.cfg {
            return;
        }
        self.title_res = cfg.title_regexes();
        self.cfg = cfg;
        if let Ok(mut s) = self.shared.lock() {
            s.config = self.cfg.clone();
        }
    }

    /// Periodic work: pause expiry, clock jumps, midnight split, sampling, flushing.
    pub async fn tick(&mut self) {
        let now = now_ms();
        let mono_elapsed = self.last_mono.elapsed().as_millis() as i64;
        let expected = self.last_wall + mono_elapsed;
        if (now - expected).abs() > 5_000 && !self.suspended {
            // Wall clock jumped (a suspend we missed, NTP, manual change): don't let open spans
            // stretch across the jump.
            tracing::info!("clock jump of {} s detected", (now - expected) / 1000);
            self.ledger.close_all(expected.min(now));
            self.next_midnight = next_local_midnight(now);
        }
        self.last_wall = now;
        self.last_mono = Instant::now();

        if let Some(Some(until)) = self.paused
            && now >= until
        {
            tracing::info!("pause expired");
            self.paused = None;
        }
        if now >= self.next_midnight {
            self.ledger.split_all(self.next_midnight);
            self.next_midnight = next_local_midnight(now);
        }
        self.mic = media::mic_active();
        if self.sys_conn.is_none() {
            self.sys_conn = zbus::Connection::system().await.ok();
        }
        if let Some(sys) = &self.sys_conn
            && let Some(hint) = session::locked_hint(sys).await
        {
            self.logind_locked = hint;
        }
        if self.desktop_loaded.elapsed().as_secs() > 600 {
            self.desktop = DesktopIndex::load();
            self.desktop_loaded = Instant::now();
        }
        self.refresh_terminals(now, true).await;
        self.refresh_pbi(now, true).await;
        self.ext.retain(|_, c| now - c.last < EXT_ALIVE_MS * 5);
        self.agents = if self.paused.is_some() || self.suspended {
            vec![]
        } else {
            self.agent_scanner
                .scan(&self.cfg.agents, &mut self.projects)
        };
        self.running = if self.cfg.general.track_running
            && self.paused.is_none()
            && !self.suspended
            && self.kwin.is_some()
        {
            crate::sources::running::scan(&self.conn, &self.desktop, self.kwin.as_ref())
                .await
                .into_iter()
                .filter_map(|(k, n)| self.apply_privacy(Subject::app(&k, &n)))
                .collect()
        } else {
            vec![]
        };
        // The KWin script re-sends every 60 s; silence means KWin restarted or dropped it.
        if now - self.kwin_last > 150_000 && !self.kwin_reloading {
            self.kwin_reloading = true;
            let conn = self.conn.clone();
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
        if let Err(e) = self.ledger.flush(&mut self.store.conn, now) {
            tracing::error!("database write failed: {e:#}");
        }
        self.last_flush = Instant::now();
    }

    fn control(&mut self, req: Request, now: i64) -> serde_json::Value {
        match req {
            Request::Status => self.status_json(now),
            Request::Pause { until_ms } => {
                self.paused = Some(until_ms);
                tracing::info!("paused until {:?}", until_ms);
                json!({"ok": true, "paused_until": until_ms})
            }
            Request::Resume => {
                self.paused = None;
                tracing::info!("resumed");
                json!({"ok": true})
            }
            Request::Reload => match crate::config::Config::load() {
                Ok(c) => {
                    self.set_config(c);
                    json!({"ok": true})
                }
                Err(e) => json!({"error": format!("{e:#}")}),
            },
        }
    }

    /// Looks inside focused/visible terminal windows. Cached per window; refreshed when the
    /// caption changes, and on the periodic tick for windows on screen.
    async fn refresh_terminals(&mut self, now: i64, periodic: bool) {
        let Some(snap) = &self.kwin else { return };
        let focused_id = snap.focused().map(|w| w.id.clone());
        let mut targets: Vec<(String, String, i64, bool, String)> = vec![];
        let mut on_screen: Vec<&Win> = snap.focused().into_iter().collect();
        on_screen.extend(snap.visible(
            self.cfg.visibility.min_fraction,
            self.cfg.visibility.min_pixels,
        ));
        for w in on_screen {
            let (app, _) = self.desktop.identify(&w.df, &w.cls, w.pid);
            if app_kind(&app, &w.cls) != AppKind::Terminal {
                continue;
            }
            let stale = match self.term_cache.get(&w.id) {
                None => true,
                Some((cap, at, _)) => {
                    *cap != w.cap
                        || (periodic
                            && now - at
                                >= self.cfg.general.sample_interval_secs as i64 * 1000 - 500)
                }
            };
            if stale {
                let focused = focused_id.as_deref() == Some(w.id.as_str());
                targets.push((w.id.clone(), w.cls.clone(), w.pid, focused, w.cap.clone()));
            }
        }
        let alive: std::collections::HashSet<String> =
            snap.windows.iter().map(|w| w.id.clone()).collect();
        self.term_cache.retain(|id, _| alive.contains(id));
        for (id, cls, pid, focused, cap) in targets {
            let info = terminal::inspect(
                &cls,
                pid,
                focused,
                &cap,
                &self.cfg.agents.tools,
                &mut self.projects,
            )
            .await
            .unwrap_or_default();
            self.term_cache.insert(id, (cap, now, info));
        }
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
            let fits = |t: &ExtTab| {
                let n = self.norm(&t.title);
                caption.is_empty() || n == caption || util::titles_match(&n, caption)
            };
            let make = |cid: u64, x: &ExtWindow, t: &ExtTab| TabPick {
                host: t.host.clone(),
                title: self.norm(&t.title),
                private: x.incognito,
                audible: t.audible && !t.muted,
                conn: Some(cid),
                tab_id: Some(t.id),
                split: match t.split {
                    Some(sid) => x
                        .tabs
                        .iter()
                        .filter(|o| o.id != t.id && o.split == Some(sid))
                        .map(|o| (o.host.clone(), self.norm(&o.title)))
                        .collect(),
                    None => vec![],
                },
            };
            if focused && let Some((cid, x)) = wins.iter().find(|(_, x)| x.focused) {
                if x.incognito {
                    return Some(TabPick {
                        private: true,
                        ..Default::default()
                    });
                }
                if let Some(t) = x.active_tab()
                    && fits(t)
                {
                    return Some(make(*cid, x, t));
                }
            }
            if let Some((cid, x, t)) = wins
                .iter()
                .filter(|(_, x)| !x.incognito)
                .find_map(|(cid, x)| x.active_tab().filter(|t| fits(t)).map(|t| (*cid, *x, t)))
            {
                return Some(make(cid, x, t));
            }
            if caption.starts_with("DevTools") {
                return Some(TabPick {
                    host: "devtools".into(),
                    title: caption.into(),
                    ..Default::default()
                });
            }
            if focused && !wins.iter().any(|(_, x)| x.focused) {
                // Browser has focus, but no window the extension can see: incognito.
                return Some(TabPick {
                    private: true,
                    ..Default::default()
                });
            }
            return None;
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
        let Some(snap) = &self.kwin else { return };
        let mut need = None;
        let mut on_screen: Vec<&Win> = snap.focused().into_iter().collect();
        on_screen.extend(snap.visible(
            self.cfg.visibility.min_fraction,
            self.cfg.visibility.min_pixels,
        ));
        for w in on_screen {
            let (app, _) = self.desktop.identify(&w.df, &w.cls, w.pid);
            if app_kind(&app, &w.cls) == AppKind::Browser && self.ext_conns(w.pid, now).is_empty() {
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
        match browser::pbi::query(&self.conn).await {
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

    fn window_subject(&self, w: &Win, focused: bool, now: i64) -> Option<Subject> {
        self.window_subject_ex(w, focused, now).map(|(s, _)| s)
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
        let kind = app_kind(&app, &w.cls);
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
                if let Some((_, _, info)) = self.term_cache.get(&w.id) {
                    s.program = info.program.clone();
                    s.project = info.project.clone();
                }
            }
            AppKind::Other => {}
        }
        self.apply_privacy(s).map(|s| (s, pick))
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
    fn player_subject(&self, p: &Player) -> Option<Subject> {
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
        let browser = p.is_browser_bridge() || app_kind(&app, "") == AppKind::Browser;
        let s = Subject {
            host: if browser {
                media::url_host(&p.url)
            } else {
                String::new()
            },
            title: util::normalize_title(&p.title, &self.title_res, self.cfg.titles.max_len),
            app,
            app_name,
            ..Default::default()
        };
        self.apply_privacy(s)
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
            for x in &c.windows {
                for t in x.tabs.iter().filter(|t| t.audible && !t.muted) {
                    if shown_tabs.contains(&(*cid, t.id)) {
                        continue;
                    }
                    let s = Subject {
                        app: app.clone(),
                        app_name: app_name.clone(),
                        host: if x.incognito {
                            String::new()
                        } else {
                            t.host.clone()
                        },
                        title: if x.incognito {
                            String::new()
                        } else {
                            self.norm(&t.title)
                        },
                        private: x.incognito,
                        ..Default::default()
                    };
                    bg.extend(self.apply_privacy(s));
                }
            }
        }
        // MPRIS players (non-browser apps, and browsers without the extension).
        for p in self.players.iter().filter(|p| p.playing) {
            let ext_covers = p.is_browser_bridge() && !self.ext_conns(p.pid as i64, now).is_empty();
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
                None if ext_covers => {}
                None => bg.extend(self.player_subject(p)),
            }
        }
        vis.dedup();
        bg.dedup();
        (vis, bg)
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
        self.ledger.set_flag(
            Kind::Inhibited,
            self.input_idle && !self.session_idle && !suspended,
            now,
        );
        self.ledger
            .set_flag(Kind::Mic, self.mic && !suspended && !paused, now);

        let mut shown: Vec<(&Win, Subject, Option<TabPick>)> = vec![];
        let mut focus = None;
        if let (Some(snap), false) = (&self.kwin, blocked) {
            if let Some(w) = snap.focused()
                && let Some((s, p)) = self.window_subject_ex(w, true, now)
            {
                focus = Some(s.clone());
                shown.push((w, s, p));
            }
            for w in snap.visible(
                self.cfg.visibility.min_fraction,
                self.cfg.visibility.min_pixels,
            ) {
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
        let busy: Vec<Subject> = self
            .agents
            .iter()
            .filter(|a| a.busy)
            .filter_map(|a| self.apply_privacy(a.subject.clone()))
            .collect();
        let busy_ids = self.ids(&busy);
        self.ledger.reconcile(Kind::AgentBusy, &busy_ids, now);
        let running = self.running.clone();
        let running_ids = self.ids(&running);
        self.ledger.reconcile(Kind::Running, &running_ids, now);
        self.focus = focus;
        self.visible = visible;
        self.media_visible = media_visible;
        self.bg_audio = bg_audio;

        let status = self.status_json(now);
        if let Ok(mut s) = self.shared.lock() {
            s.status = status;
        }
    }

    fn status_json(&self, now: i64) -> serde_json::Value {
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "now_ms": now,
            "started_ms": self.started_ms,
            "paused": self.paused.is_some(),
            "paused_until_ms": self.paused.flatten(),
            "focus": self.focus,
            "visible": self.visible,
            "media_visible": self.media_visible,
            "background_audio": self.bg_audio,
            "idle": {"input_idle": self.input_idle, "session_idle": self.session_idle,
                     "inhibited": self.input_idle && !self.session_idle},
            "locked": self.locked(),
            "suspended": self.suspended,
            "mic": self.mic,
            "agents": self.agents,
            "running": self.running.iter().map(|s| &s.app_name).collect::<Vec<_>>(),
            "terminals": self.term_cache.values().map(|(_, _, i)| i).collect::<Vec<_>>(),
            "players": self.players.iter().map(|p| json!({"name": if p.identity.is_empty() { &p.bus } else { &p.identity },
                "playing": p.playing, "pid": p.pid, "host": media::url_host(&p.url), "title": p.title})).collect::<Vec<_>>(),
            "open_spans": self.ledger.open_count(),
            "sources": {
                "kwin": {"ok": now - self.kwin_last < 150_000, "last_ms": self.kwin_last,
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

    #[test]
    fn midnight_is_in_the_future_and_within_a_day() {
        let now = now_ms();
        let m = next_local_midnight(now);
        assert!(m > now);
        assert!(m - now <= 25 * 3_600_000);
    }

    #[test]
    fn browser_suffix() {
        assert_eq!(
            strip_browser_suffix("Instagram - Brave Origin"),
            "Instagram"
        );
        assert_eq!(
            strip_browser_suffix("A - B - Brave Origin - Work"),
            "A - B"
        );
        assert_eq!(strip_browser_suffix("plain"), "plain");
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
