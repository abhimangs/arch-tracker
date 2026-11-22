//! kitty remote control, spoken directly over its socket (never by running the kitty binary,
//! which the user's shell aliases to --single-instance).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, Deserialize, Clone)]
pub struct OsWindow {
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Tab {
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub windows: Vec<KWindow>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KWindow {
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub foreground_processes: Vec<Fg>,
    // NOTE: kitty also sends "env"; it is deliberately not deserialized.
}

#[derive(Debug, Deserialize, Clone)]
pub struct Fg {
    pub pid: u32,
    #[serde(default)]
    pub cmdline: Vec<String>,
    #[serde(default)]
    pub cwd: String,
}

/// Finds the remote-control socket of the kitty instance with this pid.
pub fn socket_for(pid: i64) -> Option<PathBuf> {
    let mut dirs = vec![PathBuf::from("/tmp")];
    if let Some(r) = std::env::var_os("XDG_RUNTIME_DIR") {
        dirs.push(PathBuf::from(r));
    }
    let suffix = format!("-{pid}");
    let mut any = vec![];
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("kitty") {
                continue;
            }
            let is_sock = e.file_type().map(|t| {
                use std::os::unix::fs::FileTypeExt;
                t.is_socket()
            });
            if !matches!(is_sock, Ok(true)) {
                continue;
            }
            if name.ends_with(&suffix) {
                return Some(e.path());
            }
            any.push(e.path());
        }
    }
    (any.len() == 1).then(|| any.remove(0))
}

/// "kitty @ ls" over the socket.
pub async fn ls(sock: &Path) -> Result<Vec<OsWindow>> {
    let fut = async {
        let mut s = UnixStream::connect(sock).await?;
        let cmd = serde_json::json!({"cmd": "ls", "version": [0, 26, 0], "no_response": false, "payload": {}});
        s.write_all(format!("\x1bP@kitty-cmd{cmd}\x1b\\").as_bytes())
            .await?;
        let mut buf = Vec::with_capacity(16 * 1024);
        let mut chunk = [0u8; 8192];
        loop {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.ends_with(b"\x1b\\") {
                break;
            }
            if buf.len() > 8 * 1024 * 1024 {
                bail!("kitty response too large");
            }
        }
        anyhow::Ok(buf)
    };
    let buf = tokio::time::timeout(Duration::from_millis(800), fut)
        .await
        .context("kitty did not answer")??;
    let text = String::from_utf8_lossy(&buf);
    let body = text
        .trim_start_matches("\x1bP@kitty-cmd")
        .trim_end_matches("\x1b\\");
    #[derive(Deserialize)]
    struct Resp {
        ok: bool,
        #[serde(default)]
        data: Option<String>,
        #[serde(default)]
        error: Option<String>,
    }
    let r: Resp = serde_json::from_str(body).context("parsing kitty response")?;
    if !r.ok {
        bail!("kitty refused: {}", r.error.unwrap_or_default());
    }
    Ok(serde_json::from_str(r.data.as_deref().unwrap_or("[]"))?)
}

/// The kitty window (pane) a KWin window shows: the focused OS window if focused, otherwise the
/// OS window whose active pane title matches the KWin caption.
pub fn pick(osws: &[OsWindow], focused: bool, caption: &str) -> Option<KWindow> {
    let active_pane = |o: &OsWindow| -> Option<KWindow> {
        let tab = o.tabs.iter().find(|t| t.is_active).or(o.tabs.first())?;
        tab.windows
            .iter()
            .find(|w| w.is_active)
            .or(tab.windows.first())
            .cloned()
    };
    if osws.len() == 1 {
        return active_pane(&osws[0]);
    }
    if focused && let Some(o) = osws.iter().find(|o| o.is_focused) {
        return active_pane(o);
    }
    osws.iter()
        .filter_map(active_pane)
        .find(|w| w.title == caption)
        .or_else(|| osws.iter().find(|o| o.is_active).and_then(active_pane))
}
