//! Dashboard lock. A per-user key (a 0600 file in the data dir) is the session cookie. The
//! tray icon and `arch-tracker open` hand out one-time login codes (60 s, single use), so the
//! key itself never shows up in a URL or in browser history. Without the cookie the dashboard
//! shows only a "locked" page, so other programs on this computer can't read your history.
//!
//! The cookie is host-only for `arch-tracker.localhost`: cookies ignore ports, so a cookie on
//! 127.0.0.1 would also be sent to every local dev server you open.

use crate::util;
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Mutex, OnceLock};

pub const HOST: &str = "arch-tracker.localhost";
pub const COOKIE: &str = "arch_tracker_session";
const CODE_TTL_MS: i64 = 60_000;
const MAX_CODES: usize = 16;

static KEY: OnceLock<String> = OnceLock::new();
static PORT: AtomicU16 = AtomicU16::new(0);

/// The port the dashboard currently serves on (0 = not serving).
pub fn port() -> u16 {
    PORT.load(Ordering::Relaxed)
}

pub fn set_port(p: u16) {
    PORT.store(p, Ordering::Relaxed);
}
static CODES: Mutex<Vec<(String, i64)>> = Mutex::new(Vec::new());

/// Hex string of n random bytes from the kernel; an error rather than a weak secret.
pub fn random_hex(n: usize) -> Result<String> {
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .context("reading /dev/urandom")?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn key_path() -> std::path::PathBuf {
    util::db_path().with_file_name("dashboard.key")
}

fn load_or_create_key(path: &std::path::Path) -> Result<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(k) = read_valid_key(path) {
        return Ok(k);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let key = random_hex(32)?;
    // A name per process, so two processes starting together never share a temp file.
    let tmp = path.with_extension(format!("key.{}.tmp", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(key.as_bytes())?;
    // A hard link fails if the key exists, so a key another process just made is never replaced.
    let mut linked = std::fs::hard_link(&tmp, path);
    if matches!(&linked, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists) {
        match read_valid_key(path) {
            Some(k) => {
                let _ = std::fs::remove_file(&tmp);
                return Ok(k);
            }
            // An empty or cut-short file is no key, so replace it once. A valid key is never removed.
            None => {
                let _ = std::fs::remove_file(path);
                linked = std::fs::hard_link(&tmp, path);
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(key),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_valid_key(path) {
            Some(k) => Ok(k),
            None => bail!(
                "{} holds no valid key; remove it to make a new one",
                path.display()
            ),
        },
        Err(e) => Err(e.into()),
    }
}

/// The key in the file, if the file holds one.
fn read_valid_key(path: &std::path::Path) -> Option<String> {
    let k = std::fs::read_to_string(path).ok()?;
    let k = k.trim();
    (k.len() >= 32 && k.chars().all(|c| c.is_ascii_hexdigit())).then(|| k.to_string())
}

/// The session key, created on first use.
pub fn key() -> Result<&'static str> {
    if let Some(k) = KEY.get() {
        return Ok(k);
    }
    let k = load_or_create_key(&key_path())?;
    Ok(KEY.get_or_init(|| k))
}

/// A fresh one-time login code.
pub fn issue_code() -> Result<String> {
    key()?; // make sure a session key exists before promising a login
    let code = random_hex(16)?;
    let now = util::now_ms();
    let mut codes = CODES
        .lock()
        .map_err(|_| anyhow::anyhow!("code store poisoned"))?;
    codes.retain(|(_, t)| now - *t < CODE_TTL_MS);
    if codes.len() >= MAX_CODES {
        codes.remove(0);
    }
    codes.push((code.clone(), now));
    Ok(code)
}

/// True once per issued, unexpired code.
pub fn redeem(code: &str) -> bool {
    let now = util::now_ms();
    let Ok(mut codes) = CODES.lock() else {
        return false;
    };
    codes.retain(|(_, t)| now - *t < CODE_TTL_MS);
    match codes.iter().position(|(c, _)| ct_eq(c, code)) {
        Some(i) => {
            codes.remove(i);
            true
        }
        None => false,
    }
}

/// Constant-time string comparison (lengths are public).
fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Whether a Cookie header carries the session key.
pub fn cookie_ok(cookie_header: &str) -> bool {
    let Ok(k) = key() else {
        return false;
    };
    cookie_header
        .split(';')
        .filter_map(|p| p.trim().split_once('='))
        .any(|(n, v)| n == COOKIE && ct_eq(v, k))
}

/// Set-Cookie value for a signed-in browser (about 400 days, the most browsers keep).
pub fn set_cookie() -> Result<String> {
    Ok(format!(
        "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=34560000",
        key()?
    ))
}

/// Dashboard URL that signs the browser in; `route` is a hash route like "today" or "settings".
pub fn login_url(port: u16, route: &str) -> Result<String> {
    let route: String = route
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-'))
        .collect();
    if port == 0 {
        bail!("the dashboard is not running");
    }
    Ok(format!(
        "http://{HOST}:{port}/login?code={}&to={route}",
        issue_code()?
    ))
}

/// Opens a URL from a command-line tool (the user's own session).
pub fn open_in_browser(url: &str) -> Result<()> {
    let st = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .status()
        .context("running xdg-open")?;
    if !st.success() {
        bail!("xdg-open failed ({st})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_single_use() {
        let mut codes = CODES.lock().unwrap();
        codes.push(("abc123".into(), util::now_ms()));
        drop(codes);
        assert!(redeem("abc123"));
        assert!(!redeem("abc123"));
        assert!(!redeem("nope"));
    }

    #[test]
    fn expired_codes_fail() {
        CODES
            .lock()
            .unwrap()
            .push(("old".into(), util::now_ms() - CODE_TTL_MS - 1));
        assert!(!redeem("old"));
    }

    #[test]
    fn key_file_is_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("at-key-{}", std::process::id()));
        let p = dir.join("dashboard.key");
        let a = load_or_create_key(&p).unwrap();
        let b = load_or_create_key(&p).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_key_file_is_replaced() {
        let dir = std::env::temp_dir().join(format!("at-key-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("dashboard.key");
        std::fs::write(&p, "").unwrap();
        let a = load_or_create_key(&p).unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(load_or_create_key(&p).unwrap(), a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq("abcd", "abcd"));
        assert!(!ct_eq("abcd", "abce"));
        assert!(!ct_eq("abc", "abcd"));
    }
}
