//! Core data model shared by the daemon, the store and reports.

use serde::{Deserialize, Serialize};

/// What a span is about. Empty strings mean "not applicable" (kept non-NULL so UNIQUE works).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Subject {
    /// Stable app key: desktop-file id, window class or process name.
    pub app: String,
    /// Human name of the app ("Brave Origin Beta", "Google Meet").
    pub app_name: String,
    /// Normalized window or tab title.
    pub title: String,
    /// Browser host ("www.instagram.com"), subdomains kept.
    pub host: String,
    /// Incognito / private window: never carries title or host.
    pub private: bool,
    /// Program inside a terminal ("claude", "nvim").
    pub program: String,
    /// Project of that program: git root (home abbreviated) or cwd.
    pub project: String,
}

impl Subject {
    pub fn app(app: &str, app_name: &str) -> Self {
        Subject {
            app: app.to_string(),
            app_name: app_name.to_string(),
            ..Default::default()
        }
    }
}

/// Kinds of stored intervals. Spans describe *what*; signals describe *conditions*.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(i64)]
pub enum Kind {
    /// The focused window (with browser/terminal details).
    Focus = 1,
    /// On screen but not focused.
    Visible = 2,
    /// Audio playing from something that is not visible.
    BgAudio = 3,
    /// An AI agent process doing work (CPU or terminal output).
    AgentBusy = 4,
    /// App running (window, tray or background service).
    Running = 5,
    /// No keyboard/mouse input (raw gap, starts at the last input).
    InputIdle = 10,
    /// The compositor's idle is inhibited (video, call, presentation).
    Inhibited = 11,
    /// Media is playing in a visible window.
    MediaVisible = 12,
    /// Microphone capture is open.
    Mic = 13,
    /// Screen locked.
    Locked = 14,
    /// System suspended.
    Suspended = 15,
    /// Tracking paused by the user.
    Paused = 16,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::Focus,
        Kind::Visible,
        Kind::BgAudio,
        Kind::AgentBusy,
        Kind::Running,
        Kind::InputIdle,
        Kind::Inhibited,
        Kind::MediaVisible,
        Kind::Mic,
        Kind::Locked,
        Kind::Suspended,
        Kind::Paused,
    ];

    pub fn from_i64(v: i64) -> Option<Kind> {
        Kind::ALL.iter().copied().find(|k| *k as i64 == v)
    }
}

/// A stored interval [start_ms, end_ms).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub kind: Kind,
    pub subject: i64,
    pub start: i64,
    pub end: i64,
}
