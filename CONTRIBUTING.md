# Contributing

Thanks for helping. This is a small project; issues and pull requests are welcome.

## Build and test

You need Rust (stable), SQLite headers, and a KDE Plasma 6 Wayland session to run the daemon.

```sh
cargo build
cargo test
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
node --check web-ui/app.js
```

CI runs the same checks and builds the Arch package on every push.

## Running a development build

The installed service holds the daemon lock, so stop it first:

```sh
systemctl --user stop arch-tracker
RUST_LOG=arch_tracker=debug cargo run -- daemon
cargo run -- status      # in another terminal
systemctl --user start arch-tracker   # when done
```

The dashboard files in `web-ui/` are embedded into the binary, so rebuild after editing them.
To look at the dashboard without your own data, point `arch-tracker serve --db <file>` at a
demo database.

## Where things live

| Path | What it is |
| --- | --- |
| `src/daemon/` | The tracker actor, event loop, control socket and config watcher |
| `src/sources/` | Event sources: `kwin`, `idle`, `session`, `media`, `browser`, `terminal`, `running` |
| `src/store/` | SQLite storage and the batched ledger |
| `src/report/` | Classification of raw intervals into screen time, and day/range reports |
| `src/web/`, `web-ui/` | The dashboard server and its HTML, CSS and JavaScript |
| `kwin/` | The KWin script (embedded) |
| `extension/` | The browser extension |
| `packaging/` | PKGBUILD, systemd user unit and install hook |

## Adding a desktop

Window tracking for another desktop is a new module in `src/sources/` that produces the same
snapshot data as `kwin.rs` (windows in stacking order plus the focused one). Idle detection
already works on compositors that offer `ext-idle-notify-v1`.

## Adding an AI agent

Agents are recognised by name in `config.toml` (`[[agents.tools]]`). Defaults live in
`src/config.rs`; add one there with a test in `src/sources/terminal/tools.rs`.

## Rules

- Keep the daemon light: event-driven, no busy polling, batched writes.
- Never store arguments, environment variables, URL paths or anything from incognito windows.
- Tests and fixtures use made-up data, never real titles, tabs or home paths.
- Commit messages: `area: what changed`, for example `kwin: debounce snapshots`.
- Add a line to the `Unreleased` section of `CHANGELOG.md` for user-visible changes.
