//! One-time setup: unpacks the browser extension and registers the native-messaging host with
//! every Chromium-family browser found. Safe to re-run; the daemon also re-registers on start.

use crate::util;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

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

/// $XDG_CONFIG_HOME, default ~/.config.
fn config_home() -> PathBuf {
    let dir = util::config_dir();
    dir.parent().map(Path::to_path_buf).unwrap_or(dir)
}

/// Chromium-family user-data dirs (relative to the config dir) and their names. These are
/// trusted even when the generic test in `is_chromium_dir` fails (e.g. Opera's layout, or a
/// browser always started with --no-first-run).
const CHROMIUM_KNOWN: &[(&str, &str)] = &[
    ("google-chrome", "Google Chrome"),
    ("google-chrome-beta", "Google Chrome Beta"),
    ("google-chrome-unstable", "Google Chrome Dev"),
    ("google-chrome-for-testing", "Chrome for Testing"),
    ("chromium", "Chromium"),
    ("net.imput.helium", "Helium"),
    ("vivaldi", "Vivaldi"),
    ("vivaldi-snapshot", "Vivaldi Snapshot"),
    ("microsoft-edge", "Microsoft Edge"),
    ("microsoft-edge-beta", "Microsoft Edge Beta"),
    ("microsoft-edge-dev", "Microsoft Edge Dev"),
    ("opera", "Opera"),
    ("opera-beta", "Opera Beta"),
    ("opera-developer", "Opera Developer"),
    ("thorium", "Thorium"),
    ("yandex-browser", "Yandex Browser"),
    ("yandex-browser-beta", "Yandex Browser Beta"),
    ("cromite", "Cromite"),
];

/// A Chromium-family browser's user-data dir; its NativeMessagingHosts folder is per user.
#[derive(Debug, Clone, PartialEq)]
pub struct Chromium {
    pub name: String,
    pub dir: PathBuf,
}

impl Chromium {
    pub fn manifest_path(&self) -> PathBuf {
        self.dir
            .join("NativeMessagingHosts")
            .join(format!("{NATIVE_HOST}.json"))
    }
}

fn known_chromium_name(rel: &Path) -> Option<String> {
    let rel = rel.to_str()?;
    if let Some(brave) = rel.strip_prefix("BraveSoftware/") {
        // Brave-Browser, Brave-Browser-Beta, Brave-Origin-Beta, ...
        return Some(brave.replace("-Browser", "").replace('-', " "));
    }
    CHROMIUM_KNOWN
        .iter()
        .find(|(dir, _)| *dir == rel)
        .map(|(_, name)| name.to_string())
}

/// A browser's user-data dir has "Local State". Electron apps (Discord, VS Code, Slack, ...) have
/// one too, but keep "Preferences" at the top instead of in a profile dir ("Default",
/// "Profile 1"); apps that embed a whole Chromium for automation run it with --no-first-run, so
/// they lack the "First Run" marker a browser leaves.
fn is_chromium_dir(cfg: &Path, dir: &Path) -> bool {
    if !dir.join("Local State").is_file() {
        return false;
    }
    let known = dir
        .strip_prefix(cfg)
        .ok()
        .and_then(known_chromium_name)
        .is_some();
    known || (dir.join("First Run").exists() && has_profile(dir))
}

fn has_profile(dir: &Path) -> bool {
    dir.join("Default/Preferences").is_file()
        || sorted_subdirs(dir).iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("Profile "))
                && p.join("Preferences").is_file()
        })
}

fn sorted_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Chromium-family browsers with a user-data dir directly in the config dir or one level below
/// (BraveSoftware/Brave-Browser).
pub fn chromium_browsers() -> Vec<Chromium> {
    chromium_browsers_in(&config_home())
}

fn chromium_browsers_in(cfg: &Path) -> Vec<Chromium> {
    let mut out = vec![];
    let mut add = |dir: PathBuf| {
        let rel = dir.strip_prefix(cfg).unwrap_or(&dir).to_path_buf();
        let name = known_chromium_name(&rel).unwrap_or_else(|| rel.display().to_string());
        out.push(Chromium { name, dir });
    };
    for top in sorted_subdirs(cfg) {
        if is_chromium_dir(cfg, &top) {
            add(top);
            continue;
        }
        for sub in sorted_subdirs(&top) {
            if is_chromium_dir(cfg, &sub) {
                add(sub);
            }
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

/// This binary; "/usr/bin/arch-tracker (deleted)" after a package upgrade replaced it.
fn own_exe() -> Result<PathBuf> {
    let exe = std::fs::read_link("/proc/self/exe").context("finding own executable")?;
    let s = exe.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(rest) => PathBuf::from(rest),
        None => exe,
    })
}

fn chromium_manifest(exe: &Path) -> String {
    let manifest = serde_json::json!({
        "name": NATIVE_HOST,
        "description": "arch-tracker: receives active-tab host and title from the arch-tracker extension",
        "path": exe,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    });
    serde_json::to_string_pretty(&manifest).unwrap_or_default() + "\n"
}

/// What a registration wrote (browser, manifest file) and what it could not write.
#[derive(Debug, Default)]
pub struct Registration {
    pub written: Vec<(String, PathBuf)>,
    pub failed: Vec<(PathBuf, String)>,
}

impl Registration {
    fn write(&mut self, who: &str, file: PathBuf, text: &str) {
        let res = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| {
                if std::fs::read_to_string(&file).ok().as_deref() == Some(text) {
                    Ok(())
                } else {
                    std::fs::write(&file, text)
                }
            });
        match res {
            Ok(()) => self.written.push((who.to_string(), file)),
            // One unwritable browser dir must not stop the others.
            Err(e) => self.failed.push((file, e.to_string())),
        }
    }
}

