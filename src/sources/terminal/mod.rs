//! What runs inside terminal windows: program + project, and AI agent activity.

pub mod agents;
pub mod kitty;
pub mod procfs;
pub mod tools;

use crate::config::Tool;
use serde::Serialize;
use std::path::PathBuf;
use tools::Projects;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TermInfo {
    pub program: String,
    pub project: String,
    /// "kitty" (exact) or "proc" (best guess).
    pub source: &'static str,
}

/// Picks the main process among a pane's foreground processes: an AI tool if present, else the
/// first process whose parent is not itself in the foreground group (helpers are skipped).
fn primary(fg: &[(u32, Vec<String>, String)], tools: &[Tool]) -> Option<usize> {
    if let Some(i) = fg
        .iter()
        .position(|(_, cmd, _)| tools::match_tool(cmd, tools).is_some())
    {
        return Some(i);
    }
    let pids: Vec<u32> = fg.iter().map(|(p, _, _)| *p).collect();
    fg.iter()
        .position(|(p, _, _)| {
            procfs::stat(*p)
                .map(|s| !pids.contains(&s.ppid))
                .unwrap_or(true)
        })
        .or(if fg.is_empty() { None } else { Some(0) })
}

fn describe(
    pid: u32,
    cmd: Vec<String>,
    cwd: String,
    tools: &[Tool],
    projects: &mut Projects,
) -> (String, String, u32) {
    let program = if cmd
        .first()
        .is_some_and(|a| a.chars().any(char::is_whitespace))
    {
        let comm = procfs::stat(pid).map(|s| s.comm);
        let exe = procfs::exe(pid);
        tools::program_name_ex(&cmd, comm.as_deref(), exe.as_deref(), tools)
    } else {
        tools::program_name(&cmd, tools)
    };
    let dir = if cwd.is_empty() {
        procfs::cwd(pid)
    } else {
        Some(PathBuf::from(cwd))
    };
    let project = dir.map(|d| projects.of(&d)).unwrap_or_default();
    (program, project, pid)
}

/// Runs tmux, giving up (and killing it) after a second so a stuck server cannot stall us.
async fn tmux(args: &[&str]) -> Option<std::process::Output> {
    let out = tokio::process::Command::new("tmux")
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(std::time::Duration::from_secs(1), out)
        .await
        .ok()?
        .ok()
}

/// If the program is a tmux client, look through to the active pane of its session.
async fn through_tmux(
    program: &str,
    pid: u32,
    tools: &[Tool],
    projects: &mut Projects,
) -> Option<(String, String)> {
    if program != "tmux" {
        return None;
    }
    let clients = tmux(&["list-clients", "-F", "#{client_pid} #{client_session}"]).await?;
    let text = String::from_utf8_lossy(&clients.stdout);
    let session = text.lines().find_map(|l| {
        l.split_once(' ')
            .filter(|(p, _)| p.parse::<u32>().ok() == Some(pid))
            .map(|(_, s)| s.to_string())
    })?;
    let panes = tmux(&[
        "list-panes",
        "-s",
        "-t",
        &session,
        "-F",
        "#{window_active}#{pane_active} #{pane_pid}",
    ])
    .await?;
    let text = String::from_utf8_lossy(&panes.stdout);
    let shell: u32 = text
        .lines()
        .find_map(|l| l.strip_prefix("11 "))
        .and_then(|p| p.trim().parse().ok())?;
    let fg = procfs::stat(shell)
        .map(|s| if s.tpgid > 0 { s.tpgid as u32 } else { shell })
        .unwrap_or(shell);
    let (prog, proj, _) = describe(fg, procfs::cmdline(fg), String::new(), tools, projects);
    Some((prog, proj))
}

/// Inspects a terminal window. kitty is exact; other terminals use /proc and pick the pty
/// that most recently received input.
pub async fn inspect(
    cls: &str,
    pid: i64,
    focused: bool,
    caption: &str,
    tools: &[Tool],
    projects: &mut Projects,
    procs: Option<&procfs::ProcTable>,
) -> Option<TermInfo> {
    if cls.to_lowercase().contains("kitty")
        && let Some(sock) = kitty::socket_for(pid)
    {
        match kitty::ls(&sock).await {
            Ok(osws) => {
                let pane = kitty::pick(&osws, focused, caption)?;
                let fg: Vec<(u32, Vec<String>, String)> = pane
                    .foreground_processes
                    .into_iter()
                    .map(|f| (f.pid, f.cmdline, f.cwd))
                    .collect();
                let i = primary(&fg, tools)?;
                let (p, cmd, cwd) = fg[i].clone();
                let (mut program, mut project, ppid) = describe(p, cmd, cwd, tools, projects);
                if let Some((tp, tj)) = through_tmux(&program, ppid, tools, projects).await {
                    program = tp;
                    project = tj;
                }
                return Some(TermInfo {
                    program,
                    project,
                    source: "kitty",
                });
            }
            Err(e) => tracing::debug!("kitty remote control failed: {e:#}"),
        }
    }
    inspect_proc(pid, tools, projects, procs).await
}

async fn inspect_proc(
    term_pid: i64,
    tools: &[Tool],
    projects: &mut Projects,
    procs: Option<&procfs::ProcTable>,
) -> Option<TermInfo> {
    if term_pid <= 0 {
        return None;
    }
    let own;
    let table = match procs {
        Some(t) => t,
        None => {
            own = procfs::ProcTable::read();
            &own
        }
    };
    // Shells: direct children of the terminal that own a pty.
    let mut best: Option<(std::time::SystemTime, u32)> = None;
    for &child in table.children_of(term_pid as u32) {
        let Some(s) = table.get(child) else {
            continue;
        };
        let Some(pts) = procfs::pts_path(s.tty_nr) else {
            continue;
        };
        let fg = if s.tpgid > 0 { s.tpgid as u32 } else { s.pid };
        let atime = std::fs::metadata(&pts)
            .and_then(|m| m.accessed())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.map(|(t, _)| atime > t).unwrap_or(true) {
            best = Some((atime, fg));
        }
    }
    let (_, fg) = best?;
    let (mut program, mut project, pid) =
        describe(fg, procfs::cmdline(fg), String::new(), tools, projects);
    if let Some((tp, tj)) = through_tmux(&program, pid, tools, projects).await {
        program = tp;
        project = tj;
    }
    Some(TermInfo {
        program,
        project,
        source: "proc",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run with: KITTY_PID=<pid> cargo test live_kitty -- --ignored --nocapture
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn live_kitty() {
        let pid: i64 = std::env::var("KITTY_PID").unwrap().parse().unwrap();
        let tools = crate::config::Agents::default().tools;
        let mut projects = Projects::default();
        let exact = inspect("kitty", pid, true, "", &tools, &mut projects, None).await;
        let guess = inspect_proc(pid, &tools, &mut projects, None).await;
        println!(
            "kitty: {exact:?}
proc: {guess:?}"
        );
        assert!(exact.is_some());
    }
}
