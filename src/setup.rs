//! One-time setup: unpacks the browser extension (a Chromium build and a Firefox build) and
//! registers the native-messaging host with every Chromium- and Firefox-family browser found.
//! Safe to re-run; the daemon also re-registers on start.

use crate::util;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const EXTENSION_ID: &str = "ofgofljkhfmjkgklekeiajnkplaldokg";
pub const NATIVE_HOST: &str = "dev.archtracker.native";
/// The Firefox add-on ID (browser_specific_settings.gecko.id in extension/manifest.firefox.json).
pub const GECKO_ID: &str = "arch-tracker@abhimangs.github.io";
const MANIFEST: &str = include_str!("../extension/manifest.json");
const FIREFOX_MANIFEST: &str = include_str!("../extension/manifest.firefox.json");
const BACKGROUND: &str = include_str!("../extension/background.js");
/// The extension's icon: web-ui/icon.svg at 128 px (rsvg-convert -w 128 -h 128).
const ICON: &[u8] = include_bytes!("../extension/icon.png");
/// Where the package installs both unpacked builds (see packaging/PKGBUILD).
const PACKAGED: &str = "/usr/share/arch-tracker";
pub const RELEASES: &str = "https://github.com/abhimangs/arch-tracker/releases";

/// The unpacked Chromium build: the packaged copy (updated by pacman) if installed, else a
/// per-user copy.
pub fn extension_dir() -> PathBuf {
    packaged_or_user("extension")
}

/// The unpacked Firefox build, laid out like `extension_dir`.
pub fn firefox_extension_dir() -> PathBuf {
    packaged_or_user("extension-firefox")
}

/// The unsigned Firefox add-on, for browsers that accept one (xpinstall.signatures.required).
pub fn firefox_xpi() -> PathBuf {
    util::data_dir().join("arch-tracker-firefox-unsigned.xpi")
}

fn packaged_or_user(name: &str) -> PathBuf {
    let packaged = Path::new(PACKAGED).join(name);
    if packaged.join("manifest.json").exists() {
        return packaged;
    }
    util::data_dir().join(name)
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

/// A Firefox-family browser and the per-user dirs it reads native-messaging manifests from.
#[derive(Debug, Clone, PartialEq)]
pub struct Firefox {
    pub name: &'static str,
    pub hosts_dirs: Vec<PathBuf>,
}

impl Firefox {
    pub fn manifest_paths(&self) -> Vec<PathBuf> {
        self.hosts_dirs
            .iter()
            .map(|d| d.join(format!("{NATIVE_HOST}.json")))
            .collect()
    }
}

struct FirefoxKind {
    name: &'static str,
    /// Profile roots relative to $HOME and to the config dir; any of them means it is used.
    home: &'static [&'static str],
    xdg: &'static [&'static str],
    /// Commands on PATH that mean it is installed.
    bins: &'static [&'static str],
    /// Its own folder in $HOME: forks may read native manifests there as well as in ~/.mozilla.
    /// Only written when it exists already (creating it could change where the fork keeps
    /// its profiles).
    own: Option<&'static str>,
}

const FIREFOX_KINDS: &[FirefoxKind] = &[
    FirefoxKind {
        name: "Firefox",
        home: &[".mozilla/firefox"],
        xdg: &["mozilla/firefox"],
        bins: &[
            "firefox",
            "firefox-developer-edition",
            "firefox-nightly",
            "firefox-beta",
            "firefox-esr",
        ],
        own: None,
    },
    FirefoxKind {
        name: "Zen",
        home: &[".zen"],
        xdg: &["zen"],
        bins: &["zen-browser", "zen-twilight"],
        own: Some(".zen"),
    },
    FirefoxKind {
        name: "LibreWolf",
        home: &[".librewolf"],
        xdg: &["librewolf"],
        bins: &["librewolf"],
        own: Some(".librewolf"),
    },
    FirefoxKind {
        name: "Floorp",
        home: &[".floorp"],
        xdg: &["floorp"],
        bins: &["floorp"],
        own: Some(".floorp"),
    },
    FirefoxKind {
        name: "Waterfox",
        home: &[".waterfox"],
        xdg: &["waterfox"],
        bins: &["waterfox"],
        own: Some(".waterfox"),
    },
];

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Firefox-family browsers that are installed (on PATH) or have profiles.
pub fn firefox_browsers() -> Vec<Firefox> {
    firefox_browsers_in(&util::home(), &config_home(), &path_dirs())
}

