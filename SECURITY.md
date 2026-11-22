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
- **Dashboard.** It listens on `127.0.0.1` only, refuses requests whose Host header is not
  `127.0.0.1` or `localhost` (blocking DNS rebinding), requires a per-run token on every write, and
  sends a strict Content-Security-Policy. Page titles and other recorded text are escaped before
  they are shown.
- **Browser extension.** It sends the host and title of tabs, never paths or query strings, and
  nothing from incognito windows. It talks only to the local native-messaging host.
- **Terminals.** Only the program name and project folder are stored, never arguments or
  environment variables (which can hold secrets).
- **Sockets.** Control and ingest sockets live in `$XDG_RUNTIME_DIR/arch-tracker` with mode 0700.

## Out of scope

Programs running as your own user can read your files, including the database. arch-tracker does
not defend against them, just as it cannot protect any other file in your home directory.
