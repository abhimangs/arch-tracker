//! Index of .desktop files: turns window classes / desktop ids / systemd app units into app names.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub wm_class: String,
    /// Categories= lists TerminalEmulator / WebBrowser.
    pub terminal: bool,
    pub browser: bool,
    /// The program Exec= starts (its first word), without the arguments.
    pub exec: String,
}

#[derive(Default)]
pub struct DesktopIndex {
    by_id: HashMap<String, Arc<Entry>>,
    by_wmclass: HashMap<String, Arc<Entry>>,
}

fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![];
    match std::env::var("XDG_DATA_HOME") {
        Ok(v) if !v.is_empty() => dirs.push(PathBuf::from(v)),
        _ => dirs.push(crate::util::home().join(".local/share")),
    }
    let sys = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    dirs
}

impl DesktopIndex {
    pub fn load() -> DesktopIndex {
        let mut idx = DesktopIndex::default();
        for d in data_dirs() {
            idx.scan(&d.join("applications"), &d.join("applications"));
        }
        // Autostart entries (scripts like copyq-start.sh) only fill gaps.
        idx.scan(
            &crate::util::home().join(".config/autostart"),
            &crate::util::home().join(".config/autostart"),
        );
        idx.scan(
            Path::new("/etc/xdg/autostart"),
            Path::new("/etc/xdg/autostart"),
        );
        idx
    }

    fn scan(&mut self, dir: &Path, root: &Path) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                self.scan(&p, root);
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(rel) = p.strip_prefix(root) else {
                continue;
            };
            let id = rel.with_extension("").to_string_lossy().replace('/', "-");
            if self.by_id.contains_key(&id.to_lowercase()) {
                continue; // earlier dirs win (user overrides system)
            }
            if let Some(e) = parse_entry(&p, &id) {
                let e = Arc::new(e);
                self.by_id.insert(id.to_lowercase(), e.clone());
                if !e.wm_class.is_empty() {
                    self.by_wmclass
                        .entry(e.wm_class.to_lowercase())
                        .or_insert(e);
                }
            }
        }
    }

    pub fn by_id(&self, id: &str) -> Option<&Entry> {
        let id = id.trim_end_matches(".desktop").to_lowercase();
        self.by_id.get(&id).map(|e| e.as_ref())
    }

    pub fn by_wmclass(&self, cls: &str) -> Option<&Entry> {
        self.by_wmclass.get(&cls.to_lowercase()).map(|e| e.as_ref())
    }

    /// (terminal, browser) from the categories of the desktop entry with this id or class.
    pub fn kind_hint(&self, app_id: &str) -> Option<(bool, bool)> {
        let e = self.by_id(app_id).or_else(|| self.by_wmclass(app_id))?;
        Some((e.terminal, e.browser))
    }

    /// The app a running executable belongs to: the one whose Exec= program is the same file
    /// once links are followed, or a launcher in the same install folder
    /// (/usr/bin/brave-origin-beta links to a script next to /opt/brave.com/brave-origin-beta/brave).
    /// Web browsers win when several match; installed web apps (which start the browser
    /// binary themselves) never do.
    pub fn by_exe(&self, exe: &Path) -> Option<&Entry> {
        let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
        let dir = exe.parent()?;
        // In a shared folder (/usr/bin) only the same file says anything.
        let shared = dir.ends_with("bin") || dir.ends_with("sbin");
        let mut best: Option<(u8, &Entry)> = None;
        for e in self.by_id.values() {
            if is_web_app(&e.id, &e.wm_class) {
                continue;
            }
            let Some(prog) = resolve_program(&e.exec) else {
                continue;
            };
            let exact = prog == exe;
            if !exact && (shared || prog.parent() != Some(dir)) {
                continue;
            }
            let score = u8::from(e.browser) * 2 + u8::from(exact);
            let better = best.is_none_or(|(s, b)| score > s || (score == s && e.id < b.id));
            if better {
                best = Some((score, e));
            }
        }
        best.map(|(_, e)| e)
    }

    /// (app key, display name) for a window.
    pub fn identify(&self, df: &str, cls: &str, pid: i64) -> (String, String) {
        let df = df.trim_end_matches(".desktop");
        if !df.is_empty()
            && let Some(e) = self.by_id(df).or_else(|| self.by_wmclass(df))
        {
            return (e.id.clone(), e.name.clone());
        }
        if !cls.is_empty()
            && let Some(e) = self.by_wmclass(cls).or_else(|| self.by_id(cls))
        {
            return (e.id.clone(), e.name.clone());
        }
        if !df.is_empty() {
            return (df.to_string(), pretty(df));
        }
        if !cls.is_empty() {
            return (cls.to_string(), pretty(cls));
        }
        if pid > 0
            && let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        {
            let c = comm.trim().to_string();
            return (c.clone(), c);
        }
        ("unknown".into(), "Unknown".into())
    }
}

/// An installed web app's entry: Chromium names them after the app's 32-letter id and gives
/// their windows the class crx_<id>.
pub fn is_web_app(id: &str, wm_class: &str) -> bool {
    wm_class.to_lowercase().starts_with("crx_")
        || id
            .split(['-', '_'])
            .any(|w| w.len() == 32 && w.bytes().all(|b| (b'a'..=b'p').contains(&b)))
}

