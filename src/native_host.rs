//! Native-messaging host. Browsers start it with pipes on stdin/stdout (4-byte native-endian
//! length + JSON per message): Chromium-family browsers as "arch-tracker chrome-extension://<id>/",
//! Firefox-family ones as "arch-tracker <manifest path> <add-on id>". Each message is forwarded
//! as one JSON line to the daemon's ingest socket.

use crate::sources::browser::MAX_MESSAGE;
use crate::util;
use anyhow::Result;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// The connection to the daemon, re-opened (hello first) whenever the daemon restarted.
struct Daemon {
    path: PathBuf,
    hello: String,
    sock: Option<UnixStream>,
}

impl Daemon {
    fn connect(&self) -> Option<UnixStream> {
        let mut s = UnixStream::connect(&self.path).ok()?;
        s.write_all(format!("{}\n", self.hello).as_bytes()).ok()?;
        Some(s)
    }

    /// Sends one line. A daemon restart only shows up as a failed write on the old socket, so on
    /// failure this reconnects and sends the same line once more instead of losing it.
    fn send(&mut self, line: &[u8]) {
        for _ in 0..2 {
            if self.sock.is_none() {
                self.sock = self.connect();
            }
            let Some(s) = self.sock.as_mut() else {
                return; // daemon not running; the next message tries again
            };
            if s.write_all(line).is_ok() {
                return;
            }
            self.sock = None;
        }
    }
}

pub fn run(origin: &str) -> Result<()> {
    let browser_pid = unsafe { libc::getppid() };
    let hello = serde_json::json!({"type": "hello", "browser_pid": browser_pid, "origin": origin})
        .to_string();
    let mut daemon = Daemon {
        path: util::ingest_socket(),
        hello,
        sock: None,
    };
    daemon.sock = daemon.connect();
    let mut stdin = std::io::stdin().lock();
    loop {
        let mut len = [0u8; 4];
        if stdin.read_exact(&mut len).is_err() {
            return Ok(()); // browser closed the pipe
        }
        let n = u32::from_ne_bytes(len) as usize;
        if n > MAX_MESSAGE {
            // The daemon would skip it anyway; drop it here and keep the port open.
            std::io::copy(&mut (&mut stdin).take(n as u64), &mut std::io::sink())?;
            continue;
        }
        let mut buf = vec![0u8; n + 1];
        stdin.read_exact(&mut buf[..n])?;
        buf[n] = b'\n';
        daemon.send(&buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    /// Lines the client already sent on its (already queued) connection; fails instead of hanging.
    fn read_lines(listener: &UnixListener, n: usize) -> (UnixStream, Vec<String>) {
        listener.set_nonblocking(true).unwrap();
        let (s, _) = listener.accept().expect("no connection from the host");
        s.set_nonblocking(false).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let lines = (0..n)
            .map(|_| {
                let mut l = String::new();
                r.read_line(&mut l).unwrap();
                l.trim_end().to_string()
            })
            .collect();
        (s, lines)
    }

    #[test]
    fn resends_after_daemon_restart() {
        let dir = std::env::temp_dir().join(format!("arch-tracker-nh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ingest.sock");
        let _ = std::fs::remove_file(&path);
        let first = UnixListener::bind(&path).unwrap();
        let mut d = Daemon {
            path: path.clone(),
            hello: "hello".into(),
            sock: None,
        };
        d.send(b"one\n");
        let (conn, lines) = read_lines(&first, 2);
        assert_eq!(lines, ["hello", "one"]);

        // The daemon restarts: old connection and listener gone, a new one on the same path.
        drop(conn);
        drop(first);
        std::fs::remove_file(&path).unwrap();
        let second = UnixListener::bind(&path).unwrap();
        d.send(b"two\n");
        let (_conn, lines) = read_lines(&second, 2);
        assert_eq!(lines, ["hello", "two"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
