//! The tracker actor: owns the live picture of the desktop and turns it into spans.

use super::{Event, SharedRef};
use crate::config::Config;
use crate::desktop::DesktopIndex;
use crate::ipc::Request;
use crate::model::{Kind, Subject};
use crate::sources::kwin::{self, Win};
use crate::sources::media::{self, Player};
use crate::sources::{idle, session};
use crate::store::Store;
use crate::store::ledger::Ledger;
use crate::util::{self, now_ms};
use serde_json::json;
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
        }
        self.reconcile(now);
    }

    fn locked(&self) -> bool {
        self.screensaver_locked || self.logind_locked
    }

    fn set_config(&mut self, cfg: Config) {
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

    /// Subject for a window: app identity + normalized title (+ enrichment), after privacy rules.
    fn window_subject(&self, w: &Win) -> Option<Subject> {
        let (app, app_name) = self.desktop.identify(&w.df, &w.cls, w.pid);
        let kind = app_kind(&app, &w.cls);
        let raw = match kind {
            AppKind::Browser => strip_browser_suffix(&w.cap),
            _ => w.cap.clone(),
        };
        let s = Subject {
            app,
            app_name,
            title: util::normalize_title(&raw, &self.title_res, self.cfg.titles.max_len),
            ..Default::default()
        };
        self.apply_privacy(s)
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
    fn split_media(&self, shown: &[(&Win, Subject)]) -> (Vec<Subject>, Vec<Subject>) {
        let mut vis = vec![];
        let mut bg = vec![];
        for p in self.players.iter().filter(|p| p.playing) {
            let hit = shown.iter().find(|(w, s)| {
                if w.pid as u32 != p.pid || p.pid == 0 {
                    return false;
                }
                if p.is_browser_bridge() {
                    // The bridge reports the tab title; only the window's active tab counts.
                    let t =
                        util::normalize_title(&p.title, &self.title_res, self.cfg.titles.max_len);
                    return util::titles_match(&t, &s.title)
                        || (!s.host.is_empty() && media::url_host(&p.url) == s.host);
                }
                true
            });
            match hit {
                Some((_, s)) => vis.push(s.clone()),
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

        let mut shown: Vec<(&Win, Subject)> = vec![];
        let mut focus = None;
        if let (Some(snap), false) = (&self.kwin, blocked) {
            if let Some(w) = snap.focused()
                && let Some(s) = self.window_subject(w)
            {
                focus = Some(s.clone());
                shown.push((w, s));
            }
            for w in snap.visible(
                self.cfg.visibility.min_fraction,
                self.cfg.visibility.min_pixels,
            ) {
                if let Some(s) = self.window_subject(w) {
                    shown.push((w, s));
                }
            }
        }
        let mut visible: Vec<Subject> = shown
            .iter()
            .skip(usize::from(focus.is_some()))
            .map(|(_, s)| s.clone())
            .collect();
        visible.retain(|v| Some(v) != focus.as_ref());
        visible.dedup();
        let (media_visible, bg_audio) = if paused || suspended {
            (vec![], vec![])
        } else {
            self.split_media(&shown)
        };

        let focus_ids = self.ids(focus.as_slice());
        let visible_ids = self.ids(&visible);
        let media_ids = self.ids(&media_visible);
        let bg_ids = self.ids(&bg_audio);
        self.ledger.reconcile(Kind::Focus, &focus_ids, now);
        self.ledger.reconcile(Kind::Visible, &visible_ids, now);
        self.ledger.reconcile(Kind::MediaVisible, &media_ids, now);
        self.ledger.reconcile(Kind::BgAudio, &bg_ids, now);
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
            "players": self.players.iter().map(|p| json!({"name": if p.identity.is_empty() { &p.bus } else { &p.identity },
                "playing": p.playing, "pid": p.pid, "host": media::url_host(&p.url), "title": p.title})).collect::<Vec<_>>(),
            "open_spans": self.ledger.open_count(),
            "sources": {
                "kwin": {"ok": now - self.kwin_last < 150_000, "last_ms": self.kwin_last,
                         "windows": self.kwin.as_ref().map(|k| k.windows.len()).unwrap_or(0)},
                "idle": {"ok": self.idle_version > 0, "protocol_version": self.idle_version, "error": self.idle_error},
                "logind": {"ok": self.sys_conn.is_some()},
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
