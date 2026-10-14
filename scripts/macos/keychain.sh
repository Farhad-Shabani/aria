#!/usr/bin/env bash
#
# Imports a Developer ID certificate into a new keychain and prints the
# signing identity's hash, for ARIA_SIGN_IDENTITY. For CI runners only: it
# replaces the user's keychain search list.
#
# Reads MACOS_CERTIFICATE (a base64 .p12) and MACOS_CERTIFICATE_PASSWORD.
#
# Usage: keychain.sh <keychain path>
set -euo pipefail

keychain="${1:?usage: keychain.sh <keychain path>}"
: "${MACOS_CERTIFICATE:?}" "${MACOS_CERTIFICATE_PASSWORD:?}"

password=$(openssl rand -base64 32)
if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::add-mask::$password"
fi
security create-keychain -p "$password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$password" "$keychain"

certificate=$(mktemp)
trap 'rm -f "$certificate"' EXIT
printf '%s' "$MACOS_CERTIFICATE" | base64 --decode > "$certificate"
# -x keeps the private key from ever being exported again.
security import "$certificate" -k "$keychain" -f pkcs12 -P "$MACOS_CERTIFICATE_PASSWORD" \
    -T /usr/bin/codesign -x >&2
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$password" "$keychain" > /dev/null
security list-keychains -d user -s "$keychain" login.keychain-db

identity=$(security find-identity -v -p codesigning "$keychain" | awk '/Developer ID Application/ {print $2; exit}')
if [ -z "$identity" ]; then
    echo "the certificate holds no Developer ID Application identity" >&2
    exit 1
fi
echo "$identity"
