//! Finds AI agent processes anywhere (any terminal, focused or not) and decides whether each is
//! working: its own CPU, its terminal output, or CPU of what it spawned (builds, tests).

use super::procfs;
use super::tools::{self, Projects};
use crate::config::Agents;
use crate::model::Subject;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// A gap this long since the last scan (pause, suspend) makes the next sample meaningless.
const STALE_SECS: f64 = 120.0;
/// Descendants running longer than this are background helpers (language servers, MCP servers,
/// dev servers, browsers), not work the agent is doing right now.
const BACKGROUND_SECS: f64 = 600.0;

/// CPU ticks of work an agent started: descendants that aren't the agent itself, skipping
/// long-running helpers and everything under them. Parents come before children in `desc`.
fn work_ticks(
    desc: &[u32],
    own: &[u32],
    by_pid: &HashMap<u32, &procfs::Stat>,
    long_running: impl Fn(&procfs::Stat) -> bool,
) -> u64 {
    let mut background: HashSet<u32> = HashSet::new();
    let mut ticks = 0;
    for d in desc {
        let Some(s) = by_pid.get(d) else { continue };
        if own.contains(d) {
            continue;
        }
        if background.contains(&s.ppid) || long_running(s) {
            background.insert(*d);
            continue;
        }
        ticks += s.utime + s.stime;
    }
    ticks
}

#[derive(Clone, Copy)]
struct Sample {
    own: u64,
    children: u64,
    wchar: u64,
}

pub struct Scanner {
    prev: HashMap<u32, Sample>,
    at: Instant,
    clk_tck: f64,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            prev: HashMap::new(),
            at: Instant::now(),
            clk_tck: (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as f64,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentState {
    pub subject: Subject,
    pub pid: u32,
    pub busy: bool,
    pub cpu_percent: f64,
}

impl Scanner {
    pub fn scan(
        &mut self,
        cfg: &Agents,
        projects: &mut Projects,
        table: &procfs::ProcTable,
    ) -> Vec<AgentState> {
        let elapsed = self.at.elapsed().as_secs_f64().max(0.5);
        self.at = Instant::now();
        // After a pause or suspend the first sample would cover the whole gap.
        let stale = elapsed > STALE_SECS;
        let now_ticks = procfs::uptime_secs().map(|u| (u * self.clk_tck) as u64);
        let clk = self.clk_tck;
        let long_running = |s: &procfs::Stat| {
            now_ticks.is_some_and(|n| n.saturating_sub(s.start) as f64 / clk > BACKGROUND_SECS)
        };
        let procs = &table.procs;
        let children = &table.children;
        let by_pid: HashMap<u32, &procfs::Stat> = procs.iter().map(|s| (s.pid, s)).collect();
        let mut tool_of: HashMap<u32, usize> = HashMap::new();
        for p in procs.iter().filter(|p| p.tty_nr != 0) {
            let cmd = procfs::cmdline(p.pid);
            if let Some(t) = tools::match_tool(&cmd, &cfg.tools)
                && let Some(i) = cfg.tools.iter().position(|x| x.id == t.id)
            {
                tool_of.insert(p.pid, i);
            }
        }
        let mut out = vec![];
        let mut seen = HashSet::new();
        for (&pid, &ti) in &tool_of {
            // Only the outermost process of a tool (wrappers spawn the real binary).
            if by_pid
                .get(&pid)
                .map(|s| tool_of.get(&s.ppid) == Some(&ti))
                .unwrap_or(false)
            {
                continue;
            }
            let Some(st) = by_pid.get(&pid) else { continue };
            let desc = procfs::descendants(pid, children);
            // Nested processes of the same tool (npm wrappers, self-relaunches) are the agent:
            // their CPU and output count as its own.
            let own_pids: Vec<u32> = std::iter::once(pid)
                .chain(desc.iter().copied().filter(|d| tool_of.get(d) == Some(&ti)))
                .collect();
            // Work it started (builds, tests) counts as children; long-running helpers don't.
            let live_children = work_ticks(&desc, &own_pids, &by_pid, long_running);
            let now = Sample {
                own: own_pids
                    .iter()
                    .filter_map(|p| by_pid.get(p))
                    .map(|s| s.utime + s.stime)
                    .sum(),
                children: live_children + st.cutime + st.cstime,
                wchar: own_pids
                    .iter()
                    .map(|p| procfs::wchar(*p).unwrap_or(0))
                    .sum(),
            };
            seen.insert(pid);
            let prev = self.prev.insert(pid, now);
            let tool = &cfg.tools[ti];
            let project = procfs::cwd(pid)
                .map(|d| projects.of(&d))
                .unwrap_or_default();
            let subject = Subject {
                app: format!("agent:{}", tool.id),
                app_name: tool.name.clone(),
                program: tool.id.clone(),
                project,
                ..Default::default()
            };
            let (busy, cpu) = match prev {
                _ if stale => (false, 0.0),
                None => (false, 0.0),
                Some(p) => {
                    let pct = |a: u64, b: u64| {
                        (a.saturating_sub(b) as f64 / self.clk_tck) / elapsed * 100.0
                    };
                    let own = pct(now.own, p.own);
                    let kids = pct(now.children, p.children);
                    // Output per 10 s, whatever the sampling interval.
                    let wrote = now.wchar.saturating_sub(p.wchar) as f64 / elapsed * 10.0;
                    let busy = own >= cfg.busy_cpu_percent
                        || kids >= cfg.busy_child_cpu_percent
                        || wrote >= cfg.busy_write_bytes as f64;
                    (busy, own + kids)
                }
            };
            out.push(AgentState {
                subject,
                pid,
                busy,
                cpu_percent: (cpu * 10.0).round() / 10.0,
            });
        }
        self.prev.retain(|p, _| seen.contains(p));
        out.sort_by_key(|a| a.pid);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(pid: u32, ppid: u32, cpu: u64, start: u64) -> procfs::Stat {
        procfs::Stat {
            pid,
            comm: String::new(),
            ppid,
            tty_nr: 0,
            tpgid: 0,
            utime: cpu,
            stime: 0,
            cutime: 0,
            cstime: 0,
            start,
        }
    }

    #[test]
    fn background_helpers_do_not_count_as_work() {
        // agent 1 -> old MCP server 2 -> its worker 3; agent -> fresh cargo 4 -> rustc 5;
        // agent -> nested agent process 6 (an npm wrapper's real binary).
        let procs = [
            st(2, 1, 500, 0),
            st(3, 2, 300, 900),
            st(4, 1, 20, 900),
            st(5, 4, 80, 950),
            st(6, 1, 40, 900),
        ];
        let by_pid: HashMap<u32, &procfs::Stat> = procs.iter().map(|s| (s.pid, s)).collect();
        let desc = [2, 3, 4, 5, 6];
        let old = |s: &procfs::Stat| s.start < 100;
        assert_eq!(work_ticks(&desc, &[1, 6], &by_pid, old), 100);
    }
}
