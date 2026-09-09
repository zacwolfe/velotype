#!/usr/bin/env bash
# Rebuild Velotype from source and refresh the locally installed .app bundle
# in place, so a CLI symlink or Dock icon pointing at it picks up the latest
# build without being reconfigured.
#
# Usage: ./scripts/update_installed_app.sh
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="Velotype.app"

# Prefer /Applications, but some managed Macs block writes there even for
# the owning user (mere existence of a Velotype.app entry doesn't mean it's
# writable — a stale, permission-denied stub can be left behind). Fall back
# to ~/Applications, which works identically for launching and for the
# app's own bundle-detection logic: it only checks the shape
# *.app/Contents/MacOS/<bin>, not which directory it lives under.
if [ -w "/Applications" ]; then
    TARGET_DIR="/Applications"
else
    TARGET_DIR="$HOME/Applications"
fi

echo "==> Building and packaging."
"$PROJECT_ROOT/scripts/create_macos_app_dist.sh"

echo "==> Refreshing $TARGET_DIR/$APP_NAME"
mkdir -p "$TARGET_DIR"
rm -rf "$TARGET_DIR/$APP_NAME"
cp -R "$PROJECT_ROOT/dist/$APP_NAME" "$TARGET_DIR/"

echo "==> Done. Latest build is live at $TARGET_DIR/$APP_NAME"
