#!/usr/bin/env bash
# Builds the AUR packages in packaging/aur from this checkout, the way the release in their pkgver
# would build: arch-tracker from a source tarball of the tree, arch-tracker-bin from the release
# tarball scripts/dist.sh makes of that build. Fails when a .SRCINFO is stale, an install file
# differs from packaging/arch-tracker.install, namcap finds an error, or the two packages don't
# install the same files.
#
#   scripts/aur-check.sh      (on Arch as a normal user, with base-devel; namcap if installed)
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
aur=$root/packaging/aur
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
v=$(sed -n 's/^pkgver=//p' "$aur/arch-tracker/PKGBUILD")

for p in arch-tracker arch-tracker-bin; do
  if ! (cd "$aur/$p" && makepkg --printsrcinfo | diff -u .SRCINFO -); then
    echo "packaging/aur/$p/.SRCINFO is stale: (cd packaging/aur/$p && makepkg --printsrcinfo > .SRCINFO)"
    exit 1
  fi
  cmp "$root/packaging/arch-tracker.install" "$aur/$p/arch-tracker.install"
  if [ "$(sed -n 's/^pkgver=//p' "$aur/$p/PKGBUILD")" != "$v" ]; then
    echo "packaging/aur/$p has another pkgver than arch-tracker's $v"
    exit 1
  fi
done

# The source package, from the files git knows of (as a release's source tarball has them).
mkdir -p "$work/src" "$work/bin" "$work/x"
cp "$aur/arch-tracker"/{PKGBUILD,arch-tracker.install} "$work/src/"
(cd "$root" && git ls-files -z --cached --others --exclude-standard |
  tar --null -T - --transform "s,^,arch-tracker-$v/," -czf "$work/src/arch-tracker-$v.tar.gz")
(cd "$work/src" && makepkg --noconfirm --skipchecksums --nocheck)
pkg_src=$(ls "$work/src"/arch-tracker-"$v"-*.pkg.tar.zst)

# The -bin package, from the release tarball of that build.
tar -xf "$pkg_src" -C "$work/x" usr/bin/arch-tracker
"$root/scripts/dist.sh" "$v" "$work/x/usr/bin/arch-tracker" "$work/bin" >/dev/null
cp "$aur/arch-tracker-bin"/{PKGBUILD,arch-tracker.install} "$work/bin/"
(cd "$work/bin" && makepkg --noconfirm --skipchecksums)
pkg_bin=$(ls "$work/bin"/arch-tracker-bin-"$v"-*.pkg.tar.zst)

# The same files, each license under its own package's name.
list() { tar -tf "$1" | grep -v '^\.' | sed 's,^usr/share/licenses/[^/]*/,usr/share/licenses/PKG/,' | sort; }
diff -u <(list "$pkg_src") <(list "$pkg_bin")

if command -v namcap >/dev/null; then
  for f in "$aur/arch-tracker/PKGBUILD" "$aur/arch-tracker-bin/PKGBUILD" "$pkg_src" "$pkg_bin"; do
    out=$(namcap "$f" 2>&1 || true)
    [ -n "$out" ] && echo "$out"
    if grep -q ' E: ' <<<"$out"; then
      echo "namcap found an error in $f"
      exit 1
    fi
  done
fi
echo "ok: arch-tracker and arch-tracker-bin $v install the same $(list "$pkg_src" | wc -l) files"
