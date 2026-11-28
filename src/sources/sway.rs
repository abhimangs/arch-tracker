//! Sway window source, over its IPC socket (the i3 protocol): window, workspace and output events
//! say when something changed, and the layout tree (debounced) becomes the same snapshot the KWin
//! script sends. Sway marks each window visible or not itself (tabs and stacks included).

use super::kwin::{Screen, Snapshot, Win};
use crate::daemon::Event;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc::UnboundedSender;

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;
const SETTLE: Duration = Duration::from_millis(150);
const KEEPALIVE: Duration = Duration::from_secs(60);

pub fn spawn(sock: PathBuf, tx: UnboundedSender<Event>) {
    tokio::spawn(async move {
        let mut wait = Duration::from_secs(1);
        loop {
            match run(&sock, &tx).await {
                Ok(()) => return,
                Err(e) => tracing::warn!("Sway window tracking: {e:#}"),
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(30));
        }
    });
}

async fn write_msg(s: &mut UnixStream, kind: u32, payload: &str) -> Result<()> {
    let mut msg = Vec::with_capacity(14 + payload.len());
    msg.extend_from_slice(MAGIC);
    msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    msg.extend_from_slice(&kind.to_ne_bytes());
    msg.extend_from_slice(payload.as_bytes());
    s.write_all(&msg).await?;
    Ok(())
}

/// One message: its type (events have the high bit set) and payload.
async fn read_msg(s: &mut UnixStream) -> Result<(u32, Vec<u8>)> {
    let mut head = [0u8; 14];
    s.read_exact(&mut head).await?;
    if &head[..6] != MAGIC {
        bail!("not an i3-ipc message");
    }
    let len = u32::from_ne_bytes(head[6..10].try_into()?) as usize;
    let kind = u32::from_ne_bytes(head[10..14].try_into()?);
    if len > 64 << 20 {
        bail!("sway sent {len} bytes");
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await?;
    Ok((kind, body))
}

/// A request and its answer, on a connection of its own.
pub async fn call(sock: &Path, kind: u32, payload: &str) -> Result<Value> {
    let fut = async {
        let mut s = UnixStream::connect(sock).await?;
        write_msg(&mut s, kind, payload).await?;
        let (_, body) = read_msg(&mut s).await?;
        anyhow::Ok(serde_json::from_slice(&body)?)
    };
    tokio::time::timeout(Duration::from_secs(2), fut)
        .await
        .context("sway did not answer")?
}

async fn run(sock: &Path, tx: &UnboundedSender<Event>) -> Result<()> {
    let mut events = UnixStream::connect(sock)
        .await
        .context("connecting to sway's socket")?;
    write_msg(&mut events, SUBSCRIBE, r#"["window","workspace","output"]"#).await?;
    let (_, ok) = read_msg(&mut events).await?;
    if !String::from_utf8_lossy(&ok).contains("true") {
        bail!("sway refused the subscription");
    }
    // Events are read by a task of their own: read_msg is not cancel-safe, and dropping it
    // halfway through a message (when the settle timer wins the select) would lose its start
    // and leave every later read out of step.
    let (seen, mut changes) = tokio::sync::mpsc::channel::<()>(1);
    let mut reader = tokio::spawn(async move {
        loop {
            if let Err(e) = read_msg(&mut events).await {
                return e;
            }
            // A change is already waiting: one more says nothing new.
            if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) = seen.try_send(()) {
                return anyhow::anyhow!("stopped");
            }
        }
    });
    let _stop = AbortOnDrop(reader.abort_handle());
    send(sock, tx).await?;
    tracing::info!("Sway window tracking started");
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
            m = changes.recv() => {
                if m.is_none() {
                    let e = (&mut reader).await.unwrap_or_else(|e| anyhow::anyhow!("{e}"));
                    return Err(e.context("reading sway's events"));
                }
                if due.is_none() {
                    due = Some(tokio::time::Instant::now() + SETTLE);
                }
            }
            _ = settle => {
                due = None;
                send(sock, tx).await?;
            }
            _ = keepalive.tick() => send(sock, tx).await?,
        }
        if tx.is_closed() {
            return Ok(());
        }
    }
}

/// Stops the event reader when run() returns, whichever way.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn send(sock: &Path, tx: &UnboundedSender<Event>) -> Result<()> {
    let tree = call(sock, GET_TREE, "").await?;
    let _ = tx.send(Event::Kwin(Box::new(build(&tree))));
    Ok(())
}

