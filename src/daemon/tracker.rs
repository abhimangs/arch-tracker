//! The tracker actor: owns the live picture of the desktop and turns it into spans.

use super::{Event, SharedRef};
use crate::config::Config;
use crate::desktop::DesktopIndex;
use crate::ipc::Request;
use crate::model::{Kind, Subject};
use crate::sources::kwin::{self, Win};
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
            kwin_last: 0,
            kwin_reloading: false,
            focus: None,
            visible: vec![],
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
        }
        self.reconcile(now);
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
        if (now - expected).abs() > 5_000 {
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

    /// Recomputes which spans should be open right now.
    fn reconcile(&mut self, now: i64) {
        let paused = self.paused.is_some();
        self.ledger.set_flag(Kind::Paused, paused, now);

        let (focus, visible) = match (&self.kwin, paused) {
            (Some(snap), false) => {
                let focus = snap.focused().and_then(|w| self.window_subject(w));
                let mut visible: Vec<Subject> = snap
                    .visible(
                        self.cfg.visibility.min_fraction,
                        self.cfg.visibility.min_pixels,
                    )
                    .into_iter()
                    .filter_map(|w| self.window_subject(w))
                    .collect();
                visible.retain(|v| Some(v) != focus.as_ref());
                visible.dedup();
                (focus, visible)
            }
            _ => (None, vec![]),
        };
        let focus_ids = self.ids(focus.as_slice());
        let visible_ids = self.ids(&visible);
        self.ledger.reconcile(Kind::Focus, &focus_ids, now);
        self.ledger.reconcile(Kind::Visible, &visible_ids, now);
        self.focus = focus;
        self.visible = visible;

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
            "open_spans": self.ledger.open_count(),
            "sources": {
                "kwin": {"ok": now - self.kwin_last < 150_000, "last_ms": self.kwin_last,
                         "windows": self.kwin.as_ref().map(|k| k.windows.len()).unwrap_or(0)},
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
