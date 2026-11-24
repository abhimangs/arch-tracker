# Security policy

## Supported versions

Only the latest release receives fixes.

## Reporting a vulnerability

Please report privately through GitHub:
[Security → Report a vulnerability](https://github.com/abhimangs/arch-tracker/security/advisories/new).
Do not open a public issue for security problems. You will get an answer within a week.

## What arch-tracker protects

- **Local data only.** Everything is stored in `~/.local/share/arch-tracker/tracker.db`. Nothing is
  sent over the network.
- **Dashboard.** It listens on `127.0.0.1` only and answers as `arch-tracker.localhost`; other Host
  headers are refused (blocking DNS rebinding) and old `127.0.0.1` links are redirected. It is
  locked: everything but static assets needs a session cookie holding a per-user key
  (`~/.local/share/arch-tracker/dashboard.key`, mode 0600). The tray icon and `arch-tracker open`
  sign a browser in with one-time codes (60 s, single use) handed out over the 0700 control socket,
  so the key never appears in a URL. The cookie is host-only on `arch-tracker.localhost`, so it is
  not sent to other local servers on 127.0.0.1. Every write also needs a per-run token and a
  matching Origin, and pages get a strict Content-Security-Policy. Recorded text is escaped before
  it is shown.
- **Browser extension.** It sends the host and title of tabs, never paths or query strings, and
  from private windows only that a private window is in use. It talks only to the local
  native-messaging host. The Firefox signing key (`amo.env`) is read from your config folder, never
  stored in the repository, and passed to Mozilla's tool through the environment.
- **Terminals.** Only the program name and project folder are stored, never arguments or
  environment variables (which can hold secrets).
- **Claude Code transcripts and statistics.** `~/.claude/projects/**/*.jsonl` and
  `~/.claude/stats-cache.json` are opened read-only (`O_RDONLY | O_NOFOLLOW | O_NOATIME`)
  and never written, created, renamed or locked; symbolic links are not followed. inotify only
  watches the folders for new lines. Only each reply's
  model, time, token counts and folder are stored, with its ids reduced to a 64-bit hash. Prompts,
  replies and file contents are parsed past without being kept, and the dashboard serves totals only.
- **Sockets.** Control and ingest sockets live in `$XDG_RUNTIME_DIR/arch-tracker` with mode 0700.

## Out of scope

Programs running as your own user without a sandbox can read your files, including the database and
the dashboard key. arch-tracker does not defend against them, just as it cannot protect any other
file in your home directory. The dashboard lock keeps out programs that can only reach the network
(sandboxed apps, other users).
