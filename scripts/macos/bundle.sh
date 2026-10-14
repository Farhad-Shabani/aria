#!/usr/bin/env bash
#
# Puts the release-built daemon inside Aria.app, then signs the bundle.
#
# The CLI lives in Helpers/, not MacOS/, so the app binary is free to be
# called Aria on a case-insensitive disk. The dylib travels with it: the
# rpath is @executable_path.
#
# Usage: bundle.sh <Aria.app>
set -euo pipefail

app="${1:?usage: bundle.sh <Aria.app>}"
scripts="$(dirname "${BASH_SOURCE[0]}")"
release="$(cd "$scripts/../.." && pwd)/target/release"

mkdir -p "$app/Contents/Helpers"
cp -L "$release/aria" "$release/libwebgpu_dawn.dylib" "$app/Contents/Helpers/"
"$scripts/sign.sh" "$app"
