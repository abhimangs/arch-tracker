# arch-tracker

A local screen-time and activity tracker for KDE Plasma 6 on Wayland, written in Rust.
Nothing leaves your computer: data lives in `~/.local/share/arch-tracker/tracker.db` and the
dashboard only answers on `127.0.0.1`.

What it records:

- **Focus**: the window you are using, to the second (KWin script, event-driven).
- **Visible**: windows beside the focused one (side by side, picture-in-picture).
- **Browser tabs**: host (subdomains kept apart) and page title, through a small Brave/Chromium
  extension; Plasma Browser Integration and the session file are fallbacks. Incognito time is
  counted as "Private browsing" with no details.
- **Terminals**: the program and project in focus (kitty exactly through remote control; ghostty
  and others through /proc; tmux panes).
- **AI agents**: Claude Code, Codex, opencode, Antigravity, Cursor Agent, Hermes, Gemini CLI and
  more, per project: the time you watched them and the time they were working.
- **Idle, watching and calls**: raw no-input gaps, visible media and the microphone, so the
  dashboard can decide what counts. Change a rule and every past day is re-classified.
- **Background listening** and **running but unused** apps, never counted as screen time.

## Install (Arch)

```sh
cd packaging && makepkg -si
systemctl --user enable --now arch-tracker
arch-tracker setup     # registers the browser bridge and prints the extension steps
arch-tracker doctor    # checks every source
```

Then open http://127.0.0.1:7600/ and load the extension once in Brave
(`brave://extensions` → Developer mode → Load unpacked → `/usr/share/arch-tracker/extension`).

## Commands

| Command | What it does |
| --- | --- |
| `arch-tracker status` | What is being tracked right now and which sources work |
| `arch-tracker pause 1h` / `resume` | Pause recording (no duration = until resume) |
| `arch-tracker doctor` | Diagnose problems, with fixes |
| `arch-tracker export --from 2026-11-01 --to 2026-11-22 --format csv` | Raw intervals |
| `arch-tracker setup` | Unpack the extension, register the native-messaging host |

## Settings

Everything is in `~/.config/arch-tracker/config.toml` and on the dashboard's Settings page:
idle and call thresholds, what counts as watching, categories, AI agent names, privacy exclusions,
the dashboard port, theme and custom CSS. Edits are picked up live.

## Other desktops

Each source (windows, idle, browser, terminal) is separate, so GNOME, Hyprland or Sway support can
be added as new window sources. Idle detection (ext-idle-notify) already works on most wlroots
compositors.
