# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Codex's use counted far too low: Codex moves older conversations into its own database and
  removes their logs, so only the newest few were read. Its database is now read too (read-only;
  titles and prompts are never read): a conversation's tokens beyond the responses in the logs are
  estimated, split like your logged responses and spread over its turns, and say so on the page.
  Usage already read is kept for good, whatever Codex later removes.
- Every model Codex has run is priced at OpenAI's list price (standard tier, no batch,
  promotional or other discounts): gpt-5.6 Luna, Terra and Sol, gpt-6 Astra, Sol and Luna,
  gpt-6.1 Terra and Sol, and gpt-6.2 Astra and Sol.

### Added

- Codex's page shows how long its turns ran, from Codex's own records (back to its first use,
  runs inside VS Code included), beside the time the tracker saw it working, per day or hour.
  Cost per hour of work uses the turns.
- Archived Codex conversations, which Codex compresses (.jsonl.zst), are read too.
- Hermes Agent at list prices: its token counts per session and model are read from its own
  database (~/.hermes/state.db and each profile's, read-only; prompts and replies are never read)
  and priced on its page. Models named through a router ("anthropic/claude-…") are priced as the
  model. Settings → AI agents switches it off.
- opencode at list prices: each response's tokens are read from opencode's own database
  (~/.local/share/opencode/opencode.db, read-only; prompts and replies are never read) and priced on
  its page at the model maker's list price, whichever service ran it (DeepSeek and GLM models are
  built in now, besides Claude and OpenAI). Free tiers and stealth models count their tokens and
  show as free. Settings → AI agents switches it off.
- Codex plan-limit alerts: a desktop notification when its 5-hour or weekly limit passes a share
  you choose in Settings → Notifications (70% unless you change it), once per limit, and a banner
  in the dashboard until the limit resets. Notifications can play a sound (on by default).
- Plan value on Claude Code's and Codex's pages: pick the plan you pay for (Codex's is read from
  its logs) and see its monthly list price for the period next to the API-equivalent cost, and how
  many times over the plan pays for itself.
- Agent needs you: a notification when an AI agent you aren't looking at finishes its turn or
  waits for your answer. Claude Code's transcripts, Codex's logs and opencode's database say when a
  turn ends or a tool waits for approval (only the kind and time of their last entries are read);
  any other agent counts once it has gone quiet after working (20 seconds unless you change it).
  Clicking the notice brings its terminal forward: the kitty tab and pane, or the tmux pane, and
  the window, on whichever virtual desktop. Settings → Notifications switches it off.
- Hyprland and Sway: windows come from their IPC sockets (found on their own, also when the session
  didn't export its variables to systemd) as the same snapshots KWin gives, so focus, visible
  windows, terminals, agents and browsers work as on Plasma; a click on an agent's notice raises its
  window there too. `arch-tracker doctor` names the desktop and how to fix it, and CI runs the
  tracker under a headless Sway with real windows.
- `arch-tracker today`: the day in the terminal (screen time against your goal, categories against
  their limits, top apps and sites, projects, and each agent's work and cost); `--date` for another
  day or "yesterday", `--json` for scripts.
- A Week page: screen time day by day, categories, top apps and sites, and agent work and cost,
  each against the week before (while a week runs, against the same days of the week before).
  On the first morning of each week a notification sums up the week before and opens it.

### Changed

- The sidebar lists its pages first and the Browsers and Agents groups after them; the current page
  gets a flat, darker highlight and focus rings only show for the keyboard. Claude Code and Codex
  show their own logos, and so does Hermes.
- CI opens every dashboard page on made-up data, at desktop and phone widths and in both themes,
  and fails on any script error, failed request, empty page or "NaN" on screen.
- Cost per hour of work counts only days with both tracked work and cost, and a plan limit that has
  reset since Codex last reported it shows 0%.

### Added

- An Agents group in the sidebar, like Browsers: Claude Code and Codex (and Hermes once it is
  installed), each with its icon, the time it worked today and a dot while it runs, plus All agents.
  An agent's page shows the time it worked beside the time you watched it, per project and per hour
  or day, your sessions in its terminal with what came right before and after, and its stretches of
  work. The Agents and Claude Code pages moved here; Claude Code's cost is on its page.
- Codex at API prices: its token use per response is read from its own logs in ~/.codex (read-only;
  prompts and replies are never kept) and priced at OpenAI's list prices (standard tier, no batch or
  other discounts, long-context rates over 272K tokens), with its plan's 5-hour and weekly limits.
  Settings → AI agents switches it off.
- An OLED theme, now the default: true black with near-black panels, one quiet accent and soft
  category colours, with a Paper light mode.
- Browsers in the dashboard: a sidebar group lists every browser the extension has connected from,
  with its own icon, today's time and a dot while its extension is connected, plus All browsers
  when there are two or more. A browser's page groups its sites by domain, then subdomain, then
  page title, each with the hours of the day it was used; shows time per hour or day, categories,
  background audio and private-window time; and lists each visit of a day to the millisecond, with
  what came right before and after it, in order or by site. The Timeline can show one browser, site
  rows show which browsers their time came from, and Settings can rename or hide a browser.
  Installed web apps count under the browser that runs them.
- One-click rules: the ⋯ menu on a row in Today, Apps and sites, and Projects sets its category,
  stops recording an app or site, or renames or ignores a project, saved to config.toml at once.
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
- Project folders, picked from a folder browser in Settings that suggests the folders you worked
  in: parent folders (each folder directly inside, like `~/projects/app`, is a project) and single
  project folders, git repository or not.
- A Recompute history button in Settings, which rebuilds Trends' day summaries from the raw data.
- A Claude Code page: what its requests would cost at Anthropic's API prices (built in, so it
  works offline, with cache writes and reads, long prompts, fast mode, US-only inference and web
  search priced as Anthropic does), per day or hour, by kind of token, model and project, next to
  the time Claude worked. New requests show up seconds after Claude Code writes them, workflow
  agents' transcripts included. For the months whose transcripts Claude Code has already deleted
  (after 30 days by default), an estimate from its own statistics, with their double counting
  removed. Transcripts and statistics are only read, never changed; prompts and replies are never
  kept. Your own prices can be added under `[agents.prices]`.
- Desktop notifications: a heads-up before and a notice when a category reaches its daily limit or
  the day reaches its screen-time goal, and a reminder to take a break after a long stretch on
  screen (held during calls and videos). Each comes once a day, clicking one opens Today, one switch
  turns them all off, and Settings can send a test notice.

### Changed

- A calmer look on every page: headline numbers without boxes, solid bars instead of hatched and
  dotted ones, neutral charts, tighter headings, a wider sidebar whose times are never cut off, and a
  content width limit on very wide screens.

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

- An extension connection that went quiet for five minutes (the machine asleep) lost track of its
  browser: background audio from it was filed under a generic "Browser", and with two browsers open
  its tabs could be matched to the other one's windows. Each connection is now tied to its browser
  by the socket itself, a window or the browser's program, and `arch-tracker doctor` lists every
  browser the extension has connected from.
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
