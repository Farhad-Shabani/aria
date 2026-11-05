#!/usr/bin/env bash
#
# Prints the identity to sign Aria.app with, or - for ad-hoc.
#
# Permission grants follow the signing certificate, so a signed daemon helper
# keeps its signer. Xcode rebuilds the outer app unsigned, so the signer comes
# from the helper. Ad-hoc grants break on every build, so an unsigned helper
# gets the first codesigning identity.
#
# Usage: identity.sh <Aria.app>
set -euo pipefail

app="${1:?usage: identity.sh <Aria.app>}"
if [ -n "${ARIA_SIGN_IDENTITY:-}" ]; then
    echo "$ARIA_SIGN_IDENTITY"
    exit
fi
signer="$(codesign -dvv "$app/Contents/Helpers/Aria.app" 2>&1 | sed -n '/^Authority=/{s/^Authority=//p;q;}' || true)"
identity="$(security find-identity -v -p codesigning | awk -v signer="$signer" \
    '/\) [0-9A-F]{40} "/ && (signer == "" || index($0, "\"" signer "\"")) {print $2; exit}')"
if [ -n "$signer" ] && [ -z "$identity" ]; then
    echo "$app/Contents/Helpers/Aria.app is signed by \"$signer\", which isn't in the keychain." >&2
    echo "Another identity would void its permission grants. Set ARIA_SIGN_IDENTITY to re-sign anyway." >&2
    exit 1
fi
echo "${identity:--}"
