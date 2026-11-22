//! "arch-tracker doctor": checks every source and says how to fix what is broken.

use crate::ipc;
use crate::util;
use std::process::Command;

struct Report {
    ok: usize,
    bad: usize,
}

impl Report {
    fn line(&mut self, good: bool, what: &str, fix: &str) {
        if good {
            self.ok += 1;
            println!("  ok    {what}");
        } else {
            self.bad += 1;
            println!("  FIX   {what}");
            if !fix.is_empty() {
                for l in fix.lines() {
                    println!("        {l}");
                }
            }
        }
    }
}

fn sh(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn run() -> anyhow::Result<()> {
    let mut r = Report { ok: 0, bad: 0 };
    println!("arch-tracker doctor\n");

    let unit = sh("systemctl", &["--user", "is-active", "arch-tracker"]).unwrap_or_default();
    let enabled = sh("systemctl", &["--user", "is-enabled", "arch-tracker"]).unwrap_or_default();
    r.line(
        unit == "active",
        &format!("systemd user service: {unit} ({enabled})"),
        "systemctl --user enable --now arch-tracker\njournalctl --user -u arch-tracker -e   (to see why it stopped)",
    );
    let env = sh("systemctl", &["--user", "show-environment"]).unwrap_or_default();
    r.line(
        env.contains("WAYLAND_DISPLAY=") && env.contains("DBUS_SESSION_BUS_ADDRESS="),
        "user manager has WAYLAND_DISPLAY and DBUS_SESSION_BUS_ADDRESS",
        "Your session did not export its environment to systemd. Log out and back in, or run:\nsystemctl --user import-environment WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS",
    );

    let status = ipc::call(&ipc::Request::Status).await;
    let Ok(s) = status else {
        r.line(
            false,
            "daemon answers on the control socket",
            "Start it: systemctl --user start arch-tracker",
        );
        println!("\n{} ok, {} to fix", r.ok, r.bad);
        return Ok(());
    };
    r.line(true, "daemon answers on the control socket", "");
    let src = &s["sources"];
    r.line(
        src["kwin"]["ok"].as_bool().unwrap_or(false),
        &format!("KWin window tracking ({} windows)", src["kwin"]["windows"]),
        "The KWin script is reloaded automatically. If this persists: systemctl --user restart arch-tracker",
    );
    r.line(
        src["idle"]["ok"].as_bool().unwrap_or(false),
        &format!(
            "idle detection (ext-idle-notify v{})",
            src["idle"]["protocol_version"]
        ),
        &format!(
            "Error: {}",
            src["idle"]["error"].as_str().unwrap_or("unknown")
        ),
    );
    r.line(
        src["logind"]["ok"].as_bool().unwrap_or(false),
        "logind (lock screen, suspend)",
        "",
    );

    let ext_alive = src["browser_extension"]
        .as_array()
        .map(|a| a.iter().any(|e| e["alive"].as_bool().unwrap_or(false)))
        .unwrap_or(false);
    let ext_dir = crate::setup::extension_dir();
    r.line(
        ext_alive,
        "Brave extension connected (exact tabs)",
        &format!(
            "Open brave://extensions, switch on Developer mode, Load unpacked:\n{}\nExpected ID: {}",
            ext_dir.display(),
            crate::setup::EXTENSION_ID
        ),
    );
    let manifest = util::home().join(
        ".config/BraveSoftware/Brave-Origin-Beta/NativeMessagingHosts/dev.archtracker.native.json",
    );
    if let Ok(text) = std::fs::read_to_string(&manifest) {
        let path = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["path"].as_str().map(str::to_string))
            .unwrap_or_default();
        r.line(
            std::path::Path::new(&path).exists(),
            &format!("native messaging host points to {path}"),
            "Run: arch-tracker setup",
        );
    }
    if !ext_alive {
        // Queried lazily (only when a browser window is on screen), so ask the bus directly.
        let pbi = sh(
            "busctl",
            &["--user", "status", "org.kde.plasma.browser_integration"],
        )
        .map(|o| o.contains("PID="))
        .unwrap_or(false);
        r.line(
            pbi,
            "Plasma Browser Integration fallback available",
            "Install plasma-browser-integration and enable its browser extension",
        );
    }

    let kitty_running = sh("pgrep", &["-x", "kitty"])
        .map(|o| !o.is_empty())
        .unwrap_or(false);
    if kitty_running {
        let pid: i64 = sh("pgrep", &["-x", "-o", "kitty"])
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        r.line(
            crate::sources::terminal::kitty::socket_for(pid).is_some(),
            "kitty remote-control socket found",
            "Add to kitty.conf:  allow_remote_control socket-only\n                    listen_on unix:/tmp/kitty.sock",
        );
    }

    let db = util::db_path();
    let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    r.line(
        size > 0,
        &format!("database {} ({:.1} MB)", db.display(), size as f64 / 1e6),
        "",
    );

    let cfg = crate::config::Config::load().unwrap_or_default();
    let url = format!("http://127.0.0.1:{}/", cfg.dashboard.port);
    let reachable = std::net::TcpStream::connect(("127.0.0.1", cfg.dashboard.port)).is_ok();
    r.line(
        reachable,
        &format!("dashboard at {url}"),
        "Another program may hold the port; change it in config.toml",
    );

    let rss = sh("ps", &["-o", "rss=", "-C", "arch-tracker"])
        .and_then(|o| {
            o.lines()
                .map(|l| l.trim().parse::<u64>().unwrap_or(0))
                .max()
        })
        .unwrap_or(0);
    if rss > 0 {
        r.line(
            rss < 40_000,
            &format!("daemon memory {:.1} MB", rss as f64 / 1024.0),
            "Memory is higher than expected; please report it.",
        );
    }
    println!("\n{} ok, {} to fix", r.ok, r.bad);
    Ok(())
}
