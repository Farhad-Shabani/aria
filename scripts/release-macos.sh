#!/usr/bin/env bash
#
# Puts every license the app ships under into Aria.app, then packs it into a
# DMG in dist/.
#
# Usage: release-macos.sh <version>, after `just app notices`
set -euo pipefail

version="${1:?usage: release-macos.sh <version>}"
cd "$(dirname "${BASH_SOURCE[0]}")/.."

app=clients/build/mac/Build/Products/Release/Aria.app
licenses="$app/Contents/Resources/Licenses"
rm -rf "$licenses"
mkdir -p "$licenses"
cp LICENSE-AGPL target/THIRD-PARTY-LICENSES.md "$licenses/"
for package in clients/build/mac/SourcePackages/checkouts/*/; do
    name=$(basename "$package")
    # Vendored code, such as SwiftNIO's llhttp, keeps its license beside it.
    rsync -a --prune-empty-dirs --exclude='.git/' --exclude='Tests/' --include='*/' \
        --include='LICENSE*' --include='NOTICE*' --exclude='*' "$package" "$licenses/$name/"
    ls "$licenses/$name"/LICENSE* >/dev/null
done
# Signs again, so the licenses fall under the bundle seal.
# Ad hoc unless asked, so no personal identity ships in a release.
ARIA_SIGN_IDENTITY="${ARIA_SIGN_IDENTITY:--}" scripts/bundle.sh "$app"

# The Applications link gives the window its drag-to-install target.
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
ditto "$app" "$stage/Aria.app"
ln -s /Applications "$stage/Applications"
mkdir -p dist
hdiutil create -quiet -ov -volname Aria -srcfolder "$stage" -format UDZO \
    "dist/Aria-$version-macos-arm64.dmg"
