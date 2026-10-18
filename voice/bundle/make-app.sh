#!/bin/bash
# Assemble and sign "Hyperion Voice.app" from the release binary (run on the macOS CI runner).
# Signing uses a fixed self-signed identity so macOS keeps the microphone and speech
# permissions across rebuilds (an ad-hoc signature changes every build).
set -euo pipefail
cd "$(dirname "$0")/.."
APP="dist/Hyperion Voice.app"
rm -rf dist && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/models"
cp target/release/hyperion-voice "$APP/Contents/MacOS/"
sed -e "s/VERSION/$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)/" -e "s/BUILD/${GITHUB_RUN_NUMBER:-0}/" \
  bundle/Info.plist > "$APP/Contents/Info.plist"
cp models/*.onnx "$APP/Contents/Resources/models/"

if [ -n "${SIGN_P12:-}" ]; then
  KC="$RUNNER_TEMP/sign.keychain-db"
  echo "$SIGN_P12" | base64 --decode > "$RUNNER_TEMP/sign.p12"
  security create-keychain -p ci "$KC"
  security set-keychain-settings "$KC"
  security unlock-keychain -p ci "$KC"
  security import "$RUNNER_TEMP/sign.p12" -k "$KC" -P "$SIGN_P12_PASSWORD" -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k ci "$KC" >/dev/null
  security list-keychains -d user -s "$KC" $(security list-keychains -d user | tr -d '"')
  # The certificate is public (bundle/sign-cert.pem); trusting it lets codesign pick the identity.
  sudo security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain bundle/sign-cert.pem
  codesign --force --keychain "$KC" --sign "Hyperion Voice Self-Signed" --identifier com.thomasgreenleaf.hyperion-voice "$APP"
  rm -f "$RUNNER_TEMP/sign.p12"
else
  codesign --force --sign - --identifier com.thomasgreenleaf.hyperion-voice "$APP"
fi
codesign -dvv "$APP" 2>&1 | grep -E "Identifier|Authority|Signature" || true
(cd dist && ditto -c -k --keepParent "Hyperion Voice.app" Hyperion-Voice.zip)
ls -la dist
