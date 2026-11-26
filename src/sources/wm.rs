//! Which desktop runs this session, and so where windows come from: KWin (a script it runs), GNOME
//! (an extension), Hyprland or Sway (their IPC sockets). All of them produce the same snapshots.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum Wm {
    Kwin,
    Gnome,
    /// The Hyprland instance's socket folder.
    Hyprland(PathBuf),
    /// Sway's IPC socket.
    Sway(PathBuf),
}

impl Wm {
    pub fn name(&self) -> &'static str {
        match self {
            Wm::Kwin => "KDE Plasma",
            Wm::Gnome => "GNOME",
            Wm::Hyprland(_) => "Hyprland",
            Wm::Sway(_) => "Sway",
        }
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn runtime_dir() -> PathBuf {
    var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

/// From what the compositor exported (its socket variables, then XDG_CURRENT_DESKTOP), else from
/// the sockets it left in the runtime folder; KDE Plasma when nothing says otherwise.
pub fn detect() -> Wm {
    let run = runtime_dir();
    if let Some(sig) = var("HYPRLAND_INSTANCE_SIGNATURE")
        && let Some(d) = [
            run.join("hypr").join(&sig),
            PathBuf::from("/tmp/hypr").join(&sig),
        ]
        .into_iter()
        .find(|d| d.join(".socket.sock").exists())
    {
        return Wm::Hyprland(d);
    }
    if let Some(s) = var("SWAYSOCK").map(PathBuf::from)
        && s.exists()
    {
        return Wm::Sway(s);
    }
    let desktop = var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_lowercase();
    if desktop.split(':').any(|d| d == "kde") {
        return Wm::Kwin;
    }
    if desktop.split(':').any(|d| d == "gnome") {
        return Wm::Gnome;
    }
    if let Some(d) = hyprland_instance(&run.join("hypr")) {
        return Wm::Hyprland(d);
    }
    if let Some(s) = sway_socket(&run) {
        return Wm::Sway(s);
    }
    Wm::Kwin
}

/// The newest Hyprland instance with a socket.
fn hyprland_instance(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(".socket.sock").exists() && p.join(".socket2.sock").exists())
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .max()
        .map(|(_, p)| p)
}

/// Sway's socket, "sway-ipc.<uid>.<pid>.sock", of a sway that still runs.
fn sway_socket(run: &Path) -> Option<PathBuf> {
    std::fs::read_dir(run).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        let pid = name
            .strip_prefix("sway-ipc.")?
            .strip_suffix(".sock")?
            .rsplit('.')
            .next()?;
        Path::new("/proc").join(pid).exists().then(|| e.path())
    })
}

/// The window to bring forward for a process: one of the nearest of `pids` (a process, then its
/// parents) that has a window, titled `caption` if one is. Windows are (id, pid, title).
pub fn pick<'a>(
    wins: impl IntoIterator<Item = (&'a str, i64, &'a str)>,
    pids: &[u32],
    caption: Option<&str>,
) -> Option<String> {
    wins.into_iter()
        .filter_map(|(id, pid, title)| {
            let i = pids.iter().position(|p| *p as i64 == pid)?;
            let off = caption.is_some_and(|c| c != title);
            Some((i * 2 + off as usize, id))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, id)| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_process_with_a_window_wins_and_the_title_breaks_ties() {
        let wins = [("a", 900, "zsh"), ("b", 900, "claude"), ("c", 1, "desktop")];
        assert_eq!(
            pick(wins, &[950, 901, 900], Some("claude")).as_deref(),
            Some("b")
        );
        assert_eq!(pick(wins, &[950, 901, 900], None).as_deref(), Some("a"));
        assert_eq!(pick(wins, &[42], None), None);
    }

    #[test]
    fn sockets_left_in_the_runtime_folder_name_the_desktop() {
        let d = std::env::temp_dir().join(format!("at-wm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let inst = d.join("hypr/abc_123");
        std::fs::create_dir_all(&inst).unwrap();
        assert_eq!(hyprland_instance(&d.join("hypr")), None);
        std::fs::write(inst.join(".socket.sock"), "").unwrap();
        std::fs::write(inst.join(".socket2.sock"), "").unwrap();
        assert_eq!(hyprland_instance(&d.join("hypr")), Some(inst));
        let me = std::process::id();
        std::fs::write(d.join(format!("sway-ipc.1000.{me}.sock")), "").unwrap();
        assert_eq!(
            sway_socket(&d),
            Some(d.join(format!("sway-ipc.1000.{me}.sock")))
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