fn firefox_browsers_in(home: &Path, cfg: &Path, path: &[PathBuf]) -> Vec<Firefox> {
    let mut out = vec![];
    for k in FIREFOX_KINDS {
        let used = k.home.iter().any(|d| home.join(d).is_dir())
            || k.xdg.iter().any(|d| cfg.join(d).is_dir())
            || k.bins
                .iter()
                .any(|b| path.iter().any(|p| p.join(b).is_file()));
        if !used {
            continue;
        }
        let mut dirs = vec![];
        if let Some(own) = k.own.map(|d| home.join(d)).filter(|d| d.is_dir()) {
            dirs.push(own.join("native-messaging-hosts"));
        }
        // Every Firefox-family browser reads this one, even with its profiles in the XDG
        // config dir (Firefox 147+), so it is created when missing.
        dirs.push(home.join(".mozilla/native-messaging-hosts"));
        if k.name == "Firefox" && cfg.join("mozilla").is_dir() {
            // Proposed XDG location (Mozilla bug 2005167); harmless until Firefox reads it.
            dirs.push(cfg.join("mozilla/native-messaging-hosts"));
        }
        out.push(Firefox {
            name: k.name,
            hosts_dirs: dirs,
        });
    }
    out
}

/// A browser installed as a Flatpak or Snap.
#[derive(Debug, Clone, PartialEq)]
pub struct Sandboxed {
    pub name: &'static str,
    pub how: String,
}

const FLATPAK_BROWSERS: &[(&str, &str)] = &[
    ("com.brave.Browser", "Brave"),
    ("com.google.Chrome", "Google Chrome"),
    ("com.google.ChromeDev", "Google Chrome Dev"),
    ("org.chromium.Chromium", "Chromium"),
    (
        "io.github.ungoogled_software.ungoogled_chromium",
        "ungoogled-chromium",
    ),
    ("com.vivaldi.Vivaldi", "Vivaldi"),
    ("com.microsoft.Edge", "Microsoft Edge"),
    ("com.opera.Opera", "Opera"),
    ("ru.yandex.Browser", "Yandex Browser"),
    ("net.imput.helium", "Helium"),
    ("org.mozilla.firefox", "Firefox"),
    ("io.gitlab.librewolf-community", "LibreWolf"),
    ("app.zen_browser.zen", "Zen"),
    ("one.ablaze.floorp", "Floorp"),
    ("net.waterfox.waterfox", "Waterfox"),
];

const SNAP_BROWSERS: &[(&str, &str)] = &[
    ("firefox", "Firefox"),
    ("chromium", "Chromium"),
    ("brave", "Brave"),
    ("vivaldi", "Vivaldi"),
    ("opera", "Opera"),
    ("opera-beta", "Opera Beta"),
    ("floorp", "Floorp"),
];

/// Flatpak and Snap browsers. Their sandbox stops them from starting arch-tracker's native
/// host, so the extension can't reach the daemon from them.
pub fn sandboxed_browsers() -> Vec<Sandboxed> {
    let user_flatpak = util::data_dir()
        .parent()
        .map(|d| d.join("flatpak/app"))
        .unwrap_or_default();
    let mut out = vec![];
    for (id, name) in FLATPAK_BROWSERS {
        if [Path::new("/var/lib/flatpak/app"), &user_flatpak]
            .iter()
            .any(|d| d.join(id).is_dir())
        {
            out.push(Sandboxed {
                name,
                how: format!("Flatpak {id}"),
            });
        }
    }
    for (snap, name) in SNAP_BROWSERS {
        if Path::new("/snap").join(snap).join("current").exists() {
            out.push(Sandboxed {
                name,
                how: format!("Snap {snap}"),
            });
        }
    }
    out
}

/// Where `write_extension` put (or found) the two builds.
pub struct Extensions {
    pub chromium: PathBuf,
    pub firefox: PathBuf,
    pub firefox_xpi: PathBuf,
}

