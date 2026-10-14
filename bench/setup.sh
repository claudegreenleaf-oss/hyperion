#!/usr/bin/env bash
# One-time setup on the box. Everything lives under ~/hyperion-bench, no root needed.
#   Go toolchain, web_page_replay_go built from the catapult repo, Chrome for Testing,
#   and the npm dependencies.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE"
GO_VERSION=${GO_VERSION:-$(curl -s 'https://go.dev/dl/?mode=json' | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["version"])')}
if [ ! -x go/bin/go ]; then
  curl -sSL "https://go.dev/dl/${GO_VERSION}.linux-amd64.tar.gz" -o go.tgz
  mkdir godl && tar -C godl -xzf go.tgz && mv godl/go go && rmdir godl && rm go.tgz
fi
export PATH="$HERE/go/bin:$PATH"
if [ ! -d catapult ]; then
  git clone --depth 1 --filter=blob:none --sparse https://github.com/catapult-project/catapult.git catapult
  git -C catapult sparse-checkout set web_page_replay_go
fi
# legacyformatconvertor.go does not compile at catapult HEAD and is unused by wpr.go.
rm -f catapult/web_page_replay_go/src/webpagereplay/legacyformatconvertor.go
(cd catapult/web_page_replay_go && go build -o "$HERE/wpr" src/wpr.go)
npm install --no-audit --no-fund
mkdir -p cft
if ! ls cft/chrome/*/chrome-linux64/chrome >/dev/null 2>&1; then npx --yes @puppeteer/browsers install chrome@stable --path "$HERE/cft"; fi
echo "chrome for testing: $(ls cft/chrome/*/chrome-linux64/chrome)"
echo "wpr: $(ls -la wpr)"
