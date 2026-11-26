//! Brings an AI agent's terminal forward (a click on its notice): the tmux pane it runs in, the
//! kitty tab and pane, and the desktop window that shows them.

use crate::sources::terminal::{kitty, procfs, tmux};
use anyhow::{Result, bail};
use zbus::Connection;

pub async fn agent(conn: &Connection, pid: u32) -> Result<()> {
    let table = procfs::ProcTable::read();
    let mut chain = ancestors(&table, pid);
    if chain.is_empty() {
        bail!("process {pid} has ended");
    }
    // In tmux: show its pane, then go on from the terminal attached to that session.
    let in_tmux = chain
        .iter()
        .any(|p| table.get(*p).is_some_and(|s| s.comm.starts_with("tmux")));
    if in_tmux && let Some(client) = tmux_select(&chain).await {
        chain = ancestors(&table, client);
    }
    let mut caption = None;
    for p in &chain {
        if table.get(*p).is_none_or(|s| s.comm != "kitty") {
            continue;
        }
        if let Some(sock) = kitty::socket_for(*p as i64)
            && let Ok(osws) = kitty::ls(&sock).await
            && let Some(w) = osws
                .iter()
                .flat_map(|o| &o.tabs)
                .flat_map(|t| &t.windows)
                .find(|w| {
                    w.foreground_processes
                        .iter()
                        .any(|f| chain.contains(&f.pid))
                })
        {
            // The tab and pane; the window is raised below even if kitty refuses.
            if kitty::focus_window(&sock, w.id).await.is_ok() {
                caption = Some(w.title.clone());
            }
        }
        break;
    }
    crate::sources::kwin::activate(conn, &chain, caption.as_deref()).await
}

/// A process and its parents, nearest first.
fn ancestors(table: &procfs::ProcTable, pid: u32) -> Vec<u32> {
    let mut out = vec![];
    let mut cur = pid;
    while cur > 1 && out.len() < 64 {
        let Some(s) = table.get(cur) else { break };
        out.push(cur);
        cur = s.ppid;
    }
    out
}

/// Selects the tmux pane one of `chain` runs in; the pid of a client showing its session.
async fn tmux_select(chain: &[u32]) -> Option<u32> {
    let out = tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_pid}\t#{session_id}\t#{window_id}\t#{pane_id}",
    ])
    .await?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (session, window, pane) = text.lines().find_map(|l| {
        let mut f = l.split('\t');
        let pid: u32 = f.next()?.parse().ok()?;
        chain.contains(&pid).then(|| {
            Some((
                f.next()?.to_string(),
                f.next()?.to_string(),
                f.next()?.to_string(),
            ))
        })?
    })?;
    tmux(&["select-window", "-t", &window]).await?;
    tmux(&["select-pane", "-t", &pane]).await?;
    let out = tmux(&["list-clients", "-F", "#{client_pid}\t#{session_id}"]).await?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let (pid, s) = l.split_once('\t')?;
        (s == session).then(|| pid.parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(pid: u32, ppid: u32, comm: &str) -> procfs::Stat {
        procfs::Stat {
            pid,
            ppid,
            comm: comm.into(),
            ..Default::default()
        }
    }

    #[test]
    fn ancestors_run_from_the_process_up_to_init() {
        let t = procfs::ProcTable::from_procs(vec![
            stat(1, 0, "systemd"),
            stat(900, 1, "kitty"),
            stat(901, 900, "zsh"),
            stat(950, 901, "claude"),
        ]);
        assert_eq!(ancestors(&t, 950), vec![950, 901, 900]);
        assert!(ancestors(&t, 4242).is_empty());
    }
}
