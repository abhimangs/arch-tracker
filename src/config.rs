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
    pub projects: Projects,
    pub insights: Insights,
    pub notifications: Notifications,
    pub browsers: Browsers,
    pub categories: Vec<Category>,
}

/// How browsers show in the dashboard's menu. Display only: history is never rewritten.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Browsers {
    /// Browsers left out of the menu, by app id.
    pub hidden: Vec<String>,
    /// Shorter names, as id=name.
    pub names: Vec<String>,
}

impl Browsers {
    /// The name chosen for a browser, if any.
    pub fn name_for(&self, id: &str) -> Option<&str> {
        self.names
            .iter()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim() == id)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    }

    pub fn is_hidden(&self, id: &str) -> bool {
        self.hidden.iter().any(|h| h.trim() == id)
    }

    /// Trimmed, without blanks, repeats or names that rename nothing.
    fn sanitized(self) -> Browsers {
        let mut hidden: Vec<String> = vec![];
        for h in self
            .hidden
            .iter()
            .map(|h| h.trim())
            .filter(|h| !h.is_empty())
        {
            if !hidden.iter().any(|x| x == h) {
                hidden.push(h.to_string());
            }
        }
        let mut names: Vec<String> = vec![];
        for (k, v) in self.names.iter().filter_map(|l| l.split_once('=')) {
            let (k, v) = (k.trim(), v.trim());
            if k.is_empty() || v.is_empty() {
                continue;
            }
            names.retain(|l| l.split_once('=').map(|x| x.0) != Some(k));
            names.push(format!("{k}={v}"));
        }
        Browsers { hidden, names }
    }
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
    /// Delete recorded time older than this many days (0 = keep everything).
    pub keep_days: u64,
}

/// How coding time is grouped by project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Projects {
    /// Apps (ids or names) whose window titles name the open project: code editors.
    pub editors: Vec<String>,
    /// "from=to" lines: count project "from" (a folder or a name) as "to".
    pub aliases: Vec<String>,
    /// Folders that are not projects; terminal time there still counts, just not per project.
    pub ignore: Vec<String>,
    /// Folders whose top-level subfolders are each a project, git repository or not (e.g. ~/projects).
    pub folders: Vec<String>,
    /// Folders that are projects themselves, git repository or not (with everything inside).
    pub include: Vec<String>,
}

/// Sessions, deep work and goals on the dashboard.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Insights {
    /// A break shorter than this does not end a session (seconds).
    pub session_gap_secs: u64,
    /// A session at least this long counts as deep work (minutes).
    pub deep_work_mins: u64,
    /// Daily screen-time goal in minutes (0 = none).
    pub daily_goal_mins: u64,
}

/// Desktop notifications. `enabled = false` turns every one of them off.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Notifications {
    /// The master switch: off sends nothing at all.
    pub enabled: bool,
    /// When a category reaches its daily limit (categories' goal_mins).
    pub limits: bool,
    /// When today's screen time reaches the daily goal (insights.daily_goal_mins).
    pub goal: bool,
    /// A heads-up this many minutes before a limit or the goal (0 = none).
    pub warn_mins: u64,
    /// Remind you to take a break...
    pub breaks: bool,
    /// ...after this many minutes on screen without one.
    pub break_after_mins: u64,
    /// When Codex's plan limits pass these shares (percent, 1 to 100).
    pub plan_limits: bool,
    pub limit_5h_percent: u64,
    pub limit_week_percent: u64,
    /// Play a sound with each notice.
    pub sound: bool,
    /// On the first morning of the week, a summary of the week before.
    pub weekly: bool,
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

