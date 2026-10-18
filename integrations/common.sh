# Shared executable resolution for Raycast and the tmux watcher.
_COMMON_FILE="$(readlink -f "${BASH_SOURCE[0]}")"
ARIA_REPO="$(dirname "$(dirname "$_COMMON_FILE")")"
ARIA="$HOME/.local/bin/aria"
if [[ ! -x "$ARIA" ]]; then
    ARIA="$(command -v aria)" || ARIA="$ARIA_REPO/target/release/aria"
fi
if [[ ! -x "$ARIA" ]]; then
    echo "aria: executable missing: $ARIA" >&2
    printf 'Build it with: cargo build --release --manifest-path %q\n' "$ARIA_REPO/Cargo.toml" >&2
    exit 1
fi
ARIA_LAUNCHER_LOG="${TMPDIR:-/tmp}/aria-launcher-$(id -u).log"

# A warm daemon, so a press does not pay for loading the model.
ensure_daemon() {
    "$ARIA" --version >/dev/null || return

    local socket="${ARIA_RUNTIME_DIR:-$HOME/.aria/daemon}/daemon.sock"
    if [[ ! -S "$socket" ]]; then
        "$ARIA" daemon </dev/null >>"$ARIA_LAUNCHER_LOG" 2>&1 &
        disown
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            [[ -S "$socket" ]] && break
            sleep 0.1
        done
    fi

    # The tmux indicator is pushed, and its subscriber dies with the daemon.
    local watch="$ARIA_REPO/integrations/tmux/status.sh"
    local watch_pid="${TMPDIR:-/tmp}/aria-status-watch-$(id -u).pid"
    if [[ -x "$watch" ]] && ! kill -0 "$(cat "$watch_pid" 2>/dev/null)" 2>/dev/null; then
        "$watch" </dev/null >>"$ARIA_LAUNCHER_LOG" 2>&1 &
        echo $! >"$watch_pid"
        disown
    fi
}
