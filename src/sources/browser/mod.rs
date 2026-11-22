//! Browser tabs: our extension (exact) via native messaging, with Plasma Browser Integration and
//! the SNSS session file as fallbacks.

pub mod pbi;
pub mod snss;

use crate::daemon::Event;
use crate::util;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct ExtTab {
    pub id: i64,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub audible: bool,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub split: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct ExtWindow {
    pub id: i64,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub incognito: bool,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub tabs: Vec<ExtTab>,
}

impl ExtWindow {
    pub fn active_tab(&self) -> Option<&ExtTab> {
        self.tabs.iter().find(|t| t.active)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtMsg {
    Hello {
        #[serde(default)]
        browser_pid: u32,
    },
    Snapshot {
        #[serde(default)]
        windows: Vec<ExtWindow>,
    },
}

/// Accepts native-host connections; each one is a browser instance.
pub fn spawn_ingest(tx: UnboundedSender<Event>) -> Result<()> {
    let path = util::ingest_socket();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    tokio::spawn(async move {
        let mut next_id = 1u64;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let conn = next_id;
            next_id += 1;
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    match serde_json::from_str::<ExtMsg>(&line) {
                        Ok(msg) => {
                            let _ = tx.send(Event::BrowserExt { conn, msg });
                        }
                        Err(e) => tracing::debug!("ignoring extension message: {e}"),
                    }
                }
                let _ = tx.send(Event::BrowserGone(conn));
            });
        }
    });
    Ok(())
}