/// Subscription plans an agent's API-equivalent cost can be compared with: (agent, id, name, USD a
/// month). List prices billed monthly, no annual discount, as of PLANS_AS_OF.
pub const PLANS: &[(&str, &str, &str, f64)] = &[
    ("claude", "pro", "Claude Pro", 20.0),
    ("claude", "max-5x", "Claude Max 5x", 100.0),
    ("claude", "max-20x", "Claude Max 20x", 200.0),
    ("codex", "go", "ChatGPT Go", 8.0),
    ("codex", "plus", "ChatGPT Plus", 20.0),
    ("codex", "pro-100", "ChatGPT Pro, $100", 100.0),
    ("codex", "pro-200", "ChatGPT Pro, $200", 200.0),
    ("codex", "pro-500", "ChatGPT Pro, $500", 500.0),
    ("codex", "business", "ChatGPT Business, one seat", 25.0),
];
pub const PLANS_AS_OF: &str = "2026-11-26";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Agents {
    /// Agent process CPU usage (percent of one core) that counts as "working".
    pub busy_cpu_percent: f64,
    /// Bytes the agent writes to its terminal per 10 seconds that count as "working".
    pub busy_write_bytes: u64,
    /// CPU of the agent's child processes (builds, tests) that counts as "working".
    pub busy_child_cpu_percent: f64,
    pub tools: Vec<Tool>,
    /// Read Claude Code's token counts from its own transcripts (read-only) and show what they
    /// would cost at Anthropic's API prices.
    pub claude_cost: bool,
    /// Read Codex's token counts and plan limits from its own session logs (read-only) and show
    /// what they would cost at OpenAI's API prices.
    pub codex_cost: bool,
    /// Read Hermes Agent's token counts from its own database (read-only) and show what they
    /// would cost at list prices.
    pub hermes_cost: bool,
    /// The plan you pay for, per agent ("claude" = "max-20x"), to compare its API-equivalent cost
    /// against. Codex's is read from its logs unless set here. Ids are in PLANS.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub plans: std::collections::BTreeMap<String, String>,
    /// Your own prices, in USD per million tokens, for models the built-in table lacks or prices
    /// that changed: [agents.prices."claude-new-model"] input = 3.0, output = 15.0.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub prices: std::collections::BTreeMap<String, ModelPrice>,
}

/// A model's price in USD per million tokens. Cache writes cost 1.25x (5 minutes) and 2x (1 hour)
/// the input price; cache reads cost cache_read, or 0.1x the input price if it is left out.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
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
    /// "oled" (true black, the default), "catppuccin", "rose-pine" or "aizome" (the original look).
    pub palette: String,
    /// Flavour used in dark mode: black, mocha/macchiato/frappe, main/moon, or night.
    pub dark_flavor: String,
    /// Accent colour name from the palette (white, mauve, iris, cinnabar, …).
    pub accent: String,
    /// Category colours follow the palette instead of the colours set per category.
    pub theme_categories: bool,
    /// Show the system-tray icon.
    pub tray: bool,
    /// "24h" or "12h" clock.
    pub time_format: String,
    /// Page the dashboard opens on: now, today, timeline, apps, projects, agents, claude or trends.
    pub start_page: String,
    /// "comfortable" or "compact" spacing.
    pub density: String,
    /// "system" (follow the desktop), "reduced" or "full" animation.
    pub motion: String,
    /// Show milliseconds on the live timer.
    pub show_ms: bool,
}

/// A dashboard palette: its dark flavours and accents (the first of each is the default).
#[derive(Debug, Serialize)]
pub struct Palette {
    pub id: &'static str,
    pub name: &'static str,
    pub dark: &'static [&'static str],
    /// Light-mode flavour (fixed per palette).
    pub light: &'static str,
    pub accents: &'static [&'static str],
}

pub const PALETTES: &[Palette] = &[
    Palette {
        id: "oled",
        name: "OLED",
        dark: &["black"],
        light: "paper",
        accents: &["white", "blue", "green", "amber", "rose", "violet"],
    },
    Palette {
        id: "catppuccin",
        name: "Catppuccin",
        dark: &["mocha", "macchiato", "frappe"],
        light: "latte",
        accents: &[
            "mauve",
            "rosewater",
            "flamingo",
            "pink",
            "red",
            "maroon",
            "peach",
            "yellow",
            "green",
            "teal",
            "sky",
            "sapphire",
            "blue",
            "lavender",
        ],
    },
    Palette {
        id: "rose-pine",
        name: "Rosé Pine",
        dark: &["main", "moon"],
        light: "dawn",
        accents: &["iris", "love", "gold", "rose", "pine", "foam"],
    },
    Palette {
        id: "aizome",
        name: "Aizome",
        dark: &["night"],
        light: "fog",
        accents: &["cinnabar", "gold", "jade", "sky", "lilac", "sakura"],
    },
];

pub fn palette(id: &str) -> &'static Palette {
    PALETTES.iter().find(|p| p.id == id).unwrap_or(&PALETTES[0])
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
    /// Daily limit in minutes, shown against today's time (0 = none).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub goal_mins: u64,
}

/// A one-click change from a dashboard row's menu, saved at once to config.toml.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rule {
    /// Puts an app, site or terminal program in a category, taking it out of the others.
    Category {
        target: Target,
        value: String,
        name: Option<String>,
        category: String,
    },
    /// Stops recording an app or a site in detail.
    Exclude {
        target: Target,
        value: String,
        name: Option<String>,
    },
    /// Counts a project under another name (an existing name merges the two).
    Rename { project: String, to: String },
    /// Leaves a folder out of the projects.
    NotProject { project: String },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    App,
    Site,
    Program,
}

