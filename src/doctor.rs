//! "arch-tracker doctor": checks every source and says how to fix what is broken.

use crate::ipc;
use crate::setup;
use crate::util;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Report {
    ok: usize,
    bad: usize,
}

impl Report {
    /// Information that needs no fixing (and may not be fixable).
    fn note(&mut self, what: &str, detail: &str) {
        println!("  info  {what}");
        for l in detail.lines() {
            println!("        {l}");
        }
    }

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

/// Native-messaging bridge per detected browser, sandboxed browsers, and live connections.
fn browser_checks(r: &mut Report, connected: &[String]) {
    let chromium = setup::chromium_browsers();
    let firefox = setup::firefox_browsers();
    for b in &chromium {
        // Per-user manifests win over system-wide ones.
        let mut paths = vec![b.manifest_path()];
        paths.extend(CHROMIUM_SYSTEM.iter().map(|d| system_manifest(d)));
        let (good, what) = bridge_state(&paths);
        r.line(
            good,
            &format!(
                "{} ({}): browser bridge {what}",
                b.name,
                util::abbreviate_home(&b.dir)
            ),
            "Run: arch-tracker setup",
        );
    }
    for b in &firefox {
        let mut paths = b.manifest_paths();
        let system: &[&str] = if b.name == "LibreWolf" {
            &[
                "/usr/lib/librewolf/native-messaging-hosts",
                FIREFOX_SYSTEM[0],
            ]
        } else {
            FIREFOX_SYSTEM
        };
        paths.extend(system.iter().map(|d| system_manifest(d)));
        let (good, what) = bridge_state(&paths);
        r.line(
            good,
            &format!("{}: browser bridge {what}", b.name),
            "Run: arch-tracker setup",
        );
    }
    if chromium.is_empty() && firefox.is_empty() {
        r.note(
            "no Chromium- or Firefox-family browser found",
            "Start your browser once, then run: arch-tracker setup",
        );
    }
    for b in setup::sandboxed_browsers() {
        r.note(
            &format!(
                "{} ({}) is sandboxed: the extension can't reach arch-tracker from it",
                b.name, b.how
            ),
            "Its windows are still tracked; for exact tabs install it from pacman or the AUR.",
        );
    }
    r.line(
        !connected.is_empty(),
        &if connected.is_empty() {
            "browser extension connected (exact tabs)".to_string()
        } else {
            format!("browser extension connected (exact tabs): {}", connected.join(", "))
        },
        &format!(
            "Chromium family: open chrome://extensions (or brave://extensions), switch on Developer mode,\n\
             Load unpacked: {}   (expected ID: {})\n\
             Firefox family: install the signed add-on from {}\n\
             (or about:debugging > Load Temporary Add-on: {})\n\
             \"arch-tracker setup\" prints every step.",
            setup::extension_dir().display(),
            setup::EXTENSION_ID,
            setup::RELEASES,
            setup::firefox_extension_dir().join("manifest.json").display(),
        ),
    );
}

const CHROMIUM_SYSTEM: &[&str] = &[
    "/etc/chromium/native-messaging-hosts",
    "/etc/opt/chrome/native-messaging-hosts",
];
const FIREFOX_SYSTEM: &[&str] = &[
    "/usr/lib/mozilla/native-messaging-hosts",
    "/usr/lib64/mozilla/native-messaging-hosts",
];

fn system_manifest(dir: &str) -> PathBuf {
    Path::new(dir).join(format!("{}.json", setup::NATIVE_HOST))
}

/// Checks the first manifest that exists (the one the browser uses): ours, for our extension,
/// pointing at a program that exists. Returns (ok, description).
fn bridge_state(paths: &[PathBuf]) -> (bool, String) {
    let Some(file) = paths.iter().find(|p| p.exists()) else {
        return (false, "not registered".into());
    };
    let shown = util::abbreviate_home(file);
    let Some(v) = std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return (false, format!("unreadable: {shown}"));
    };
    let allowed = |key: &str, want: &str| {
        v[key]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(want)))
    };
    let origin = format!("chrome-extension://{}/", setup::EXTENSION_ID);
    if !allowed("allowed_origins", &origin) && !allowed("allowed_extensions", setup::GECKO_ID) {
        return (
            false,
            format!("does not allow the arch-tracker extension: {shown}"),
        );
    }
    let exe = v["path"].as_str().unwrap_or_default();
    if !Path::new(exe).is_file() {
        return (
            false,
            format!("points to a missing program {exe:?}: {shown}"),
        );
    }
    (true, format!("registered: {shown} -> {exe}"))
}