struct Found<'a> {
    node: &'a Value,
    output: String,
    floating: bool,
}

fn walk<'a>(n: &'a Value, output: &str, floating: bool, out: &mut Vec<Found<'a>>) {
    let output = if n["type"] == "output" {
        n["name"].as_str().unwrap_or("")
    } else {
        output
    };
    // The scratchpad's hidden windows live under the "__i3" output.
    if output == "__i3" {
        return;
    }
    let kids = n["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let floats = n["floating_nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if n["pid"].is_i64() && kids.is_empty() && floats.is_empty() {
        out.push(Found {
            node: n,
            output: output.to_string(),
            floating,
        });
    }
    for k in kids {
        walk(k, output, floating, out);
    }
    for k in floats {
        walk(k, output, true, out);
    }
}

fn rect(v: &Value) -> (i64, i64, i64, i64) {
    let n = |k: &str| v[k].as_i64().unwrap_or(0);
    (n("x"), n("y"), n("width"), n("height"))
}

/// The snapshot for sway's layout tree: tiled windows, then floating ones, then fullscreen ones.
pub fn build(tree: &Value) -> Snapshot {
    let mut found = vec![];
    walk(tree, "", false, &mut found);
    found.sort_by_key(|f| {
        (
            f.node["fullscreen_mode"].as_i64().unwrap_or(0) > 0,
            f.floating,
        )
    });
    let mut active = None;
    let windows = found
        .iter()
        .map(|f| {
            let n = f.node;
            let id = n["id"].as_i64().unwrap_or(0).to_string();
            if n["focused"] == true {
                active = Some(id.clone());
            }
            let cls = n["app_id"]
                .as_str()
                .or_else(|| n["window_properties"]["class"].as_str())
                .unwrap_or("")
                .to_string();
            let (x, y, w, h) = rect(&n["rect"]);
            Win {
                id,
                pid: n["pid"].as_i64().unwrap_or(0),
                df: cls.clone(),
                cls,
                cap: n["name"].as_str().unwrap_or("").to_string(),
                hidden: n["visible"] != true,
                x,
                y,
                w,
                h,
                all_desks: true,
                out: f.output.clone(),
                fs: n["fullscreen_mode"].as_i64().unwrap_or(0) > 0,
                normal: true,
                ..Default::default()
            }
        })
        .collect();
    let screens = tree["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["type"] == "output" && o["name"] != "__i3" && o["active"] != false)
        .map(|o| {
            let (x, y, w, h) = rect(&o["rect"]);
            Screen { x, y, w, h }
        })
        .collect();
    Snapshot {
        active,
        desktop: String::new(),
        activity: String::new(),
        screens,
        windows,
    }
}

/// Brings forward the window of the nearest of `pids` (see wm::pick).
pub async fn focus(sock: &Path, pids: &[u32], caption: Option<&str>) -> Result<()> {
    let tree = call(sock, GET_TREE, "").await?;
    let mut found = vec![];
    walk(&tree, "", false, &mut found);
    let wins: Vec<(String, i64, String)> = found
        .iter()
        .map(|f| {
            (
                f.node["id"].as_i64().unwrap_or(0).to_string(),
                f.node["pid"].as_i64().unwrap_or(0),
                f.node["name"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let Some(id) = super::wm::pick(
        wins.iter().map(|(i, p, t)| (i.as_str(), *p, t.as_str())),
        pids,
        caption,
    ) else {
        bail!("no sway window for the agent");
    };
    let r = call(sock, RUN_COMMAND, &format!("[con_id={id}] focus")).await?;
    if r[0]["success"] != true {
        bail!("sway said: {r}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = r#"{"id":1,"type":"root","name":"root","rect":{"x":0,"y":0,"width":1920,"height":1080},
      "nodes":[
        {"id":2147483647,"type":"output","name":"__i3","nodes":[
          {"id":2147483646,"type":"workspace","name":"__i3_scratch","nodes":[],"floating_nodes":[
            {"id":40,"type":"floating_con","name":"Notes","app_id":"notes","pid":400,"visible":false,
             "rect":{"x":0,"y":0,"width":10,"height":10},"nodes":[],"floating_nodes":[]}]}]},
        {"id":3,"type":"output","name":"HEADLESS-1","active":true,
         "rect":{"x":0,"y":0,"width":1920,"height":1080},"nodes":[
          {"id":4,"type":"workspace","name":"1","rect":{"x":0,"y":0,"width":1920,"height":1080},
           "nodes":[
            {"id":5,"type":"con","name":"claude","app_id":"foot","pid":300,"visible":true,
             "focused":true,"fullscreen_mode":0,"rect":{"x":0,"y":0,"width":960,"height":1080},
             "nodes":[],"floating_nodes":[]},
            {"id":6,"type":"con","name":null,"layout":"tabbed","rect":{"x":960,"y":0,"width":960,"height":1080},
             "nodes":[
              {"id":7,"type":"con","name":"Files","app_id":"org.gnome.Nautilus","pid":301,
               "visible":true,"focused":false,"rect":{"x":960,"y":24,"width":960,"height":1056},
               "nodes":[],"floating_nodes":[]},
              {"id":8,"type":"con","name":"Docs","app_id":null,"window_properties":{"class":"firefox"},
               "pid":302,"visible":false,"focused":false,
               "rect":{"x":960,"y":24,"width":960,"height":1056},"nodes":[],"floating_nodes":[]}],
             "floating_nodes":[]}],
           "floating_nodes":[
            {"id":9,"type":"floating_con","name":"Calc","app_id":"qalculate","pid":303,"visible":true,
             "focused":false,"rect":{"x":100,"y":100,"width":400,"height":300},"nodes":[],"floating_nodes":[]}]},
          {"id":10,"type":"workspace","name":"2","nodes":[
            {"id":11,"type":"con","name":"film","app_id":"mpv","pid":304,"visible":false,
             "fullscreen_mode":1,"rect":{"x":0,"y":0,"width":1920,"height":1080},"nodes":[],"floating_nodes":[]}],
           "floating_nodes":[]}]}]}"#;

    #[test]
    fn sway_says_which_windows_show_and_floating_ones_stack_on_top() {
        let tree: Value = serde_json::from_str(TREE).unwrap();
        let s = build(&tree);
        assert_eq!(s.active.as_deref(), Some("5"));
        let ids: Vec<&str> = s.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["5", "7", "8", "9", "11"]);
        let cls: Vec<&str> = s.windows.iter().map(|w| w.cls.as_str()).collect();
        assert_eq!(
            cls,
            ["foot", "org.gnome.Nautilus", "firefox", "qalculate", "mpv"]
        );
        assert_eq!(s.screens.len(), 1);
        // The terminal has focus; Files and the floating calculator show, the tab behind Files
        // and the video on the other workspace don't.
        let vis: Vec<&str> = s
            .visible(0.05, 1000)
            .iter()
            .map(|w| w.cls.as_str())
            .collect();
        assert_eq!(vis, ["qalculate", "org.gnome.Nautilus"]);
        assert_eq!(s.windows[0].out, "HEADLESS-1");
    }

    #[tokio::test]
    async fn the_socket_is_spoken_to_like_sway_does() {
        use tokio::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("at-sway-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sway-ipc.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let commands = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let seen = commands.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let seen = seen.clone();
                tokio::spawn(async move {
                    while let Ok((kind, body)) = read_msg(&mut s).await {
                        let body = String::from_utf8_lossy(&body).to_string();
                        match kind {
                            SUBSCRIBE => {
                                write_msg(&mut s, SUBSCRIBE, r#"{"success":true}"#)
                                    .await
                                    .unwrap();
                                write_msg(&mut s, 0x8000_0003, r#"{"change":"focus"}"#)
                                    .await
                                    .unwrap();
                            }
                            GET_TREE => write_msg(&mut s, GET_TREE, TREE).await.unwrap(),
                            _ => {
                                seen.lock().unwrap().push(body);
                                write_msg(&mut s, RUN_COMMAND, r#"[{"success":true}]"#)
                                    .await
                                    .unwrap();
                            }
                        }
                    }
                });
            }
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s2 = sock.clone();
        tokio::spawn(async move { run(&s2, &tx).await });
        for _ in 0..2 {
            let m = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .unwrap();
            let Some(Event::Kwin(s)) = m else {
                panic!("no snapshot")
            };
            assert_eq!(s.active.as_deref(), Some("5"));
        }
        focus(&sock, &[351, 300], Some("claude")).await.unwrap();
        assert_eq!(*commands.lock().unwrap(), ["[con_id=5] focus"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
