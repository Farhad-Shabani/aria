#!/usr/bin/env bash
#
# Writes dist/appcast.xml, the signed Sparkle feed for one release's DMG.
#
# Usage: SPARKLE_ED_PRIVATE_KEY=... appcast.sh <sign_update> <dmg> <version> <owner/repo>
set -euo pipefail

usage="usage: appcast.sh <sign_update> <dmg> <version> <owner/repo>"
sign_update="${1:?$usage}"
dmg="${2:?$usage}"
version="${3:?$usage}"
repo="${4:?$usage}"
private_key="${SPARKLE_ED_PRIVATE_KEY:?SPARKLE_ED_PRIVATE_KEY is unset}"
release="https://github.com/$repo/releases"

mount=$(mktemp -d)
hdiutil attach -nobrowse -readonly -mountpoint "$mount" "$dmg" >/dev/null
trap 'hdiutil detach "$mount" >/dev/null' EXIT
app_info="$mount/Aria.app/Contents/Info.plist"
minimum_macos=$(plutil -extract LSMinimumSystemVersion raw "$app_info")
app_public_key=$(plutil -extract SUPublicEDKey raw "$app_info")

public_key=$(printf '%s' "$private_key" | swift -e '
import CryptoKit
import Foundation
guard let line = readLine(), let seed = Data(base64Encoded: line),
    let key = try? Curve25519.Signing.PrivateKey(rawRepresentation: seed)
else { exit(1) }
print(key.publicKey.rawRepresentation.base64EncodedString())
')
if [ "$public_key" != "$app_public_key" ]; then
    echo "error: SPARKLE_ED_PRIVATE_KEY does not match the app's SUPublicEDKey" >&2
    exit 1
fi

enclosure_attributes=$(printf '%s' "$private_key" | "$sign_update" --ed-key-file - "$dmg")

mkdir -p dist
cat > dist/appcast.xml <<EOF
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>Aria</title>
    <item>
      <title>Aria $version</title>
      <pubDate>$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S +0000')</pubDate>
      <sparkle:version>$version</sparkle:version>
      <sparkle:shortVersionString>$version</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>$minimum_macos</sparkle:minimumSystemVersion>
      <sparkle:fullReleaseNotesLink>$release/tag/v$version</sparkle:fullReleaseNotesLink>
      <enclosure url="$release/download/v$version/$(basename "$dmg")" type="application/octet-stream" $enclosure_attributes/>
    </item>
  </channel>
</rss>
EOF
printf '%s' "$private_key" | "$sign_update" --ed-key-file - dist/appcast.xml
