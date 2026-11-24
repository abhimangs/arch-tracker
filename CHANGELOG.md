# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Dashboard redesign: a sidebar with a sliding highlight, and new Now (live, to the millisecond),
  Timeline (sessions with exact start and end) and Projects (coding time per project) pages. Today
  adds focus stats, a daily goal and category limits.
- Live updates over server-sent events, the moment focus changes.
- Settings for the clock, milliseconds, density, motion, start page, goals, sessions, deep work,
  code editors, project renames and history retention.
- System-tray icon: today's screen time in the tooltip, click to open the dashboard, pause and
  resume from the menu or with a middle click.
- Dashboard themes: Catppuccin (Mocha, Macchiato, Frappé, Latte) and Rosé Pine (Main, Moon, Dawn)
  besides the original, with every accent, a header palette picker, and category colours that
  follow the theme.
- `arch-tracker open` opens the dashboard signed in.
- Firefox-family browsers (Firefox, Zen, LibreWolf, Floorp, Waterfox): a Firefox build of the
  extension, bridge registration, `scripts/sign-firefox.sh` for private Mozilla signing, and a
  system-wide bridge in the package. Chromium-family browsers are found by their profile files, so
  Helium and others work without a list.
- A man page.
- Project folders: each folder directly inside a folder you list (like `~/projects`) counts as a
  project, git repository or not.
- A Recompute history button in Settings, which rebuilds Trends' day summaries from the raw data.

### Changed

- A session is an unbroken stretch of screen time: switching apps or sites no longer ends one (the
  Timeline still lists each app), so session counts and deep work mean something.
- The Now page's window clock follows the window or tab, not its title; plainer labels, and the
  tracker's own memory and CPU use.
- Thin scrollbars in the theme's colours.
- The dashboard is locked to you and served as `http://arch-tracker.localhost:<port>/`.
- A dashboard port change applies however the config changes, not only from Settings.
- A broken `config.toml` no longer stops the daemon: it starts with defaults and says why.
- Agent "working" time ignores long-running helpers (MCP, language and dev servers), counts
  wrapped agent processes, and measures output per 10 seconds.
- Exports stream instead of being built in memory; ranges use a small per-day cache.
- `arch-tracker status` prints a readable summary (`--json` for the raw status).

### Fixed

- The Today page multiplied its refresh timer every minute.
- Agent and running-app time kept counting while suspended or paused.
- A failed database write could lose spans or overwrite another span's end.
- Spans could cross midnight; idle gaps across midnight were split before the threshold.
- Terminal titles (which hold command lines) and rewritten process titles stored arguments; untitled
  pages stored their URL; a private window the extension couldn't see could be recorded as a normal
  tab.
- Browser audio was counted twice without Plasma Browser Integration.
- Lost KWin, idle and D-Bus connections now recover instead of stopping a source for good.
- "Pause until resumed" survives a restart.
- Settings saves no longer undo hand edits, and invalid fields are named.
- `brave://` category rules, CSV formula escaping, daylight-saving hours, and keyboard access to
  the day chart.
- A broken `config.toml` is kept as `config.toml.broken` instead of being overwritten, and the theme
  picker no longer replaces it.
- Trends recomputes days after a time-zone change instead of showing numbers from the old zone.
- Sessions and projects no longer count an extra second for each focus switch.
- Saving settings from an old browser tab no longer empties your custom CSS.
- Starting the daemon and `arch-tracker serve` together on first run no longer leaves them with
  different dashboard keys.
- The live status no longer shows media titles of private windows.
- Clearing old history no longer loses the details of something in use at that moment.
- A very large `keep_days` value can no longer delete all history.
- Kitty windows are never matched to another kitty instance.
- Compositors with idle protocol v1 no longer record every idle gap as "inhibited".
- An unfocused private window is no longer recorded as a normal tab with the same title.
- The dashboard's tab title no longer changes with the page, which restarted the window clock every
  time you opened Now.

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
