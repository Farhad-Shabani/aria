#!/usr/bin/env bash
#
# Rebuilds src/transcription/layer/vocabulary/english.fst, an fst set, from
# SCOWL 2020.12.07. The fuzzy pass leaves these words as heard, so the sizes and
# lists here decide which near misses still get corrected: size 50 already
# holds "flicker", which a listed "Flickr" should catch.
#
# Usage: english-words.sh [scowl-2020.12.07.tar.gz]
#
# Without an archive the script downloads it. Either way it checks the
# archive against the SHA-256 that THIRD-PARTY-NOTICES.md records.

set -euo pipefail
# The same bytes on every machine: [a-z] and sort order follow the locale.
export LC_ALL=C

readonly ARCHIVE_URL="https://downloads.sourceforge.net/project/wordlist/SCOWL/2020.12.07/scowl-2020.12.07.tar.gz"
readonly ARCHIVE_SHA256="5587667caa20c4891390c2d42dbb4d5c4c3f41bee77af1457ece3ba23fb859cc"

die() {
    echo "english-words.sh: $*" >&2
    exit 1
}

crate="$(cd "$(dirname "$0")/.." && pwd)"
output="$crate/src/transcription/layer/vocabulary/english.fst"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

archive="${1:-}"
if [[ -z "$archive" ]]; then
    archive="$work/scowl.tar.gz"
    curl --fail --location --silent --show-error --output "$archive" "$ARCHIVE_URL"
fi
[[ -f "$archive" ]] || die "no archive at $archive"
if command -v sha256sum >/dev/null; then
    actual="$(sha256sum "$archive" | cut -d ' ' -f 1)"
else
    actual="$(shasum -a 256 "$archive" | cut -d ' ' -f 1)"
fi
[[ "$actual" == "$ARCHIVE_SHA256" ]] || die "archive SHA-256 is $actual"

tar -xzf "$archive" -C "$work"
final="$work/scowl-2020.12.07/final"

{
    cat "$final"/{english,american,british}-words.{10,20,35} \
        | iconv -f ISO-8859-1 -t UTF-8 \
        | grep -E '^[a-z]+$'
    cat "$final"/english-contractions.{10,35} \
        | iconv -f ISO-8859-1 -t UTF-8 \
        | tr '[:upper:]' '[:lower:]' \
        | grep -E "^[a-z][a-z']{2,}$"
} | sort -u >"$work/words.txt"

cargo run --quiet --release --manifest-path "$crate/Cargo.toml" \
    --example english_fst -- "$output" <"$work/words.txt"
echo "english-words.sh: wrote $(wc -l <"$work/words.txt" | tr -d ' ') words to $output"
