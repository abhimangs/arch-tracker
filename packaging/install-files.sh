#!/bin/sh
# Installs arch-tracker's files into a package: the binary, the systemd user unit, both browser
# extensions and the Firefox bridge, the GNOME Shell extension, the man page and the license.
# Every PKGBUILD calls it (packaging/ and packaging/aur/*), so they all install the same files.
#
#   install-files.sh <source tree> <binary> <pkgdir> <pkgname>
set -eu
src=$1 bin=$2 pkg=$3 name=$4

install -Dm755 "$bin" "$pkg/usr/bin/arch-tracker"
install -Dm644 "$src/packaging/arch-tracker.service" "$pkg/usr/lib/systemd/user/arch-tracker.service"
install -Dm644 "$src/extension/manifest.json" "$pkg/usr/share/arch-tracker/extension/manifest.json"
install -Dm644 "$src/extension/background.js" "$pkg/usr/share/arch-tracker/extension/background.js"
install -Dm644 "$src/extension/manifest.firefox.json" "$pkg/usr/share/arch-tracker/extension-firefox/manifest.json"
install -Dm644 "$src/extension/background.js" "$pkg/usr/share/arch-tracker/extension-firefox/background.js"
# Firefox, Zen, Floorp and Waterfox read this system-wide bridge; LibreWolf its own folder.
install -Dm644 "$src/packaging/dev.archtracker.native.firefox.json" "$pkg/usr/lib/mozilla/native-messaging-hosts/dev.archtracker.native.json"
install -Dm644 "$src/packaging/dev.archtracker.native.firefox.json" "$pkg/usr/lib/librewolf/native-messaging-hosts/dev.archtracker.native.json"
# GNOME Shell extension (arch-tracker setup switches it on).
install -Dm644 -t "$pkg/usr/share/gnome-shell/extensions/arch-tracker@archtracker.dev" \
  "$src/gnome/arch-tracker@archtracker.dev/metadata.json" "$src/gnome/arch-tracker@archtracker.dev/extension.js"
install -Dm644 "$src/packaging/arch-tracker.1" "$pkg/usr/share/man/man1/arch-tracker.1"
install -Dm644 "$src/LICENSE" "$pkg/usr/share/licenses/$name/LICENSE"
