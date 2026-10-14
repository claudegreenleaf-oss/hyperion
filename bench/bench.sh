#!/usr/bin/env bash
# Orchestrates a benchmark session on the box.
#   bench.sh record --bin CHROME --archive A.wprgo --sites candidates.json --out sites.json
#   bench.sh run --stock CHROME --hyp CHROME --archive A.wprgo [--profile p25|p100|p0] [run.js options]
#   bench.sh selftest --stock CHROME --hyp CHROME   prerender/prefetch preflight on a synthetic site
#   bench.sh check          verify netns, replay server reachability and RTT, then clean up
# Meant to be run under systemd-run --user (see README.md). It creates the hbench namespace,
# starts the replay server and a virtual display inside it, runs the driver, and tears
# everything down by pidfile and namespace name only.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
MODE=${1:-}
shift || true
WPR=${HB_WPR:-$HERE/wpr}
RUN_DIR=$HERE/.run
mkdir -p "$RUN_DIR"
SERVER_CORES=${HB_SERVER_CORES:-52-53}
DRIVER_CORES=${HB_DRIVER_CORES:-54-55}
CHROME_CORES=${HB_CHROME_CORES:-56-63}
DISPLAY_NUM=${HB_DISPLAY:-97}
HOSTIP=10.77.0.1

ARCHIVE=""
PROFILE=p25
PREFLIGHT=1
PASS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --archive) ARCHIVE=$2; shift 2 ;;
    --no-preflight) PREFLIGHT=0; shift ;;
    --profile) PROFILE=$2; PASS+=("$1" "$2"); shift 2 ;;
    --server-cores) SERVER_CORES=$2; shift 2 ;;
    --driver-cores) DRIVER_CORES=$2; shift 2 ;;
    --chrome-cores) CHROME_CORES=$2; shift 2 ;;
    *) PASS+=("$1"); shift ;;
  esac
done

ME=$(id -un)
PASS_FILE=${HB_SUDO_PASS_FILE:-$HERE/.sudo-pass}
S() {
  if [ "$(id -u)" -eq 0 ]; then "$@"
  elif [ -r "$PASS_FILE" ]; then sudo -S -p '' "$@" <"$PASS_FILE"
  else sudo -n "$@"; fi
}
# Run a command inside the namespace as root, or as the unprivileged user with the display set.
nsx() { S ip netns exec hbench "$@"; }
nsme() { S ip netns exec hbench sudo -u "$ME" -H env "DISPLAY=:$DISPLAY_NUM" "$@"; }

WPR_PID_FILE=$RUN_DIR/wpr.pid
XVFB_PID_FILE=$RUN_DIR/xvfb.pid
SELFTEST_PID_FILE=$RUN_DIR/selftest.pid

stop_pidfile() {
  local f=$1 sig=${2:-TERM}
  if [ -f "$f" ]; then
    local p; p=$(cat "$f")
    if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then kill -"$sig" "$p" 2>/dev/null || true; fi
    for _ in $(seq 1 100); do kill -0 "$p" 2>/dev/null || break; sleep 0.1; done
    rm -f "$f"
  fi
}

cleanup() {
  set +e
  stop_pidfile "$XVFB_PID_FILE"
  stop_pidfile "$SELFTEST_PID_FILE"
  if [ "$MODE" = record ] || [ "$MODE" = check ]; then stop_pidfile "$WPR_PID_FILE" INT; else stop_pidfile "$WPR_PID_FILE"; fi
  "$HERE/netns.sh" down
}
trap cleanup EXIT INT TERM

start_wpr() {
  local mode=$1 archive=$2 quiet=""
  [ "$mode" = replay ] && quiet="--quiet_mode"
  # Replay server in the host namespace, bound to the veth address only.
  (
    cd "$HERE/catapult/web_page_replay_go"
    taskset -c "$SERVER_CORES" "$WPR" "$mode" --host="$HOSTIP" --http_port=8080 --https_port=8443 $quiet "$archive" \
      >"$RUN_DIR/wpr.log" 2>&1 &
    echo $! >"$WPR_PID_FILE"
  )
  for _ in $(seq 1 150); do
    nsx bash -c "exec 3<>/dev/tcp/$HOSTIP/8443" 2>/dev/null && return 0
    sleep 0.2
  done
  echo "replay server not reachable from the namespace" >&2
  tail -5 "$RUN_DIR/wpr.log" >&2
  exit 1
}

