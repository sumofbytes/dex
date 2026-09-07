#!/usr/bin/env bash
# Profile dex with Parca: eBPF CPU profiles via parca-agent, plus memory helpers.
#
#   scripts/profile-parca.sh build         # build target/profiling/dex (frame pointers + DWARF)
#   scripts/profile-parca.sh agent         # start parca-agent -> parca UI (default :7070)
#   scripts/profile-parca.sh run [SECS]    # run dex under the agent; drive a real session (SECS is display-only hint, dex runs until you quit)
#   scripts/profile-parca.sh mem [SECS]    # log dex RSS every 2s to parca-profiles/*.csv (logs each sample)
#   scripts/profile-parca.sh heap          # attach heaptrack to a running dex (allocations)
#   scripts/profile-parca.sh mark [MSG]    # timestamp a phase boundary (correlate in the UI)
#   scripts/profile-parca.sh stop          # stop parca-agent
#
# Workflow: build && agent && run — then open http://localhost:7070, pick the
# target/profiling/dex process, and use Compare to diff this time range vs the
# previous run. Keep the binary you profiled around: the agent symbolizes from
# /proc/<pid>/exe + DWARF at ingest time, so rebuilding changes what old
# profiles resolve to.
#
# Env overrides: PARCA_ADDR (default localhost:7070), NODE (default hostname),
# PARCA_BIN / PARCA_AGENT_BIN (default: on PATH, else $HOME/parca{,-agent}).
set -euo pipefail

BIN=target/profiling/dex
PROF_DIR=parca-profiles
PARCA_ADDR=${PARCA_ADDR:-localhost:7070}
NODE=${NODE:-$(hostname)}

die() { echo "profile-parca: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || die "missing '$1'"; }
# Binaries may live in $HOME instead of PATH (e.g. ~/parca, ~/parca-agent).
bin_path() {
  command -v "$1" >/dev/null && { command -v "$1"; return; }
  [ -x "$HOME/$1" ] && { echo "$HOME/$1"; return; }
  die "missing '$1' (not on PATH and no executable $HOME/$1)"
}
ui_up() { curl -sf -o /dev/null "http://$PARCA_ADDR/"; }

cmd_build() {
  RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling
  file "$BIN" | grep -q 'not stripped' \
    || die "$BIN is stripped — check [profile.profiling] (debug=true, strip=false)"
  echo "built: $BIN ($(du -h "$BIN" | cut -f1))"
}

cmd_agent() {
  AGENT_BIN=${PARCA_AGENT_BIN:-$(bin_path parca-agent)}
  need curl
  ui_up || die "parca UI not reachable at http://$PARCA_ADDR — start it first: ${PARCA_BIN:-$HOME/parca} --config-path $HOME/parca.yaml"
  if pgrep -x parca-agent >/dev/null; then echo "parca-agent already running (pid $(pgrep -x parca-agent | tr '\n' ' '))"; return 0; fi
  mkdir -p "$PROF_DIR"
  # Auth in the foreground: a backgrounded sudo can't prompt for a password.
  sudo -v || die "sudo auth failed"
  sudo "$AGENT_BIN" \
    --node="$NODE" \
    --remote-store-address="$PARCA_ADDR" \
    --remote-store-insecure >>"$PROF_DIR/parca-agent.log" 2>&1 &
  sleep 2
  if ! pgrep -x parca-agent >/dev/null; then
    tail -n 20 "$PROF_DIR/parca-agent.log" >&2
    die "parca-agent failed to start (log: $PROF_DIR/parca-agent.log)"
  fi
  echo "parca-agent up (pid $(pgrep -x parca-agent)); UI: http://$PARCA_ADDR"
}

cmd_run() {
  [ -x "$BIN" ] || cmd_build
  need curl
  ui_up || die "parca UI not reachable at http://$PARCA_ADDR"
  mkdir -p "$PROF_DIR"
  local secs=${1:-120}
  {
    echo "start: $(date +%T)"
    echo "drive dex now: stream a long reply, run tools, grow the transcript, then idle"
    echo "end:   ~$(date -d "+${secs} seconds" +%T)"
  } >&2
  exec "$BIN"
}

cmd_mem() {
  need ps
  mkdir -p "$PROF_DIR"
  local secs=${1:-60} out="$PROF_DIR/mem-$(date +%Y%m%d-%H%M%S).csv"
  echo "profiling dex RSS for ${secs}s (every 2s) -> $out" >&2
  echo "ts,pid,rss_kib,etime" > "$out"
  local start=$SECONDS misses=0 row
  local end=$((start + secs))
  while [ "$SECONDS" -lt "$end" ]; do
    # -C dex catches both the TUI client and the daemon it spawns
    row=$(ps -C dex -o pid=,rss=,etime= 2>/dev/null \
      | awk -v ts="$(date +%s)" '{print ts","$1","$2","$3}')
    if [ -n "$row" ]; then
      misses=0
      printf '%s\n' "$row" >> "$out"
      awk -F, -v e="$((SECONDS - start))" -v t="$secs" \
        '{printf "[%ds/%ds] pid=%s rss=%sKiB etime=%s\n", e, t, $2, $3, $4}' <<<"$row" >&2
    else
      misses=$((misses + 1))
      [ $((misses % 5)) -eq 1 ] && \
        echo "[$((SECONDS - start))/${secs}s] no dex process running (ps -C dex empty)" >&2
    fi
    sleep 2
  done
  echo "wrote $out"
  tail -n 4 "$out"
}

cmd_heap() {
  need heaptrack
  local pid
  pid=$(pgrep -f "$BIN" | head -1) || die "no running dex — start $BIN first"
  echo "attaching heaptrack to pid $pid (Ctrl-C to detach)"
  heaptrack -p "$pid"
  echo "afterwards: heaptrack_print <heaptrack.dex.*.zst> | less"
}

cmd_mark() {
  mkdir -p "$PROF_DIR"
  printf '%s %s\n' "$(date +%T)" "$*" | tee -a "$PROF_DIR/markers.log"
}

cmd_stop() {
  sudo pkill -x parca-agent 2>/dev/null || true
  echo "parca-agent stopped"
}

case "${1:-}" in
  build) cmd_build ;;
  agent) cmd_agent ;;
  run)   shift; cmd_run "$@" ;;
    mem)   shift; cmd_mem "$@" ;;
    heap)  cmd_heap ;;
    mark)  shift; cmd_mark "$@" ;;
    stop)  cmd_stop ;;
    *) die "usage: $0 {build|agent|run [SECS]|mem [SECS]|heap|mark [MSG]|stop}" ;;
esac
