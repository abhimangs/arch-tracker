//! Small helpers: time, XDG paths, durations, title normalization.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Every D-Bus method call gives up after this long, so a hung service (plasmashell, a media
/// player, logind) cannot freeze recording.
pub const DBUS_TIMEOUT: Duration = Duration::from_secs(2);

/// A system-bus connection with the method timeout.
pub async fn system_bus() -> zbus::Result<zbus::Connection> {
    zbus::connection::Builder::system()
        .method_timeout(DBUS_TIMEOUT)
        .build()
        .await
}

/// Hands memory freed by a big job (a report, a transcript pass) back to the system, so the
/// daemon's footprint stays small between jobs.
pub fn trim_heap() {
    // SAFETY: malloc_trim only releases free pages; it has no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub(crate) fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("arch-tracker")
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("arch-tracker")
}

pub fn runtime_dir() -> PathBuf {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })),
    };
    base.join("arch-tracker")
}

pub fn db_path() -> PathBuf {
    data_dir().join("tracker.db")
}

pub fn control_socket() -> PathBuf {
    runtime_dir().join("control.sock")
}

pub fn ingest_socket() -> PathBuf {
    runtime_dir().join("ingest.sock")
}

/// "/home/user/projects/x" -> "~/projects/x"
pub fn abbreviate_home(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Parses "90" (minutes), "45s", "30m", "1h", "1h30m", "2d" into milliseconds.
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(mins) = s.parse::<i64>() {
        return Some(mins * 60_000);
    }
    let mut total = 0i64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        total += n * match c {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            _ => return None,
        };
    }
    if !num.is_empty() {
        return None;
    }
    Some(total)
}

/// Start of the next local day after now_ms, in ms (DST-safe).
#[cfg(test)]
pub fn next_local_midnight(now_ms: i64) -> i64 {
    local_day_bounds(now_ms).1
}

/// [start, end) of the local day containing ms.
pub fn local_day_bounds(ms: i64) -> (i64, i64) {
    let tz = jiff::tz::TimeZone::system();
    let fallback = (
        ms - ms.rem_euclid(86_400_000),
        ms - ms.rem_euclid(86_400_000) + 86_400_000,
    );
    let Ok(ts) = jiff::Timestamp::from_millisecond(ms) else {
        return fallback;
    };
    let zdt = ts.to_zoned_datetime(tz);
    let start = zdt.start_of_day();
    let end = zdt.tomorrow().and_then(|t| t.start_of_day());
    match (start, end) {
        (Ok(s), Ok(e)) => (
            s.timestamp().as_millisecond(),
            e.timestamp().as_millisecond(),
        ),
        _ => fallback,
    }
}

/// [start, end) of a local date "YYYY-MM-DD".
pub fn date_bounds(date: &str) -> Option<(i64, i64)> {
    let d: jiff::civil::Date = date.parse().ok()?;
    let tz = jiff::tz::TimeZone::system();
    let start = d
        .to_zoned_datetime(tz.clone())
        .ok()?
        .timestamp()
        .as_millisecond();
    let end = d
        .tomorrow()
        .ok()?
        .to_zoned_datetime(tz)
        .ok()?
        .timestamp()
        .as_millisecond();
    Some((start, end))
}

/// Local date "YYYY-MM-DD" of ms.
pub fn local_date(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|ts| {
            ts.to_zoned_datetime(jiff::tz::TimeZone::system())
                .date()
                .to_string()
        })
        .unwrap_or_default()
}

/// Removes noise that changes without the "thing" changing: unread counters like "(3) ",
/// spinner/status glyphs that agents put in terminal titles, and repeated whitespace.
pub fn normalize_title(raw: &str, extra: &[regex_lite::Regex], max_len: usize) -> String {
    let mut t = raw.trim();
    loop {
        let before = t;
        t = t.trim_start_matches(|c: char| is_status_glyph(c) || c.is_whitespace());
        if let Some(rest) = strip_counter(t) {
            t = rest;
        }
        if t == before {
            break;
        }
    }
    let mut out: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(host) = url_title(&out) {
        out = host;
    }
    for re in extra {
        out = re.replace_all(&out, "").trim().to_string();
    }
    if out.chars().count() > max_len {
        out = out.chars().take(max_len).collect();
    }
    out
}

/// Pages without a <title> (or still loading) show their URL as the title, and directory
/// listings show the path. Such titles are reduced to the host (paths and queries are never
/// stored); None if the title does not look like a URL.
fn url_title(t: &str) -> Option<String> {
    if t.starts_with("Index of /") {
        return Some("Index of".into());
    }
    let first = t.split_whitespace().next()?;
    let lower = first.to_ascii_lowercase();
    if lower.starts_with("view-source:") {
        return Some(url_title(&t["view-source:".len()..]).unwrap_or_default());
    }
    if ["data:", "blob:", "javascript:"]
        .iter()
        .any(|p| lower.starts_with(p))
    {
        return Some(String::new());
    }
    if let Some((scheme, _)) = first.split_once("://")
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        let host = match url::Url::parse(first) {
            Ok(u) => {
                let h = u.host_str().unwrap_or_default().to_lowercase();
                let h = match u.port() {
                    Some(p) => format!("{h}:{p}"),
                    None => h,
                };
                match u.scheme() {
                    "file" => String::new(),
                    "http" | "https" | "ws" | "wss" | "ftp" => h,
                    _ if h.is_empty() => String::new(),
                    s => format!("{s}://{h}"),
                }
            }
            Err(_) => String::new(),
        };
        return Some(host);
    }
    // "www.example.com/search?q=..." (browsers drop the scheme and lowercase the host).
    let end = first.find(['/', '?', '#'])?;
    let host = &first[..end];
    looks_like_host(host).then(|| host.to_string())
}

