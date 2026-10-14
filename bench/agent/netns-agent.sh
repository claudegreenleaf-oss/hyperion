#!/usr/bin/env bash
# Shaped network namespace with real internet (and tailnet) access for the agent search
# benchmark. Separate from the replay benchmark's namespace, so both can coexist.
#   netns-agent.sh up <profile>    create hagent, NAT it out through the host
#   netns-agent.sh set <profile>   change shaping in place
#   netns-agent.sh down
# Profiles add to the real path: good (none), 3g (100 ms RTT, 1.6 Mbit/s, 0.5% loss),
# bad (300 ms RTT, 1 Mbit/s, 2% loss), awful (600 ms RTT, 0.5 Mbit/s, 4% loss).
# Loss is applied in each direction.
set -euo pipefail
NS=hagent HV=ha-h NV=ha-n HOST_IP=10.78.0.1 NS_IP=10.78.0.2
HERE=$(cd "$(dirname "$0")/.." && pwd)
PASS_FILE=${HB_SUDO_PASS_FILE:-$HERE/.sudo-pass}
run() {
  if [ "$(id -u)" -eq 0 ]; then "$@"
  elif [ -r "$PASS_FILE" ]; then sudo -S -p '' "$@" <"$PASS_FILE"
  else sudo -n "$@"; fi
}
params() {
  case "$1" in
    good)  DELAY=0ms;   RATE=200mbit; LOSS=0% ;;
    3g)    DELAY=50ms;  RATE=1600kbit; LOSS=0.5% ;;
    bad)   DELAY=150ms; RATE=1mbit;   LOSS=2% ;;
    awful) DELAY=300ms; RATE=500kbit; LOSS=4% ;;
    *) echo "unknown profile $1" >&2; exit 2 ;;
  esac
}
shape() {
  params "$1"
  for spec in "$HV" "netns:$NV"; do
    if [[ $spec == netns:* ]]; then dev=${spec#netns:}; pre=(ip netns exec "$NS"); else dev=$spec; pre=(); fi
    run "${pre[@]}" tc qdisc replace dev "$dev" root netem delay "$DELAY" rate "$RATE" loss "$LOSS" limit 20000
  done
}
down() {
  run iptables -t nat -D POSTROUTING -s 10.78.0.0/24 -j MASQUERADE 2>/dev/null || true
  if run ip netns list 2>/dev/null | grep -qw "$NS"; then run ip netns del "$NS"; fi
  if ip link show "$HV" >/dev/null 2>&1; then run ip link del "$HV"; fi
}
up() {
  down
  run ip netns add "$NS"
  run ip link add "$HV" type veth peer name "$NV"
  run ip link set "$NV" netns "$NS"
  run ip addr add "$HOST_IP/24" dev "$HV"
  run ip link set "$HV" up
  run ip netns exec "$NS" ip addr add "$NS_IP/24" dev "$NV"
  run ip netns exec "$NS" ip link set "$NV" up
  run ip netns exec "$NS" ip link set lo up
  run ip netns exec "$NS" ip route add default via "$HOST_IP"
  run iptables -t nat -A POSTROUTING -s 10.78.0.0/24 -j MASQUERADE
  run mkdir -p /etc/netns/$NS
  # run() feeds sudo its password on stdin, so write the file inside the privileged shell.
  run sh -c "echo nameserver 1.1.1.1 > /etc/netns/$NS/resolv.conf"
  shape "$1"
}
case "${1:-}" in
  up) up "${2:?profile}" ;;
  set) shape "${2:?profile}" ;;
  down) down ;;
  *) echo "usage: $0 up|set <good|3g|bad|awful> | down" >&2; exit 2 ;;
esac
