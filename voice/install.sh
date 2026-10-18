#!/bin/bash
# Install or update Hyperion Voice from the newest successful CI build, and keep it running
# (a LaunchAgent starts it at login and restarts it if it ever crashes).
set -euo pipefail
REPO=claudegreenleaf-oss/hyperion
LABEL=com.thomasgreenleaf.hyperion-voice
APPDIR="$HOME/Applications"
APP="$APPDIR/Hyperion Voice.app"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

RUN=${1:-$(gh run list -R "$REPO" --workflow "Hyperion Voice" --status success --limit 1 --json databaseId -q '.[0].databaseId')}
gh run download "$RUN" -R "$REPO" -n hyperion-voice -D "$TMP"
ZIP=$(find "$TMP" -name Hyperion-Voice.zip | head -1)

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
mkdir -p "$APPDIR"
rm -rf "$APP"
ditto -x -k "$ZIP" "$APPDIR"
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array><string>$APP/Contents/MacOS/hyperion-voice</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>ThrottleInterval</key><integer>5</integer>
</dict>
</plist>
EOF
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "installed run $RUN; log: ~/Library/Logs/HyperionVoice.log"
