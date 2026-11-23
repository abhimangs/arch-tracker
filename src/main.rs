mod config;
mod daemon;
mod desktop;
mod doctor;
mod export;
mod ipc;
mod model;
mod native_host;
mod report;
mod setup;
mod sources;
mod store;
mod tray;
mod util;
mod web;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

/// Local screen-time and activity tracker for KDE Plasma (Wayland).
#[derive(Parser)]
#[command(name = "arch-tracker", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the tracker (normally started by systemd: arch-tracker.service).
    Daemon,
    /// Show what is being tracked right now and which sources work.
    Status {
        /// Print raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Pause tracking, e.g. "pause 1h", "pause 30m"; no duration = until "resume".
    Pause { duration: Option<String> },
    /// Resume tracking after a pause.
    Resume,
    /// Unpack the Brave extension and register the native-messaging host.
    Setup,
    /// Check every data source and explain how to fix problems.
    Doctor,
    /// Open the dashboard in your browser, signed in (e.g. "open settings").
    Open {
        /// Page: today, apps, agents, trends or settings.
        page: Option<String>,
    },
    /// Read-only dashboard for a database, without tracking (e.g. a copied tracker.db).
    Serve {
        /// Database to show (default: your own tracker.db).
        #[arg(long)]
        db: Option<std::path::PathBuf>,
        /// Port on 127.0.0.1 (default: dashboard port + 1).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Export raw recorded intervals, e.g. "export --from 2026-11-01 --to 2026-11-22 --format csv".
    Export {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value = "csv")]
        format: String,
    },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("arch-tracker: {e:#}");
        std::process::exit(1);
    }
}

fn init_logging(daemon: bool) {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("arch_tracker=info,warn"));
    let journald = daemon && stderr_is_journal();
    if journald && let Ok(layer) = tracing_journald::layer() {
        tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .init();
        return;
    }
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}

/// systemd sets JOURNAL_STREAM=dev:ino for the stream it connected; only trust it if stderr is
/// still that stream (a terminal launched from a systemd scope inherits the variable).
fn stderr_is_journal() -> bool {
    let Some(v) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let v = v.to_string_lossy();
    let Some((dev, ino)) = v.split_once(':') else {
        return false;
    };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(2, &mut st) } != 0 {
        return false;
    }
    dev.parse::<u64>().ok() == Some(st.st_dev) && ino.parse::<u64>().ok() == Some(st.st_ino)
}

fn real_main() -> Result<()> {
    // Browsers start native hosts as "<path> chrome-extension://<id>/".
    if let Some(origin) = std::env::args()
        .nth(1)
        .filter(|a| a.starts_with("chrome-extension://"))
    {
        return native_host::run(&origin);
    }
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon => {
            init_logging(true);
            daemon::run()
        }
        Cmd::Status { json } => {
            let v = block_on(ipc::call(&ipc::Request::Status))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_status(&v);
            }
            Ok(())
        }
        Cmd::Pause { duration } => {
            let until_ms = match duration.as_deref() {
                None => None,
                Some(d) => match util::parse_duration(d) {
                    Some(ms) => Some(util::now_ms() + ms),
                    None => bail!("can't read duration {d:?} (try 30m, 1h, 1h30m)"),
                },
            };
            block_on(ipc::call(&ipc::Request::Pause { until_ms }))?;
            match duration {
                Some(d) => println!("Paused for {d}."),
                None => println!("Paused until you run: arch-tracker resume"),
            }
            Ok(())
        }
        Cmd::Setup => setup::run(),
        Cmd::Open { page } => {
            let route = page.unwrap_or_else(|| "today".into());
            let v = block_on(ipc::call(
                &serde_json::json!({"cmd": "login_url", "route": route}),
            ))?;
            let Some(url) = v["url"].as_str() else {
                bail!(
                    "{}",
                    v["error"].as_str().unwrap_or("the daemon gave no URL")
                );
            };
            println!("Opening the dashboard…");
            web::auth::open_in_browser(url)
        }
        Cmd::Doctor => block_on(doctor::run()),
        Cmd::Serve { db, port } => {
            init_logging(false);
            block_on(serve(db.unwrap_or_else(util::db_path), port))
        }
        Cmd::Export { from, to, format } => {
            let to = to.unwrap_or_else(|| from.clone());
            let conn = store::open_readonly(&util::db_path())?;
            let (_, body) = export::export(&conn, &from, &to, &format)?;
            print!("{body}");
            Ok(())
        }
        Cmd::Resume => {
            block_on(ipc::call(&ipc::Request::Resume))?;
            println!("Tracking resumed.");
            Ok(())
        }
    }
}

async fn serve(db: std::path::PathBuf, port: Option<u16>) -> Result<()> {
    if !db.exists() {
        bail!("no database at {}", db.display());
    }
    let cfg = config::Config::load().unwrap_or_default();
    let port = port.unwrap_or(cfg.dashboard.port.saturating_add(1));
    let status =
        serde_json::json!({"viewer": true, "version": env!("CARGO_PKG_VERSION"), "db": db});
    let shared = std::sync::Arc::new(std::sync::Mutex::new(daemon::Shared {
        status,
        config: cfg,
    }));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let web = web::Web::new(shared, tx, db.clone(), true)?;
    web.listen(port).await?;
    println!(
        "Read-only dashboard for {} (Ctrl+C to stop). Sign in once with:\n  {}",
        db.display(),
        web.login_url("today")?
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

fn print_status(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}
