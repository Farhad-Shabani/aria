#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd -P)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/aria-shell.XXXXXX")"
fixture="$(cd "$fixture" && pwd -P)"
trap 'rm -rf "$fixture"' EXIT

repo="$fixture/repo with 'quotes' and \$dollars"
test_home="$fixture/home"
caller="$fixture/caller"
mkdir -p "$repo/target/release" "$repo/integrations" \
    "$test_home/.local/bin" "$test_home/.aria" "$caller"
cp -R "$REPO/integrations/common.sh" "$REPO/integrations/raycast" "$REPO/integrations/tmux" \
    "$repo/integrations/"
printf 'exit 99\n' >"$test_home/.aria/env"
cat >"$repo/target/release/aria" <<'STUB'
#!/usr/bin/env bash
printf '%s\0' "$PWD" "$@"
exit 37
STUB
chmod +x "$repo/target/release/aria"

ln -s "$repo/target/release/aria" "$test_home/.local/bin/aria-link"
ln -s aria-link "$test_home/.local/bin/aria"
cd "$caller"

ln -s "$repo/integrations/raycast" "$fixture/raycast"
test_path="$fixture/path bin"
mkdir "$test_path"
for command in bash dirname readlink id; do
    ln -s "$(command -v "$command")" "$test_path/$command"
done
cp "$repo/target/release/aria" "$test_path/aria"
resolve() {
    env HOME="$test_home" PATH="$test_path" bash -eu -c 'source "$1"; printf "%s\n" "$ARIA"' \
        bash "$repo/integrations/common.sh"
}
[[ "$(resolve)" == "$test_home/.local/bin/aria" ]]
mv "$test_home/.local/bin/aria" "$test_home/.local/bin/aria.saved"
[[ "$(resolve)" == "$test_path/aria" ]]
mv "$test_path/aria" "$test_path/aria.saved"
[[ "$(resolve)" == "$repo/target/release/aria" ]]

# A missing build fails before any daemon or watcher can start.
mv "$repo/target/release/aria" "$repo/target/release/aria.saved"
# Symlinked entry points run last, so the build check below covers them.
ln -s "$repo/integrations/raycast/dictate.sh" "$fixture/dictate-link.sh"
for executable in "$repo/integrations/tmux/status.sh" "$fixture/raycast/dictate.sh" \
    "$fixture/raycast/dictate-copy.sh" "$fixture/raycast/ask.sh" "$fixture/dictate-link.sh"; do
    status=0
    env HOME="$test_home" PATH="$test_path" "$executable" \
        >"$fixture/stdout" 2>"$fixture/stderr" || status=$?
    [[ "$status" -eq 1 ]]
    [[ ! -s "$fixture/stdout" ]]
    grep -q 'executable missing:' "$fixture/stderr"
    grep -q 'cargo build --release --manifest-path' "$fixture/stderr"
done

# Run the printed build instruction against a fake cargo to check shell quoting.
mkdir "$fixture/tools"
cat >"$fixture/tools/cargo" <<'STUB'
#!/usr/bin/env bash
printf '%s\0' "$@"
STUB
chmod +x "$fixture/tools/cargo"
build_command="$(sed -n 's/^Build it with: //p' "$fixture/stderr")"
PATH="$fixture/tools:$PATH" bash -c "$build_command" >"$fixture/actual"
printf '%s\0' build --release --manifest-path "$repo/Cargo.toml" >"$fixture/expected"
cmp "$fixture/expected" "$fixture/actual"

# Evaluate the launcher's quoting without sending anything to Terminal.
if [[ "$(uname -s)" == Darwin ]]; then
    sed -n '/^on run argv$/,/^end run$/p' "$REPO/integrations/raycast/ask.sh" |
        sed '/tell application "Terminal"/d; /end tell/d; s/do script /return /' \
        >"$fixture/quote.applescript"
    mv "$repo/target/release/aria.saved" "$repo/target/release/aria"
    terminal_command="$(osascript "$fixture/quote.applescript" "$repo/target/release/aria")"
    status=0
    bash -c "$terminal_command" >"$fixture/actual" || status=$?
    [[ "$status" -eq 37 ]]
    printf '%s\0' "$caller" ask >"$fixture/expected"
    cmp "$fixture/expected" "$fixture/actual"
fi

# Exercise the event consumer with our status callback, without a tmux server.
sed -n '/^watch_events() {$/,/^}$/p' "$REPO/integrations/tmux/status.sh" >"$fixture/watch-events.sh"
source "$fixture/watch-events.sh"
RECORDING=recording
TRANSCRIBING=working
set_status() { printf '%s\n' "$1"; }
printf '%s\n' recording transcribing delivering delivered recording 'failed BUSY' \
    recording cancelled recording 'chat 1 codex thinking' | watch_events >"$fixture/actual"
printf '%s\n' recording working working '' recording '' recording '' recording '' >"$fixture/expected"
cmp "$fixture/expected" "$fixture/actual"

printf 'PASS: executable resolution, cwd/args/status, missing-build errors, Terminal quoting (macOS), and event disconnect cleanup\n'
