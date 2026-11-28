#!/usr/bin/env bash
# The binary release tarball, arch-tracker-<version>-x86_64-linux.tar.gz: the binary and every
# file the package installs, laid out as in the repository, so packaging/install-files.sh works
# from it. The release workflow publishes it; the AUR's arch-tracker-bin installs from it.
#
#   scripts/dist.sh <version> <binary> <out dir>
set -euo pipefail
v=${1:?usage: dist.sh <version> <binary> <out dir>}
bin=${2:?binary}
out=${3:?out dir}
root=$(cd "$(dirname "$0")/.." && pwd)
name="arch-tracker-$v-x86_64-linux"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
d="$stage/$name"
mkdir -p "$d/packaging" "$d/extension" "$d/web-ui" "$d/gnome/arch-tracker@archtracker.dev"
install -m755 "$bin" "$d/arch-tracker"
cp "$root"/extension/{manifest.json,manifest.firefox.json,background.js,icon.png} "$d/extension/"
cp "$root"/web-ui/icon.svg "$d/web-ui/"
cp "$root"/gnome/arch-tracker@archtracker.dev/{metadata.json,extension.js} "$d/gnome/arch-tracker@archtracker.dev/"
cp "$root"/packaging/{arch-tracker.service,arch-tracker.1,arch-tracker.install,dev.archtracker.native.firefox.json,install-files.sh} "$d/packaging/"
cp "$root"/{README.md,LICENSE,CHANGELOG.md} "$d/"
mkdir -p "$out"
tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
echo "$out/$name.tar.gz"
