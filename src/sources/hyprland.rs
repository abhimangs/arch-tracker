//! Hyprland window source: its request socket lists the windows and monitors, and its event socket
//! says when something changed. Each change (debounced) becomes the same snapshot the KWin script
//! sends, with the windows on the monitors' current workspaces as the ones you can see.

use super::kwin::{Screen, Snapshot, Win};
use crate::daemon::Event;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::UnboundedSender;

/// Changes closer together than this make one snapshot.
const SETTLE: Duration = Duration::from_millis(150);
/// A snapshot at least this often, so the tracker never thinks the source is gone.
const KEEPALIVE: Duration = Duration::from_secs(60);

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct WsRef {
    pub id: i64,
    pub name: String,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct Client {
    pub address: String,
    #[serde(default = "yes")]
    pub mapped: bool,
    pub hidden: bool,
    pub at: [i64; 2],
    pub size: [i64; 2],
    pub workspace: WsRef,
    pub floating: bool,
    pub pinned: bool,
    pub monitor: i64,
    pub class: String,
    pub title: String,
    pub pid: i64,
    /// A number (0 none, 1 maximized, 2 fullscreen) in newer versions, a bool in older ones.
    pub fullscreen: serde_json::Value,
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i64,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub scale: f64,
    pub transform: i64,
    #[serde(rename = "activeWorkspace")]
    pub active_workspace: WsRef,
    #[serde(rename = "specialWorkspace")]
    pub special_workspace: WsRef,
    pub disabled: bool,
}

/// Watches Hyprland for as long as the daemon runs, reconnecting when it restarts.
pub fn spawn(dir: PathBuf, tx: UnboundedSender<Event>) {
    tokio::spawn(async move {
        let mut wait = Duration::from_secs(1);
        loop {
            match run(&dir, &tx).await {
                Ok(()) => return,
                Err(e) => tracing::warn!("Hyprland window tracking: {e:#}"),
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(30));
        }
    });
}

async fn run(dir: &Path, tx: &UnboundedSender<Event>) -> Result<()> {
    let events = UnixStream::connect(dir.join(".socket2.sock"))
        .await
        .context("connecting to Hyprland's event socket")?;
    let mut lines = BufReader::new(events).lines();
    send(dir, tx).await?;
    tracing::info!("Hyprland window tracking started");
    let mut due: Option<tokio::time::Instant> = None;
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await;
    loop {
        let settle = async {
            match due {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { bail!("Hyprland closed its event socket") };
                if relevant(&line) && due.is_none() {
                    due = Some(tokio::time::Instant::now() + SETTLE);
                }
            }
            _ = settle => {
                due = None;
                send(dir, tx).await?;
            }
            _ = keepalive.tick() => send(dir, tx).await?,
        }
        if tx.is_closed() {
            return Ok(());
        }
    }
}

/// Events that change which windows there are, where, or which one has focus.
fn relevant(line: &str) -> bool {
    let event = line.split(">>").next().unwrap_or("");
    matches!(
        event,
        "activewindowv2"
            | "openwindow"
            | "closewindow"
            | "movewindowv2"
            | "windowtitlev2"
            | "workspacev2"
            | "focusedmonv2"
            | "activespecialv2"
            | "fullscreen"
            | "changefloatingmode"
            | "moveworkspacev2"
            | "monitoraddedv2"
            | "monitorremovedv2"
            | "minimized"
            | "pin"
    ) || event == "activewindow"
        || event == "workspace"
}

async fn send(dir: &Path, tx: &UnboundedSender<Event>) -> Result<()> {
    let snap = snapshot(dir).await?;
    let _ = tx.send(Event::Kwin(Box::new(snap)));
    Ok(())
}

/// One request on Hyprland's request socket ("j/clients"); its whole answer.
pub async fn request(dir: &Path, what: &str) -> Result<Vec<u8>> {
    let fut = async {
        let mut s = UnixStream::connect(dir.join(".socket.sock")).await?;
        s.write_all(what.as_bytes()).await?;
        let mut buf = vec![];
        s.read_to_end(&mut buf).await?;
        anyhow::Ok(buf)
    };
    tokio::time::timeout(Duration::from_secs(2), fut)
        .await
        .with_context(|| format!("Hyprland did not answer {what}"))?
}

pub async fn snapshot(dir: &Path) -> Result<Snapshot> {
    let clients: Vec<Client> = serde_json::from_slice(&request(dir, "j/clients").await?)
        .context("reading Hyprland's window list")?;
    let monitors: Vec<Monitor> = serde_json::from_slice(&request(dir, "j/monitors").await?)
        .context("reading Hyprland's monitors")?;
    let active: Client =
        serde_json::from_slice(&request(dir, "j/activewindow").await?).unwrap_or_default();
    Ok(build(&clients, &monitors, &active.address))
}

fn fullscreen(v: &serde_json::Value) -> bool {
    v.as_bool().unwrap_or(false) || v.as_i64().is_some_and(|n| n > 0)
}

/// The snapshot for Hyprland's windows and monitors: stacked tiled, then floating, then
/// fullscreen, then the special workspace's, each least recently focused first.
pub fn build(clients: &[Client], monitors: &[Monitor], active: &str) -> Snapshot {
    let monitors: Vec<&Monitor> = monitors.iter().filter(|m| !m.disabled).collect();
    let shown: Vec<i64> = monitors
        .iter()
        .flat_map(|m| [m.active_workspace.id, m.special_workspace.id])
        .filter(|id| *id != 0)
        .collect();
    let special = |c: &Client| c.workspace.id < 0;
    let mut order: Vec<&Client> = clients.iter().filter(|c| c.mapped).collect();
    order.sort_by_key(|c| {
        (
            special(c),
            fullscreen(&c.fullscreen),
            c.floating || c.pinned,
            std::cmp::Reverse(c.focus_history_id),
        )
    });
    let windows = order
        .into_iter()
        .map(|c| {
            let on = shown.contains(&c.workspace.id) || c.pinned;
            Win {
                id: c.address.clone(),
                pid: c.pid,
                cls: c.class.clone(),
                df: c.class.clone(),
                cap: c.title.clone(),
                hidden: c.hidden,
                x: c.at[0],
                y: c.at[1],
                w: c.size[0],
                h: c.size[1],
                desks: vec![format!("ws:{}", c.workspace.id)],
                all_desks: on,
                out: monitors
                    .iter()
                    .find(|m| m.id == c.monitor)
                    .map(|m| m.name.clone())
                    .unwrap_or_default(),
                fs: fullscreen(&c.fullscreen),
                normal: true,
                ..Default::default()
            }
        })
        .collect::<Vec<_>>();
    let screens = monitors
        .iter()
        .map(|m| {
            let scale = if m.scale > 0.0 { m.scale } else { 1.0 };
            let (w, h) = (
                (m.width as f64 / scale).round() as i64,
                (m.height as f64 / scale).round() as i64,
            );
            let (w, h) = if m.transform % 2 == 1 { (h, w) } else { (w, h) };
            Screen {
                x: m.x,
                y: m.y,
                w,
                h,
            }
        })
        .collect();
    Snapshot {
        active: windows
            .iter()
            .any(|w| w.id == active && !active.is_empty())
            .then(|| active.to_string()),
        desktop: "current".into(),
        activity: String::new(),
        screens,
        windows,
    }
}

/// Brings forward the window of the nearest of `pids` (see wm::pick).
pub async fn focus(dir: &Path, pids: &[u32], caption: Option<&str>) -> Result<()> {
    let clients: Vec<Client> = serde_json::from_slice(&request(dir, "j/clients").await?)?;
    let Some(address) = super::wm::pick(
        clients
            .iter()
            .map(|c| (c.address.as_str(), c.pid, c.title.as_str())),
        pids,
        caption,
    ) else {
        bail!("no Hyprland window for the agent");
    };
    let r = request(dir, &format!("dispatch focuswindow address:{address}")).await?;
    if r.trim_ascii() != b"ok" {
        bail!("Hyprland said: {}", String::from_utf8_lossy(&r));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const CLIENTS: &str = r#"[
      {"address":"0xa1","mapped":true,"hidden":false,"at":[0,0],"size":[1280,1440],
       "workspace":{"id":1,"name":"1"},"floating":false,"pseudo":false,"monitor":0,
       "class":"kitty","title":"claude","initialClass":"kitty","initialTitle":"kitty","pid":900,
       "xwayland":false,"pinned":false,"fullscreen":0,"fullscreenClient":0,"grouped":[],
       "tags":[],"swallowing":"0x0","focusHistoryID":0,"inhibitingIdle":false},
      {"address":"0xa2","mapped":true,"hidden":false,"at":[1280,0],"size":[1280,1440],
       "workspace":{"id":1,"name":"1"},"floating":false,"monitor":0,"class":"brave-browser",
       "title":"Docs","pid":910,"pinned":false,"fullscreen":0,"focusHistoryID":1},
      {"address":"0xa3","mapped":true,"hidden":false,"at":[200,200],"size":[600,400],
       "workspace":{"id":1,"name":"1"},"floating":true,"monitor":0,"class":"qalculate",
       "title":"Calc","pid":920,"pinned":false,"fullscreen":false,"focusHistoryID":2},
      {"address":"0xa4","mapped":true,"hidden":false,"at":[0,0],"size":[2560,1440],
       "workspace":{"id":2,"name":"2"},"floating":false,"monitor":0,"class":"mpv",
       "title":"film.mkv","pid":930,"pinned":false,"fullscreen":2,"focusHistoryID":3}
    ]"#;
    pub(crate) const MONITORS: &str = r#"[
      {"id":0,"name":"DP-1","description":"x","width":3200,"height":1800,"refreshRate":60.0,
       "x":0,"y":0,"activeWorkspace":{"id":1,"name":"1"},"specialWorkspace":{"id":0,"name":""},
       "reserved":[0,0,0,0],"scale":1.25,"transform":0,"focused":true,"dpmsStatus":true,
       "vrr":false,"disabled":false}
    ]"#;

    #[test]
    fn windows_on_current_workspaces_count_and_floating_ones_stack_on_top() {
        let clients: Vec<Client> = serde_json::from_str(CLIENTS).unwrap();
        let monitors: Vec<Monitor> = serde_json::from_str(MONITORS).unwrap();
        let s = build(&clients, &monitors, "0xa1");
        assert_eq!(s.active.as_deref(), Some("0xa1"));
        assert_eq!(s.screens.len(), 1);
        assert_eq!((s.screens[0].w, s.screens[0].h), (2560, 1440));
        let ids: Vec<&str> = s.windows.iter().map(|w| w.id.as_str()).collect();
        // Bottom to top: tiled (least recently focused first), floating, fullscreen.
        assert_eq!(ids, ["0xa2", "0xa1", "0xa3", "0xa4"]);
        assert_eq!(s.focused().map(|w| w.cls.as_str()), Some("kitty"));
        // The terminal, the browser beside it and the calculator over it are on screen; the
        // fullscreen video on workspace 2 is not.
        let vis: Vec<&str> = s
            .visible(0.05, 1000)
            .iter()
            .map(|w| w.cls.as_str())
            .collect();
        assert_eq!(vis, ["qalculate", "brave-browser"]);
        assert_eq!(s.windows[0].out, "DP-1");
        // No focused window (an empty workspace).
        assert_eq!(build(&clients, &monitors, "").active, None);
    }

    #[tokio::test]
    async fn the_sockets_are_spoken_to_like_hyprland_does() {
        use tokio::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("at-hypr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let req = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let ev = UnixListener::bind(dir.join(".socket2.sock")).unwrap();
        // The request socket: one request per connection, answered, then closed.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = req.accept().await.unwrap();
                let mut buf = vec![0u8; 256];
                let n = s.read(&mut buf).await.unwrap();
                let what = String::from_utf8_lossy(&buf[..n]).to_string();
                let answer = match what.as_str() {
                    "j/clients" => CLIENTS.to_string(),
                    "j/monitors" => MONITORS.to_string(),
                    "j/activewindow" => r#"{"address":"0xa2"}"#.to_string(),
                    _ => "ok".to_string(),
                };
                seen2.lock().unwrap().push(what);
                s.write_all(answer.as_bytes()).await.unwrap();
            }
        });
        tokio::spawn(async move {
            let (mut s, _) = ev.accept().await.unwrap();
            s.write_all(b"activewindowv2>>a1\nurgent>>a2\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let d = dir.clone();
        tokio::spawn(async move { run(&d, &tx).await });
        let first = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap();
        let Some(Event::Kwin(s)) = first else {
            panic!("no snapshot")
        };
        assert_eq!(s.active.as_deref(), Some("0xa2"));
        // The focus change brings one more.
        let next = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap();
        assert!(matches!(next, Some(Event::Kwin(_))));
        focus(&dir, &[950, 900], Some("claude")).await.unwrap();
        assert!(
            seen.lock()
                .unwrap()
                .contains(&"dispatch focuswindow address:0xa1".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