/// Longest value a rule takes.
const RULE_MAX: usize = 300;

/// A value from a menu, trimmed. With `keyed`, a program key ("program\u{1f}project") keeps
/// only the program part; the separator is the one control character allowed.
fn rule_text(field: &'static str, what: &str, value: &str, keyed: bool) -> Result<String, Invalid> {
    if value.chars().count() > RULE_MAX {
        return Err(invalid(
            field,
            format!("{what} is longer than {RULE_MAX} characters."),
        ));
    }
    if value
        .chars()
        .any(|c| c.is_control() && !(keyed && c == '\u{1f}'))
    {
        return Err(invalid(
            field,
            format!("{what} has a control character in it."),
        ));
    }
    let head = if keyed {
        value.split('\u{1f}').next().unwrap_or("")
    } else {
        value
    };
    let head = head.trim();
    if head.is_empty() {
        return Err(invalid(field, format!("{what} can't be empty.")));
    }
    Ok(head.to_string())
}

/// Names are compared without case and outside spaces.
fn same_name(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl Default for General {
    fn default() -> Self {
        General {
            sample_interval_secs: 10,
            flush_interval_secs: 30,
            track_running: true,
            week_start: "monday".into(),
            keep_days: 0,
        }
    }
}

impl Default for Projects {
    fn default() -> Self {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect();
        Projects {
            editors: v(&[
                "code",
                "code-oss",
                "visual-studio-code",
                "cursor",
                "windsurf",
                "positron",
                "dev.zed.Zed",
                "zed",
                "org.kde.kate",
                "kate",
                "org.kde.kdevelop",
                "jetbrains-idea",
                "jetbrains-idea-ce",
                "jetbrains-pycharm",
                "jetbrains-pycharm-ce",
                "jetbrains-clion",
                "jetbrains-rustrover",
                "jetbrains-webstorm",
                "jetbrains-goland",
                "sublime_text",
                "neovide",
                "org.gnome.TextEditor",
                "lapce",
                "helix",
            ]),
            aliases: vec![],
            ignore: vec![],
            folders: vec![],
            include: vec![],
        }
    }
}

impl Default for Insights {
    fn default() -> Self {
        Insights {
            session_gap_secs: 60,
            deep_work_mins: 25,
            daily_goal_mins: 0,
        }
    }
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications {
            enabled: true,
            limits: true,
            goal: true,
            warn_mins: 5,
            breaks: true,
            break_after_mins: 50,
            plan_limits: true,
            limit_5h_percent: 70,
            limit_week_percent: 70,
            sound: true,
            weekly: true,
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
            claude_cost: true,
            codex_cost: true,
            hermes_cost: true,
            plans: Default::default(),
            prices: Default::default(),
        }
    }
}

impl Default for Dashboard {
    fn default() -> Self {
        Dashboard {
            port: 7600,
            theme: "system".into(),
            palette: "oled".into(),
            dark_flavor: "black".into(),
            accent: "white".into(),
            theme_categories: true,
            tray: true,
            time_format: "24h".into(),
            start_page: "today".into(),
            density: "comfortable".into(),
            motion: "system".into(),
            show_ms: true,
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
        goal_mins: 0,
    }
}

pub fn default_categories() -> Vec<Category> {
    vec![
        cat(
            "AI",
            "#b49be8",
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
            "#6f9ee8",
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
            "#f08fb0",
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
            "#ef6f5e",
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
            "#7cc59a",
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
            "#e8b84a",
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
            "#4fc1c0",
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
            "#a9c95a",
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
            "#f0955a",
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
            "#9aa3ba",
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
                "vicinae-server",
                "ksplashqml",
                "org.kde.discover",
                "org.kde.spectacle",
                "com.github.wwmm.easyeffects",
                "copyq",
            ],
            &["pacman", "paru", "yay", "systemctl", "journalctl"],
        ),
        cat("Browsing", "#5fb4e0", &[], &[], &[]),
        cat("Other", "#6b7392", &[], &[], &[]),
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
            projects: Projects::default(),
            insights: Insights::default(),
            notifications: Notifications::default(),
            browsers: Browsers::default(),
            categories: default_categories(),
        }
    }
}

/// A value the dashboard can't save, with the config key it belongs to ("dashboard.port").
#[derive(Debug, Clone, PartialEq)]
pub struct Invalid {
    pub field: &'static str,
    pub message: String,
}