/// Unpacks both builds (unless the package installed them) and writes the unsigned .xpi.
pub fn write_extension() -> Result<Extensions> {
    let chromium = extension_dir();
    if !chromium.starts_with(PACKAGED) {
        write_files(
            &chromium,
            &[
                ("manifest.json", MANIFEST.as_bytes()),
                ("background.js", BACKGROUND.as_bytes()),
                ("icon.png", ICON),
            ],
        )?;
    }
    let firefox = firefox_extension_dir();
    if !firefox.starts_with(PACKAGED) {
        write_files(
            &firefox,
            &[
                ("manifest.json", FIREFOX_MANIFEST.as_bytes()),
                ("background.js", BACKGROUND.as_bytes()),
                ("icon.png", ICON),
            ],
        )?;
    }
    let firefox_xpi = firefox_xpi();
    let xpi = zip_store(&[
        ("manifest.json", FIREFOX_MANIFEST.as_bytes()),
        ("background.js", BACKGROUND.as_bytes()),
        ("icon.png", ICON),
    ]);
    if let Some(d) = firefox_xpi.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&firefox_xpi, xpi)
        .with_context(|| format!("writing {}", firefox_xpi.display()))?;
    Ok(Extensions {
        chromium,
        firefox,
        firefox_xpi,
    })
}

fn write_files(dir: &Path, files: &[(&str, &[u8])]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, text) in files {
        std::fs::write(dir.join(name), text)
            .with_context(|| format!("writing {}", dir.join(name).display()))?;
    }
    Ok(())
}

/// A zip archive (an .xpi) of `files`, stored uncompressed: a few small files.
fn zip_store(files: &[(&str, &[u8])]) -> Vec<u8> {
    const DOS_DATE: u16 = (1 << 5) | 1; // 1980-01-01, midnight: no build-time noise
    let mut out = vec![];
    let mut central = vec![];
    for (name, data) in files {
        let offset = out.len() as u32;
        let (crc, size, name_len) = (crc32(data), data.len() as u32, name.len() as u16);
        // Fields shared by the local header and the central directory entry, from "version
        // needed" (2.0) to the extra-field length (0).
        let mut common = vec![];
        common.extend_from_slice(&20u16.to_le_bytes());
        common.extend_from_slice(&0u16.to_le_bytes()); // flags
        common.extend_from_slice(&0u16.to_le_bytes()); // stored
        common.extend_from_slice(&0u16.to_le_bytes()); // time
        common.extend_from_slice(&DOS_DATE.to_le_bytes());
        common.extend_from_slice(&crc.to_le_bytes());
        common.extend_from_slice(&size.to_le_bytes());
        common.extend_from_slice(&size.to_le_bytes());
        common.extend_from_slice(&name_len.to_le_bytes());
        common.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&common);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&((3u16 << 8) | 20).to_le_bytes()); // made by: Unix, 2.0
        central.extend_from_slice(&common);
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&(0o100644u32 << 16).to_le_bytes()); // -rw-r--r--
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let (cd_offset, cd_size, n) = (out.len() as u32, central.len() as u32, files.len() as u16);
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
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

