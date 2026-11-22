# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-11-22

### Added

- `arch-tracker serve`: a read-only dashboard for any tracker database, without tracking.
- Continuous integration on every push: formatting, clippy, tests, a dashboard script check, and an
  Arch package build in an archlinux container.
- Tagged releases publish the Arch package, a binary tarball and SHA-256 checksums.
- README screenshots, security policy, contributing guide, code of conduct, and issue and pull
  request templates.

### Changed

- Release builds use thin LTO and are stripped, for a smaller binary.

### Fixed

- `arch-tracker doctor` no longer reports the Plasma Browser Integration fallback as missing before
  a browser window has been on screen.

## [0.1.0] - 2026-11-22

### Added

- Focus and visible-window tracking through a KWin script loaded at runtime.
- Idle detection with Wayland `ext-idle-notify` v2 (raw no-input gaps), plus lock-screen and
  suspend handling with a flush before sleep.
- Brave/Chromium extension with native messaging for exact active tabs (host and title only);
  Plasma Browser Integration and session-file fallbacks; incognito counted without details; web
  apps shown as their own apps; split-view tabs counted as visible.
- Terminal tracking: kitty remote control, a `/proc` fallback for other terminals, and tmux panes.
- AI agents per project, with the time you watched them and the time they were working.
- Media and microphone signals for "watching" and "call" time; background listening.
- Running apps from windows, systemd app units and tray icons, never counted as screen time.
- Query-time classification: idle, call and watching rules apply to all past days.
- Local dashboard: day ribbon, apps and sites, agents, trends, settings, custom CSS, light and dark
  themes, and a live port change.
- Command line: `status`, `pause`, `resume`, `setup`, `doctor` and `export`.
- Arch PKGBUILD and a systemd user unit.

[Unreleased]: https://github.com/abhimangs/arch-tracker/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/abhimangs/arch-tracker/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/abhimangs/arch-tracker/tree/v0.1.0