fn invalid(field: &'static str, message: impl Into<String>) -> Invalid {
    Invalid {
        field,
        message: message.into(),
    }
}

fn whole(field: &'static str, v: u64, lo: u64, hi: u64, what: &str) -> Result<(), Invalid> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(invalid(
            field,
            format!("{what} must be between {lo} and {hi}."),
        ))
    }
}

fn real(field: &'static str, v: f64, lo: f64, hi: f64, what: &str) -> Result<(), Invalid> {
    if v.is_finite() && (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(invalid(
            field,
            format!("{what} must be between {lo} and {hi}."),
        ))
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
        // A huge hand-edited value would overflow the prune cutoff (0 keeps everything).
        self.general.keep_days = self.general.keep_days.min(36_500);
        self.general.flush_interval_secs = self.general.flush_interval_secs.clamp(5, 600);
        self.idle.threshold_secs = self.idle.threshold_secs.clamp(30, 3600 * 4);
        self.idle.call_threshold_secs = self.idle.call_threshold_secs.clamp(30, 3600 * 8);
        self.titles.max_len = self.titles.max_len.clamp(20, 1000);
        self.notifications.warn_mins = self.notifications.warn_mins.min(240);
        self.notifications.break_after_mins = self.notifications.break_after_mins.clamp(5, 480);
        self.notifications.limit_5h_percent = self.notifications.limit_5h_percent.clamp(1, 100);
        self.notifications.limit_week_percent = self.notifications.limit_week_percent.clamp(1, 100);
        if self.dashboard.port < 1024 {
            self.dashboard.port = 7600;
        }
        if !matches!(self.dashboard.theme.as_str(), "system" | "dark" | "light") {
            self.dashboard.theme = "system".into();
        }
        let d = &mut self.dashboard;
        if !matches!(d.time_format.as_str(), "24h" | "12h") {
            d.time_format = "24h".into();
        }
        if !matches!(
            d.start_page.as_str(),
            "now" | "today" | "timeline" | "apps" | "projects" | "agents" | "claude" | "trends"
        ) {
            d.start_page = "today".into();
        }
        if !matches!(d.density.as_str(), "comfortable" | "compact") {
            d.density = "comfortable".into();
        }
        if !matches!(d.motion.as_str(), "system" | "reduced" | "full") {
            d.motion = "system".into();
        }
        let pal = palette(&self.dashboard.palette);
        self.dashboard.palette = pal.id.into();
        if !pal.dark.contains(&self.dashboard.dark_flavor.as_str()) {
            self.dashboard.dark_flavor = pal.dark[0].into();
        }
        if !pal.accents.contains(&self.dashboard.accent.as_str()) {
            self.dashboard.accent = pal.accents[0].into();
        }
        if !matches!(self.privacy.exclude_mode.as_str(), "hide" | "drop") {
            self.privacy.exclude_mode = "hide".into();
        }
        self.browsers = self.browsers.sanitized();
        self.general.week_start = self.general.week_start.trim().to_lowercase();
        if !matches!(self.general.week_start.as_str(), "monday" | "sunday") {
            self.general.week_start = "monday".into();
        }
        self
    }

    /// Checks the values the dashboard edits, so a bad save is refused before anything changes.
    /// Keys only set by hand (visibility.min_pixels, titles) are left to sanitized().
    pub fn validate(&self) -> Result<(), Invalid> {
        let port = self.dashboard.port as u64;
        whole("dashboard.port", port, 1024, 65535, "The port")?;
        let g = &self.general;
        whole(
            "general.sample_interval_secs",
            g.sample_interval_secs,
            2,
            300,
            "Check every (seconds)",
        )?;
        whole(
            "general.flush_interval_secs",
            g.flush_interval_secs,
            5,
            600,
            "Save every (seconds)",
        )?;
        let i = &self.idle;
        whole(
            "idle.threshold_secs",
            i.threshold_secs,
            30,
            4 * 3600,
            "Idle after (seconds)",
        )?;
        whole(
            "idle.call_threshold_secs",
            i.call_threshold_secs,
            30,
            8 * 3600,
            "Calls allow (seconds)",
        )?;
        let v = self.visibility.min_fraction;
        real(
            "visibility.min_fraction",
            v,
            0.01,
            1.0,
            "The visible share of a window",
        )?;
        let a = &self.agents;
        real(
            "agents.busy_cpu_percent",
            a.busy_cpu_percent,
            0.1,
            400.0,
            "Agent CPU (%)",
        )?;
        real(
            "agents.busy_child_cpu_percent",
            a.busy_child_cpu_percent,
            0.1,
            1600.0,
            "Build CPU (%)",
        )?;
        whole(
            "agents.busy_write_bytes",
            a.busy_write_bytes,
            0,
            10_000_000,
            "Terminal output (bytes)",
        )?;
        for (model, p) in &a.prices {
            let ok = |v: f64| v.is_finite() && (0.0..=10_000.0).contains(&v);
            if model.trim().is_empty()
                || !ok(p.input)
                || !ok(p.output)
                || !p.cache_read.is_none_or(ok)
            {
                return Err(invalid(
                    "agents.prices",
                    format!(
                        "The price of {model:?} must be between 0 and 10000 dollars per million tokens."
                    ),
                ));
            }
        }
        let mut ids = std::collections::HashSet::new();
        for t in &a.tools {
            if t.id.trim().is_empty() {
                return Err(invalid("agents.tools", "Every known agent needs an id."));
            }
            if !ids.insert(t.id.trim().to_lowercase()) {
                return Err(invalid(
                    "agents.tools",
                    format!("The agent id {:?} is listed twice.", t.id),
                ));
            }
        }
        if !matches!(self.privacy.exclude_mode.as_str(), "hide" | "drop") {
            return Err(invalid("privacy.exclude_mode", "Choose hide or drop."));
        }
        whole(
            "general.keep_days",
            self.general.keep_days,
            0,
            36_500,
            "Keep history (days)",
        )?;
        let ins = &self.insights;
        whole(
            "insights.session_gap_secs",
            ins.session_gap_secs,
            5,
            3600,
            "Session break (seconds)",
        )?;
        whole(
            "insights.deep_work_mins",
            ins.deep_work_mins,
            5,
            480,
            "Deep work (minutes)",
        )?;
        whole(
            "insights.daily_goal_mins",
            ins.daily_goal_mins,
            0,
            24 * 60,
            "Daily goal (minutes)",
        )?;
        for (agent, plan) in &self.agents.plans {
            if !PLANS.iter().any(|(a, id, _, _)| a == agent && id == plan) {
                return Err(invalid(
                    "agents.plans",
                    format!("There is no plan \"{plan}\" for {agent}."),
                ));
            }
        }
        let nt = &self.notifications;
        whole(
            "notifications.warn_mins",
            nt.warn_mins,
            0,
            240,
            "Heads-up (minutes)",
        )?;
        whole(
            "notifications.break_after_mins",
            nt.break_after_mins,
            5,
            480,
            "Break after (minutes)",
        )?;
        whole(
            "notifications.limit_5h_percent",
            nt.limit_5h_percent,
            1,
            100,
            "5-hour limit alert (percent)",
        )?;
        whole(
            "notifications.limit_week_percent",
            nt.limit_week_percent,
            1,
            100,
            "Weekly limit alert (percent)",
        )?;
        for line in &self.projects.aliases {
            let ok = line
                .split_once('=')
                .is_some_and(|(a, b)| !a.trim().is_empty() && !b.trim().is_empty());
            if !ok {
                return Err(invalid(
                    "projects.aliases",
                    format!("{line:?} should look like old=new."),
                ));
            }
        }
        if !matches!(self.general.week_start.as_str(), "monday" | "sunday") {
            return Err(invalid(
                "general.week_start",
                "Weeks start on monday or sunday.",
            ));
        }
        let mut names = std::collections::HashSet::new();
        for c in &self.categories {
            if c.name.trim().is_empty() {
                return Err(invalid("categories", "Every category needs a name."));
            }
            if !names.insert(c.name.trim().to_lowercase()) {
                return Err(invalid(
                    "categories",
                    format!("The category {:?} is listed twice.", c.name),
                ));
            }
            let hex = c.color.strip_prefix('#').unwrap_or("");
            if !matches!(hex.len(), 3 | 4 | 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(invalid(
                    "categories",
                    format!("The colour of {:?} must look like #5fb4e0.", c.name),
                ));
            }
        }
        Ok(())
    }
}

