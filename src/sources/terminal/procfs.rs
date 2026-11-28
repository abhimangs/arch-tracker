//! Minimal /proc readers (own-user processes only).

use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct Stat {
    pub pid: u32,
    pub comm: String,
    pub ppid: u32,
    pub tty_nr: i32,
    pub tpgid: i32,
    pub utime: u64,
    pub stime: u64,
    pub cutime: u64,
    pub cstime: u64,
    /// Start time in clock ticks after boot.
    pub start: u64,
}

pub fn stat(pid: u32) -> Option<Stat> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(pid, &s)
}

fn parse_stat(pid: u32, s: &str) -> Option<Stat> {
    // comm may contain spaces and parens: it ends at the last ')'.
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let f: Vec<&str> = s.get(close + 2..)?.split(' ').collect();
    // f[0] = state (field 3), so field N is f[N - 3].
    let g = |n: usize| f.get(n - 3).copied().unwrap_or("0");
    Some(Stat {
        pid,
        comm,
        ppid: g(4).parse().ok()?,
        tty_nr: g(7).parse().ok()?,
        tpgid: g(8).parse().ok()?,
        utime: g(14).parse().ok()?,
        stime: g(15).parse().ok()?,
        cutime: g(16).parse().ok()?,
        cstime: g(17).parse().ok()?,
        start: g(22).parse().unwrap_or(0),
    })
}

/// Seconds since boot.
pub fn uptime_secs() -> Option<f64> {
    std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

pub fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|c| *c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Target of /proc/<pid>/exe (the executable actually running).
pub fn exe(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

pub fn cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Bytes written by the process (terminal output, files, sockets).
pub fn wchar(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("wchar:"))
        .and_then(|v| v.trim().parse().ok())
}

/// All processes owned by the current user.
pub fn own_processes() -> Vec<Stat> {
    use std::os::unix::fs::MetadataExt;
    let uid = unsafe { libc::getuid() };
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    let mut out = vec![];
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        match e.metadata() {
            Ok(m) if m.uid() == uid => {}
            _ => continue,
        }
        if let Some(s) = stat(pid) {
            out.push(s);
        }
    }
    out
}

/// One read of /proc (own processes), shared by everything that looks at processes in a tick.
pub struct ProcTable {
    pub procs: Vec<Stat>,
    pub children: HashMap<u32, Vec<u32>>,
    index: HashMap<u32, usize>,
}

impl ProcTable {
    pub fn read() -> Self {
        Self::from_procs(own_processes())
    }

    pub fn from_procs(procs: Vec<Stat>) -> Self {
        let children = children_map(&procs);
        let index = procs.iter().enumerate().map(|(i, s)| (s.pid, i)).collect();
        ProcTable {
            procs,
            children,
            index,
        }
    }

    pub fn get(&self, pid: u32) -> Option<&Stat> {
        self.index.get(&pid).map(|i| &self.procs[*i])
    }

    pub fn children_of(&self, pid: u32) -> &[u32] {
        self.children
            .get(&pid)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

pub fn children_map(procs: &[Stat]) -> HashMap<u32, Vec<u32>> {
    let mut m: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        m.entry(p.ppid).or_default().push(p.pid);
    }
    m
}

pub fn descendants(root: u32, children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut out = vec![];
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if let Some(c) = children.get(&p) {
            for k in c {
                if out.len() > 4096 {
                    return out;
                }
                out.push(*k);
                stack.push(*k);
            }
        }
    }
    out
}

/// /dev/pts/N for a tty_nr, if it is a pseudo-terminal.
pub fn pts_path(tty_nr: i32) -> Option<PathBuf> {
    let t = tty_nr as u32;
    let major = (t >> 8) & 0xfff;
    let minor = (t & 0xff) | ((t >> 12) & 0xfff00);
    if !(136..=143).contains(&major) {
        return None;
    }
    Some(PathBuf::from(format!(
        "/dev/pts/{}",
        (major - 136) * 256 + minor
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_spaces_in_comm() {
        let line = "1234 (tmux: client) S 1000 1234 1234 34817 1234 4194560 100 0 0 0 7 3 1 2 20 0 1 0 12345 0 0";
        let s = parse_stat(1234, line).unwrap();
        assert_eq!(s.comm, "tmux: client");
        assert_eq!(s.ppid, 1000);
        assert_eq!(s.tty_nr, 34817);
        assert_eq!(s.tpgid, 1234);
        assert_eq!((s.utime, s.stime, s.cutime, s.cstime), (7, 3, 1, 2));
        assert_eq!(s.start, 12345);
    }

    #[test]
    fn pts_numbers() {
        // major 136, minor 1 -> /dev/pts/1
        assert_eq!(pts_path((136 << 8) | 1), Some(PathBuf::from("/dev/pts/1")));
        assert_eq!(pts_path(1025), None);
    }

    #[test]
    fn reads_self() {
        let me = std::process::id();
        assert!(stat(me).is_some());
        assert!(!cmdline(me).is_empty());
    }
}
