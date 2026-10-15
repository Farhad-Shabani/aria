#!/usr/bin/env bash
#
# Signs Aria.app and packs it into a DMG in the repository's dist/.
#
# Usage: package.sh <Aria.app> <version>
set -euo pipefail

app="${1:?usage: package.sh <Aria.app> <version>}"
version="${2:?usage: package.sh <Aria.app> <version>}"
scripts="$(dirname "${BASH_SOURCE[0]}")"
dist="$(cd "$scripts/../.." && pwd)/dist"
dmg="$dist/aria-$version-macos-arm64.dmg"
# Ad hoc unless asked, so no personal identity ships in a release.
export ARIA_SIGN_IDENTITY="${ARIA_SIGN_IDENTITY:--}"

"$scripts/sign.sh" "$app"

# The Applications link gives the window its drag-to-install target.
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
ditto "$app" "$stage/Aria.app"
ln -s /Applications "$stage/Applications"
mkdir -p "$dist"
hdiutil create -quiet -ov -volname Aria -srcfolder "$stage" -format UDZO "$dmg"
if [ "$ARIA_SIGN_IDENTITY" != - ]; then
    codesign --force --timestamp --sign "$ARIA_SIGN_IDENTITY" "$dmg"
fi
