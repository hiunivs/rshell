#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
DIST_DIR="$ROOT_DIR/dist/macos"
APP_PATH="$DIST_DIR/RShell.app"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -1)"

: "${RSHELL_BUILD_NUMBER:?Set a monotonically increasing numeric build number}"
: "${RSHELL_MACOS_APPCAST_URL:?Set the stable HTTPS appcast URL}"
: "${RSHELL_SPARKLE_PUBLIC_KEY:?Set the Sparkle EdDSA public key}"
: "${RSHELL_MACOS_SIGN_IDENTITY:?Set the Developer ID Application identity}"
: "${RSHELL_APPLE_NOTARY_PROFILE:?Set the notarytool keychain profile}"
: "${SPARKLE_FRAMEWORK_PATH:?Point to the vetted Sparkle.framework directory}"

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
mkdir -p "$WORK_DIR/RShell.app/Contents/MacOS" "$WORK_DIR/RShell.app/Contents/Frameworks"

lipo -create \
    "$ROOT_DIR/target/aarch64-apple-darwin/release/rshell" \
    "$ROOT_DIR/target/x86_64-apple-darwin/release/rshell" \
    -output "$WORK_DIR/RShell.app/Contents/MacOS/rshell"
chmod 755 "$WORK_DIR/RShell.app/Contents/MacOS/rshell"
ditto "$SPARKLE_FRAMEWORK_PATH" "$WORK_DIR/RShell.app/Contents/Frameworks/Sparkle.framework"
cp "$ROOT_DIR/packaging/macos/Info.plist" "$WORK_DIR/RShell.app/Contents/Info.plist"

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
# re-sign nested code from the inside out with RShell's Developer ID. Do not use --deep for
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

mkdir -p "$DIST_DIR"
rm -rf "$APP_PATH"
ditto "$WORK_DIR/RShell.app" "$APP_PATH"
ditto -c -k --sequesterRsrc --keepParent \
    "$WORK_DIR/RShell.app" "$DIST_DIR/RShell-$VERSION.zip"

echo "Created $DIST_DIR/RShell-$VERSION.zip"
echo "Generate the signed appcast.xml with Sparkle's generate_appcast tool before publishing."