/// The program an Exec= line starts: its first word, past "env" and VAR=value settings.
fn exec_program(exec: &str) -> String {
    let mut words = exec.split_whitespace().map(|w| w.trim_matches(['"', '\'']));
    let mut first = words.next().unwrap_or("");
    if first == "env" || first.ends_with("/env") {
        first = words
            .find(|w| !w.contains('=') && !w.starts_with('-'))
            .unwrap_or("");
    }
    first.to_string()
}

/// Where an Exec= program lives, links followed: as written when it is a path, else found
/// on PATH.
fn resolve_program(prog: &str) -> Option<PathBuf> {
    if prog.is_empty() {
        return None;
    }
    let path = if prog.contains('/') {
        PathBuf::from(prog)
    } else {
        let dirs = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
        dirs.split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join(prog))
            .find(|p| p.is_file())?
    };
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

/// "com.notion.app.desktop.notion" -> "Notion", "brave-origin-beta" -> "Brave Origin Beta"
pub fn pretty(id: &str) -> String {
    let last = if id.contains('.') {
        id.rsplit('.').next().unwrap_or(id)
    } else {
        id
    };
    last.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_entry(path: &Path, id: &str) -> Option<Entry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_main = false;
    let (mut name, mut wm) = (String::new(), String::new());
    let mut exec = String::new();
    let (mut terminal, mut browser) = (false, false);
    let mut is_app = true;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "Name" => name = v.trim().to_string(),
            "StartupWMClass" => wm = v.trim().to_string(),
            "Exec" => exec = exec_program(v),
            "Type" => is_app = v.trim() == "Application",
            "Categories" => {
                for c in v.split(';') {
                    match c.trim() {
                        "TerminalEmulator" => terminal = true,
                        "WebBrowser" => browser = true,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if !is_app || name.is_empty() {
        return None;
    }
    Some(Entry {
        id: id.to_string(),
        name,
        wm_class: wm,
        terminal,
        browser,
        exec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_names() {
        assert_eq!(pretty("com.notion.app.desktop.notion"), "Notion");
        assert_eq!(pretty("brave-origin-beta"), "Brave Origin Beta");
        assert_eq!(pretty("kitty"), "Kitty");
    }

    #[test]
    fn reads_categories() {
        let path = std::env::temp_dir().join(format!(
            "at-desktop-{}-{}.desktop",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            "[Desktop Entry]\nName=Term\nCategories=System;TerminalEmulator;\n",
        )
        .unwrap();
        let e = parse_entry(&path, "term").unwrap();
        std::fs::remove_file(&path).ok();
        assert!(e.terminal && !e.browser);
    }

    fn entry(id: &str, exec: &str, browser: bool) -> Entry {
        Entry {
            id: id.into(),
            name: id.into(),
            wm_class: String::new(),
            terminal: false,
            browser,
            exec: exec.into(),
        }
    }

    #[test]
    fn exec_programs() {
        assert_eq!(
            exec_program("/usr/bin/brave-origin-beta %U"),
            "/usr/bin/brave-origin-beta"
        );
        assert_eq!(
            exec_program("env GDK_BACKEND=x11 \"/opt/a b\" --x"),
            "/opt/a"
        );
        assert_eq!(
            exec_program("/usr/bin/env FOO=1 helium-browser %U"),
            "helium-browser"
        );
        assert_eq!(exec_program(""), "");
    }

    #[test]
    fn executables_find_their_app() {
        let dir = std::env::temp_dir().join(format!("at-exe-{}", std::process::id()));
        let opt = dir.join("opt/browser-x");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&opt).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for f in [opt.join("launcher"), opt.join("engine"), bin.join("tool")] {
            std::fs::write(&f, "").unwrap();
        }
        let launcher = bin.join("browser-x");
        let _ = std::fs::remove_file(&launcher);
        std::os::unix::fs::symlink(opt.join("launcher"), &launcher).unwrap();
        let p = |x: &Path| x.to_string_lossy().into_owned();
        let mut idx = DesktopIndex::default();
        for e in [
            entry("browser-x", &p(&launcher), true),
            // An installed web app starts the engine itself; it must not win.
            entry(
                "browser-x-abcdefghijklmnopabcdefghijklmnop-Default",
                &p(&opt.join("engine")),
                false,
            ),
            entry("tool", &p(&bin.join("tool")), false),
        ] {
            idx.by_id.insert(e.id.to_lowercase(), Arc::new(e));
        }
        assert_eq!(
            idx.by_exe(&opt.join("engine")).map(|e| e.id.as_str()),
            Some("browser-x")
        );
        assert_eq!(
            idx.by_exe(&bin.join("tool")).map(|e| e.id.as_str()),
            Some("tool")
        );
        std::fs::write(bin.join("other"), "").unwrap();
        assert!(
            idx.by_exe(&bin.join("other")).is_none(),
            "a shared bin folder is no clue"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn web_app_entries() {
        assert!(is_web_app(
            "brave-abcdefghijklmnopabcdefghijklmnop-Default",
            ""
        ));
        assert!(is_web_app("x", "crx_abc"));
        assert!(!is_web_app("brave-origin-beta", "brave-origin-beta"));
    }

    #[test]
    fn identifies_real_apps() {
        let idx = DesktopIndex::load();
        // kitty is installed on the dev machine; skip quietly elsewhere.
        if idx.by_id("kitty").is_some() {
            assert_eq!(idx.identify("kitty", "kitty", 0).0, "kitty");
        }
    }
}
