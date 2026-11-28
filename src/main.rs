mod agentwatch;
mod alerts;
mod config;
mod daemon;
mod demo;
mod desktop;
mod doctor;
mod export;
mod focus;
mod ipc;
mod model;
mod native_host;
mod report;
mod setup;
mod sources;
mod store;
mod today;
mod tray;
mod util;
mod web;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

/// Local screen-time and activity tracker for KDE Plasma, GNOME, Hyprland and Sway (Wayland).
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
    /// Today's screen time, categories, apps and sites, projects and agents, in the terminal.
    Today {
        /// Another day: YYYY-MM-DD or "yesterday".
        #[arg(long)]
        date: Option<String>,
        /// Print JSON.
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
        /// Page: today, apps, agents, claude, trends or settings.
        page: Option<String>,
    },
    /// Write a new database of made-up activity (for screenshots and trying the dashboard).
    #[command(hide = true)]
    Demo {
        /// The new database file to create.
        #[arg(long)]
        db: std::path::PathBuf,
        /// How many days, ending today.
        #[arg(long, default_value_t = 7)]
        days: u32,
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
    // Browsers start native hosts as "<path> chrome-extension://<id>/" (Chromium family) or
    // "<path> <manifest path> <add-on id>" (Firefox family).
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if let Some(origin) = native_host::origin_from_args(&args) {
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
                    // A pause of nothing would end before it began.
                    Some(ms) if ms > 0 => Some(util::now_ms().saturating_add(ms)),
                    Some(_) => bail!("a pause needs a length above zero (try 30m, 1h, 1h30m)"),
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
        Cmd::Today { date, json } => today::run(date, json),
        Cmd::Setup => setup::run(),
        Cmd::Demo { db, days } => demo::run(&db, days.clamp(1, 60)),
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
            let format = export::Format::parse(&format)?;
            let range = export::bounds(&from, &to)?;
            let conn = store::open_readonly(&util::db_path())?;
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            match export::write(&conn, range, format, &mut out) {
                // Piped into head or similar: stopping early is fine.
                Err(e) if is_broken_pipe(&e) => Ok(()),
                r => r,
            }
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
        ..Default::default()
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

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

/// The readable form of "arch-tracker status" (--json prints everything).
fn print_status(v: &serde_json::Value) {
    let flag = |p: &str| v.pointer(p).and_then(|x| x.as_bool()).unwrap_or(false);
    let num = |p: &str| v.pointer(p).and_then(|x| x.as_i64());
    let watching = v
        .get("media_visible")
        .and_then(|m| m.as_array())
        .is_some_and(|m| !m.is_empty());
    let state = if flag("/paused") {
        match num("/paused_until_ms") {
            Some(ms) => format!("Paused until {}", clock(ms)),
            None => "Paused until you resume".into(),
        }
    } else if flag("/locked") {
        "Locked".into()
    } else if flag("/suspended") {
        "Suspended".into()
    } else if flag("/idle/input_idle") && watching {
        "Watching".into()
    } else if flag("/mic") {
        "In a call".into()
    } else if flag("/idle/input_idle") {
        "No input".into()
    } else {
        "Active".into()
    };
    println!("State    {state}");
    println!("Focus    {}", describe_focus(v.get("focus")));
    if let Some(today) = today_line() {
        println!("Today    {today}");
    }
    println!("Sources");
    let src = |p: &str| v.pointer(&format!("/sources/{p}"));
    let desktop = src("kwin/desktop")
        .and_then(|d| d.as_str())
        .unwrap_or("KDE Plasma");
    let windows = if flag("/sources/kwin/ok") {
        let n = num("/sources/kwin/windows").unwrap_or(0);
        format!("ok, {n} from {desktop}")
    } else if desktop == "KDE Plasma" {
        "not reporting windows (the tracker reloads its KWin script; see arch-tracker doctor)"
            .into()
    } else {
        format!("{desktop} is not reporting windows (see arch-tracker doctor)")
    };
    println!("  windows    {windows}");
    let idle = if flag("/sources/idle/ok") {
        let v = num("/sources/idle/protocol_version").unwrap_or(0);
        format!("ok, ext-idle-notify v{v}")
    } else {
        let e = src("idle/error").and_then(|e| e.as_str()).unwrap_or("");
        let e = if e.is_empty() { "not available" } else { e };
        format!("error: {e} (idle time is counted)")
    };
    println!("  idle       {idle}");
    let logind = if flag("/sources/logind/ok") {
        "ok"
    } else {
        "not connected (lock and suspend are not seen)"
    };
    println!("  logind     {logind}");
    let ext: Vec<&serde_json::Value> = src("browser_extension")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .filter(|c| c["alive"].as_bool() == Some(true))
                .collect()
        })
        .unwrap_or_default();
    let browser = if !ext.is_empty() {
        let sum = |k: &str| ext.iter().filter_map(|c| c[k].as_i64()).sum::<i64>();
        format!(
            "extension connected, {} windows, {} tabs",
            sum("windows"),
            sum("tabs")
        )
    } else if let Some(tabs) = src("plasma_browser_integration").and_then(|p| p["tabs"].as_i64()) {
        format!("extension not connected; Plasma Browser Integration fallback, {tabs} tabs")
    } else {
        "extension not connected".into()
    };
    println!("  browser    {browser}");
    let version = v.get("version").and_then(|x| x.as_str()).unwrap_or("?");
    match num("/started_ms") {
        Some(ms) => println!("arch-tracker {version}, running since {}", clock(ms)),
        None => println!("arch-tracker {version}"),
    }
}

