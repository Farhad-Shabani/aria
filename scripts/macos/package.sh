#!/usr/bin/env bash
#
# Signs Aria.app and packs it into a DMG in the repository's dist/.
#
# Usage: package.sh <Aria.app>
set -euo pipefail

app="${1:?usage: package.sh <Aria.app>}"
scripts="$(dirname "${BASH_SOURCE[0]}")"
dist="$(cd "$scripts/../.." && pwd)/dist"
dmg="$dist/Aria.dmg"
# Ad hoc unless asked, so no personal identity ships in a release.
export ARIA_SIGN_IDENTITY="${ARIA_SIGN_IDENTITY:--}"

"$scripts/sign.sh" "$app"

# The Applications link gives the window its drag-to-install target. The
# .DS_Store holds the layout: 160px icons at 180,190 and 480,190 in a 660x400
# window. dmgbuild wrote it against the volume name "Aria" and
# /.background.png, so both must stay. Regenerate it the same way.
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
ditto "$app" "$stage/Aria.app"
ln -s /Applications "$stage/Applications"
mkdir -p "$dist"
cp "$scripts/dmg-background.png" "$stage/.background.png"
cp "$scripts/dmg.DS_Store" "$stage/.DS_Store"
hdiutil create -quiet -ov -volname Aria -srcfolder "$stage" -format ULFO "$dmg"
if [ "$ARIA_SIGN_IDENTITY" != - ]; then
    codesign --force --timestamp --sign "$ARIA_SIGN_IDENTITY" "$dmg"
fi
