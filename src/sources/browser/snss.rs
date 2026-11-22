//! Minimal reader for Chromium's SNSS session file ("Session_*"): which tab is selected in
//! each window. Used only to break ties when two tabs have the same title and the extension is
//! not available. The file Brave is writing is found through /proc/<pid>/fd.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const SET_TAB_WINDOW: u8 = 0;
const SET_TAB_INDEX_IN_WINDOW: u8 = 2;
const SET_SELECTED_TAB_IN_INDEX: u8 = 8;
const TAB_CLOSED: u8 = 16;
const WINDOW_CLOSED: u8 = 17;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Selection {
    /// Session tab ids that are the selected tab of an open window.
    pub selected: HashSet<i64>,
}

fn i32_at(b: &[u8], off: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

pub fn parse(data: &[u8]) -> Option<Selection> {
    if data.get(0..4)? != b"SNSS" {
        return None;
    }
    let mut off = 8;
    let mut tab_window: HashMap<i32, i32> = HashMap::new();
    let mut tab_index: HashMap<i32, i32> = HashMap::new();
    let mut win_sel: HashMap<i32, i32> = HashMap::new();
    let mut closed_tabs = HashSet::new();
    let mut closed_wins = HashSet::new();
    while off + 2 <= data.len() {
        let size = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
        off += 2;
        if size == 0 || off + size > data.len() {
            break; // partially written tail
        }
        let id = data[off];
        let p = &data[off + 1..off + size];
        off += size;
        match id {
            SET_TAB_WINDOW => {
                if let (Some(w), Some(t)) = (i32_at(p, 0), i32_at(p, 4)) {
                    tab_window.insert(t, w);
                }
            }
            SET_TAB_INDEX_IN_WINDOW => {
                if let (Some(t), Some(i)) = (i32_at(p, 0), i32_at(p, 4)) {
                    tab_index.insert(t, i);
                }
            }
            SET_SELECTED_TAB_IN_INDEX => {
                if let (Some(w), Some(i)) = (i32_at(p, 0), i32_at(p, 4)) {
                    win_sel.insert(w, i);
                }
            }
            TAB_CLOSED => {
                if let Some(t) = i32_at(p, 0) {
                    closed_tabs.insert(t);
                }
            }
            WINDOW_CLOSED => {
                if let Some(w) = i32_at(p, 0) {
                    closed_wins.insert(w);
                }
            }
            _ => {}
        }
    }
    let mut per_window: HashMap<i32, Vec<i32>> = HashMap::new();
    for (t, w) in &tab_window {
        if !closed_tabs.contains(t) && !closed_wins.contains(w) {
            per_window.entry(*w).or_default().push(*t);
        }
    }
    let mut sel = Selection::default();
    for (w, mut tabs) in per_window {
        tabs.sort_by_key(|t| tab_index.get(t).copied().unwrap_or(i32::MAX));
        if let Some(i) = win_sel.get(&w)
            && let Some(t) = tabs.get(*i as usize)
        {
            sel.selected.insert(*t as i64);
        }
    }
    Some(sel)
}

/// The plain (unencrypted) session file the browser process currently has open.
pub fn open_session_file(browser_pid: u32) -> Option<PathBuf> {
    let rd = std::fs::read_dir(format!("/proc/{browser_pid}/fd")).ok()?;
    rd.flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .find(|p| {
            let s = p.to_string_lossy();
            s.contains("/Sessions/Session_")
        })
}

#[derive(Default)]
pub struct Cache {
    key: Option<(PathBuf, u64, std::time::SystemTime)>,
    value: Option<Selection>,
}

impl Cache {
    pub fn get(&mut self, path: &Path) -> Option<&Selection> {
        let meta = std::fs::metadata(path).ok()?;
        let key = (path.to_path_buf(), meta.len(), meta.modified().ok()?);
        if self.key.as_ref() != Some(&key) {
            self.value = std::fs::read(path).ok().and_then(|d| parse(&d));
            self.key = Some(key);
        }
        self.value.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(id: u8, ints: &[i32]) -> Vec<u8> {
        let mut payload = vec![id];
        for i in ints {
            payload.extend_from_slice(&i.to_le_bytes());
        }
        let mut out = (payload.len() as u16).to_le_bytes().to_vec();
        out.extend(payload);
        out
    }

    #[test]
    fn selected_tab_per_window() {
        let mut d = b"SNSS".to_vec();
        d.extend_from_slice(&3i32.to_le_bytes());
        // window 1 has tabs 10, 11, 12 (indexes 0, 1, 2); tab 11 closed; selected index 1 -> tab 12
        for (t, i) in [(10, 0), (11, 1), (12, 2)] {
            d.extend(cmd(SET_TAB_WINDOW, &[1, t]));
            d.extend(cmd(SET_TAB_INDEX_IN_WINDOW, &[t, i]));
        }
        d.extend(cmd(TAB_CLOSED, &[11, 0, 0]));
        d.extend(cmd(SET_SELECTED_TAB_IN_INDEX, &[1, 1]));
        d.extend_from_slice(&[9, 0, 1]); // truncated tail is ignored
        let s = parse(&d).unwrap();
        assert_eq!(s.selected, [12i64].into_iter().collect());
    }

    #[test]
    fn rejects_other_files() {
        assert!(parse(b"NOPE").is_none());
    }
}
