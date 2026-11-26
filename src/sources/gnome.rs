//! GNOME: its Shell extension (gnome/arch-tracker@archtracker.dev) sends the same window snapshots
//! the KWin script sends, to the same D-Bus service, and raises a window when asked. Setup puts the
//! extension where GNOME looks for it and switches it on.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use zbus::Connection;

pub const UUID: &str = "arch-tracker@archtracker.dev";
const METADATA: &str = include_str!("../../gnome/arch-tracker@archtracker.dev/metadata.json");
const EXTENSION: &str = include_str!("../../gnome/arch-tracker@archtracker.dev/extension.js");

/// The package's copy, installed with arch-tracker.
fn system_dir() -> PathBuf {
    PathBuf::from("/usr/share/gnome-shell/extensions").join(UUID)
}

/// Where a copy of your own goes when the package's isn't there (a build from source).
fn user_dir() -> PathBuf {
    crate::util::xdg("XDG_DATA_HOME", ".local/share")
        .join("gnome-shell/extensions")
        .join(UUID)
}

/// Raises the window of the nearest of `pids` that has one, titled `caption` if one is.
pub async fn activate(conn: &Connection, pids: &[u32], caption: Option<&str>) -> Result<()> {
    let reply = conn
        .call_method(
            Some("org.gnome.Shell"),
            "/dev/archtracker/Gnome",
            Some("dev.archtracker.Gnome1"),
            "Activate",
            &(pids, caption.unwrap_or("")),
        )
        .await
        .context("asking the GNOME Shell extension")?;
    if !reply.body().deserialize::<bool>()? {
        bail!("no GNOME window for the agent");
    }
    Ok(())
}

/// Puts the extension in place (unless the package did) and switches it on. Returns where it is.
pub fn install() -> Result<PathBuf> {
    let dir = if system_dir().join("metadata.json").exists() {
        system_dir()
    } else {
        let dir = user_dir();
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("metadata.json"), METADATA)?;
        std::fs::write(dir.join("extension.js"), EXTENSION)?;
        dir
    };
    let ok = std::process::Command::new("gnome-extensions")
        .args(["enable", UUID])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        // Not known to the running Shell yet (it reads new extensions at login): turn it on in
        // the settings it reads then.
        let list = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.shell", "enabled-extensions"])
            .output()
            .context("running gsettings")?;
        let list = String::from_utf8_lossy(&list.stdout).trim().to_string();
        if !list.contains(UUID) {
            let next = match list.trim_start_matches("@as ").trim() {
                "[]" | "" => format!("['{UUID}']"),
                l => format!("{}, '{UUID}']", l.trim_end_matches(']')),
            };
            let status = std::process::Command::new("gsettings")
                .args(["set", "org.gnome.shell", "enabled-extensions", &next])
                .status()?;
            if !status.success() {
                bail!("gsettings could not switch the extension on");
            }
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_names_itself_and_the_daemon_alike() {
        let meta: serde_json::Value = serde_json::from_str(METADATA).unwrap();
        assert_eq!(meta["uuid"], UUID);
        assert!(meta["shell-version"].as_array().unwrap().len() >= 3);
        // It speaks to the service and method the daemon serves.
        assert!(EXTENSION.contains(&format!("'{}'", super::super::kwin::BUS_NAME)));
        assert!(EXTENSION.contains(&format!("'{}'", super::super::kwin::OBJECT_PATH)));
        assert!(EXTENSION.contains("'dev.archtracker.KWin1'") && EXTENSION.contains("'Snapshot'"));
        assert!(EXTENSION.contains("/dev/archtracker/Gnome") && EXTENSION.contains("Activate("));
    }
}
