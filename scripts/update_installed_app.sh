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

# Drop the build intermediate. It is a fully valid bundle carrying the same
# CFBundleIdentifier as the installed copy, so leaving it behind registers a
# second launchable Velotype with LaunchServices — and Finder's "Open With"
# can then pick either one. They look identical right after this script runs
# and silently diverge as soon as dist/ is rebuilt without installing, which
# makes "I'm testing the latest build" impossible to trust.
# Safe to delete: create_macos_app_dist.sh clears dist/ at the start of every run.
rm -rf "$PROJECT_ROOT/dist/$APP_NAME"
echo "==> Removed build intermediate dist/$APP_NAME (kept LaunchServices unambiguous)"

# Keep a CLI entry point pointing at whichever bundle we just refreshed.
# ~/.local/bin needs no sudo, unlike the /usr/local/bin symlink the app's own
# "Install CLI Tool" menu item creates — and that one silently rots if it was
# aimed at a bundle location this script no longer writes to, leaving `velotype`
# resolving to a dangling path while the real build sits elsewhere.
INSTALLED_BINARY="$TARGET_DIR/$APP_NAME/Contents/MacOS/velotype"
CLI_DIR="$HOME/.local/bin"
CLI_LINK="$CLI_DIR/velotype"
mkdir -p "$CLI_DIR"
ln -sf "$INSTALLED_BINARY" "$CLI_LINK"
echo "==> CLI: $CLI_LINK -> $INSTALLED_BINARY"

# Warn if an earlier /usr/local/bin symlink still exists and would shadow ours,
# or is dangling; either way it confuses which build actually launches.
LEGACY_LINK="/usr/local/bin/velotype"
if [ -L "$LEGACY_LINK" ] && [ "$(readlink "$LEGACY_LINK")" != "$INSTALLED_BINARY" ]; then
    if [ ! -e "$LEGACY_LINK" ]; then
        echo "==> NOTE: $LEGACY_LINK is dangling (-> $(readlink "$LEGACY_LINK"))."
    else
        echo "==> NOTE: $LEGACY_LINK points elsewhere (-> $(readlink "$LEGACY_LINK"))."
    fi
    echo "    'velotype' resolves via whichever directory comes first on PATH."
    command -v velotype >/dev/null 2>&1 && echo "    currently winning: $(command -v velotype)"
fi

echo "==> Done. Latest build is live at $TARGET_DIR/$APP_NAME"
