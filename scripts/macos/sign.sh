#!/usr/bin/env bash
#
# Signs Aria.app from the inside out: its dylibs, then the daemon, then the
# bundle, so each seal covers the final signatures below it.
#
# A real signature keys the Accessibility grant to the identifier and
# certificate, so it survives a rebuild; ad-hoc keys it to the binary's hash
# and dies on every build. The first codesigning identity signs, or ad-hoc
# when none exists. Override with ARIA_SIGN_IDENTITY.
#
# Usage: sign.sh <Aria.app>
set -euo pipefail

app="${1:?usage: sign.sh <Aria.app>}"
identity="${ARIA_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | awk '/\) [0-9A-F]{40} "/ {print $2; exit}')}"
identity="${identity:--}"
# Notarization needs the hardened runtime and a timestamp. Ad-hoc code gets
# neither. It has no Team ID, so the runtime would refuse the daemon's dylib.
if [ "$identity" = - ]; then
    sign() { codesign --force --sign - "$@"; }
else
    sign() { codesign --force --options runtime --timestamp --sign "$identity" "$@"; }
fi
find "$app/Contents" -name '*.dylib' -not -path '*/Resources/*' | while read -r library; do
    sign "$library"
done
# The daemon, not the app, records the microphone.
sign --entitlements "$(dirname "${BASH_SOURCE[0]}")/aria.entitlements" "$app/Contents/Helpers/aria"
sign "$app"
