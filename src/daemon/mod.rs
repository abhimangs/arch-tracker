//! The daemon: one Tokio current-thread runtime, one tracker actor fed by event sources.

pub mod control;
pub mod tracker;

use crate::config::Config;
use crate::ipc::Request;
use crate::sources::{browser, idle, kwin, media, session};
use crate::store::Store;
use crate::util;
use anyhow::{Context, Result, bail};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tracker::Tracker;

/// Everything that can wake the tracker.
pub enum Event {
    Control(Request, oneshot::Sender<serde_json::Value>),
    ConfigChanged(Box<Config>),
    /// config.toml could not be loaded (the previous settings stay in effect).
    ConfigError(String),
    Kwin(Box<kwin::Snapshot>),
    /// input: true = pure input-idle notification; false = inhibitor-respecting one.
    Idle {
        input: bool,
        idle: bool,
        at: i64,
    },
    IdleReady {
        version: u32,
    },
    IdleUnavailable(String),
    Locked(bool),
    Sleep {
        going: bool,
        done: Option<oneshot::Sender<()>>,
    },
    Media(Vec<media::Player>),
    BrowserExt {
        conn: u64,
        msg: browser::ExtMsg,
    },
    BrowserGone(u64),
}

/// State shared with the web server.
pub struct Shared {
    pub status: serde_json::Value,
    pub config: Config,
    /// Bumped whenever the status is republished: the dashboard's live stream waits on it.
    pub live: tokio::sync::watch::Sender<u64>,
    /// Reaches the notifier (Settings' test notification); None without a daemon.
    pub alerts: Option<mpsc::UnboundedSender<crate::alerts::Cmd>>,
    /// How far Claude Code's transcripts have been read; None until the first pass ends.
    pub claude_scan: Option<crate::sources::claude::Progress>,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            status: serde_json::Value::Null,
            config: Config::default(),
            live: tokio::sync::watch::channel(0).0,
            alerts: None,
            claude_scan: None,
        }
    }
}

pub type SharedRef = Arc<Mutex<Shared>>;

pub fn run() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async())
}

