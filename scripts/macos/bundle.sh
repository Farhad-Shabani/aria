#!/usr/bin/env bash
#
# Puts the release-built daemon inside Aria.app, then signs the bundle.
#
# The daemon gets its own bundle, so its permissions show Aria's name and
# icon. Helpers/aria links to it for the CLI. The dylib sits beside it for
# the @executable_path rpath.
#
# Usage: bundle.sh <Aria.app>
set -euo pipefail

app="${1:?usage: bundle.sh <Aria.app>}"
scripts="$(dirname "${BASH_SOURCE[0]}")"
root="$(cd "$scripts/../.." && pwd)"
helpers="$app/Contents/Helpers"
daemon="$helpers/Aria.app/Contents"

# Only this script writes Helpers/, so a rebuild starts it clean.
rm -rf "$helpers"
mkdir -p "$daemon/MacOS" "$daemon/Resources"
cp -L "$root/target/release/aria" "$root/target/release/libwebgpu_dawn.dylib" "$daemon/MacOS/"
cp "$app/Contents/Resources/Aria.icns" "$daemon/Resources/"
# The binary embeds this plist too, so both carry one identity.
cp "$root/crates/aria/Info.plist" "$daemon/Info.plist"
plutil -insert CFBundleExecutable -string aria "$daemon/Info.plist"
plutil -insert CFBundlePackageType -string APPL "$daemon/Info.plist"
plutil -insert CFBundleIconFile -string Aria "$daemon/Info.plist"
plutil -insert LSUIElement -bool true "$daemon/Info.plist"
ln -s Aria.app/Contents/MacOS/aria "$helpers/aria"
"$scripts/sign.sh" "$app"
