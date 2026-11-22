//! ~/.config/arch-tracker/config.toml: every timing and rule is editable here or in the dashboard.

use crate::util;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub idle: Idle,
    pub visibility: Visibility,
    pub agents: Agents,
    pub dashboard: Dashboard,
    pub privacy: Privacy,
    pub titles: Titles,
    pub categories: Vec<Category>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct General {
    /// How often running apps, terminals, agents and the mic are sampled.
    pub sample_interval_secs: u64,
    /// How often the database is written (a crash loses at most this much).
    pub flush_interval_secs: u64,
    /// Record apps that are running (windows, tray, background) as "running".
    pub track_running: bool,
    /// First day of the week for trends: "monday" or "sunday".
    pub week_start: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Idle {
    /// No input for this long = idle (the whole gap is removed, back to the last input).
    pub threshold_secs: u64,
    /// While the microphone is open (a call), allow this long without input.
    pub call_threshold_secs: u64,
    /// Media playing in a visible window keeps time counting as "passive".
    pub media_keeps_active: bool,
    /// Also treat compositor idle inhibitors (no media detected) as passive.
    pub count_inhibited_as_passive: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Visibility {
    /// A window counts as visible if at least this fraction of it is unoccluded...
    pub min_fraction: f64,
    /// ...and at least this many pixels are.
    pub min_pixels: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Agents {
    /// Agent process CPU usage (percent of one core) that counts as "working".
    pub busy_cpu_percent: f64,
    /// Bytes the agent writes to its terminal per sample that count as "working".
    pub busy_write_bytes: u64,
    /// CPU of the agent's child processes (builds, tests) that counts as "working".
    pub busy_child_cpu_percent: f64,
    pub tools: Vec<Tool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    pub id: String,
    pub name: String,
    /// Executable names or path segments that identify the tool.
    pub matches: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Dashboard {
    pub port: u16,
    /// "system", "dark" or "light".
    pub theme: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Privacy {
    /// App keys or names that are never recorded in detail.
    pub exclude_apps: Vec<String>,
    /// Hosts (and their subdomains) that are never recorded in detail.
    pub exclude_hosts: Vec<String>,
    /// "hide": time counts as "Hidden" with no details. "drop": not recorded at all.
    pub exclude_mode: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Titles {
    /// Extra regexes removed from titles before storing.
    pub strip_patterns: Vec<String>,
    pub max_len: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Category {
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub apps: Vec<String>,
    #[serde(default)]
    pub programs: Vec<String>,
}

impl Default for General {
    fn default() -> Self {
        General {
            sample_interval_secs: 10,
            flush_interval_secs: 30,
            track_running: true,
            week_start: "monday".into(),
        }
    }
}

impl Default for Idle {
    fn default() -> Self {
        Idle {
            threshold_secs: 180,
            call_threshold_secs: 300,
            media_keeps_active: true,
            count_inhibited_as_passive: false,
        }
    }
}

impl Default for Visibility {
    fn default() -> Self {
        Visibility {
            min_fraction: 0.05,
            min_pixels: 40_000,
        }
    }
}

fn tool(id: &str, name: &str, matches: &[&str]) -> Tool {
    Tool {
        id: id.into(),
        name: name.into(),
        matches: matches.iter().map(|s| s.to_string()).collect(),
    }
}

impl Default for Agents {
    fn default() -> Self {
        Agents {
            busy_cpu_percent: 3.0,
            busy_write_bytes: 2048,
            busy_child_cpu_percent: 10.0,
            tools: vec![
                tool("claude", "Claude Code", &["claude", "claude-code"]),
                tool("codex", "Codex", &["codex"]),
                tool("opencode", "opencode", &["opencode"]),
                tool("agy", "Antigravity", &["agy", "antigravity"]),
                tool("cursor-agent", "Cursor Agent", &["cursor-agent"]),
                tool("hermes", "Hermes", &["hermes", "hermes-agent"]),
                tool("gemini", "Gemini CLI", &["gemini"]),
                tool("aider", "Aider", &["aider"]),
                tool("crush", "Crush", &["crush"]),
                tool("goose", "Goose", &["goose"]),
                tool("amp", "Amp", &["amp"]),
                tool("qwen", "Qwen Code", &["qwen"]),
                tool("copilot", "Copilot CLI", &["copilot"]),
            ],
        }
    }
}

impl Default for Dashboard {
    fn default() -> Self {
        Dashboard {
            port: 7600,
            theme: "system".into(),
        }
    }
}

impl Default for Privacy {
    fn default() -> Self {
        Privacy {
            exclude_apps: vec![],
            exclude_hosts: vec![],
            exclude_mode: "hide".into(),
        }
    }
}

impl Default for Titles {
    fn default() -> Self {
        Titles {
            strip_patterns: vec![],
            max_len: 200,
        }
    }
}

fn cat(name: &str, color: &str, hosts: &[&str], apps: &[&str], programs: &[&str]) -> Category {
    let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect();
    Category {
        name: name.into(),
        color: color.into(),
        hosts: v(hosts),
        apps: v(apps),
        programs: v(programs),
    }
}

pub fn default_categories() -> Vec<Category> {
    vec![
        cat(
            "AI",
            "#c084fc",
            &[
                "chatgpt.com",
                "claude.ai",
                "gemini.google.com",
                "aistudio.google.com",
                "perplexity.ai",
                "chat.deepseek.com",
                "grok.com",
                "copilot.microsoft.com",
                "chat.mistral.ai",
                "huggingface.co",
            ],
            &["com.anthropic.Claude", "claude", "chatgpt"],
            &[
                "claude",
                "codex",
                "opencode",
                "agy",
                "cursor-agent",
                "hermes",
                "gemini",
                "aider",
                "crush",
                "goose",
                "amp",
                "qwen",
                "copilot",
            ],
        ),
        cat(
            "Development",
            "#60a5fa",
            &[
                "github.com",
                "gitlab.com",
                "stackoverflow.com",
                "docs.rs",
                "crates.io",
                "localhost",
                "127.0.0.1",
                "developer.mozilla.org",
                "npmjs.com",
                "archlinux.org",
                "rust-lang.org",
                "vercel.com",
                "cloudflare.com",
            ],
            &[
                "kitty",
                "com.mitchellh.ghostty",
                "ghostty",
                "code",
                "code-oss",
                "cursor",
                "dev.zed.Zed",
                "zed",
                "org.kde.kate",
                "com.usebruno.Bruno",
                "jetbrains-idea",
                "postman",
            ],
            &[
                "nvim", "vim", "git", "lazygit", "cargo", "npm", "pnpm", "bun", "make", "python",
                "node", "ssh", "htop", "btop", "docker",
            ],
        ),
        cat(
            "Social",
            "#f472b6",
            &[
                "instagram.com",
                "x.com",
                "twitter.com",
                "reddit.com",
                "facebook.com",
                "linkedin.com",
                "threads.net",
                "bsky.app",
                "pinterest.com",
                "snapchat.com",
            ],
            &[],
            &[],
        ),
        cat(
            "Video",
            "#f87171",
            &[
                "youtube.com",
                "netflix.com",
                "primevideo.com",
                "hotstar.com",
                "twitch.tv",
                "vimeo.com",
                "jiocinema.com",
                "crunchyroll.com",
            ],
            &["vlc", "org.videolan.VLC", "mpv", "io.mpv.Mpv", "celluloid"],
            &["mpv", "vlc"],
        ),
        cat(
            "Messaging",
            "#34d399",
            &[
                "web.whatsapp.com",
                "web.telegram.org",
                "discord.com",
                "slack.com",
                "messages.google.com",
                "mail.google.com",
                "outlook.live.com",
            ],
            &[
                "discord",
                "slack",
                "org.telegram.desktop",
                "telegram-desktop",
                "signal",
                "whatsapp",
            ],
            &[],
        ),
        cat(
            "Music",
            "#fbbf24",
            &[
                "music.youtube.com",
                "open.spotify.com",
                "music.amazon.in",
                "music.amazon.com",
                "soundcloud.com",
                "jiosaavn.com",
                "gaana.com",
            ],
            &[
                "spotify",
                "org.strawberrymusicplayer.strawberry",
                "strawberry",
                "Amazon Music",
                "elisa",
            ],
            &[],
        ),
        cat(
            "Meetings",
            "#2dd4bf",
            &[
                "meet.google.com",
                "zoom.us",
                "teams.microsoft.com",
                "teams.live.com",
            ],
            &["zoom", "Google Meet", "teams"],
            &[],
        ),
        cat(
            "Productivity",
            "#a3e635",
            &[
                "notion.so",
                "notion.site",
                "docs.google.com",
                "sheets.google.com",
                "calendar.google.com",
                "drive.google.com",
                "obsidian.md",
                "linear.app",
                "figma.com",
                "canva.com",
                "trello.com",
            ],
            &[
                "com.notion.app.desktop.notion",
                "notion",
                "obsidian",
                "md.obsidian.Obsidian",
                "libreoffice",
                "org.kde.okular",
                "okular",
            ],
            &[],
        ),
        cat(
            "Learning",
            "#fb923c",
            &[
                "coursera.org",
                "udemy.com",
                "khanacademy.org",
                "nptel.ac.in",
                "wikipedia.org",
                "edx.org",
                "leetcode.com",
                "geeksforgeeks.org",
            ],
            &[],
            &[],
        ),
        cat(
            "System",
            "#94a3b8",
            &["brave://settings", "brave://extensions"],
            &[
                "org.kde.dolphin",
                "dolphin",
                "systemsettings",
                "org.kde.systemsettings",
                "plasmashell",
                "org.kde.plasmashell",
                "krunner",
                "org.kde.krunner",
                "ulauncher",
                "vicinae",
                "org.kde.discover",
                "org.kde.spectacle",
                "com.github.wwmm.easyeffects",
                "copyq",
            ],
            &["pacman", "paru", "yay", "systemctl", "journalctl"],
        ),
        cat("Browsing", "#38bdf8", &[], &[], &[]),
        cat("Other", "#64748b", &[], &[], &[]),
    ]
}

impl Default for Config {
    fn default() -> Self {
        Config {
            general: General::default(),
            idle: Idle::default(),
            visibility: Visibility::default(),
            agents: Agents::default(),
            dashboard: Dashboard::default(),
            privacy: Privacy::default(),
            titles: Titles::default(),
            categories: default_categories(),
        }
    }
}

const HEADER: &str = "# arch-tracker configuration. Edit here or in the dashboard (Settings).\n\
# Changes are picked up automatically. Timings in [idle] re-classify all history.\n\n";

pub fn config_path() -> PathBuf {
    util::config_dir().join("config.toml")
}

pub fn custom_css_path() -> PathBuf {
    util::config_dir().join("custom.css")
}

impl Config {
    /// Loads the config, creating it with defaults the first time.
    pub fn load_or_create() -> Result<Config> {
        let path = config_path();
        if !path.exists() {
            let cfg = Config::default();
            cfg.save()?;
            return Ok(cfg);
        }
        Config::load()
    }

    pub fn load() -> Result<Config> {
        let path = config_path();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg.sanitized())
    }

    /// Clamps values that would break the daemon.
    pub fn sanitized(mut self) -> Config {
        self.general.sample_interval_secs = self.general.sample_interval_secs.clamp(2, 300);
        self.general.flush_interval_secs = self.general.flush_interval_secs.clamp(5, 600);
        self.idle.threshold_secs = self.idle.threshold_secs.clamp(30, 3600 * 4);
        self.idle.call_threshold_secs = self.idle.call_threshold_secs.clamp(30, 3600 * 8);
        self.titles.max_len = self.titles.max_len.clamp(20, 1000);
        if self.dashboard.port < 1024 {
            self.dashboard.port = 7600;
        }
        if !matches!(self.dashboard.theme.as_str(), "system" | "dark" | "light") {
            self.dashboard.theme = "system".into();
        }
        if !matches!(self.privacy.exclude_mode.as_str(), "hide" | "drop") {
            self.privacy.exclude_mode = "hide".into();
        }
        self
    }

    /// Writes the config atomically, keeping comments and unknown keys already in the file.
    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let fresh: toml_edit::DocumentMut = toml::to_string_pretty(self)?.parse()?;
        let doc = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
        {
            Some(mut existing) => {
                merge_tables(existing.as_table_mut(), fresh.as_table());
                existing.to_string()
            }
            None => format!("{HEADER}{fresh}"),
        };
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, doc)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Compiled title-strip regexes (invalid ones are skipped).
    pub fn title_regexes(&self) -> Vec<regex_lite::Regex> {
        self.titles
            .strip_patterns
            .iter()
            .filter_map(|p| regex_lite::Regex::new(p).ok())
            .collect()
    }
}

fn merge_tables(dst: &mut toml_edit::Table, src: &toml_edit::Table) {
    for (key, item) in src.iter() {
        match (dst.get_mut(key), item) {
            (Some(toml_edit::Item::Table(d)), toml_edit::Item::Table(s)) => merge_tables(d, s),
            (Some(existing), _) => {
                let decor = existing.as_value().map(|v| v.decor().clone());
                *existing = item.clone();
                if let (Some(decor), Some(v)) = (decor, existing.as_value_mut()) {
                    *v.decor_mut() = decor;
                }
            }
            (None, _) => {
                dst.insert(key, item.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_defaults() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, Config::default());
    }

    #[test]
    fn partial_file_uses_defaults() {
        let cfg: Config = toml::from_str("[idle]\nthreshold_secs = 60\n").unwrap();
        assert_eq!(cfg.idle.threshold_secs, 60);
        assert_eq!(cfg.idle.call_threshold_secs, 300);
        assert_eq!(cfg.dashboard.port, 7600);
        assert!(!cfg.categories.is_empty());
    }

    #[test]
    fn merge_keeps_comments() {
        let mut existing: toml_edit::DocumentMut =
            "# mine\n[idle]\n# how long\nthreshold_secs = 180 # inline\nextra = 1\n"
                .parse()
                .unwrap();
        let fresh: toml_edit::DocumentMut = "[idle]\nthreshold_secs = 60\n".parse().unwrap();
        merge_tables(existing.as_table_mut(), fresh.as_table());
        let out = existing.to_string();
        assert!(out.contains("# mine"));
        assert!(out.contains("# how long"));
        assert!(out.contains("threshold_secs = 60"));
        assert!(out.contains("extra = 1"));
    }
}
