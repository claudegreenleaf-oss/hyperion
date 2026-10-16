#!/usr/bin/env bash
# Runs the agent search benchmark across network profiles, one after another.
#   [ARMS=relay] run-agent.sh <tag> [profiles...]   (default profiles: good 3g bad awful)
set -uo pipefail
cd "$(dirname "$0")"
TAG=$1; shift
PROFILES=${*:-good 3g bad awful}
PASS=../.sudo-pass
./netns-agent.sh up good
for p in $PROFILES; do
  ./netns-agent.sh set "$p"
  sudo -S -p "" ip netns exec hagent sudo -u "$USER" -H env PATH="$PATH" HOME="$HOME" \
    ../agent-venv/bin/python agentbench.py --profile "$p" --arms "${ARMS:-relay,fetch,browser}" --out "../results/agent-$TAG.jsonl" < "$PASS"
done
./netns-agent.sh down
echo "AGENT_BENCH_DONE"