impl Config {
    /// Applies a menu rule. Every check runs before anything changes, so a refused rule changes nothing.
    pub fn apply_rule(&mut self, rule: &Rule) -> Result<(), Invalid> {
        match rule {
            Rule::Category {
                target,
                value,
                name,
                category,
            } => {
                let what = match target {
                    Target::App => "The app",
                    Target::Site => "The site",
                    Target::Program => "The program",
                };
                let keyed = *target == Target::Program;
                let mut key = rule_text("categories", what, value, keyed)?;
                if *target == Target::Site {
                    key = key.to_lowercase();
                }
                let name = match name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                    Some(n) => Some(rule_text("categories", "The name", n, false)?),
                    None => None,
                };
                let wanted = category.trim();
                let Some(pos) = self
                    .categories
                    .iter()
                    .position(|c| same_name(&c.name, wanted))
                else {
                    return Err(invalid(
                        "categories",
                        format!("There is no category called {wanted}."),
                    ));
                };
                // An app also matches by its display name, as the classifier does.
                let gone = |v: &str| {
                    same_name(v, &key)
                        || (*target == Target::App
                            && name.as_deref().is_some_and(|n| same_name(v, n)))
                };
                for c in &mut self.categories {
                    match target {
                        Target::App => c.apps.retain(|v| !gone(v)),
                        Target::Site => c.hosts.retain(|v| !gone(v)),
                        Target::Program => c.programs.retain(|v| !gone(v)),
                    }
                }
                let c = &mut self.categories[pos];
                match target {
                    Target::App => c.apps.push(key),
                    Target::Site => c.hosts.push(key),
                    Target::Program => c.programs.push(key),
                }
            }
            Rule::Exclude { target, value, .. } => match target {
                Target::App => {
                    let app = rule_text("privacy.exclude_apps", "The app", value, false)?;
                    if !self.privacy.exclude_apps.iter().any(|a| same_name(a, &app)) {
                        self.privacy.exclude_apps.push(app);
                    }
                }
                Target::Site => {
                    let host = rule_text("privacy.exclude_hosts", "The site", value, false)?
                        .to_lowercase();
                    if !self
                        .privacy
                        .exclude_hosts
                        .iter()
                        .any(|h| same_name(h, &host))
                    {
                        self.privacy.exclude_hosts.push(host);
                    }
                }
                Target::Program => {
                    return Err(invalid(
                        "privacy",
                        "Only apps and sites can be left out from a menu.",
                    ));
                }
            },
            Rule::Rename { project, to } => {
                let project = rule_text("projects.aliases", "The project", project, false)?;
                let to = rule_text("projects.aliases", "The new name", to, false)?;
                if project.contains('=') {
                    return Err(invalid(
                        "projects.aliases",
                        "A project name can't contain =.",
                    ));
                }
                if to.contains('=') {
                    return Err(invalid("projects.aliases", "The new name can't contain =."));
                }
                // Replaces the project's old alias line, if it had one.
                self.projects
                    .aliases
                    .retain(|line| !matches!(line.split_once('='), Some((from, _)) if from.trim() == project));
                self.projects.aliases.push(format!("{project}={to}"));
            }
            Rule::NotProject { project } => {
                let project = rule_text("projects.ignore", "The folder", project, false)?;
                if !(project.starts_with('/') || project.starts_with('~')) {
                    return Err(invalid(
                        "projects.ignore",
                        "Only folders (paths starting with / or ~) can be marked as not a project.",
                    ));
                }
                self.projects.include.retain(|p| p.trim() != project);
                if !self.projects.ignore.iter().any(|p| p.trim() == project) {
                    self.projects.ignore.push(project);
                }
            }
        }
        Ok(())
    }
}