fn register(exe: &Path) -> Registration {
    let mut reg = Registration::default();
    let text = chromium_manifest(exe);
    for b in chromium_browsers() {
        reg.write(&b.name, b.manifest_path(), &text);
    }
    reg
}

/// Registers this binary as the native host. Returns the manifest paths written.
pub fn register_native_host() -> Result<Vec<PathBuf>> {
    let reg = register(&own_exe()?);
    for (file, e) in &reg.failed {
        tracing::warn!("could not write {}: {e}", file.display());
    }
    Ok(reg.written.into_iter().map(|(_, f)| f).collect())
}

pub fn run() -> Result<()> {
    let ext = write_extension()?;
    let reg = register(&own_exe()?);
    println!("Native messaging host registered for:");
    for (who, file) in &reg.written {
        println!("  {who:<20} {}", util::abbreviate_home(file));
    }
    if reg.written.is_empty() {
        println!(
            "  (no Chromium-family browser found in {}; start your browser once, then run setup again)",
            util::abbreviate_home(&config_home())
        );
    }
    for (file, e) in &reg.failed {
        println!("  could not write {}: {e}", file.display());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "{}").unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("arch-tracker-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_chromium_browsers_not_electron_apps() {
        let cfg = scratch("chromium");
        // Browsers: profile dirs, "First Run".
        for b in [
            "BraveSoftware/Brave-Origin-Beta",
            "net.imput.helium",
            "SomeNewBrowser",
        ] {
            touch(&cfg.join(b).join("Local State"));
            touch(&cfg.join(b).join("First Run"));
            touch(&cfg.join(b).join("Default/Preferences"));
        }
        touch(&cfg.join("Other/Browser/Local State"));
        touch(&cfg.join("Other/Browser/First Run"));
        touch(&cfg.join("Other/Browser/Profile 2/Preferences"));
        // Known name without "First Run" (always started with --no-first-run).
        touch(&cfg.join("chromium/Local State"));
        touch(&cfg.join("chromium/Default/Preferences"));
        // Electron app: "Local State" and "Preferences" at the top, no profile dir.
        touch(&cfg.join("SomeChatApp/Local State"));
        touch(&cfg.join("SomeChatApp/Preferences"));
        // Embedded automation Chromium: a profile but no "First Run".
        touch(&cfg.join("SomeAgentApp/Local State"));
        touch(&cfg.join("SomeAgentApp/Default/Preferences"));
        // Leftover from an uninstalled browser: only our old registration.
        touch(&cfg.join("BraveSoftware/Brave-Browser/NativeMessagingHosts/x.json"));

        let found: Vec<(String, PathBuf)> = chromium_browsers_in(&cfg)
            .into_iter()
            .map(|b| (b.name, b.dir.strip_prefix(&cfg).unwrap().to_path_buf()))
            .collect();
        let want = [
            ("Brave Origin Beta", "BraveSoftware/Brave-Origin-Beta"),
            ("Other/Browser", "Other/Browser"),
            ("SomeNewBrowser", "SomeNewBrowser"),
            ("Chromium", "chromium"),
            ("Helium", "net.imput.helium"),
        ];
        let want: Vec<(String, PathBuf)> = want
            .iter()
            .map(|(n, d)| (n.to_string(), PathBuf::from(d)))
            .collect();
        assert_eq!(found, want);
        let _ = std::fs::remove_dir_all(&cfg);
    }

    #[test]
    fn keeps_going_after_a_failed_write() {
        let dir = scratch("write");
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "a file where a directory should be").unwrap();
        let mut reg = Registration::default();
        reg.write("A", blocked.join("NativeMessagingHosts/h.json"), "x");
        reg.write("B", dir.join("ok/NativeMessagingHosts/h.json"), "x");
        assert_eq!(reg.failed.len(), 1);
        assert_eq!(reg.written.len(), 1);
        assert_eq!(reg.written[0].0, "B");
        assert_eq!(std::fs::read_to_string(&reg.written[0].1).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chromium_manifest_allows_our_extension() {
        let v: serde_json::Value =
            serde_json::from_str(&chromium_manifest(Path::new("/usr/bin/arch-tracker"))).unwrap();
        assert_eq!(v["name"], NATIVE_HOST);
        assert_eq!(v["path"], "/usr/bin/arch-tracker");
        assert_eq!(v["type"], "stdio");
        assert_eq!(
            v["allowed_origins"][0],
            "chrome-extension://ofgofljkhfmjkgklekeiajnkplaldokg/"
        );
    }
}
