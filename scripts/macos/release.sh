#!/usr/bin/env bash
#
# Puts every license the app ships under into Aria.app, then signs and packs
# it into a DMG in dist/.
#
# Usage: release.sh <version>, after `just app notices`
set -euo pipefail

version="${1:?usage: release.sh <version>}"
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

app=clients/build/mac/Build/Products/Release/Aria.app
licenses="$app/Contents/Resources/Licenses"
rm -rf "$licenses"
mkdir -p "$licenses"
cp LICENSE-GPL target/THIRD-PARTY-LICENSES.md "$licenses/"
for package in clients/build/mac/SourcePackages/checkouts/*/; do
    name=$(basename "$package")
    # Vendored code, such as SwiftNIO's llhttp, keeps its license beside it.
    rsync -a --prune-empty-dirs --exclude='.git/' --exclude='Tests/' --include='*/' \
        --include='LICENSE*' --include='NOTICE*' --exclude='*' "$package" "$licenses/$name/"
    ls "$licenses/$name"/LICENSE* >/dev/null
done
# Signs again, so the licenses fall under the bundle seal.
scripts/macos/package.sh "$app" "$version"
