#!/usr/bin/env bash
# Installs Aria on Linux from ARIA_DIST, the release folder this script sits
# in, or else the latest GitHub release, then hands off to `aria setup`.
# macOS installs from the DMG instead. Re-running upgrades in place, keeps the
# setup answers, and downloads nothing.
# `--uninstall` reverses it and leaves ~/.aria, models included, alone.
# Every other argument goes to setup.
set -euo pipefail

releases="https://github.com/Farhad-Shabani/aria/releases"
if [ "$(uname -sm)" != "Linux x86_64" ]; then
    echo "install.sh supports Linux x86_64; on macOS, install Aria.dmg from $releases" >&2
    exit 1
fi

# Piped into bash, the script has no folder, so it never reads the caller's
# working directory as a release.
script="${BASH_SOURCE[0]:-}"
dist="${ARIA_DIST:-}"
if [ -z "$dist" ] && [ -n "$script" ] && [ -f "$(dirname "$script")/SHA256SUMS" ]; then
    dist=$(cd "$(dirname "$script")" && pwd)
fi
bin_dir="$HOME/.local/bin"
lib_dir="$HOME/.local/lib/aria"

# A running daemon keeps the socket and would make the service install
# refuse. Its pid is in the lock file.
stop_daemon() {
    lock="${ARIA_RUNTIME_DIR:-$HOME/.aria/daemon}/daemon.lock"
    [ -r "$lock" ] || return 0
    pid=$(cat "$lock" 2>/dev/null || true)
    case "$pid" in ''|*[!0-9]*) return 0 ;; esac
    kill "$pid" 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 1
    done
    echo "the daemon (pid $pid) did not stop; stop it, then rerun" >&2
    exit 1
}

if [ "${1:-}" = --uninstall ]; then
    if [ -e "$bin_dir/aria" ] || [ -L "$bin_dir/aria" ]; then
        "$bin_dir/aria" service uninstall \
            || echo "warning: the daemon is still registered; run \`aria service uninstall\`" >&2
    fi
    stop_daemon
    rm -rf "$lib_dir"
    rm -f "$bin_dir/aria"
    echo "removed aria; ~/.aria and its models were left alone"
    exit 0
fi

remote=
if [ -z "$dist" ]; then
    remote=1
    # One tag for both downloads, so a release published mid-install can't split them.
    tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$releases/latest")
    case "$tag" in
        */releases/tag/*) ;;
        *) echo "no published release at $releases" >&2; exit 1 ;;
    esac
    base="$releases/download/${tag##*/}"
    dist=$(mktemp -d)
    trap 'rm -rf "$dist"' EXIT
    curl -fsSL -o "$dist/SHA256SUMS" "$base/SHA256SUMS"
fi
[ -f "$dist/SHA256SUMS" ] || { echo "no release at $dist; set ARIA_DIST" >&2; exit 1; }
file=$(awk '/linux-x86_64\.tar\.gz$/ {print $2}' "$dist/SHA256SUMS")
[ -n "$file" ] || { echo "no Linux x86_64 build in $dist/SHA256SUMS" >&2; exit 1; }
if [ -n "$remote" ]; then
    curl -fL --progress-bar -o "$dist/$file" "$base/$file"
fi
(cd "$dist" && grep " $file\$" SHA256SUMS | sha256sum --check --quiet -)

# Unpacks beside the old install, so a failed extract leaves it in place.
rm -rf "$lib_dir.new"
mkdir -p "$bin_dir" "$lib_dir.new"
tar -xzf "$dist/$file" --strip-components=1 -C "$lib_dir.new"
stop_daemon
rm -rf "$lib_dir"
mv "$lib_dir.new" "$lib_dir"
ln -sfn "$lib_dir/bin/aria" "$bin_dir/aria"
echo "installed $lib_dir"

case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "note: add $bin_dir to PATH" ;;
esac
resolved=$(command -v aria || true)
if [ -n "$resolved" ] && [ "$resolved" != "$bin_dir/aria" ]; then
    echo "note: PATH resolves aria to $resolved; put $bin_dir first"
fi

if [ -f "$HOME/.aria/config.yaml" ]; then
    case " $* " in *" --yes "*) ;; *) set -- --yes "$@" ;; esac
    case " $* " in *" --skip-models "*) ;; *) set -- --skip-models "$@" ;; esac
fi
# Piped into bash, stdin is the script, so setup's prompts read the terminal.
if [ ! -t 0 ] && (: </dev/tty) 2>/dev/null; then
    "$bin_dir/aria" setup "$@" </dev/tty
else
    "$bin_dir/aria" setup "$@"
fi