/// "brave-origin-beta (pid 1234)" from /proc/<pid>/exe (/opt/brave.com/brave-origin-beta/brave).
fn process_label(pid: u64) -> String {
    let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return format!("browser pid {pid}");
    };
    let name = |p: Option<&Path>| {
        p.and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    };
    let file = name(Some(&exe)).unwrap_or_default();
    let file = file.trim_end_matches(" (deleted)");
    let label = match name(exe.parent()) {
        Some(dir) if dir != "bin" && dir != "sbin" => dir,
        _ => file.to_string(),
    };
    format!("{} (pid {pid})", label.trim_end_matches("-bin"))
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
    let desktop = src["kwin"]["desktop"].as_str().unwrap_or("KDE Plasma");
    let (source, fix) = match desktop {
        "Hyprland" => (
            "Hyprland's IPC socket",
            "The tracker finds Hyprland through HYPRLAND_INSTANCE_SIGNATURE or the sockets in $XDG_RUNTIME_DIR/hypr.\nAdd to hyprland.conf: exec-once = systemctl --user import-environment HYPRLAND_INSTANCE_SIGNATURE WAYLAND_DISPLAY XDG_CURRENT_DESKTOP\nthen: systemctl --user restart arch-tracker",
        ),
        "Sway" => (
            "Sway's IPC socket",
            "The tracker finds sway through SWAYSOCK or the socket in $XDG_RUNTIME_DIR.\nAdd to the sway config: exec systemctl --user import-environment SWAYSOCK WAYLAND_DISPLAY XDG_CURRENT_DESKTOP\nthen: systemctl --user restart arch-tracker",
        ),
        "GNOME" => (
            "the GNOME Shell extension",
            "Turn it on: gnome-extensions enable arch-tracker@archtracker.dev\nthen log out and back in (GNOME on Wayland loads new extensions at login).",
        ),
        _ => (
            "a KWin script",
            "The KWin script is reloaded automatically. If this persists: systemctl --user restart arch-tracker",
        ),
    };
    r.line(
        src["kwin"]["ok"].as_bool().unwrap_or(false),
        &format!(
            "{desktop} window tracking through {source} ({} windows)",
            src["kwin"]["windows"]
        ),
        fix,
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

    let connected: Vec<String> = src["browser_extension"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| e["alive"].as_bool().unwrap_or(false))
                .map(|e| {
                    let pid = e["browser_pid"].as_u64().unwrap_or(0);
                    match (e["app"].as_str(), e["name"].as_str()) {
                        (Some(_), Some(name)) => format!("{name} (pid {pid})"),
                        _ => process_label(pid),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let ext_alive = !connected.is_empty();
    browser_checks(&mut r, &connected);
    if let Ok(conn) = crate::store::open_readonly(&util::db_path())
        && let Ok(links) = crate::store::browser_links(&conn)
        && !links.is_empty()
    {
        let names: Vec<&str> = links.iter().map(|l| l.name.as_str()).collect();
        r.note(
            "browsers the extension has connected from",
            &names.join(", "),
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
    let url = format!("http://arch-tracker.localhost:{}/", cfg.dashboard.port);
    let reachable = std::net::TcpStream::connect(("127.0.0.1", cfg.dashboard.port)).is_ok();
    r.line(
        reachable,
        &format!("dashboard at {url} (open it with: arch-tracker open)"),
        "Another program may hold the port: pick another one in config.toml; the daemon moves without a restart.",
    );
    if let Some(e) = s["config_error"].as_str() {
        r.line(
            false,
            "config.toml loads",
            &format!("{e}\nThe last settings that loaded are in use until the file is fixed."),
        );
    }

    let rss = sh("ps", &["-o", "rss=", "-C", "arch-tracker"])
        .and_then(|o| {
            o.lines()
                .map(|l| l.trim().parse::<u64>().unwrap_or(0))
                .max()
        })
        .unwrap_or(0);
    if rss > 0 {
        r.line(
            // The target is under 15 MB; about 9 MB of that is the shared executable.
            rss < 20_000,
            &format!("daemon memory {:.1} MB", rss as f64 / 1024.0),
            "Memory is higher than expected; please report it.",
        );
    }
    println!("\n{} ok, {} to fix", r.ok, r.bad);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_states() {
        let dir = std::env::temp_dir().join(format!("arch-tracker-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = std::env::current_exe().unwrap();
        let write = |name: &str, v: serde_json::Value| {
            let p = dir.join(name);
            std::fs::write(&p, v.to_string()).unwrap();
            p
        };
        let chromium = write(
            "c.json",
            serde_json::json!({"path": exe, "allowed_origins": [format!("chrome-extension://{}/", setup::EXTENSION_ID)]}),
        );
        let firefox = write(
            "f.json",
            serde_json::json!({"path": exe, "allowed_extensions": [setup::GECKO_ID]}),
        );
        let gone = write(
            "g.json",
            serde_json::json!({"path": dir.join("no-such-binary"), "allowed_extensions": [setup::GECKO_ID]}),
        );
        let other = write(
            "o.json",
            serde_json::json!({"path": exe, "allowed_origins": ["chrome-extension://someoneelse/"]}),
        );
        let missing = dir.join("missing.json");

        assert!(bridge_state(std::slice::from_ref(&chromium)).0);
        assert!(bridge_state(&[missing.clone(), firefox.clone()]).0);
        assert_eq!(
            bridge_state(std::slice::from_ref(&missing)),
            (false, "not registered".into())
        );
        // The first manifest found is the one the browser uses, even if a later one is fine.
        let (ok, what) = bridge_state(&[gone, firefox]);
        assert!(!ok && what.contains("missing program"), "{what}");
        assert!(bridge_state(&[other]).1.contains("does not allow"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn labels_processes() {
        let me = std::process::id() as u64;
        assert!(process_label(me).ends_with(&format!(" (pid {me})")));
        assert_eq!(
            process_label(u32::MAX as u64 + 1),
            format!("browser pid {}", u32::MAX as u64 + 1)
        );
    }
}