/// "Brave, github.com" / "kitty, nvim in ~/p/x" / "Editor, notes.md", at most 72 characters.
fn describe_focus(f: Option<&serde_json::Value>) -> String {
    let Some(f) = f.filter(|f| f.is_object()) else {
        return "nothing focused".into();
    };
    let s = |k: &str| f[k].as_str().unwrap_or("").to_string();
    let mut bits = vec![s("app_name")];
    let private = f["private"].as_bool().unwrap_or(false);
    if private {
        bits.push("private window".into());
    } else if !s("host").is_empty() {
        bits.push(s("host"));
    }
    if !s("program").is_empty() {
        bits.push(if s("project").is_empty() {
            s("program")
        } else {
            format!("{} in {}", s("program"), s("project"))
        });
    } else if s("host").is_empty() && !private && !s("title").is_empty() {
        bits.push(s("title"));
    }
    let text = bits
        .into_iter()
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    truncate(&text, 72)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Today's totals, read from the database (what the dashboard shows).
fn today_line() -> Option<String> {
    let cfg = config::Config::load().unwrap_or_default();
    let conn = store::open_readonly(&util::db_path()).ok()?;
    let r = report::build_day(&conn, &cfg, &util::local_date(util::now_ms())).ok()?;
    let t = &r.totals;
    Some(format!(
        "{} on screen ({} active, {} watching, {} in calls)",
        hm(t.screen),
        hm(t.times.active),
        hm(t.times.passive),
        hm(t.times.call)
    ))
}

fn hm(secs: i64) -> String {
    let (h, m) = (secs / 3600, secs / 60 % 60);
    match (h, m) {
        (0, 0) => format!("{secs}s"),
        (0, _) => format!("{m}m"),
        _ => format!("{h}h {m:02}m"),
    }
}

/// Local "HH:MM", with the date when it isn't today.
fn clock(ms: i64) -> String {
    let Ok(ts) = jiff::Timestamp::from_millisecond(ms) else {
        return "?".into();
    };
    let z = ts.to_zoned_datetime(jiff::tz::TimeZone::system());
    if util::local_date(ms) == util::local_date(util::now_ms()) {
        z.strftime("%H:%M").to_string()
    } else {
        z.strftime("%Y-%m-%d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_is_described_without_private_details() {
        let f = serde_json::json!({"app_name": "Browser", "host": "example.org", "title": "A page",
            "private": false, "program": "", "project": ""});
        assert_eq!(describe_focus(Some(&f)), "Browser, example.org");
        let f = serde_json::json!({"app_name": "Browser", "host": "", "title": "",
            "private": true, "program": "", "project": ""});
        assert_eq!(describe_focus(Some(&f)), "Browser, private window");
        let f = serde_json::json!({"app_name": "Terminal", "host": "", "title": "x",
            "private": false, "program": "nvim", "project": "~/p/demo"});
        assert_eq!(describe_focus(Some(&f)), "Terminal, nvim in ~/p/demo");
        let long = "t".repeat(200);
        let f = serde_json::json!({"app_name": "Editor", "title": long});
        assert_eq!(describe_focus(Some(&f)).chars().count(), 72);
        assert_eq!(
            describe_focus(Some(&serde_json::Value::Null)),
            "nothing focused"
        );
        assert_eq!(hm(42), "42s");
        assert_eq!(hm(3 * 60), "3m");
        assert_eq!(hm(3600 + 5 * 60), "1h 05m");
    }
}
