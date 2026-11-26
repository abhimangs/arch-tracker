#!/usr/bin/env bash
# Runs the tracker under a headless compositor, with terminal windows open, and checks what it
# records: that it finds the desktop on its own, which window has focus, and that focus changes
# are followed. Everything runs on a private D-Bus session with throwaway folders.
#
#   cargo build && scripts/desktop-smoke.sh sway    (or gnome)
#
# Environment: ARCH_TRACKER_BIN (default target/debug/arch-tracker).
set -euo pipefail

desk=${1:?usage: desktop-smoke.sh sway|gnome}
bin=$(realpath "${ARCH_TRACKER_BIN:-target/debug/arch-tracker}")
root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
export HOME=$tmp/home XDG_CONFIG_HOME=$tmp/config XDG_DATA_HOME=$tmp/data XDG_RUNTIME_DIR=$tmp/run
export BIN=$bin ROOT=$root TMP=$tmp
mkdir -p "$HOME" "$XDG_CONFIG_HOME/arch-tracker" "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
cat >"$XDG_CONFIG_HOME/arch-tracker/config.toml" <<'EOF'
[dashboard]
port = 7695
tray = false
EOF

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    for f in "$tmp"/*.log; do echo "--- $f"; tail -n 40 "$f" || true; done
  fi
  rm -rf "$tmp"
  exit "$status"
}
trap cleanup EXIT

# Inside the D-Bus session: wait_for <jq test> <what>, polling the daemon's status for a minute.
read -r -d '' helpers <<'EOF' || true
wait_for() {
  for _ in $(seq 60); do
    if "$BIN" status --json 2>/dev/null | jq -e "$1" >/dev/null 2>&1; then
      echo "ok: $2"
      return 0
    fi
    sleep 1
  done
  echo "FAILED: $2"
  "$BIN" status --json | jq '{focus, windows: .sources.kwin}' || true
  return 1
}
wayland_socket() {
  for _ in $(seq 100); do
    s=$(cd "$XDG_RUNTIME_DIR" && ls wayland-* 2>/dev/null | grep -v '\.lock$' | head -n 1 || true)
    if [ -n "$s" ]; then echo "$s"; return 0; fi
    sleep 0.2
  done
  return 1
}
EOF
export HELPERS=$helpers

case "$desk" in
sway)
  cat >"$tmp/sway.conf" <<'EOF'
output HEADLESS-1 resolution 1280x800
EOF
  dbus-run-session -- bash -c "$HELPERS"'
    set -euo pipefail
    WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
      sway -c "$TMP/sway.conf" >"$TMP/sway.log" 2>&1 &
    export WAYLAND_DISPLAY=$(wayland_socket)
    export SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock | head -n 1)
    foot sleep 600 >"$TMP/foot1.log" 2>&1 &
    # The tracker finds sway from the socket it left, without SWAYSOCK.
    env -u SWAYSOCK "$BIN" daemon >"$TMP/daemon.log" 2>&1 &
    wait_for ".sources.kwin.desktop == \"Sway\" and .sources.kwin.ok" "the tracker reads Sway'"'"'s windows"
    wait_for ".focus.app == \"foot\" and .focus.program == \"sleep\"" "focus on the terminal and what runs in it"
    foot cat >"$TMP/foot2.log" 2>&1 &
    wait_for ".focus.program == \"cat\"" "focus follows a new window"
    swaymsg -q "focus left"
    wait_for ".focus.program == \"sleep\"" "focus follows swaymsg"
  '
  ;;
gnome)
  ext="$XDG_DATA_HOME/gnome-shell/extensions/arch-tracker@archtracker.dev"
  mkdir -p "$ext"
  cp "$root"/gnome/arch-tracker@archtracker.dev/* "$ext/"
  dbus-run-session -- bash -c "$HELPERS"'
    set -euo pipefail
    gsettings set org.gnome.shell enabled-extensions "[\"arch-tracker@archtracker.dev\"]"
    gsettings set org.gnome.shell disable-user-extensions false
    XDG_SESSION_TYPE=wayland gnome-shell --headless --wayland --virtual-monitor 1280x800 \
      >"$TMP/gnome.log" 2>&1 &
    export WAYLAND_DISPLAY=$(wayland_socket)
    XDG_CURRENT_DESKTOP=GNOME "$BIN" daemon >"$TMP/daemon.log" 2>&1 &
    foot sleep 600 >"$TMP/foot1.log" 2>&1 &
    first=$!
    wait_for ".sources.kwin.desktop == \"GNOME\" and .sources.kwin.ok" "the extension sends GNOME'"'"'s windows"
    wait_for ".focus.app == \"foot\" and .focus.program == \"sleep\"" "focus on the terminal and what runs in it"
    foot cat >"$TMP/foot2.log" 2>&1 &
    wait_for ".focus.program == \"cat\"" "focus follows a new window"
    # What a click on an agent'"'"'s notice does: the extension raises the first window.
    gdbus call --session --dest org.gnome.Shell --object-path /dev/archtracker/Gnome \
      --method dev.archtracker.Gnome1.Activate "[uint32 $first]" "" >/dev/null
    wait_for ".focus.program == \"sleep\"" "the extension raises a window when asked"
    wait_for ".sources.idle.ok" "idle time from Mutter'"'"'s idle monitor"
  '
  ;;
*)
  echo "unknown desktop: $desk" >&2
  exit 2
  ;;
esac
echo "ok: $desk"
