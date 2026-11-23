#!/usr/bin/env bash
# Builds the browser extension for both browser families, unsigned, for local testing:
#   dist/chromium/                     unpacked Chromium build (Load unpacked)
#   dist/firefox/                      unpacked Firefox build (about:debugging, web-ext)
#   dist/arch-tracker-chromium.zip
#   dist/arch-tracker-firefox.xpi      unsigned: Firefox release and Zen only take it as a
#                                      temporary add-on; scripts/sign-firefox.sh signs it.
# DIST_DIR overrides the output folder.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
src=$root/extension
out=${DIST_DIR:-$root/dist}

version_of() { sed -n 's/^ *"version": *"\([^"]*\)".*/\1/p' "$1" | head -n1; }
v_chromium=$(version_of "$src/manifest.json")
v_firefox=$(version_of "$src/manifest.firefox.json")
if [[ -z $v_chromium || $v_chromium != "$v_firefox" ]]; then
  echo "error: extension versions differ: manifest.json has '$v_chromium'," \
    "manifest.firefox.json has '$v_firefox'" >&2
  exit 1
fi

pack() { # <dir> <archive>
  rm -f "$2"
  if command -v zip >/dev/null; then
    (cd "$1" && zip -q -X -r "$2" .)
  elif command -v bsdtar >/dev/null; then
    bsdtar --format zip -cf "$2" -C "$1" .
  else
    echo "error: need zip or bsdtar to pack $2" >&2
    exit 1
  fi
}

mkdir -p "$out"
out=$(cd "$out" && pwd)
rm -rf "$out/chromium" "$out/firefox"
mkdir -p "$out/chromium" "$out/firefox"
cp "$src/manifest.json" "$src/background.js" "$out/chromium/"
cp "$src/background.js" "$out/firefox/"
cp "$src/manifest.firefox.json" "$out/firefox/manifest.json"
pack "$out/chromium" "$out/arch-tracker-chromium.zip"
pack "$out/firefox" "$out/arch-tracker-firefox.xpi"

echo "arch-tracker extension $v_chromium:"
echo "  $out/chromium/  and  $out/arch-tracker-chromium.zip"
echo "  $out/firefox/   and  $out/arch-tracker-firefox.xpi (unsigned)"
