#!/usr/bin/env bash
# Has Mozilla sign the Firefox build so Firefox, Zen and every other Firefox-based browser keep it
# installed (release builds drop unsigned add-ons on restart). Signing is "unlisted": private,
# nothing is published on addons.mozilla.org. Output: dist/signed/*.xpi
#
# Needs an addons.mozilla.org API key (https://addons.mozilla.org/developers/addon/api/key/),
# either in the environment (AMO_JWT_ISSUER, AMO_JWT_SECRET) or in
# ~/.config/arch-tracker/amo.env, mode 0600, never in this repository:
#   AMO_JWT_ISSUER=user:12345:67
#   AMO_JWT_SECRET=0123abcd...
# Mozilla signs each extension version once: raise "version" in both extension manifests first.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=${DIST_DIR:-$root/dist}
env_file=${XDG_CONFIG_HOME:-$HOME/.config}/arch-tracker/amo.env

if [[ -z ${AMO_JWT_ISSUER:-} || -z ${AMO_JWT_SECRET:-} ]] && [[ -f $env_file ]]; then
  mode=$(stat -c %a "$env_file")
  if [[ $mode != 600 && $mode != 400 ]]; then
    echo "error: $env_file holds a secret but has mode $mode; run: chmod 600 '$env_file'" >&2
    exit 1
  fi
  # Plain KEY=value lines only; the file is read, never executed.
  while IFS='=' read -r key value; do
    case $key in
      AMO_JWT_ISSUER) AMO_JWT_ISSUER=$value ;;
      AMO_JWT_SECRET) AMO_JWT_SECRET=$value ;;
    esac
  done < <(grep -E '^(AMO_JWT_ISSUER|AMO_JWT_SECRET)=' "$env_file")
fi
if [[ -z ${AMO_JWT_ISSUER:-} || -z ${AMO_JWT_SECRET:-} ]]; then
  echo "error: no addons.mozilla.org API key." >&2
  echo "Create one at https://addons.mozilla.org/developers/addon/api/key/ and put it in" >&2
  echo "$env_file (chmod 600) as AMO_JWT_ISSUER=... and AMO_JWT_SECRET=..." >&2
  exit 1
fi

"$root/scripts/build-extensions.sh"
mkdir -p "$out/signed"
# web-ext reads the key from the environment, so it never shows in the process list.
if ! WEB_EXT_API_KEY=$AMO_JWT_ISSUER WEB_EXT_API_SECRET=$AMO_JWT_SECRET \
  npx --yes web-ext@latest sign --channel=unlisted \
  --source-dir "$out/firefox" --artifacts-dir "$out/signed"; then
  echo "Signing failed. If Mozilla says this version already exists, raise \"version\" in" >&2
  echo "extension/manifest.json and extension/manifest.firefox.json and run this again." >&2
  exit 1
fi
echo "Signed add-on (install it by opening the file in Firefox or Zen):"
ls -1 "$out/signed"/*.xpi
