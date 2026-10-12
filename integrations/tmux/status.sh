#!/usr/bin/env bash

# Pushes the dictation stage into @aria-status, which polling is too slow to do.

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
source "$SCRIPT_DIR/../common.sh"
"$ARIA" --version >/dev/null || exit

RECORDING="#[fg=#e06c75]◉#[fg=default]"
TRANSCRIBING="#[fg=#e5c07b]◌#[fg=default]"

set_status() {
    if [[ -n "$1" ]]; then
        tmux set -g @aria-status "$1" 2>/dev/null
    else
        tmux set -gu @aria-status 2>/dev/null
    fi
}

watch_events() {
    while IFS= read -r stage; do
        case "$stage" in
            recording) set_status "$RECORDING" ;;
            transcribing | delivering) set_status "$TRANSCRIBING" ;;
            delivered | cancelled | failed*) set_status "" ;;
        esac
    done
    # A disconnected stream cannot tell us whether the press is still active.
    set_status ""
}

# A daemon still loading its model refuses the subscription, and one being
# replaced takes its socket away for a moment. Both are worth waiting out; a
# socket gone this long is a daemon that has left.
SOCKET="${ARIA_RUNTIME_DIR:-/tmp/aria-$(id -u)}/daemon.sock"
GRACE=20

missing=0
while ((missing < GRACE)); do
    if [[ -S "$SOCKET" ]]; then
        missing=0
        "$ARIA" events 2>/dev/null | watch_events
    else
        ((missing++))
    fi
    sleep 0.5
done

# The daemon is gone, so no press can be in flight.
set_status ""
