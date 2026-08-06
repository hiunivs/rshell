#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
ARCHIVE_DIR="${1:-$ROOT_DIR/dist/macos}"

: "${SPARKLE_BIN_DIR:?Point to the bin directory from the same vetted Sparkle release}"
: "${RSHELL_MACOS_DOWNLOAD_PREFIX:?Set the HTTPS directory that will host update archives}"

case "$RSHELL_MACOS_DOWNLOAD_PREFIX" in
    https://*) ;;
    *) echo "RSHELL_MACOS_DOWNLOAD_PREFIX must use HTTPS" >&2; exit 1 ;;
esac
if [[ ! -x "$SPARKLE_BIN_DIR/generate_appcast" ]]; then
    echo "Sparkle generate_appcast tool was not found" >&2
    exit 1
fi
if ! find "$ARCHIVE_DIR" -maxdepth 1 -name 'RShell-*.zip' -print -quit | grep -q .; then
    echo "No RShell update ZIP was found in $ARCHIVE_DIR" >&2
    exit 1
fi

"$SPARKLE_BIN_DIR/generate_appcast" \
    --download-url-prefix "$RSHELL_MACOS_DOWNLOAD_PREFIX" \
    "$ARCHIVE_DIR"

APPCAST="$ARCHIVE_DIR/appcast.xml"
if [[ ! -f "$APPCAST" ]]; then
    echo "generate_appcast did not create $APPCAST" >&2
    exit 1
fi

# Sparkle 2.9+ detects SURequireSignedFeed in the archived App and signs the feed,
# release notes, and update archive. Any later modification requires regenerating it.
echo "Created signed $APPCAST"
