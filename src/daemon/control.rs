//! Serves the control socket for the CLI.

use super::Event;
use crate::ipc::Request;
use crate::util;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

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
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
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
