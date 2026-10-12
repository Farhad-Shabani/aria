#!/usr/bin/env bash
#
# Builds a long-form speech corpus for streaming runs: consecutive LibriSpeech
# test-clean utterances concatenated per chapter into 90-115 s recordings, one
# chapter per speaker, decoded to the canonical 16 kHz mono 16-bit WAV. Long
# recordings are what make agreement trims fire; the single-utterance corpus
# from corpus.sh rarely spans a buffer cap.
#
# Every recording stays under aria-bench's 120 s utterance ceiling.
#
# Usage: corpus-long.sh <output-directory> [recordings] [librispeech-cache]
#
# The cache argument shares the 346 MB archive with an existing corpus, e.g.
# build/corpus/librispeech. To measure against it:
#
#   aria-bench stt --corpus build/corpus-long/manifest.tsv --cadence 750/20000 \
#       --model parakeet=~/models/parakeet/parakeet-tdt-0.6b-v3

set -euo pipefail

readonly ARCHIVE_URL="https://www.openslr.org/resources/12/test-clean.tar.gz"
readonly SPEAKERS=40
readonly SAMPLE_RATE=16000
readonly MIN_MS=90000
readonly MAX_MS=115000

die() {
    echo "corpus-long.sh: $*" >&2
    exit 1
}

require() {
    command -v "$1" >/dev/null || die "needs $1 — brew install ${2:-$1}"
}

# The whole archive, once. Fewer speakers on disk than the set holds means a
# partial download, which gets replaced.
download() {
    local on_disk=0
    if [ -d "$SPEECH/LibriSpeech/test-clean" ]; then
        on_disk=$(find "$SPEECH/LibriSpeech/test-clean" -mindepth 1 -maxdepth 1 -type d | wc -l)
    fi
    [ "$on_disk" -ge "$SPEAKERS" ] && return 0

    rm -rf "$SPEECH/LibriSpeech"
    echo "downloading LibriSpeech test-clean (346 MB)..."
    curl --silent --fail "$ARCHIVE_URL" | tar -xz -C "$SPEECH"
}

# Milliseconds, not floored seconds: the guards compare accumulated audio
# against the 115 s window, and per-file flooring could smuggle a bundle past
# the bench's hard 120 s utterance ceiling.
millis_of() {
    local duration
    duration=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$1")
    [ -n "$duration" ] || return 1
    awk -v d="$duration" 'BEGIN { printf "%d", d * 1000 }'
}

# One speaker's first chapter, its utterances in transcript order, bundled
# until the total lands in the 90-115 s window. A chapter too short for the
# window builds nothing and the speaker is skipped.
build_recording() {
    local chapter_dir="$1" speaker="$2" chapter="$3"
    local trans="$chapter_dir/$speaker-$chapter.trans.txt"
    [ -f "$trans" ] || return 1

    local list reference="" total=0 key text flac millis
    list=$(mktemp)
    while read -r key text; do
        flac="$chapter_dir/$key.flac"
        [ -f "$flac" ] || die "the archive is missing $flac"
        millis=$(millis_of "$flac") || die "ffprobe failed for $flac"
        [ $((total + millis)) -gt "$MAX_MS" ] && break
        echo "file '$flac'" >>"$list"
        reference="$reference $text"
        total=$((total + millis))
        [ "$total" -ge "$MIN_MS" ] && break
    done < <(sort "$trans")

    if [ "$total" -lt "$MIN_MS" ]; then
        rm -f "$list"
        return 1
    fi

    # Explicit failure checks: this function runs as an `if` condition, where
    # `set -e` is suppressed for its whole body.
    local wav="$AUDIO/$speaker-$chapter.wav"
    ffmpeg -nostdin -loglevel error -y -f concat -safe 0 -i "$list" \
        -ac 1 -ar "$SAMPLE_RATE" -c:a pcm_s16le "$wav" || die "ffmpeg failed for $wav"
    rm -f "$list"
    printf '%s\t%s\n' "$wav" "${reference# }" >>"$MANIFEST"
    echo "$speaker-$chapter: $((total / 1000))s"
}

build_manifest() {
    local speaker_dir chapter_dir speaker chapter
    local built=0

    : >"$MANIFEST"
    for speaker_dir in "$SPEECH"/LibriSpeech/test-clean/*/; do
        [ "$built" -ge "$WANTED" ] && break
        speaker=$(basename "$speaker_dir")
        chapter_dir=$(find "$speaker_dir" -mindepth 1 -maxdepth 1 -type d | sort | head -1)
        chapter=$(basename "$chapter_dir")
        if build_recording "$chapter_dir" "$speaker" "$chapter"; then
            built=$((built + 1))
        fi
    done

    [ "$built" -gt 0 ] || die "no recordings were built; the download may have failed"
    echo "$built recordings, $(cut -f2 "$MANIFEST" | wc -w | tr -d ' ') reference words -> $MANIFEST"
}

main() {
    local output="${1:-}"
    [ -n "$output" ] || die "usage: corpus-long.sh <output-directory> [recordings] [librispeech-cache]"

    readonly WANTED="${2:-12}"
    readonly SPEECH="${3:-$output/librispeech}"
    readonly AUDIO="$output/wav"
    readonly MANIFEST="$output/manifest.tsv"

    require ffmpeg
    require ffprobe ffmpeg
    mkdir -p "$SPEECH" "$AUDIO"
    download
    build_manifest
}

main "$@"
