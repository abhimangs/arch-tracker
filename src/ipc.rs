//! CLI <-> daemon control socket: one JSON request per line, one JSON response per line.

use crate::util;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Pause until this time (ms); None = until resumed.
    Pause {
        until_ms: Option<i64>,
    },
    Resume,
    Reload,
}

pub async fn call<T: Serialize>(req: &T) -> Result<serde_json::Value> {
    let path = util::control_socket();
    let stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "connecting to {} (is the daemon running? try: systemctl --user status arch-tracker)",
            path.display()
        )
    })?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut reader = BufReader::new(r);
    let mut resp = String::new();
    reader.read_line(&mut resp).await?;
    if resp.is_empty() {
        bail!("daemon closed the connection");
    }
    Ok(serde_json::from_str(&resp)?)
}
