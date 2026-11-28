#!/usr/bin/env bash
# Installs Aria on Linux from ARIA_DIST, the release folder this script sits
# in, or else the latest GitHub release, then hands off to `aria setup`.
# macOS installs from the DMG instead. Re-running upgrades in place, keeps the
# setup answers, and downloads nothing.
# `--uninstall` reverses it and leaves ~/.aria, models included, alone.
# Every other argument goes to setup.
set -euo pipefail

repo=Farhad-Shabani/aria
releases="https://github.com/$repo/releases"
file=aria-linux-x86_64.tar.gz
if [ "$(uname -sm)" != "Linux x86_64" ]; then
    echo "install.sh supports Linux x86_64; on macOS, install Aria.dmg from $releases" >&2
    exit 1
fi

# Piped into bash, the script has no folder, so it never reads the caller's
# working directory as a release.
script="${BASH_SOURCE[0]:-}"
dist="${ARIA_DIST:-}"
if [ -z "$dist" ] && [ -n "$script" ] && [ -f "$(dirname "$script")/$file" ]; then
    dist=$(cd "$(dirname "$script")" && pwd)
fi
bin_dir="$HOME/.local/bin"
lib_dir="$HOME/.local/lib/aria"
input_rule=/etc/udev/rules.d/70-aria-input.rules

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
    if [ -e "$input_rule" ]; then
        echo "note: remove the hotkey's input rule with \`sudo rm $input_rule\`"
    fi
    exit 0
fi

# A private copy, so no one can swap the tarball after its check.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
if [ -z "$dist" ]; then
    # One tag for both downloads, so a release published mid-install can't split them.
    tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$releases/latest")
    case "$tag" in
        */releases/tag/*) ;;
        *) echo "no published release at $releases" >&2; exit 1 ;;
    esac
    base="$releases/download/${tag##*/}"
    curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS"
    curl -fL --progress-bar -o "$work/$file" "$base/$file"
else
    for name in SHA256SUMS "$file"; do
        [ -f "$dist/$name" ] || { echo "no $name in $dist; set ARIA_DIST to a release folder" >&2; exit 1; }
    done
    cp "$dist/SHA256SUMS" "$dist/$file" "$work/"
fi
(cd "$work" && grep -x "[0-9a-f]\{64\}  $file" SHA256SUMS | sha256sum --check --quiet -) || {
    echo "$file doesn't match its checksum; nothing was installed" >&2
    exit 1
}

upgrade=false
[ -d "$lib_dir" ] && upgrade=true

# Unpacks beside the old install, so a failed extract leaves it in place.
rm -rf "$lib_dir.new"
mkdir -p "$bin_dir" "$lib_dir.new"
tar -xzf "$work/$file" --strip-components=1 -C "$lib_dir.new"
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

rule="$lib_dir/share/udev/70-aria-input.rules"
if [ -f "$rule" ] && ! cmp -s "$rule" "$input_rule"; then
    echo "The hotkey reads your keyboard directly. A udev rule gives the logged-in"
    echo "user access to keyboards, mice and touchpads. Aria only reads them, but"
    echo "every program you run could read every key and inject input."
    answer=
    if (: </dev/tty) 2>/dev/null; then
        printf 'Install %s with sudo? [y/N] ' "$input_rule"
        read -r answer </dev/tty || answer=
    fi
    case "$answer" in
        [yY]*)
            if ! { sudo install -m 0644 "$rule" "$input_rule" \
                && sudo udevadm trigger --subsystem-match=input --action=change; }; then
                echo "the input rule was not installed; \`aria doctor\` shows how to add it later" >&2
            fi
            ;;
        *) echo "skipped the input rule; \`aria doctor\` shows how to add it later" ;;
    esac
fi

# A settings file alone may come from dotfiles, so only an upgrade skips setup's
# questions.
if [ "$upgrade" = true ] && [ -f "$HOME/.aria/settings.toml" ]; then
    case " $* " in *" --yes "*) ;; *) set -- --yes "$@" ;; esac
    case " $* " in *" --skip-models "*) ;; *) set -- --skip-models "$@" ;; esac
fi
# Piped into bash, stdin is the script, so setup's prompts read the terminal.
if [ ! -t 0 ] && (: </dev/tty) 2>/dev/null; then
    "$bin_dir/aria" setup "$@" </dev/tty
else
    "$bin_dir/aria" setup "$@"
fi
