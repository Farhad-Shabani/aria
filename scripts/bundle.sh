#!/usr/bin/env bash
#
# Puts the release-built daemon inside Aria.app, then signs the bundle.
#
# The CLI lives in Helpers/, not MacOS/, so the app binary is free to be
# called Aria on a case-insensitive disk. The dylib travels with it: the
# rpath is @executable_path.
#
# A real signature keys the Accessibility grant to the identifier and
# certificate, so it survives a rebuild; ad-hoc keys it to the binary's hash
# and dies on every build. The first codesigning identity signs, or ad-hoc
# when none exists. Override with ARIA_SIGN_IDENTITY.
#
# Usage: bundle.sh <Aria.app>
set -euo pipefail

app="${1:?usage: bundle.sh <Aria.app>}"
release="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/release"

mkdir -p "$app/Contents/Helpers"
cp -L "$release/aria" "$release/libwebgpu_dawn.dylib" "$app/Contents/Helpers/"
identity="${ARIA_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | awk '/\) [0-9A-F]{40} "/ {print $2; exit}')}"
identity="${identity:--}"
# Notarization needs the hardened runtime and a timestamp. Ad-hoc code gets
# neither. It has no Team ID, so the runtime would refuse the daemon's dylib.
if [ "$identity" = - ]; then
    sign() { codesign --force --sign - "$@"; }
else
    sign() { codesign --force --options runtime --timestamp --sign "$identity" "$@"; }
fi
# Inner code signs first, so the app's seal covers the final signatures.
find "$app/Contents" -name '*.dylib' -not -path '*/Resources/*' | while read -r library; do
    sign "$library"
done
# The daemon, not the app, records the microphone.
sign --entitlements "$(dirname "${BASH_SOURCE[0]}")/aria.entitlements" "$app/Contents/Helpers/aria"
sign "$app"
