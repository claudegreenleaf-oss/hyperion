#!/usr/bin/env bash
# Create or remove the benchmark network namespace.
#   netns.sh up [p25|p100]   create namespace, veth pair, and netem delay
#   netns.sh down            remove the namespace (idempotent)
#   netns.sh rtt             print the measured RTT from inside the namespace
# Only the namespace hbench and the veth pair hbv-h/hbv-n are touched. Loopback and
# every other interface on the machine are left alone.
set -euo pipefail
NS=hbench
HV=hbv-h
NV=hbv-n
HOST_IP=10.77.0.1
NS_IP=10.77.0.2

# Privilege: root runs directly. Otherwise sudo is used, with the password read from the
# file named by HB_SUDO_PASS_FILE (default .sudo-pass next to this script) when sudo
# would prompt. Without such a file, passwordless sudo is required.
HERE=$(cd "$(dirname "$0")" && pwd)
PASS_FILE=${HB_SUDO_PASS_FILE:-$HERE/.sudo-pass}
run() {
  if [ "$(id -u)" -eq 0 ]; then "$@"
  elif [ -r "$PASS_FILE" ]; then sudo -S -p '' "$@" <"$PASS_FILE"
  else sudo -n "$@"; fi
}

profile_params() {
  case "${1:-p25}" in
    p25)  DELAY=12.5ms; RATE=100mbit ;;   # 25 ms RTT, 100 Mbit/s
    p100) DELAY=50ms;   RATE=20mbit ;;    # 100 ms RTT, 20 Mbit/s
    p0)   DELAY=0ms;    RATE=1000mbit ;;  # no shaping beyond the veth itself
    *) echo "unknown profile $1" >&2; exit 2 ;;
  esac
}

down() {
  if run ip netns list 2>/dev/null | grep -qw "$NS"; then run ip netns del "$NS"; fi
  if ip link show "$HV" >/dev/null 2>&1; then run ip link del "$HV"; fi
}

up() {
  profile_params "${1:-p25}"
  down
  run ip netns add "$NS"
  run ip link add "$HV" type veth peer name "$NV"
  run ip link set "$NV" netns "$NS"
  run ip addr add "$HOST_IP/24" dev "$HV"
  run ip addr add "10.77.0.3/24" dev "$HV"   # second host address, used by the preflight site
  run ip link set "$HV" up
  run ip netns exec "$NS" ip addr add "$NS_IP/24" dev "$NV"
  run ip netns exec "$NS" ip link set "$NV" up
  run ip netns exec "$NS" ip link set lo up
  # netem on the egress of each end, so one direction costs DELAY and a round trip costs 2*DELAY.
  run tc qdisc add dev "$HV" root netem delay "$DELAY" rate "$RATE" limit 20000
  run ip netns exec "$NS" tc qdisc add dev "$NV" root netem delay "$DELAY" rate "$RATE" limit 20000
}

rtt() {
  run ip netns exec "$NS" ping -c 5 -i 0.2 -q "$HOST_IP" | awk -F'[/ =]+' '/^rtt|^round-trip/ {print $7}'
}

case "${1:-}" in
  up) up "${2:-p25}" ;;
  down) down ;;
  rtt) rtt ;;
  *) echo "usage: $0 up [p25|p100|p0] | down | rtt" >&2; exit 2 ;;
esac
