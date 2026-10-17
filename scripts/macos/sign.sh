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
sparkle="$app/Contents/Frameworks/Sparkle.framework"
sign "$sparkle/Versions/B/XPCServices/Installer.xpc"
sign --preserve-metadata=entitlements "$sparkle/Versions/B/XPCServices/Downloader.xpc"
sign "$sparkle/Versions/B/Autoupdate"
sign "$sparkle/Versions/B/Updater.app"
sign "$sparkle"
# Without the entitlement, recording is silent and macOS never asks. The
# app needs it too: a daemon the app spawns itself is charged to the app.
entitlements="$(dirname "${BASH_SOURCE[0]}")/aria.entitlements"
sign --entitlements "$entitlements" "$app/Contents/Helpers/Aria.app"
sign --entitlements "$entitlements" "$app"
