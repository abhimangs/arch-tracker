//! Finds AI agent processes anywhere (any terminal, focused or not) and decides whether each is
//! working: its own CPU, its terminal output, or CPU of what it spawned (builds, tests).

use super::procfs;
use super::tools::{self, Projects};
use crate::config::Agents;
use crate::model::Subject;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

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
    pub fn scan(&mut self, cfg: &Agents, projects: &mut Projects) -> Vec<AgentState> {
        let elapsed = self.at.elapsed().as_secs_f64().max(0.5);
        self.at = Instant::now();
        let procs = procfs::own_processes();
        let children = procfs::children_map(&procs);
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
            let desc = procfs::descendants(pid, &children);
            let live_children: u64 = desc
                .iter()
                .filter_map(|d| by_pid.get(d))
                .map(|s| s.utime + s.stime)
                .sum();
            let now = Sample {
                own: st.utime + st.stime,
                children: live_children + st.cutime + st.cstime,
                wchar: procfs::wchar(pid).unwrap_or(0),
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
                None => (false, 0.0),
                Some(p) => {
                    let pct = |a: u64, b: u64| {
                        (a.saturating_sub(b) as f64 / self.clk_tck) / elapsed * 100.0
                    };
                    let own = pct(now.own, p.own);
                    let kids = pct(now.children, p.children);
                    let wrote = now.wchar.saturating_sub(p.wchar);
                    let busy = own >= cfg.busy_cpu_percent
                        || kids >= cfg.busy_child_cpu_percent
                        || wrote >= cfg.busy_write_bytes;
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
