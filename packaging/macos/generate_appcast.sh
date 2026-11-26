#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 hiunivs
# SPDX-License-Identifier: AGPL-3.0-only
# Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

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

# RSHELL_SPARKLE_PRIVATE_KEY_FILE optionally points at the exported EdDSA private
# key file. Using it avoids interactive Keychain ACL prompts on headless CI
# runners; without it the key is read from the login Keychain as usual.
ED_KEY_ARGS=()
if [[ -n "${RSHELL_SPARKLE_PRIVATE_KEY_FILE:-}" ]]; then
    ED_KEY_ARGS+=(--ed-key-file "$RSHELL_SPARKLE_PRIVATE_KEY_FILE")
fi

# Sparkle resolves archive names relative to the prefix URL. Without a trailing
# slash the last path segment (e.g. "stable") is dropped as if it were a file,
# producing broken download URLs, so always append one.
RSHELL_MACOS_DOWNLOAD_PREFIX="${RSHELL_MACOS_DOWNLOAD_PREFIX%/}/"

"$SPARKLE_BIN_DIR/generate_appcast" \
    --download-url-prefix "$RSHELL_MACOS_DOWNLOAD_PREFIX" \
    ${ED_KEY_ARGS[@]+"${ED_KEY_ARGS[@]}"} \
    "$ARCHIVE_DIR"

APPCAST="$ARCHIVE_DIR/appcast.xml"
if [[ ! -f "$APPCAST" ]]; then
    echo "generate_appcast did not create $APPCAST" >&2
    exit 1
fi

# Sparkle 2.9+ detects SURequireSignedFeed in the archived App and signs the feed,
# release notes, and update archive. Any later modification requires regenerating it.
echo "Created signed $APPCAST"
