#!/usr/bin/env bash
# After a release is published on GitHub: sets both AUR packages to its version, fills in the
# checksums of its source and binary tarballs, and rewrites .SRCINFO. Commit that, then push each
# folder to its AUR repository (see packaging/aur/README.md).
#
#   scripts/aur-bump.sh 0.2.0      (on Arch, for makepkg --printsrcinfo)
set -euo pipefail
v=${1:?usage: aur-bump.sh <version>}
root=$(cd "$(dirname "$0")/.." && pwd)
aur=$root/packaging/aur
url=https://github.com/abhimangs/arch-tracker
sum() { curl -fsSL "$1" | sha256sum | cut -d' ' -f1; }
src=$(sum "$url/archive/v$v.tar.gz")
bin=$(sum "$url/releases/download/v$v/arch-tracker-$v-x86_64-linux.tar.gz")
for p in arch-tracker arch-tracker-bin; do
  f="$aur/$p/PKGBUILD"
  sed -i -e "s/^pkgver=.*/pkgver=$v/" -e "s/^pkgrel=.*/pkgrel=1/" "$f"
  case $p in
  arch-tracker) sed -i "s/^sha256sums=.*/sha256sums=('$src')/" "$f" ;;
  *) sed -i "s/^sha256sums=.*/sha256sums=('$bin')/" "$f" ;;
  esac
  cp "$root/packaging/arch-tracker.install" "$aur/$p/"
  (cd "$aur/$p" && makepkg --printsrcinfo >.SRCINFO)
done
echo "packaging/aur is at $v: check it with scripts/aur-check.sh, commit, then push to the AUR."
