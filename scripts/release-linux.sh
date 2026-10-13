#!/usr/bin/env bash
#
# Builds the aria binary, then packs it with every license it ships under into
# a tarball in dist/.
#
# Usage: release-linux.sh <version>, after `just notices`
set -euo pipefail

version="${1:?usage: release-linux.sh <version>}"
cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [ "$(uname -sm)" != "Linux x86_64" ]; then
    echo "release-linux.sh builds on Linux x86_64 only" >&2
    exit 1
fi

cargo build --locked --release --bin aria
stage="target/linux-stage/aria-$version"
rm -rf target/linux-stage
mkdir -p "$stage/bin" "$stage/share/doc/aria"
cp target/release/aria "$stage/bin/"
cp LICENSE-GPL target/THIRD-PARTY-LICENSES.md "$stage/share/doc/aria/"
mkdir -p dist
tar -czf "dist/aria-$version-linux-x86_64.tar.gz" -C target/linux-stage "aria-$version"
