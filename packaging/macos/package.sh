#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 hiunivs
# SPDX-License-Identifier: AGPL-3.0-only
# Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
DIST_DIR="$ROOT_DIR/dist/macos"
SOURCE_DIST_DIR="$ROOT_DIR/dist/source"
APP_PATH="$DIST_DIR/RShell.app"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -1)"
EXPECTED_TAG="v$VERSION"

: "${RSHELL_BUILD_NUMBER:?Set a monotonically increasing numeric build number}"
: "${RSHELL_MACOS_APPCAST_URL:?Set the stable HTTPS appcast URL}"
: "${RSHELL_SPARKLE_PUBLIC_KEY:?Set the Sparkle EdDSA public key}"
: "${RSHELL_MACOS_SIGN_IDENTITY:?Set the signing identity (Developer ID or a persistent self-signed certificate)}"
: "${SPARKLE_FRAMEWORK_PATH:?Point to the vetted Sparkle.framework directory}"
: "${SPARKLE_LICENSE_PATH:?Point to the LICENSE file from the same vetted Sparkle distribution}"

# Notarization is optional. Without an Apple Developer account, leave
# RSHELL_APPLE_NOTARY_PROFILE unset: the app is still signed (a self-signed
# certificate keeps Sparkle's code-signing consistency check working), but
# Gatekeeper will ask first-time users to open it via right-click -> Open.

case "$RSHELL_MACOS_APPCAST_URL" in
    https://*/appcast.xml) ;;
    *) echo "RSHELL_MACOS_APPCAST_URL must be an HTTPS URL ending in /appcast.xml" >&2; exit 1 ;;
