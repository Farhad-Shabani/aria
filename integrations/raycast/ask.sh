#!/usr/bin/env bash

# @raycast.schemaVersion 1
# @raycast.title Aria Ask
# @raycast.mode silent
# @raycast.icon 💬
# @raycast.packageName Aria

set -e
SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
source "$SCRIPT_DIR/../common.sh"

ensure_daemon

osascript - "$ARIA" <<'APPLESCRIPT'
on run argv
    tell application "Terminal"
        do script ((quoted form of (item 1 of argv)) & " ask")
    end tell
end run
APPLESCRIPT
