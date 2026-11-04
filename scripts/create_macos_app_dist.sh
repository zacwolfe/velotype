#!/usr/bin/env bash
# Create a macOS .app for Velotype
# Usage: ./scripts/create_app_dist.sh
set -euo pipefail

BINARY_NAME="velotype"
APP_NAME="Velotype"

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$PROJECT_ROOT/dist"
ICON_SOURCE="$PROJECT_ROOT/resources/AppIcon.png"

echo "==> Clean old dists."
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

echo "==> Build Release binary."
cargo build --release

echo "==> Create App Bundle struct."
APP_DIR="$DIST_DIR/$APP_NAME.app"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"
cp "$PROJECT_ROOT/target/release/$BINARY_NAME" "$APP_DIR/Contents/MacOS/"
cp resources/macos/Info.plist "$APP_DIR/Contents/"
cp resources/macos/$BINARY_NAME.icns "$APP_DIR/Contents/Resources/$BINARY_NAME.icns"

echo "==> Copy resources files"
[ -f "$PROJECT_ROOT/README.md" ] && cp "$PROJECT_ROOT/README.md" "$APP_DIR/Contents/Resources/"

# Sign the finished bundle. cargo's linker-signed ad-hoc executable is valid on
# its own, but once it sits inside a bundle that also carries Info.plist and
# Resources macOS refuses to launch it ("code has no resources but signature
# indicates they must be present" -> SIGKILL / CODESIGNING Invalid Page).
# Re-signing the bundle as a whole writes the resource envelope macOS expects.
echo "==> Clear extended attributes"
xattr -cr "$APP_DIR"

echo "==> Sign app bundle (ad-hoc)"
codesign --force --deep --sign - "$APP_DIR"
codesign --verify --verbose=2 "$APP_DIR"

echo "==> ✅ Done"
echo "    Output: $APP_DIR"
echo "       Use: open '$APP_DIR'"