esac
if [[ -n "${RSHELL_MACOS_BETA_APPCAST_URL:-}" && "$RSHELL_MACOS_BETA_APPCAST_URL" != https://*/appcast.xml ]]; then
    echo "RSHELL_MACOS_BETA_APPCAST_URL must be an HTTPS URL ending in /appcast.xml" >&2
    exit 1
fi

if [[ ! "$RSHELL_BUILD_NUMBER" =~ ^[1-9][0-9]*$ ]]; then
    echo "RSHELL_BUILD_NUMBER must be a positive integer" >&2
    exit 1
fi
if [[ ! -d "$SPARKLE_FRAMEWORK_PATH" ]]; then
    echo "Sparkle.framework not found: $SPARKLE_FRAMEWORK_PATH" >&2
    exit 1
fi
if [[ ! -f "$SPARKLE_LICENSE_PATH" ]]; then
    echo "Sparkle license not found: $SPARKLE_LICENSE_PATH" >&2
    exit 1
fi
for LEGAL_FILE in \
    LICENSE NOTICE COMMERCIAL-LICENSE.md THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.html; do
    if [[ ! -f "$ROOT_DIR/$LEGAL_FILE" ]]; then
        echo "Required legal file not found: $ROOT_DIR/$LEGAL_FILE" >&2
        exit 1
    fi
done

if [[ -n "$(git -C "$ROOT_DIR" status --porcelain --untracked-files=normal)" ]]; then
    echo "Release builds require a clean Git worktree" >&2
    exit 1
fi
TAG_COMMIT="$(git -C "$ROOT_DIR" rev-parse -q --verify "refs/tags/$EXPECTED_TAG^{commit}" 2>/dev/null || true)"
HEAD_COMMIT="$(git -C "$ROOT_DIR" rev-parse HEAD)"
if [[ -z "$TAG_COMMIT" || "$TAG_COMMIT" != "$HEAD_COMMIT" ]]; then
    echo "Release commit must be tagged exactly $EXPECTED_TAG" >&2
    exit 1
fi

if command -v rustup >/dev/null 2>&1; then
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
fi
(
    cd "$ROOT_DIR"
    cargo build --locked --release --target aarch64-apple-darwin
    cargo build --locked --release --target x86_64-apple-darwin
)

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rshell-macos.XXXXXX")"
trap 'rm -rf "$WORK_DIR"' EXIT
LICENSES_DIR="$WORK_DIR/RShell.app/Contents/Resources/Licenses"
mkdir -p \
    "$WORK_DIR/RShell.app/Contents/MacOS" \
    "$WORK_DIR/RShell.app/Contents/Frameworks" \
    "$LICENSES_DIR"

lipo -create \
    "$ROOT_DIR/target/aarch64-apple-darwin/release/rshell" \
    "$ROOT_DIR/target/x86_64-apple-darwin/release/rshell" \
    -output "$WORK_DIR/RShell.app/Contents/MacOS/rshell"
chmod 755 "$WORK_DIR/RShell.app/Contents/MacOS/rshell"
ditto "$SPARKLE_FRAMEWORK_PATH" "$WORK_DIR/RShell.app/Contents/Frameworks/Sparkle.framework"
cp "$ROOT_DIR/packaging/macos/Info.plist" "$WORK_DIR/RShell.app/Contents/Info.plist"
cp "$ROOT_DIR/LICENSE" "$LICENSES_DIR/RShell-AGPL-3.0-only.txt"
cp "$ROOT_DIR/NOTICE" "$LICENSES_DIR/RShell-NOTICE.txt"
cp "$ROOT_DIR/COMMERCIAL-LICENSE.md" "$LICENSES_DIR/RShell-COMMERCIAL-LICENSING.md"
cp "$ROOT_DIR/THIRD_PARTY_NOTICES.md" "$LICENSES_DIR/THIRD_PARTY_NOTICES.md"
cp "$ROOT_DIR/THIRD_PARTY_LICENSES.html" "$LICENSES_DIR/THIRD_PARTY_LICENSES.html"
cp "$ROOT_DIR/assets/fonts/OFL.txt" "$LICENSES_DIR/Noto-Sans-SC-OFL-1.1.txt"
cp "$ROOT_DIR/assets/icons/lucide/LICENSE" "$LICENSES_DIR/Lucide-ISC.txt"
cp "$SPARKLE_LICENSE_PATH" "$LICENSES_DIR/Sparkle.txt"

PLIST="$WORK_DIR/RShell.app/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$VERSION" "$PLIST"
plutil -replace CFBundleVersion -string "$RSHELL_BUILD_NUMBER" "$PLIST"
plutil -replace SUFeedURL -string "$RSHELL_MACOS_APPCAST_URL" "$PLIST"
plutil -replace SUPublicEDKey -string "$RSHELL_SPARKLE_PUBLIC_KEY" "$PLIST"

SPARKLE_DEST="$WORK_DIR/RShell.app/Contents/Frameworks/Sparkle.framework"
SPARKLE_VERSION_DIR="$SPARKLE_DEST/Versions/B"
if [[ ! -d "$SPARKLE_VERSION_DIR" ]]; then
    echo "Expected a Sparkle 2 framework with Versions/B" >&2
    exit 1
fi

# Sparkle distributions contain ad-hoc-signed helpers. A non-Xcode archive workflow must
# re-sign nested code from the inside out with RShell's signing identity. Do not use --deep for
# signing because the Downloader service has its own entitlements.
codesign --force --timestamp --options runtime \
    --sign "$RSHELL_MACOS_SIGN_IDENTITY" \
    "$SPARKLE_VERSION_DIR/XPCServices/Installer.xpc"
if [[ -d "$SPARKLE_VERSION_DIR/XPCServices/Downloader.xpc" ]]; then
    codesign --force --timestamp --options runtime --preserve-metadata=entitlements \
        --sign "$RSHELL_MACOS_SIGN_IDENTITY" \
        "$SPARKLE_VERSION_DIR/XPCServices/Downloader.xpc"
fi
codesign --force --timestamp --options runtime \
    --sign "$RSHELL_MACOS_SIGN_IDENTITY" \
    "$SPARKLE_VERSION_DIR/Autoupdate"
codesign --force --timestamp --options runtime \
    --sign "$RSHELL_MACOS_SIGN_IDENTITY" \
    "$SPARKLE_VERSION_DIR/Updater.app"
codesign --force --timestamp --options runtime \
    --sign "$RSHELL_MACOS_SIGN_IDENTITY" \
    "$SPARKLE_DEST"
codesign --verify --deep --strict --verbose=2 "$SPARKLE_DEST"

codesign --force --timestamp --options runtime \
    --sign "$RSHELL_MACOS_SIGN_IDENTITY" \
    "$WORK_DIR/RShell.app/Contents/MacOS/rshell"
codesign --force --timestamp --options runtime \
    --entitlements "$ROOT_DIR/packaging/macos/entitlements.plist" \
    --sign "$RSHELL_MACOS_SIGN_IDENTITY" \
    "$WORK_DIR/RShell.app"
codesign --verify --deep --strict --verbose=2 "$WORK_DIR/RShell.app"

if [[ -n "${RSHELL_APPLE_NOTARY_PROFILE:-}" ]]; then
    ditto -c -k --keepParent "$WORK_DIR/RShell.app" "$WORK_DIR/RShell-notarize.zip"
    NOTARY_KEYCHAIN_ARGS=()
    if [[ -n "${RSHELL_APPLE_NOTARY_KEYCHAIN:-}" ]]; then
        NOTARY_KEYCHAIN_ARGS+=(--keychain "$RSHELL_APPLE_NOTARY_KEYCHAIN")
    fi
    xcrun notarytool submit "$WORK_DIR/RShell-notarize.zip" --wait \
        --keychain-profile "$RSHELL_APPLE_NOTARY_PROFILE" "${NOTARY_KEYCHAIN_ARGS[@]}"
    xcrun stapler staple "$WORK_DIR/RShell.app"
    xcrun stapler validate "$WORK_DIR/RShell.app"
    codesign --verify --deep --strict --verbose=2 "$WORK_DIR/RShell.app"
    spctl --assess --type execute --verbose=2 "$WORK_DIR/RShell.app"
else
    echo "RSHELL_APPLE_NOTARY_PROFILE not set; skipping notarization (self-signed distribution)."
fi

mkdir -p "$DIST_DIR" "$SOURCE_DIST_DIR"
rm -rf "$APP_PATH"
ditto "$WORK_DIR/RShell.app" "$APP_PATH"
ditto -c -k --sequesterRsrc --keepParent \
    "$WORK_DIR/RShell.app" "$DIST_DIR/RShell-$VERSION.zip"

# Ship the exact tagged source together with all locked Cargo dependency
# sources needed to rebuild it, so recipients do not depend on third-party
# registries remaining available.
SOURCE_ROOT="$WORK_DIR/RShell-$VERSION"
mkdir -p "$SOURCE_ROOT/.cargo"
git -C "$ROOT_DIR" archive "$EXPECTED_TAG" | tar -x -C "$SOURCE_ROOT"
(
    cd "$SOURCE_ROOT"
    cargo vendor --locked --versioned-dirs vendor > .cargo/config.toml
)
tar -czf "$SOURCE_DIST_DIR/RShell-$VERSION-source.tar.gz" \
    -C "$WORK_DIR" "RShell-$VERSION"

echo "Created $DIST_DIR/RShell-$VERSION.zip"
echo "Created $SOURCE_DIST_DIR/RShell-$VERSION-source.tar.gz"
echo "Generate the signed appcast.xml with Sparkle's generate_appcast tool before publishing."
