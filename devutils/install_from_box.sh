#!/bin/bash
# Install the app built by cross_build_linux.sh on the a6000-box as /Applications/Hyperion.app.
# Usage: install_from_box.sh [ssh command]   (default: ssh a6000-box)
set -euo pipefail
SSH=${1:-"ssh a6000-box"}
APP=/Applications/Hyperion.app
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

$SSH 'cd ~/hyperion-mac/build/src/out/Default && tar -cf - Helium.app' | tar -xf - -C "$TMP"
plist="$TMP/Helium.app/Contents/Info.plist"
plutil -replace CFBundleDisplayName -string Hyperion "$plist"
plutil -replace CFBundleName -string Hyperion "$plist"
codesign --force --deep --sign - "$TMP/Helium.app"

pid=$(pgrep -f "$APP/Contents/MacOS/Helium$" || true)
if [ -n "$pid" ]; then
  kill -TERM $pid
  while kill -0 $pid 2>/dev/null; do sleep 1; done
fi
rm -rf "$APP"
ditto "$TMP/Helium.app" "$APP"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP"
echo "installed $APP"