/// "www.example.com", "localhost:3000", "127.0.0.1:8080" (lowercase, as browsers show them).
fn looks_like_host(h: &str) -> bool {
    let (name, port) = match h.rsplit_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (h, None),
    };
    if let Some(p) = port
        && (p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    if name == "localhost" {
        return true;
    }
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.chars()
                .all(|c| (c.is_alphanumeric() && !c.is_uppercase()) || c == '-')
    };
    if labels.len() < 2 || !labels.iter().all(label_ok) {
        return false;
    }
    let tld = labels[labels.len() - 1];
    let ipv4 = labels.len() == 4 && labels.iter().all(|l| l.parse::<u8>().is_ok());
    ipv4 || (tld.chars().count() >= 2 && tld.chars().all(char::is_alphabetic))
}

/// Loose title equality: media sessions drop site suffixes ("Video" vs "Video - YouTube").
pub fn titles_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim().to_lowercase(), b.trim().to_lowercase());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    short.chars().count() >= 8
        && (long.starts_with(short.as_str()) || long.contains(short.as_str()))
}

fn is_status_glyph(c: char) -> bool {
    matches!(c as u32,
        0x2800..=0x28FF   // braille spinners
        | 0x2580..=0x25FF // block elements + geometric shapes (● ◐ ■)
        | 0x2600..=0x27BF // misc symbols + dingbats (✳ ✶ ✻ ✽ ✓)
        | 0x2219 | 0x00B7 | 0x2022 // bullets
    ) || c == '*'
}

/// "(12) WhatsApp" -> "WhatsApp", "(99+) Inbox" -> "Inbox"
fn strip_counter(t: &str) -> Option<&str> {
    let rest = t.strip_prefix('(')?;
    let close = rest.find(')')?;
    let inner = &rest[..close];
    if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_digit() || c == '+') {
        return None;
    }
    Some(rest[close + 1..].trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Some(90 * 60_000));
        assert_eq!(parse_duration("1h30m"), Some(90 * 60_000));
        assert_eq!(parse_duration("45s"), Some(45_000));
        assert_eq!(parse_duration("2d"), Some(2 * 86_400_000));
        assert_eq!(parse_duration("1x"), None);
        assert_eq!(parse_duration("1h3"), None);
    }

    #[test]
    fn title_matching() {
        assert!(titles_match(
            "Borrow checker in five minutes",
            "Borrow checker in five minutes - YouTube"
        ));
        assert!(titles_match("Instagram", "instagram"));
        assert!(!titles_match("Home", "Home Depot deals - Brave"));
        assert!(!titles_match("", "x"));
    }

    #[test]
    fn titles() {
        let n = |s| normalize_title(s, &[], 200);
        assert_eq!(
            n("(12) Rust ownership explained - YouTube"),
            "Rust ownership explained - YouTube"
        );
        assert_eq!(n("(99+) WhatsApp"), "WhatsApp");
        assert_eq!(n("✳ Claude Code"), "Claude Code");
        assert_eq!(n("⠂ ⠐ working   hard"), "working hard");
        assert_eq!(n("(draft) notes"), "(draft) notes");
        assert_eq!(n("#general"), "#general");
    }

    #[test]
    fn url_titles_keep_only_the_host() {
        let n = |s| normalize_title(s, &[], 200);
        assert_eq!(n("www.example.com/search?q=secret"), "www.example.com");
        assert_eq!(n("www.example.com/search?q=two words"), "www.example.com");
        assert_eq!(n("(2) mail.example.com/inbox/42"), "mail.example.com");
        assert_eq!(n("https://host.example.org/a/b?c=d#e"), "host.example.org");
        assert_eq!(n("http://localhost:3000/admin"), "localhost:3000");
        assert_eq!(n("127.0.0.1:8080/debug/vars"), "127.0.0.1:8080");
        assert_eq!(n("file:///home/user/notes.txt"), "");
        assert_eq!(n("Index of /home/user/"), "Index of");
        assert_eq!(n("view-source:https://a.example.com/x"), "a.example.com");
        assert_eq!(n("data:text/html,hello"), "");
        assert_eq!(n("chrome://settings/privacy"), "chrome://settings");
        // Not URLs.
        assert_eq!(n("www.example.com"), "www.example.com");
        assert_eq!(n("Node.js/Express tutorial"), "Node.js/Express tutorial");
        assert_eq!(n("TCP/IP basics"), "TCP/IP basics");
        assert_eq!(n("1.5/10 rating"), "1.5/10 rating");
        assert_eq!(n("v2.0/beta notes"), "v2.0/beta notes");
    }
}