async fn run_async() -> Result<()> {
    let rdir = util::runtime_dir();
    std::fs::create_dir_all(&rdir)?;
    set_mode_700(&rdir);
    let _lock = acquire_lock(&rdir.join("daemon.lock"))?;

    // A broken config.toml must not keep the daemon down (systemd would restart it forever):
    // start with defaults and show the error in the status until the file is fixed.
    let (cfg, cfg_error) = match Config::load_or_create() {
        Ok(c) => (c, None),
        Err(e) => {
            tracing::warn!("config.toml has an error, using defaults: {e:#}");
            (Config::default(), Some(format!("{e:#}")))
        }
    };
    let store = Store::open(&util::db_path())?;
    let shared: SharedRef = Arc::new(Mutex::new(Shared {
        config: cfg.clone(),
        ..Default::default()
    }));
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();

    control::spawn(tx.clone())?;
    browser::spawn_ingest(tx.clone())?;
    match crate::setup::register_native_host() {
        Ok(paths) => tracing::info!(
            "native messaging host registered in {} browser profile dir(s)",
            paths.len()
        ),
        Err(e) => tracing::warn!("could not register the native messaging host: {e:#}"),
    }

    let conn = zbus::connection::Builder::session()
        .method_timeout(util::DBUS_TIMEOUT)
        .name(kwin::BUS_NAME)
        .serve_at(kwin::OBJECT_PATH, kwin::KwinIface { tx: tx.clone() })
        .build()
        .await
        .context("connecting to the session bus")?;
    if let Err(e) = kwin::load_script(&conn).await {
        tracing::warn!("could not load the KWin script: {e:#}");
    }

    idle::spawn(tx.clone());
    session::spawn(conn.clone(), tx.clone()).await;
    media::spawn(conn.clone(), tx.clone()).await;

    let web = crate::web::Web::new(shared.clone(), tx.clone(), util::db_path(), false)?;
    serve_dashboard(&web, cfg.dashboard.port).await;
    let tray = crate::tray::Tray::new(
        conn.clone(),
        shared.clone(),
        tx.clone(),
        web.clone(),
        util::db_path(),
    );
    tray.set_enabled(cfg.dashboard.tray).await;
    let alerts = crate::alerts::spawn(conn.clone(), shared.clone(), web.clone(), util::db_path());
    if let Ok(mut s) = shared.lock() {
        s.alerts = Some(alerts);
    }
    crate::sources::claude::spawn(shared.clone(), util::db_path());
    crate::sources::codex::spawn(shared.clone(), util::db_path());
    crate::sources::hermes::spawn(shared.clone(), util::db_path());
    let _watcher = match watch_config(tx.clone()) {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!("config.toml changes will not be picked up live: {e:#}");
            None
        }
    };

    let mut tracker = Tracker::new(store, cfg.clone(), shared.clone(), conn.clone());
    tracker.set_config_error(cfg_error);
    tracker.start();

    let mut tick = tokio::time::interval(Duration::from_secs(cfg.general.sample_interval_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;

    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);
    tracing::info!("arch-tracker daemon started");

    let mut interval_secs = cfg.general.sample_interval_secs;
    loop {
        let want = tracker.sample_interval_secs();
        if want != interval_secs {
            interval_secs = want;
            tick = tokio::time::interval(Duration::from_secs(want));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.reset();
        }
        tokio::select! {
            Some(ev) = rx.recv() => {
                let dash = match &ev {
                    Event::ConfigChanged(c) => Some((c.dashboard.port, c.dashboard.tray)),
                    _ => None,
                };
                tracker.handle(ev).await;
                // The port and the tray apply however the config changed (dashboard or file).
                if let Some((port, show_tray)) = dash {
                    serve_dashboard(&web, port).await;
                    tray.set_enabled(show_tray).await;
                }
            }
            _ = tick.tick() => tracker.tick().await,
            _ = sleep_until_opt(tracker.deadline()) => tracker.on_deadline(),
            _ = hup.recv() => {
                match Config::load() {
                    Ok(c) => {
                        let (port, show_tray) = (c.dashboard.port, c.dashboard.tray);
                        tracker.handle(Event::ConfigChanged(Box::new(c))).await;
                        serve_dashboard(&web, port).await;
                        tray.set_enabled(show_tray).await;
                    }
                    Err(e) => {
                        tracing::warn!("config reload failed: {e:#}");
                        tracker.handle(Event::ConfigError(format!("{e:#}"))).await;
                    }
                }
            }
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Stopping]);
    tracker.shutdown();
    kwin::unload_script(&conn).await;
    tracing::info!("arch-tracker daemon stopped");
    Ok(())
}

/// Serves the dashboard on port (a no-op if it already does); a busy port is reported in the
/// status the dashboard and `doctor` show, and the old port keeps serving.
async fn serve_dashboard(web: &std::sync::Arc<crate::web::Web>, port: u16) {
    if web.port() == Some(port) {
        return;
    }
    match web.listen(port).await {
        Ok(()) => web.set_error(None),
        Err(e) => {
            tracing::warn!("dashboard could not start on port {port}: {e:#}");
            web.set_error(Some(format!("Port {port} is not available ({e}).")));
        }
    }
}

async fn sleep_until_opt(t: Option<tokio::time::Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Reloads config.toml when it changes on disk (hand edits and dashboard saves).
fn watch_config(tx: mpsc::UnboundedSender<Event>) -> Result<notify::RecommendedWatcher> {
    use notify::Watcher;
    let path = crate::config::config_path();
    let dir = path
        .parent()
        .context("config path has no directory")?
        .to_path_buf();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if !ev.paths.iter().any(|p| p.file_name() == path.file_name()) {
            return;
        }
        if !(ev.kind.is_modify() || ev.kind.is_create()) {
            return;
        }
        match Config::load() {
            Ok(c) => {
                let _ = tx.send(Event::ConfigChanged(Box::new(c)));
            }
            Err(e) => {
                tracing::warn!("config.toml has an error, keeping the previous settings: {e:#}");
                let _ = tx.send(Event::ConfigError(format!("{e:#}")));
            }
        }
    })?;
    w.watch(&dir, notify::RecursiveMode::NonRecursive)?;
    Ok(w)
}

fn set_mode_700(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
}

/// One daemon per user: an flock held for the daemon's lifetime.
fn acquire_lock(path: &std::path::Path) -> Result<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!("another arch-tracker daemon is already running");
    }
    Ok(f)
}
