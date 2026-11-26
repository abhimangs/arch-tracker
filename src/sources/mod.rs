//! Event sources. Each desktop-specific source produces the same plain data: windows come from
//! KWin (kwin), the GNOME Shell extension (gnome), Hyprland (hyprland) or Sway (sway) as one kind
//! of snapshot, chosen by wm.

pub mod browser;
pub mod claude;
pub mod codex;
pub mod gnome;
pub mod hermes;
pub mod hyprland;
pub mod idle;
pub mod kwin;
pub mod media;
pub mod opencode;
pub mod running;
pub mod session;
pub mod sway;
pub mod terminal;
pub mod wm;
