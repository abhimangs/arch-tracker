//! KWin window source: a KWin script (embedded) pushes snapshots to our D-Bus service.

use crate::daemon::Event;
use crate::util;
use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::sync::mpsc;
use zbus::Connection;

pub const BUS_NAME: &str = "dev.archtracker.Daemon";
pub const OBJECT_PATH: &str = "/dev/archtracker/KWin";
const SCRIPT_NAME: &str = "arch-tracker";
const SCRIPT: &str = include_str!("../../kwin/arch-tracker.js");

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Snapshot {
    pub active: Option<String>,
    #[serde(default)]
    pub desktop: String,
    #[serde(default)]
    pub activity: String,
    #[serde(default)]
    pub screens: Vec<Screen>,
    /// Stacking order, bottom to top.
    #[serde(default)]
    pub windows: Vec<Win>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Screen {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Win {
    pub id: String,
    pub pid: i64,
    pub cls: String,
    pub df: String,
    pub cap: String,
    pub min: bool,
    pub hidden: bool,
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
    pub desks: Vec<String>,
    pub all_desks: bool,
    pub acts: Vec<String>,
    pub out: String,
    pub fs: bool,
    pub skip: bool,
    pub normal: bool,
    pub dialog: bool,
    pub utility: bool,
    pub special: bool,
    pub dock: bool,
    pub desktop_win: bool,
    pub tf: Option<String>,
}

impl Snapshot {
    pub fn window(&self, id: &str) -> Option<&Win> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// The focused window, with transient dialogs resolved to their top-level parent.
    pub fn focused(&self) -> Option<&Win> {
        let mut w = self.window(self.active.as_deref()?)?;
        for _ in 0..8 {
            match w.tf.as_deref().and_then(|p| self.window(p)) {
                Some(parent) => w = parent,
                None => break,
            }
        }
        Some(w)
    }

    fn on_current(&self, w: &Win) -> bool {
        let desk = w.all_desks || w.desks.is_empty() || w.desks.contains(&self.desktop);
        let act = w.acts.is_empty() || w.acts.contains(&self.activity);
        desk && act
    }

    /// Windows a person can see right now (excluding the focused one), top-most first.
    /// A window is visible if enough of it is not covered by windows stacked above it.
    #[cfg(test)]
    pub fn visible(&self, min_fraction: f64, min_pixels: i64) -> Vec<&Win> {
        self.visible_indices(min_fraction, min_pixels)
            .into_iter()
            .map(|i| &self.windows[i])
            .collect()
    }

    /// Like visible, as indices into windows.
    pub fn visible_indices(&self, min_fraction: f64, min_pixels: i64) -> Vec<usize> {
        let bounds = self.screen_rects();
        let focused = self.focused().map(|w| w.id.clone());
        let active = self.active.clone();
        let mut covered: Vec<Rect> = vec![];
        let mut out = vec![];
        for (i, w) in self.windows.iter().enumerate().rev() {
            let eligible = !w.min
                && !w.hidden
                && !w.dock
                && !w.desktop_win
                && (w.normal || w.dialog || w.utility)
                && self.on_current(w);
            if !eligible {
                continue;
            }
            let r = Rect {
                x: w.x,
                y: w.y,
                w: w.w,
                h: w.h,
            };
            let on_screen: Vec<Rect> = bounds.iter().filter_map(|b| r.intersect(b)).collect();
            let total: i64 = on_screen.iter().map(|r| r.area()).sum();
            let mut pieces = on_screen;
            for c in &covered {
                pieces = pieces.iter().flat_map(|p| p.subtract(c)).collect();
            }
            let vis: i64 = pieces.iter().map(|p| p.area()).sum();
            covered.extend(pieces);
            if total <= 0 {
                continue;
            }
            let is_focused = focused.as_deref() == Some(w.id.as_str())
                || active.as_deref() == Some(w.id.as_str())
                || (w.tf.is_some() && w.tf == focused);
            if !is_focused && vis >= min_pixels && (vis as f64) >= min_fraction * total as f64 {
                out.push(i);
            }
        }
        out
    }

    fn screen_rects(&self) -> Vec<Rect> {
        if self.screens.is_empty() {
            return vec![Rect {
                x: -100_000,
                y: -100_000,
                w: 200_000,
                h: 200_000,
            }];
        }
        self.screens
            .iter()
            .map(|s| Rect {
                x: s.x,
                y: s.y,
                w: s.w,
                h: s.h,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

impl Rect {
    fn area(&self) -> i64 {
        self.w.max(0) * self.h.max(0)
    }

    fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x1 = self.x.max(o.x);
        let y1 = self.y.max(o.y);
        let x2 = (self.x + self.w).min(o.x + o.w);
        let y2 = (self.y + self.h).min(o.y + o.h);
        (x2 > x1 && y2 > y1).then_some(Rect {
            x: x1,
            y: y1,
            w: x2 - x1,
            h: y2 - y1,
        })
    }

    /// self minus o, as up to four disjoint rectangles.
    fn subtract(&self, o: &Rect) -> Vec<Rect> {
        let Some(i) = self.intersect(o) else {
            return vec![*self];
        };
        let mut out = vec![];
        if i.y > self.y {
            out.push(Rect {
                x: self.x,
                y: self.y,
                w: self.w,
                h: i.y - self.y,
            });
        }
        if i.y + i.h < self.y + self.h {
            out.push(Rect {
                x: self.x,
                y: i.y + i.h,
                w: self.w,
                h: self.y + self.h - (i.y + i.h),
            });
        }
        if i.x > self.x {
            out.push(Rect {
                x: self.x,
                y: i.y,
                w: i.x - self.x,
                h: i.h,
            });
        }
        if i.x + i.w < self.x + self.w {
            out.push(Rect {
                x: i.x + i.w,
                y: i.y,
                w: self.x + self.w - (i.x + i.w),
                h: i.h,
            });
        }
        out
    }
}

/// D-Bus object the KWin script calls.
pub struct KwinIface {
    pub tx: mpsc::UnboundedSender<Event>,
}

#[zbus::interface(name = "dev.archtracker.KWin1")]
impl KwinIface {
    fn snapshot(&self, json: &str) {
        match serde_json::from_str::<Snapshot>(json) {
            Ok(s) => {
                let _ = self.tx.send(Event::Kwin(Box::new(s)));
            }
            Err(e) => tracing::warn!("bad KWin snapshot: {e}"),
        }
    }
}

async fn scripting(
    conn: &Connection,
    method: &str,
    body: &(impl serde::Serialize + zbus::Type),
) -> Result<zbus::Message> {
    conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        method,
        body,
    )
    .await
    .with_context(|| format!("KWin Scripting.{method}"))
}

/// (Re)loads the embedded script into KWin.
pub async fn load_script(conn: &Connection) -> Result<()> {
    let path = util::runtime_dir().join("kwin-script.js");
    std::fs::write(&path, SCRIPT)?;
    let _ = scripting(conn, "unloadScript", &(SCRIPT_NAME,)).await;
    let path_s = path.to_string_lossy().to_string();
    scripting(conn, "loadScript", &(path_s.as_str(), SCRIPT_NAME)).await?;
    scripting(conn, "start", &()).await?;
    tracing::info!("KWin script loaded");
    Ok(())
}

pub async fn unload_script(conn: &Connection) {
    let _ = scripting(conn, "unloadScript", &(SCRIPT_NAME,)).await;
}

const ACTIVATE_NAME: &str = "arch-tracker-activate";

/// Brings forward the window of the nearest process in `pids` (a process, then its parents) that
/// has one, preferring one titled `caption`. A one-off KWin script does it, so focus-stealing
/// prevention doesn't hold it back; KWin switches virtual desktops for it if it has to.
pub async fn activate(conn: &Connection, pids: &[u32], caption: Option<&str>) -> Result<()> {
    let path = util::runtime_dir().join("kwin-activate.js");
    std::fs::write(&path, activate_script(pids, caption))?;
    let path_s = path.to_string_lossy().to_string();
    let _ = scripting(conn, "unloadScript", &(ACTIVATE_NAME,)).await;
    scripting(conn, "loadScript", &(path_s.as_str(), ACTIVATE_NAME)).await?;
    scripting(conn, "start", &()).await?;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let _ = scripting(conn, "unloadScript", &(ACTIVATE_NAME,)).await;
    Ok(())
}

fn activate_script(pids: &[u32], caption: Option<&str>) -> String {
    let pids = serde_json::to_string(pids).unwrap_or_else(|_| "[]".into());
    let caption = serde_json::to_string(&caption).unwrap_or_else(|_| "null".into());
    format!(
        "(function () {{
    var pids = {pids};
    var caption = {caption};
    var best = null, rank = 1e9;
    workspace.windowList().forEach(function (w) {{
        var i = pids.indexOf(w.pid);
        if (i < 0 || !w.normalWindow) return;
        var r = i * 2 + (caption !== null && w.caption !== caption ? 1 : 0);
        if (r < rank) {{ best = w; rank = r; }}
    }});
    if (best) {{
        if (best.minimized) best.minimized = false;
        workspace.activeWindow = best;
    }}
}})();
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: &str, x: i64, y: i64, w: i64, h: i64) -> Win {
        Win {
            id: id.into(),
            x,
            y,
            w,
            h,
            normal: true,
            ..Default::default()
        }
    }

    fn snap(active: &str, wins: Vec<Win>) -> Snapshot {
        Snapshot {
            active: Some(active.into()),
            screens: vec![Screen {
                x: 0,
                y: 0,
                w: 1920,
                h: 1200,
            }],
            windows: wins,
            ..Default::default()
        }
    }

    #[test]
    fn maximized_windows_hide_everything_below() {
        let s = snap(
            "c",
            vec![
                win("a", 0, 0, 1920, 1200),
                win("b", 0, 0, 1920, 1200),
                win("c", 0, 0, 1920, 1200),
            ],
        );
        assert!(s.visible(0.05, 40_000).is_empty());
    }

    #[test]
    fn side_by_side_both_visible() {
        let s = snap(
            "b",
            vec![win("a", 0, 0, 960, 1200), win("b", 960, 0, 960, 1200)],
        );
        let v: Vec<&str> = s
            .visible(0.05, 40_000)
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(v, vec!["a"]);
    }

    #[test]
    fn minimized_and_sliver_not_visible() {
        let mut m = win("m", 0, 0, 800, 600);
        m.min = true;
        // "s" is only a 10px strip visible next to the focused window.
        let s = snap(
            "t",
            vec![m, win("s", 0, 0, 1000, 1200), win("t", 10, 0, 1910, 1200)],
        );
        assert!(s.visible(0.05, 40_000).is_empty());
    }

    #[test]
    fn the_activation_script_names_the_processes_and_the_title() {
        let js = activate_script(&[950, 900], Some("a \"quoted\" title"));
        assert!(js.contains("var pids = [950,900];"), "{js}");
        assert!(
            js.contains(r#"var caption = "a \"quoted\" title";"#),
            "{js}"
        );
        assert!(activate_script(&[1], None).contains("var caption = null;"));
    }

    #[test]
    fn dialog_resolves_to_parent() {
        let mut d = win("d", 100, 100, 400, 300);
        d.tf = Some("p".into());
        let s = snap("d", vec![win("p", 0, 0, 1920, 1200), d]);
        assert_eq!(s.focused().unwrap().id, "p");
    }
}
