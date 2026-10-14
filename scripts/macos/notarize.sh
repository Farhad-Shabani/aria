#!/usr/bin/env bash
#
# Notarizes a signed DMG, staples the ticket, and checks it the way
# Gatekeeper will.
#
# Reads the App Store Connect API key: APPLE_NOTARIZATION_KEY (the .p8
# contents), APPLE_NOTARIZATION_KEY_ID and APPLE_NOTARIZATION_ISSUER_ID.
#
# Usage: notarize.sh <dmg>
set -euo pipefail

dmg="${1:?usage: notarize.sh <dmg>}"
: "${APPLE_NOTARIZATION_KEY:?}" "${APPLE_NOTARIZATION_KEY_ID:?}" "${APPLE_NOTARIZATION_ISSUER_ID:?}"

key=$(mktemp)
result=$(mktemp)
trap 'rm -f "$key" "$result"' EXIT
printf '%s' "$APPLE_NOTARIZATION_KEY" > "$key"
auth=(--key "$key" --key-id "$APPLE_NOTARIZATION_KEY_ID" --issuer "$APPLE_NOTARIZATION_ISSUER_ID")

xcrun notarytool submit "$dmg" "${auth[@]}" --wait --output-format json > "$result"
xcrun notarytool log "$(plutil -extract id raw -o - "$result")" "${auth[@]}"
# submit --wait exits 0 on a rejection too; only the status says.
status=$(plutil -extract status raw -o - "$result")
if [ "$status" != Accepted ]; then
    echo "notarization ended $status" >&2
    exit 1
fi
xcrun stapler staple "$dmg"
spctl -a -vv -t open --context context:primary-signature "$dmg"