start_env() {
  local net=$1 wpr_mode=$2
  "$HERE/netns.sh" up "$net"
  start_wpr "$wpr_mode" "$ARCHIVE"
  # Virtual display inside the namespace, owned by the unprivileged user.
  nsme sh -c "echo \$\$ > '$XVFB_PID_FILE'; exec taskset -c '$SERVER_CORES' Xvfb :$DISPLAY_NUM -screen 0 1600x1000x24 -ac -nolisten tcp" \
    >"$RUN_DIR/xvfb.log" 2>&1 &
  sleep 1.5
}

# Preflight: one pair on a local synthetic site with prerender rules. Both arms must show a
# prerender activation or prefetch hit, otherwise Preload pages is silently off.
preflight() {
  # Only the binaries are taken from the run arguments.
  local stock="" hyp="" carg="" prev=""
  for a in "$@"; do
    [ "$prev" = --stock ] && stock=$a
    [ "$prev" = --hyp ] && hyp=$a
    [ "$prev" = --chrome-arg ] && carg=$a
    prev=$a
  done
  ( node "$HERE/selftest-server.js" 8080 10.77.0.3 >"$RUN_DIR/selftest.log" 2>&1 & echo $! >"$SELFTEST_PID_FILE" )
  sleep 1
  rm -f "$RUN_DIR/preflight.jsonl"
  local rc=0
  nsme taskset -c "$DRIVER_CORES" node "$HERE/run.js" --chrome-cores "$CHROME_CORES" --stock "$stock" --hyp "$hyp" --chrome-arg "$carg" \
    --sites "$HERE/selftest-sites.json" --pairs 1 --aa-pairs 0 --warmup 0 --train 2 --cooldown 1 --block-cooldown 1 \
    --out "$RUN_DIR/preflight.jsonl" >&2 || rc=1
  stop_pidfile "$SELFTEST_PID_FILE"
  [ $rc = 0 ] && { node "$HERE/preflight-check.js" "$RUN_DIR/preflight.jsonl" >&2 || rc=1; }
  return $rc
}

case "$MODE" in
  exec)
    # Debug helper: bring up the namespace, replay server, display and selftest site, then run a command inside.
    ARCHIVE=$RUN_DIR/check.wprgo
    start_env "$PROFILE" record
    ( node "$HERE/selftest-server.js" 8080 10.77.0.3 >"$RUN_DIR/selftest.log" 2>&1 & echo $! >"$SELFTEST_PID_FILE" )
    sleep 1
    nsme "${PASS[@]}"
    ;;
  selftest)
    ARCHIVE=$RUN_DIR/check.wprgo
    start_env "$PROFILE" record
    preflight "${PASS[@]}"
    ;;
  check)
    ARCHIVE=$RUN_DIR/check.wprgo
    start_env "$PROFILE" record
    echo "measured RTT (ms): $("$HERE/netns.sh" rtt)"
    ;;
  record)
    [ -n "$ARCHIVE" ] || { echo "--archive required" >&2; exit 2; }
    mkdir -p "$(dirname "$ARCHIVE")"
    rm -f "$ARCHIVE"
    start_env p0 record
    nsme taskset -c "$DRIVER_CORES" node "$HERE/record.js" --chrome-cores "$CHROME_CORES" --archive "$ARCHIVE" "${PASS[@]}"
    ;;
  run)
    [ -f "$ARCHIVE" ] || { echo "--archive must be an existing .wprgo file" >&2; exit 2; }
    if [ "$PREFLIGHT" = 1 ]; then
      # The synthetic preflight site is reached through the replay server in record mode, so run it
      # first against a throwaway archive, then swap in the real archive for replay.
      ARCH_REAL=$ARCHIVE
      ARCHIVE=$RUN_DIR/check.wprgo
      start_env "$PROFILE" record
      preflight "${PASS[@]}" || { echo "preflight failed: Preload pages does not appear to be working in one arm" >&2; exit 3; }
      stop_pidfile "$WPR_PID_FILE" INT
      ARCHIVE=$ARCH_REAL
      start_wpr replay "$ARCHIVE"
    else
      start_env "$PROFILE" replay
    fi
    RTT=$("$HERE/netns.sh" rtt || true)
    echo "measured RTT (ms) on profile $PROFILE: $RTT" >&2
    cd "$HERE"
    nsme taskset -c "$DRIVER_CORES" node "$HERE/run.js" --chrome-cores "$CHROME_CORES" --label "rtt=${RTT}" "${PASS[@]}"
    ;;
  *) sed -n '2,10p' "$0"; exit 2 ;;
esac