impl Config {
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
                // Your plans are written whole, so one you take out is gone from the file too.
                if let Some(agents) = existing.get_mut("agents").and_then(|a| a.as_table_mut()) {
                    match fresh.get("agents").and_then(|a| a.get("plans")) {
                        Some(p) => {
                            agents.insert("plans", p.clone());
                        }
                        None => {
                            agents.remove("plans");
                        }
                    }
                }
                existing.to_string()
            }
            None => {
                // An unparsable file is replaced, so keep a copy of it first.
                if path.exists() {
                    let broken = path.with_extension("toml.broken");
                    std::fs::copy(&path, &broken)?;
                    tracing::warn!(
                        "config.toml did not parse; kept a copy at {}",
                        broken.display()
                    );
                }
                format!("{HEADER}{fresh}")
            }
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
    fn validation_names_the_bad_field() {
        assert_eq!(Config::default().validate(), Ok(()));
        let field = |f: &dyn Fn(&mut Config)| {
            let mut c = Config::default();
            f(&mut c);
            c.validate().unwrap_err().field
        };
        assert_eq!(field(&|c| c.dashboard.port = 0), "dashboard.port");
        assert_eq!(field(&|c| c.dashboard.port = 80), "dashboard.port");
        assert_eq!(field(&|c| c.idle.threshold_secs = 5), "idle.threshold_secs");
        assert_eq!(
            field(&|c| c.general.flush_interval_secs = 0),
            "general.flush_interval_secs"
        );
        assert_eq!(
            field(&|c| c.visibility.min_fraction = f64::NAN),
            "visibility.min_fraction"
        );
        assert_eq!(
            field(&|c| c.agents.busy_cpu_percent = -1.0),
            "agents.busy_cpu_percent"
        );
        assert_eq!(
            field(&|c| c.privacy.exclude_mode = "keep".into()),
            "privacy.exclude_mode"
        );
        assert_eq!(
            field(&|c| c.general.week_start = "friday".into()),
            "general.week_start"
        );
        assert_eq!(field(&|c| c.categories[0].name = " ".into()), "categories");
        assert_eq!(
            field(&|c| c.categories[0].color = "red".into()),
            "categories"
        );
        assert_eq!(
            field(&|c| c.agents.tools.push(c.agents.tools[0].clone())),
            "agents.tools"
        );
        // Hand-only keys are not the dashboard's to refuse.
        let mut c = Config::default();
        c.visibility.min_pixels = -5;
        c.titles.max_len = 0;
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn notification_settings_are_checked() {
        let mut c = Config::default();
        assert!(c.notifications.enabled, "on unless switched off");
        c.notifications.warn_mins = 999;
        assert_eq!(c.validate().unwrap_err().field, "notifications.warn_mins");
        c.notifications.warn_mins = 5;
        c.notifications.break_after_mins = 1;
        assert_eq!(
            c.validate().unwrap_err().field,
            "notifications.break_after_mins"
        );
        assert_eq!(c.sanitized().notifications.break_after_mins, 5);
        // A config from before notifications existed gets the defaults.
        let old: Config = toml::from_str("[insights]\ndaily_goal_mins = 60\n").unwrap();
        assert_eq!(old.notifications, Notifications::default());
    }

    #[test]
    fn roundtrip_defaults() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, Config::default());
    }

    #[test]
    fn palette_choices_are_sanitized() {
        let mut cfg = Config::default();
        cfg.dashboard.palette = "rose-pine".into();
        cfg.dashboard.dark_flavor = "mocha".into();
        cfg.dashboard.accent = "mauve".into();
        let cfg = cfg.sanitized();
        assert_eq!(cfg.dashboard.dark_flavor, "main");
        assert_eq!(cfg.dashboard.accent, "iris");
        let mut cfg = Config::default();
        cfg.dashboard.palette = "nope".into();
        assert_eq!(cfg.sanitized().dashboard.palette, "oled");
        let cfg: Config = toml::from_str("[dashboard]\ntheme = \"dark\"\n").unwrap();
        assert_eq!(cfg.dashboard.palette, "oled");
        assert_eq!(cfg.dashboard.dark_flavor, "black");
        assert_eq!(cfg.dashboard.accent, "white");
        assert!(cfg.dashboard.tray && cfg.dashboard.theme_categories);
    }

    #[test]
    fn start_page_now_is_kept() {
        let cfg: Config = toml::from_str("[dashboard]\nstart_page = \"now\"\n").unwrap();
        assert_eq!(cfg.sanitized().dashboard.start_page, "now");
        let cfg: Config = toml::from_str("[dashboard]\nstart_page = \"nope\"\n").unwrap();
        assert_eq!(cfg.sanitized().dashboard.start_page, "today");
    }

    #[test]
    fn huge_keep_days_is_clamped() {
        let cfg: Config = toml::from_str("[general]\nkeep_days = 150000000000\n").unwrap();
        assert_eq!(cfg.sanitized().general.keep_days, 36_500);
        let cfg: Config = toml::from_str("[general]\nkeep_days = 0\n").unwrap();
        assert_eq!(cfg.sanitized().general.keep_days, 0);
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

    fn two_categories() -> Config {
        Config {
            categories: vec![
                cat("Work", "#5fb4e0", &["work.example.test"], &["Notes"], &[]),
                cat("Play", "#e07a5f", &[], &[], &["htop"]),
            ],
            ..Config::default()
        }
    }

    #[test]
    fn rule_moves_a_site_between_categories() {
        let mut c = two_categories();
        let rule = Rule::Category {
            target: Target::Site,
            value: " Work.Example.TEST ".into(),
            name: None,
            category: "play".into(),
        };
        c.apply_rule(&rule).unwrap();
        assert!(c.categories[0].hosts.is_empty());
        assert_eq!(c.categories[1].hosts, vec!["work.example.test"]);
    }

    #[test]
    fn rule_matches_an_app_by_its_name() {
        let mut c = two_categories();
        let rule = Rule::Category {
            target: Target::App,
            value: "org.example.notes".into(),
            name: Some("Notes".into()),
            category: "Play".into(),
        };
        c.apply_rule(&rule).unwrap();
        assert!(c.categories[0].apps.is_empty());
        assert_eq!(c.categories[1].apps, vec!["org.example.notes"]);
    }

    #[test]
    fn rule_program_key_keeps_only_the_program() {
        let mut c = two_categories();
        let rule = Rule::Category {
            target: Target::Program,
            value: "htop\u{1f}tracker".into(),
            name: None,
            category: "Work".into(),
        };
        c.apply_rule(&rule).unwrap();
        assert!(c.categories[1].programs.is_empty());
        assert_eq!(c.categories[0].programs, vec!["htop"]);
    }

    #[test]
    fn rule_excludes_apps_and_sites_once() {
        let mut c = Config::default();
        let app = |value: &str| Rule::Exclude {
            target: Target::App,
            value: value.into(),
            name: None,
        };
        c.apply_rule(&app("org.example.game")).unwrap();
        c.apply_rule(&app("ORG.example.game")).unwrap();
        assert_eq!(c.privacy.exclude_apps, vec!["org.example.game"]);
        let site = Rule::Exclude {
            target: Target::Site,
            value: "Ads.example.test".into(),
            name: None,
        };
        c.apply_rule(&site).unwrap();
        assert_eq!(c.privacy.exclude_hosts, vec!["ads.example.test"]);
        let program = Rule::Exclude {
            target: Target::Program,
            value: "htop".into(),
            name: None,
        };
        assert!(c.apply_rule(&program).is_err());
    }

    #[test]
    fn rule_rename_replaces_the_old_alias() {
        let mut c = Config::default();
        c.projects.aliases = vec!["~/code/old=Old name".into(), "~/code/other=Other".into()];
        let rename = Rule::Rename {
            project: "~/code/old".into(),
            to: "Shiny".into(),
        };
        c.apply_rule(&rename).unwrap();
        assert_eq!(
            c.projects.aliases,
            vec!["~/code/other=Other", "~/code/old=Shiny"]
        );
        let bad = Rule::Rename {
            project: "~/code/x".into(),
            to: "a=b".into(),
        };
        assert_eq!(c.apply_rule(&bad).unwrap_err().field, "projects.aliases");
        assert_eq!(c.projects.aliases.len(), 2);
    }

    #[test]
    fn rule_not_project_moves_a_folder_to_ignore() {
        let mut c = Config::default();
        c.projects.include = vec!["~/scratch".into(), "~/keep".into()];
        let rule = Rule::NotProject {
            project: "~/scratch".into(),
        };
        c.apply_rule(&rule).unwrap();
        assert_eq!(c.projects.ignore, vec!["~/scratch"]);
        assert_eq!(c.projects.include, vec!["~/keep"]);
        let bare = Rule::NotProject {
            project: "scratch".into(),
        };
        assert!(c.apply_rule(&bare).is_err());
    }

    #[test]
    fn rule_refuses_unknown_categories_and_bad_values() {
        let mut c = two_categories();
        let nope = Rule::Category {
            target: Target::App,
            value: "x".into(),
            name: None,
            category: "Nope".into(),
        };
        let err = c.apply_rule(&nope).unwrap_err();
        assert_eq!(
            err,
            invalid("categories", "There is no category called Nope.")
        );
        let long = Rule::Category {
            target: Target::Site,
            value: "a".repeat(301),
            name: None,
            category: "Work".into(),
        };
        assert!(c.apply_rule(&long).is_err());
        let control = Rule::Category {
            target: Target::Site,
            value: "bad\nhost".into(),
            name: None,
            category: "Work".into(),
        };
        assert!(c.apply_rule(&control).is_err());
        assert_eq!(c, two_categories());
    }

    #[test]
    fn rules_parse_from_the_dashboard_json() {
        let r: Rule = serde_json::from_str(
            r#"{"kind":"category","target":"program","value":"htop\u001ftracker","category":"Work"}"#,
        )
        .unwrap();
        assert_eq!(
            r,
            Rule::Category {
                target: Target::Program,
                value: "htop\u{1f}tracker".into(),
                name: None,
                category: "Work".into(),
            }
        );
        let r: Rule =
            serde_json::from_str(r#"{"kind":"not_project","project":"~/scratch"}"#).unwrap();
        assert_eq!(
            r,
            Rule::NotProject {
                project: "~/scratch".into()
            }
        );
    }
}
