//! Event sources. Each desktop-specific source sits behind plain data types, so other
//! desktops (GNOME, Hyprland, Sway) can be added as new modules producing the same events.

pub mod browser;
pub mod claude;
pub mod codex;
pub mod hermes;
pub mod idle;
pub mod kwin;
pub mod media;
pub mod opencode;
pub mod running;
pub mod session;
pub mod terminal;
