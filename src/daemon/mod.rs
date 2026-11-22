//! The daemon: one Tokio current-thread runtime, one tracker actor fed by event sources.

pub mod control;
pub mod tracker;

use crate::config::Config;
use crate::ipc::Request;
use crate::sources::{idle, kwin, media, session};
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
}

/// State shared with the web server.
#[derive(Default)]
pub struct Shared {
    pub status: serde_json::Value,
    pub config: Config,
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

    let cfg = Config::load_or_create().context("loading config")?;
    let store = Store::open(&util::db_path())?;
    let shared: SharedRef = Arc::new(Mutex::new(Shared {
        status: serde_json::Value::Null,
        config: cfg.clone(),
    }));
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();

    control::spawn(tx.clone())?;

    let conn = zbus::connection::Builder::session()
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

    let mut tracker = Tracker::new(store, cfg.clone(), shared.clone(), conn.clone());
    tracker.start();

    let mut tick = tokio::time::interval(Duration::from_secs(cfg.general.sample_interval_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;

    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);
    tracing::info!("arch-tracker daemon started");

    loop {
        tokio::select! {
            Some(ev) = rx.recv() => tracker.handle(ev).await,
            _ = tick.tick() => tracker.tick().await,
            _ = hup.recv() => {
                match Config::load() {
                    Ok(c) => tracker.handle(Event::ConfigChanged(Box::new(c))).await,
                    Err(e) => tracing::warn!("config reload failed: {e:#}"),
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
