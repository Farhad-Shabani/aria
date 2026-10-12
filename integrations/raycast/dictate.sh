#!/usr/bin/env bash

# @raycast.schemaVersion 1
# @raycast.title Aria Dictate
# @raycast.mode silent
# @raycast.icon 🎙️
# @raycast.packageName Aria

set -e
SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
source "$SCRIPT_DIR/../common.sh"

ensure_daemon

exec "$ARIA" dictate --toggle --stream
