#!/usr/bin/env bash
#
# Fails if a bundled binary needs a newer macOS than LSMinimumSystemVersion.
#
# Usage: check-minos.sh <Aria.app>
set -euo pipefail

app="${1:?usage: check-minos.sh <Aria.app>}"
floor=$(plutil -extract LSMinimumSystemVersion raw "$app/Contents/Info.plist")

status=0
while IFS= read -r -d '' file; do
    kind=$(file -b "$file")
    [[ $kind == Mach-O* ]] || continue
    name="${file#"$app"/}"
    # Skips Mac Catalyst minimums, which use iOS numbering.
    minos=$(vtool -arch arm64 -show-build "$file" |
        awk '$1 == "platform" { platform = $2 } platform == "MACOS" && $1 == "minos" { print $2 }')
    if [ -z "$minos" ]; then
        echo "error: $name has no arm64 macOS minimum" >&2
        status=1
    elif [ "$(printf '%s\n' "$floor" "$minos" | sort -V | tail -1)" != "$floor" ]; then
        echo "error: $name needs macOS $minos, above the app's $floor" >&2
        status=1
    fi
done < <(find "$app" -type f -print0)
exit "$status"
