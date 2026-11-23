//! Browser tabs: our extension (exact) via native messaging, with Plasma Browser Integration and
//! the SNSS session file as fallbacks.

pub mod pbi;
pub mod snss;

use crate::daemon::Event;
use crate::util;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc::UnboundedSender;

/// Longest message (one JSON line) accepted from a native host; longer ones are skipped.
/// A snapshot of hundreds of tabs is tens of kilobytes.
pub const MAX_MESSAGE: usize = 4 << 20;

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
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    // E.g. EMFILE: retrying at once would spin a core until a descriptor frees up.
                    tracing::warn!("browser ingest socket: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            let conn = next_id;
            next_id += 1;
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut buf = Vec::new();
                loop {
                    match read_line_capped(&mut reader, &mut buf, MAX_MESSAGE).await {
                        Ok(Line::Full) => match serde_json::from_slice::<ExtMsg>(&buf) {
                            Ok(msg) => {
                                let _ = tx.send(Event::BrowserExt { conn, msg });
                            }
                            Err(e) => tracing::debug!("ignoring extension message: {e}"),
                        },
                        Ok(Line::TooLong) => {
                            tracing::warn!("skipped a browser message over {MAX_MESSAGE} bytes")
                        }
                        Ok(Line::End) | Err(_) => break,
                    }
                }
                let _ = tx.send(Event::BrowserGone(conn));
            });
        }
    });
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Line {
    /// `buf` holds one line, without its newline.
    Full,
    /// The line was longer than the cap and was skipped; `buf` is empty.
    TooLong,
    End,
}

/// Reads one '\n'-terminated line into `buf`, holding at most `max` bytes in memory: a longer
/// line is consumed and dropped. A last line without a newline still counts.
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Line> {
    buf.clear();
    let mut too_long = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(match (too_long, buf.is_empty()) {
                (true, _) => Line::TooLong,
                (false, true) => Line::End,
                (false, false) => Line::Full,
            });
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let part = &chunk[..newline.unwrap_or(chunk.len())];
        if !too_long && buf.len() + part.len() > max {
            too_long = true;
            buf.clear();
            buf.shrink_to(0);
        }
        if !too_long {
            buf.extend_from_slice(part);
        }
        let used = newline.map_or(chunk.len(), |i| i + 1);
        r.consume(used);
        if newline.is_some() {
            return Ok(if too_long { Line::TooLong } else { Line::Full });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn lines(input: &[u8], max: usize) -> Vec<(Line, Vec<u8>)> {
        let mut r = BufReader::with_capacity(4, input);
        let mut buf = Vec::new();
        let mut out = vec![];
        loop {
            let l = read_line_capped(&mut r, &mut buf, max).await.unwrap();
            if l == Line::End {
                return out;
            }
            out.push((l, buf.clone()));
        }
    }

    #[tokio::test]
    async fn capped_lines() {
        let got = lines(b"{\"a\":1}\nthis line is too long\n\nlast", 10).await;
        assert_eq!(
            got,
            vec![
                (Line::Full, b"{\"a\":1}".to_vec()),
                (Line::TooLong, vec![]),
                (Line::Full, vec![]),
                (Line::Full, b"last".to_vec()),
            ]
        );
        assert_eq!(lines(b"", 10).await, vec![]);
        assert_eq!(
            lines(b"exactly10!\n", 10).await,
            vec![(Line::Full, b"exactly10!".to_vec())]
        );
        assert_eq!(
            lines(b"elevenbytes", 10).await,
            vec![(Line::TooLong, vec![])]
        );
    }

    #[test]
    fn parses_messages() {
        let hello: ExtMsg =
            serde_json::from_str(r#"{"type":"hello","browser_pid":42,"origin":"x"}"#).unwrap();
        assert!(matches!(hello, ExtMsg::Hello { browser_pid: 42 }));
        let snap: ExtMsg = serde_json::from_str(
            r#"{"type":"snapshot","windows":[{"id":1,"focused":true,"incognito":false,"type":"normal",
                "tabs":[{"id":3,"active":true,"audible":false,"muted":false,"host":"example.com","title":"Example","split":null}]}]}"#,
        )
        .unwrap();
        let ExtMsg::Snapshot { windows } = snap else {
            panic!("not a snapshot")
        };
        assert_eq!(windows[0].active_tab().unwrap().host, "example.com");
    }
}
