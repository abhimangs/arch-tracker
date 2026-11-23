//! One-time setup: unpacks the browser extension and registers the native-messaging host with
//! every Chromium-family browser config found. Safe to re-run; the daemon also re-registers on start.

use crate::util;
use anyhow::{Context, Result};
use std::path::PathBuf;

pub const EXTENSION_ID: &str = "ofgofljkhfmjkgklekeiajnkplaldokg";
pub const NATIVE_HOST: &str = "dev.archtracker.native";
/// The Firefox add-on ID (browser_specific_settings.gecko.id in extension/manifest.firefox.json).
pub const GECKO_ID: &str = "arch-tracker@abhimangs.github.io";
const MANIFEST: &str = include_str!("../extension/manifest.json");
const BACKGROUND: &str = include_str!("../extension/background.js");

/// The packaged copy (updated by pacman) if installed, else a per-user copy.
pub fn extension_dir() -> PathBuf {
    let packaged = PathBuf::from("/usr/share/arch-tracker/extension");
    if packaged.join("manifest.json").exists() {
        return packaged;
    }
    util::data_dir().join("extension")
}

/// Browser config dirs that hold a NativeMessagingHosts folder.
fn browser_dirs() -> Vec<PathBuf> {
    let cfg = util::home().join(".config");
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(cfg.join("BraveSoftware")) {
        out.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    for d in [
        "google-chrome",
        "google-chrome-beta",
        "chromium",
        "vivaldi",
        "microsoft-edge",
        "thorium",
    ] {
        let p = cfg.join(d);
        if p.is_dir() {
            out.push(p);
        }
    }
    out
}

pub fn write_extension() -> Result<PathBuf> {
    let dir = extension_dir();
    if dir.starts_with("/usr") {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("manifest.json"), MANIFEST)?;
    std::fs::write(dir.join("background.js"), BACKGROUND)?;
    Ok(dir)
}

/// Registers this binary as the native host. Returns the manifest paths written.
pub fn register_native_host() -> Result<Vec<PathBuf>> {
    let exe = std::fs::read_link("/proc/self/exe").context("finding own executable")?;
    let manifest = serde_json::json!({
        "name": NATIVE_HOST,
        "description": "arch-tracker: receives active-tab host and title from the arch-tracker extension",
        "path": exe,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    });
    let text = serde_json::to_string_pretty(&manifest)? + "\n";
    let mut written = vec![];
    for d in browser_dirs() {
        let nm = d.join("NativeMessagingHosts");
        if std::fs::create_dir_all(&nm).is_err() {
            continue;
        }
        let f = nm.join(format!("{NATIVE_HOST}.json"));
        if std::fs::read_to_string(&f).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&f, &text)?;
        }
        written.push(f);
    }
    Ok(written)
}

pub fn run() -> Result<()> {
    let ext = write_extension()?;
    let hosts = register_native_host()?;
    println!("Native messaging host registered for:");
    for h in &hosts {
        println!("  {}", h.display());
    }
    if hosts.is_empty() {
        println!("  (no Chromium-family browser config found under ~/.config)");
    }
    println!();
    println!("Load the extension once in Brave:");
    println!("  1. Open brave://extensions and switch on Developer mode (top right).");
    println!("  2. Click \"Load unpacked\" and choose: {}", ext.display());
    println!("  3. Its ID must be {EXTENSION_ID}.");
    println!(
        "Optional: \"Allow in Incognito\" only lets arch-tracker count private time; it never"
    );
    println!("records hosts or titles from incognito windows.");
    Ok(())
}