/// Firefox's format: the add-on ID instead of origins.
fn firefox_manifest(exe: &Path) -> String {
    let manifest = serde_json::json!({
        "name": NATIVE_HOST,
        "description": "arch-tracker: receives active-tab host and title from the arch-tracker extension",
        "path": exe,
        "type": "stdio",
        "allowed_extensions": [GECKO_ID],
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
    // Firefox-family browsers share ~/.mozilla/native-messaging-hosts: write each file once.
    let text = firefox_manifest(exe);
    let mut done: Vec<PathBuf> = vec![];
    for b in firefox_browsers() {
        for file in b.manifest_paths() {
            if !done.contains(&file) {
                reg.write(b.name, file.clone(), &text);
                done.push(file);
            }
        }
    }
    reg
}

/// Installed binaries: packaged (/usr, /opt) or `cargo install`ed / copied into a bin dir.
/// Anything else, like target/debug during `cargo run -- daemon`, is a development build.
fn is_installed(exe: &Path, home: &Path) -> bool {
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .map(|c| PathBuf::from(c).join("bin"))
        .unwrap_or_else(|| home.join(".cargo/bin"));
    [
        Path::new("/usr"),
        Path::new("/opt"),
        &cargo_bin,
        &home.join(".cargo/bin"),
        &home.join(".local/bin"),
    ]
    .iter()
    .any(|d| exe.starts_with(d))
}

/// Called by the daemon on start: registers this binary with every browser found, but only an
/// installed one, so a development run doesn't point real browsers at target/debug.
/// `arch-tracker setup` always registers the binary it runs from. Returns the manifests written.
pub fn register_native_host() -> Result<Vec<PathBuf>> {
    let exe = own_exe()?;
    if !is_installed(&exe, &util::home()) {
        tracing::info!(
            "not registering {} as the browser bridge: not an installed binary (run \"{} setup\" to use it anyway)",
            exe.display(),
            exe.display()
        );
        return Ok(vec![]);
    }
    let reg = register(&exe);
    for (file, e) in &reg.failed {
        tracing::warn!("could not write {}: {e}", file.display());
    }
    Ok(reg.written.into_iter().map(|(_, f)| f).collect())
}

pub fn run() -> Result<()> {
    let exe = own_exe()?;
    let ext = write_extension()?;
    let reg = register(&exe);
    println!(
        "Browser bridge: native messaging host {NATIVE_HOST} -> {}",
        exe.display()
    );
    for (who, file) in &reg.written {
        println!("  {who:<20} {}", util::abbreviate_home(file));
    }
    if reg.written.is_empty() {
        println!(
            "  (no browser found in {}; start your browser once, then run setup again)",
            util::abbreviate_home(&config_home())
        );
    }
    for (file, e) in &reg.failed {
        println!("  could not write {}: {e}", file.display());
    }
    if !is_installed(&exe, &util::home()) {
        println!(
            "  (a development build: the installed daemon points browsers back at itself when it starts)"
        );
    }
    let sandboxed = sandboxed_browsers();
    if !sandboxed.is_empty() {
        println!();
        for b in &sandboxed {
            println!(
                "  {} ({}) is sandboxed: it can't start arch-tracker's native host,",
                b.name, b.how
            );
            println!(
                "  so the extension can't connect there. Install it from pacman or the AUR instead."
            );
        }
    }
    if crate::sources::wm::detect() == crate::sources::wm::Wm::Gnome {
        println!();
        match crate::sources::gnome::install() {
            Ok(dir) => {
                println!(
                    "GNOME: the arch-tracker Shell extension is on ({}).",
                    util::abbreviate_home(&dir)
                );
                println!(
                    "  GNOME on Wayland loads new extensions at login: log out and back in once."
                );
            }
            Err(e) => println!("GNOME: could not switch the Shell extension on: {e:#}"),
        }
    }
    let chromium = !chromium_browsers().is_empty();
    let firefox = !firefox_browsers().is_empty();
    println!();
    println!("Then load the extension once in each browser.");
    if chromium || !firefox {
        println!();
        print_chromium_steps(&ext.chromium);
    }
    if firefox || !chromium {
        println!();
        print_firefox_steps(&ext);
    }
    Ok(())
}

fn print_chromium_steps(dir: &Path) {
    println!("Chromium family (Brave, Chrome, Chromium, Helium, Vivaldi, Edge, Opera, ...):");
    println!("  1. Open chrome://extensions (brave://, vivaldi://, edge:// or opera://extensions)");
    println!("     and switch on Developer mode.");
    println!("  2. Click \"Load unpacked\" and choose: {}", dir.display());
    println!("  3. Its ID must be {EXTENSION_ID}.");
    println!(
        "  Optional: \"Allow in Incognito\" only lets arch-tracker count private time; it never"
    );
    println!("  records hosts, titles or audio from incognito windows.");
}

fn print_firefox_steps(ext: &Extensions) {
    println!("Firefox family (Firefox, Zen, LibreWolf, Floorp, Waterfox):");
    println!("  Firefox and Zen only keep add-ons signed by Mozilla: install the signed");
    println!("  arch-tracker-firefox-<version>.xpi from {RELEASES}");
    println!("  (open it in the browser, or about:addons > gear menu > Install Add-on From File).");
    println!("  To try it without signing: open about:debugging#/runtime/this-firefox, click");
    println!(
        "  \"Load Temporary Add-on\" and choose {}",
        ext.firefox.join("manifest.json").display()
    );
    println!("  (it is removed when the browser quits).");
    println!(
        "  LibreWolf, Floorp, Waterfox, Firefox Developer Edition, Nightly and ESR can keep the"
    );
    println!("  unsigned build: set xpinstall.signatures.required to false in about:config, then");
    println!("  install {}", util::abbreviate_home(&ext.firefox_xpi));
    println!(
        "  Optional: allowing it in private windows only lets arch-tracker count private time."
    );
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
    fn finds_firefox_family_browsers() {
        let root = scratch("firefox");
        let (home, cfg, bin) = (root.join("home"), root.join("config"), root.join("bin"));
        // Firefox with XDG profiles (147+), Zen by its folder, LibreWolf on PATH only.
        std::fs::create_dir_all(cfg.join("mozilla/firefox")).unwrap();
        std::fs::create_dir_all(home.join(".zen")).unwrap();
        touch(&bin.join("librewolf"));
        let found = firefox_browsers_in(&home, &cfg, &[bin]);
        let mozilla = home.join(".mozilla/native-messaging-hosts");
        assert_eq!(
            found,
            vec![
                Firefox {
                    name: "Firefox",
                    hosts_dirs: vec![mozilla.clone(), cfg.join("mozilla/native-messaging-hosts")],
                },
                Firefox {
                    name: "Zen",
                    hosts_dirs: vec![home.join(".zen/native-messaging-hosts"), mozilla.clone()],
                },
                // ~/.librewolf does not exist yet: not created, ~/.mozilla is enough.
                Firefox {
                    name: "LibreWolf",
                    hosts_dirs: vec![mozilla],
                },
            ]
        );
        assert!(firefox_browsers_in(&home.join("nobody"), &cfg.join("none"), &[]).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn firefox_manifest_allows_our_addon() {
        let v: serde_json::Value =
            serde_json::from_str(&firefox_manifest(Path::new("/usr/bin/arch-tracker"))).unwrap();
        assert_eq!(v["name"], NATIVE_HOST);
        assert_eq!(v["path"], "/usr/bin/arch-tracker");
        assert_eq!(v["allowed_extensions"][0], GECKO_ID);
        assert!(v.get("allowed_origins").is_none());
    }

    #[test]
    fn extension_builds_agree() {
        let chromium: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        let firefox: serde_json::Value = serde_json::from_str(FIREFOX_MANIFEST).unwrap();
        assert_eq!(chromium["version"], firefox["version"]);
        assert_eq!(
            firefox["browser_specific_settings"]["gecko"]["id"],
            GECKO_ID
        );
        assert_eq!(firefox["background"]["scripts"][0], "background.js");
        assert_eq!(firefox["background"]["persistent"], true);
        assert_eq!(chromium["background"]["service_worker"], "background.js");
        assert!(
            chromium["key"].is_string(),
            "the key fixes the extension ID"
        );
        assert!(firefox.get("key").is_none());
    }

    #[test]
    fn extension_icon_is_a_128px_png_both_manifests_use() {
        assert_eq!(&ICON[..8], b"\x89PNG\r\n\x1a\n");
        let be32 = |i: usize| u32::from_be_bytes(ICON[i..i + 4].try_into().unwrap());
        assert_eq!((be32(16), be32(20)), (128, 128));
        for m in [MANIFEST, FIREFOX_MANIFEST] {
            let v: serde_json::Value = serde_json::from_str(m).unwrap();
            assert_eq!(v["icons"]["128"], "icon.png");
        }
    }

    #[test]
    fn xpi_is_a_valid_zip() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let files: [(&str, &[u8]); 2] = [("manifest.json", b"{}"), ("background.js", b"let a;")];
        let zip = zip_store(&files);
        let eocd = zip.len() - 22;
        let u16_at = |i: usize| u16::from_le_bytes([zip[i], zip[i + 1]]) as usize;
        let u32_at = |i: usize| u32::from_le_bytes(zip[i..i + 4].try_into().unwrap()) as usize;
        assert_eq!(u32_at(eocd), 0x0605_4b50);
        assert_eq!(u16_at(eocd + 10), 2);
        let mut at = u32_at(eocd + 16);
        for (name, data) in files {
            assert_eq!(u32_at(at), 0x0201_4b50);
            assert_eq!(u32_at(at + 16), crc32(data) as usize);
            let n = u16_at(at + 28);
            assert_eq!(&zip[at + 46..at + 46 + n], name.as_bytes());
            let local = u32_at(at + 42);
            assert_eq!(u32_at(local), 0x0403_4b50);
            let start = local + 30 + u16_at(local + 26) + u16_at(local + 28);
            assert_eq!(&zip[start..start + data.len()], data);
            at += 46 + n;
        }
        assert_eq!(at, eocd);
    }

    #[test]
    fn only_installed_binaries_count() {
        let home = Path::new("/home/someone");
        for exe in [
            "/usr/bin/arch-tracker",
            "/opt/arch-tracker/arch-tracker",
            "/home/someone/.cargo/bin/arch-tracker",
            "/home/someone/.local/bin/arch-tracker",
        ] {
            assert!(is_installed(Path::new(exe), home), "{exe}");
        }
        for exe in [
            "/home/someone/src/arch-tracker/target/debug/arch-tracker",
            "/home/someone/src/arch-tracker/target/release/arch-tracker",
            "/tmp/arch-tracker",
        ] {
            assert!(!is_installed(Path::new(exe), home), "{exe}");
        }
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
