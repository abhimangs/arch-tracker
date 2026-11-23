//! Serves the control socket for the CLI.

use super::Event;
use crate::ipc::Request;
use crate::sources::browser::{Line, read_line_capped};
use crate::util;
use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

/// Requests are a line of JSON; nothing legitimate comes close to this.
const MAX_REQUEST: usize = 64 << 10;

/// Requests the control socket answers itself, without the tracker.
#[derive(serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Local {
    /// A one-time dashboard sign-in URL (for `arch-tracker open`).
    LoginUrl {
        #[serde(default)]
        route: String,
    },
}

fn local(line: &str) -> Option<serde_json::Value> {
    let Local::LoginUrl { route } = serde_json::from_str::<Local>(line).ok()?;
    Some(
        match crate::web::auth::login_url(crate::web::auth::port(), &route) {
            Ok(url) => serde_json::json!({ "url": url }),
            Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
        },
    )
}

pub fn spawn(tx: mpsc::UnboundedSender<Event>) -> Result<()> {
    let path = util::control_socket();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    // E.g. EMFILE: retrying at once would spin a core.
                    tracing::warn!("control socket: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut reader = BufReader::new(r);
                let mut buf = Vec::new();
                loop {
                    match read_line_capped(&mut reader, &mut buf, MAX_REQUEST).await {
                        Ok(Line::Full) => {}
                        Ok(Line::TooLong) => {
                            let _ = w.write_all(b"{\"error\":\"request too long\"}\n").await;
                            continue;
                        }
                        _ => break,
                    }
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if let Some(resp) = local(&line) {
                        let mut out = resp.to_string();
                        out.push('\n');
                        if w.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    let resp = match serde_json::from_str::<Request>(&line) {
                        Ok(req) => {
                            let (otx, orx) = oneshot::channel();
                            if tx.send(Event::Control(req, otx)).is_err() {
                                break;
                            }
                            orx.await
                                .unwrap_or_else(|_| serde_json::json!({"error": "daemon stopping"}))
                        }
                        Err(e) => serde_json::json!({"error": format!("bad request: {e}")}),
                    };
                    let mut out = resp.to_string();
                    out.push('\n');
                    if w.write_all(out.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Ok(())
}
