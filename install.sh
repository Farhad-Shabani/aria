#!/usr/bin/env bash
# Installs the packaged Aria from the release folder this script sits in, or
# from ARIA_DIST, then hands off to `aria setup`. Re-running upgrades in
# place. `--uninstall` reverses it and leaves ~/models and ~/.aria alone.
# Every other argument goes to setup.
set -euo pipefail

dist="${ARIA_DIST:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
bin_dir="$HOME/.local/bin"
lib_dir="$HOME/.local/lib/aria"

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
    Darwin-arm64) pattern='macos-arm64\.zip$' ;;
    Linux-x86_64) pattern='linux-x86_64\.tar\.gz$' ;;
    *) echo "unsupported platform: $os $arch" >&2; exit 1 ;;
esac

app_dir() {
    for dir in /Applications "$HOME/Applications"; do
        [ -d "$dir/Aria.app" ] && { echo "$dir"; return; }
    done
    [ -w /Applications ] && echo /Applications || echo "$HOME/Applications"
}

# The app may have spawned its own daemon, which keeps the socket and would
# make the service install refuse. Its pid is in the lock file.
stop_daemon() {
    pkill -x Aria 2>/dev/null || true
    lock="${ARIA_RUNTIME_DIR:-/tmp/aria-$(id -u)}/daemon.lock"
    [ -r "$lock" ] || return 0
    pid=$(cat "$lock" 2>/dev/null || true)
    case "$pid" in ''|*[!0-9]*) return 0 ;; esac
    kill "$pid" 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 1
    done
}

if [ "${1:-}" = --uninstall ]; then
    if command -v aria >/dev/null && ! aria service uninstall; then
        echo "warning: the daemon is still registered; run \`aria service uninstall\`" >&2
    fi
    stop_daemon
    if [ "$os" = Darwin ]; then
        rm -rf "$(app_dir)/Aria.app"
    else
        rm -rf "$lib_dir"
    fi
    rm -f "$bin_dir/aria"
    echo "removed aria; ~/models and ~/.aria were left alone"
    exit 0
fi

[ -f "$dist/SHA256SUMS" ] || { echo "no release at $dist; set ARIA_DIST" >&2; exit 1; }
file=$(awk "/$pattern/ {print \$2}" "$dist/SHA256SUMS")
[ -n "$file" ] || { echo "no artifact for $os $arch in $dist/SHA256SUMS" >&2; exit 1; }
if command -v sha256sum >/dev/null; then
    (cd "$dist" && grep " $file\$" SHA256SUMS | sha256sum --check --quiet -)
else
    (cd "$dist" && grep " $file\$" SHA256SUMS | shasum -a 256 --check --quiet -)
fi

mkdir -p "$bin_dir"
stop_daemon
if [ "$os" = Darwin ]; then
    apps=$(app_dir)
    mkdir -p "$apps"
    rm -rf "$apps/Aria.app"
    ditto -x -k "$dist/$file" "$apps"
    ln -sfn "$apps/Aria.app/Contents/Helpers/aria" "$bin_dir/aria"
    echo "installed $apps/Aria.app"
else
    rm -rf "$lib_dir"
    mkdir -p "$lib_dir"
    tar -xzf "$dist/$file" --strip-components=1 -C "$lib_dir"
    ln -sfn "$lib_dir/bin/aria" "$bin_dir/aria"
    echo "installed $lib_dir"
fi

case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "note: add $bin_dir to PATH" ;;
esac
resolved=$(command -v aria || true)
if [ -n "$resolved" ] && [ "$resolved" != "$bin_dir/aria" ]; then
    echo "note: PATH resolves aria to $resolved; put $bin_dir first"
fi

"$bin_dir/aria" setup "$@"
# The install killed the menu bar app; the service registered by setup owns
# the daemon now, so the app finds it instead of spawning its own.
if [ "$os" = Darwin ]; then
    open "$apps/Aria.app"
fi
