#!/bin/bash
# Deploy the relay to scotus-vps. Run from the repo root.
# Keeps a copy of the previous code so a bad deploy rolls back with one command.
set -euo pipefail
HOST=${HOST:-scotus-vps}
cd "$(dirname "$0")/.."

ssh "$HOST" 'set -e; cd /opt/hyperion-relay; ts=$(date +%Y%m%d-%H%M%S); mkdir -p backups; tar czf backups/relay-$ts.tgz relay; ls -t backups | tail -n +6 | xargs -r -I{} rm backups/{}'
scp -q server.py answer.py "$HOST:/opt/hyperion-relay/relay/"
ssh "$HOST" rm -f /opt/hyperion-relay/relay/answer.html
scp -q requirements.txt "$HOST:/opt/hyperion-relay/relay/requirements.txt"
ssh "$HOST" /opt/hyperion-relay/relay-venv/bin/pip install -q -r /opt/hyperion-relay/relay/requirements.txt
scp -q deploy/hyperion-relay.service deploy/hyperion-relay.slice "$HOST:/etc/systemd/system/"
ssh "$HOST" 'set -e
systemctl daemon-reload
systemctl restart hyperion-relay
for i in $(seq 1 20); do curl -fsS -m 2 http://100.104.232.110:8790/healthz >/dev/null 2>&1 && break; sleep 0.5; done
curl -fsS -m 3 http://100.104.232.110:8790/healthz; echo
systemctl is-active hyperion-relay hyperion-searxng'
