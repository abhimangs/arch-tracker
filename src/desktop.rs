//! Index of .desktop files: turns window classes / desktop ids / systemd app units into app names.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub wm_class: String,
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
    let (mut name, mut wm, mut exec) = (String::new(), String::new(), String::new());
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
            "Exec" => exec = v.trim().to_string(),
            "Type" => is_app = v.trim() == "Application",
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
    fn identifies_real_apps() {
        let idx = DesktopIndex::load();
        // kitty is installed on the dev machine; skip quietly elsewhere.
        if idx.by_id("kitty").is_some() {
            assert_eq!(idx.identify("kitty", "kitty", 0).0, "kitty");
        }
    }
}
