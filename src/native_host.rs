//! Native-messaging host: Brave starts "arch-tracker chrome-extension://<id>/" and talks to it on
//! stdin (4-byte length + JSON). Each message is forwarded as one JSON line to the daemon.

use crate::util;
use anyhow::Result;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

fn connect(hello: &str) -> Option<UnixStream> {
    let mut s = UnixStream::connect(util::ingest_socket()).ok()?;
    s.write_all(hello.as_bytes()).ok()?;
    s.write_all(b"\n").ok()?;
    Some(s)
}

pub fn run(origin: &str) -> Result<()> {
    let browser_pid = unsafe { libc::getppid() };
    let hello = serde_json::json!({"type": "hello", "browser_pid": browser_pid, "origin": origin})
        .to_string();
    let mut sock = connect(&hello);
    let mut stdin = std::io::stdin().lock();
    loop {
        let mut len = [0u8; 4];
        if stdin.read_exact(&mut len).is_err() {
            return Ok(()); // browser closed the pipe
        }
        let n = u32::from_ne_bytes(len) as usize;
        if n > 16 * 1024 * 1024 {
            return Ok(());
        }
        let mut buf = vec![0u8; n];
        stdin.read_exact(&mut buf)?;
        if sock.is_none() {
            sock = connect(&hello);
        }
        if let Some(s) = sock.as_mut()
            && (s.write_all(&buf).is_err() || s.write_all(b"\n").is_err())
        {
            sock = None; // daemon restarted; reconnect on the next message
        }
    }
}
