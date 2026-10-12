#!/usr/bin/env bash
#
# Builds the speech corpus `just accuracy` measures against: LibriSpeech
# test-clean decoded to the canonical 16 kHz mono 16-bit WAV the daemon
# records, and a manifest pairing each recording with what was said.
#
# LibriSpeech is read audiobook prose under CC BY 4.0 — clean recordings with
# no disfluencies. It measures whether a backend hears words at all, not how it
# handles dictation.
#
# The manifest interleaves the 40 speakers round-robin, so any utterance count
# spans them all instead of exhausting one reader at a time.
#
# Usage: corpus.sh <output-directory> [utterances]

set -euo pipefail

readonly ARCHIVE_URL="https://www.openslr.org/resources/12/test-clean.tar.gz"
readonly SPEAKERS=40
readonly SAMPLE_RATE=16000

die() {
    echo "corpus.sh: $*" >&2
    exit 1
}

require() {
    command -v "$1" >/dev/null || die "needs $1 — brew install $1"
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

# Every utterance, speakers interleaved: each speaker's recordings are ranked
# in order, then rank sorts before speaker. Deterministic, so two machines
# build the same manifest.
references() {
    find "$SPEECH" -name '*.trans.txt' -print0 \
        | xargs -0 cat \
        | sed 's/ /\t/' \
        | sort \
        | awk -F'\t' '{ split($1, id, "-"); print ++seen[id[1]] "\t" $0 }' \
        | sort -t $'\t' -k1,1n -k2,2 \
        | cut -f2-
}

# LibriSpeech names a recording `<speaker>-<chapter>-<index>` and files it
# under `<speaker>/<chapter>/`, so the key alone locates it.
recording_for() {
    local key="$1" chapter="${1#*-}"
    printf '%s/LibriSpeech/test-clean/%s/%s/%s.flac' \
        "$SPEECH" "${key%%-*}" "${chapter%%-*}" "$key"
}

# Converts to the one format capture accepts, so a corpus cannot fail later
# for a reason that belongs here.
decode() {
    ffmpeg -nostdin -loglevel error -y -i "$1" \
        -ac 1 -ar "$SAMPLE_RATE" -c:a pcm_s16le "$2" 2>/dev/null
}

build_manifest() {
    local key reference recording wav
    local written=0

    : > "$MANIFEST"
    while IFS=$'\t' read -r key reference; do
        [ "$written" -lt "$WANTED" ] || break

        recording="$(recording_for "$key")"
        [ -f "$recording" ] || die "the archive is missing $recording"

        wav="$AUDIO/$key.wav"
        decode "$recording" "$wav" || die "could not decode $recording"

        printf '%s\t%s\n' "$wav" "$reference" >> "$MANIFEST"
        written=$((written + 1))
    done < <(references)

    [ "$written" -gt 0 ] || die "no utterances were built; the download may have failed"
    echo "$written utterances, $(cut -f2 "$MANIFEST" | wc -w | tr -d ' ') reference words -> $MANIFEST"
}

main() {
    local output="${1:-}"
    [ -n "$output" ] || die "usage: corpus.sh <output-directory> [utterances]"

    readonly WANTED="${2:-100}"
    readonly SPEECH="$output/librispeech"
    readonly AUDIO="$output/wav"
    readonly MANIFEST="$output/manifest.tsv"

    require ffmpeg
    mkdir -p "$SPEECH" "$AUDIO"
    download
    build_manifest
}

main "$@"
