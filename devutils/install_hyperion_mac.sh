#!/bin/bash
# Install the Hyperion browser from a finished arm64 CI build as /Applications/Hyperion.app
# (stock Helium.app is left in place as the fallback).
set -euo pipefail
REPO=claudegreenleaf-oss/hyperion
APP=/Applications/Hyperion.app
TMP=$(mktemp -d)
trap 'hdiutil detach -quiet "$TMP/mnt" 2>/dev/null || true; rm -rf "$TMP"' EXIT

RUN=${1:-$(gh run list -R "$REPO" --workflow "Build macOS binaries of Helium" --status success --limit 10 \
  --json databaseId -q '.[].databaseId' | while read -r r; do
    gh api "repos/$REPO/actions/runs/$r/artifacts" -q '.artifacts[].name' | grep -q 'arm64-macos\.dmg$' && { echo "$r"; break; }
  done)}
[ -n "$RUN" ] || { echo "no finished build with a DMG yet" >&2; exit 1; }

NAME=$(gh api "repos/$REPO/actions/runs/$RUN/artifacts" -q '.artifacts[].name' | grep 'arm64-macos\.dmg$' | head -1)
gh run download "$RUN" -R "$REPO" -n "$NAME" -D "$TMP"
DMG=$(find "$TMP" -name '*arm64-macos.dmg' | head -1)

hdiutil attach -quiet -nobrowse -mountpoint "$TMP/mnt" "$DMG"
SRC=$(find "$TMP/mnt" -maxdepth 1 -name '*.app' | head -1)
osascript -e 'quit app "Hyperion"' 2>/dev/null || true
rm -rf "$APP"
ditto "$SRC" "$APP"
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
echo "installed $NAME from run $RUN to $APP"
