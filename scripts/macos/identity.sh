#!/usr/bin/env bash
#
# Prints the identity to sign Aria.app with, or - for ad-hoc.
#
# Permission grants follow the signing certificate, so the choice must not
# depend on keychain order or on what a build left behind. ARIA_SIGN_IDENTITY
# picks it, else the keychain's only codesigning identity. Several identities
# and no pick fail instead of guessing.
#
# Usage: identity.sh
set -euo pipefail

if [ -n "${ARIA_SIGN_IDENTITY:-}" ]; then
    echo "$ARIA_SIGN_IDENTITY"
    exit
fi
identities="$(security find-identity -v -p codesigning | grep -E '\) [0-9A-F]{40} "' || true)"
if [ -z "$identities" ]; then
    echo -
elif [ "$(grep -c . <<<"$identities")" -gt 1 ]; then
    echo "Several codesigning identities exist. Set ARIA_SIGN_IDENTITY to one's hash:" >&2
    echo "$identities" >&2
    exit 1
else
    awk '{print $2}' <<<"$identities"
fi
