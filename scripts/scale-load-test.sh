#!/usr/bin/env bash
# Scale baseline: spawn N concurrent microVMs, measure the REAL resident cost
# (RSS, host threads, file descriptors) and the spawn-latency distribution, then
# tear them down and check nothing leaked. Run before tuning the daemon for scale —
# the numbers say which bottleneck (registry lock, thread-per-VM, fds, RAM) actually
# binds, instead of guessing. Portable across macOS (HVF) and Linux (KVM).
#
#   N=50 IMG=alpine:3 scripts/scale-load-test.sh
#
# Env: N (default 20), IMG (default alpine:3), HOLD (in-guest keep-alive command),
#      MEM (per-VM RAM, e.g. 256MiB — smaller fits more VMs).
set -euo pipefail

N="${N:-20}"
IMG="${IMG:-alpine:3}"
HOLD="${HOLD:-sleep 600}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/amber"; [ -x "$BIN" ] || BIN="$ROOT/amber"
[ -x "$BIN" ] || { echo "no amber binary (build first)"; exit 127; }
OS="$(uname -s)"

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

# Per-pid "rss_kb threads fds", portable.
metrics_pid() {
  local pid="$1" rss thr fds
  if [ "$OS" = "Darwin" ]; then
    rss=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')
    thr=$(ps -M -p "$pid" 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')
    fds=$(lsof -p "$pid" 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')
  else
    rss=$(awk -v ps="$(getconf PAGESIZE)" '{print int($2*ps/1024)}' "/proc/$pid/statm" 2>/dev/null || echo 0)
    thr=$(awk '/^Threads:/{print $2}' "/proc/$pid/status" 2>/dev/null || echo 0)
    fds=$(ls "/proc/$pid/fd" 2>/dev/null | wc -l | tr -d ' ')
  fi
  echo "${rss:-0} ${thr:-0} ${fds:-0}"
}

# Worker VM processes: the daemon's `__vm`/`restore` children. Filter to the actual
# amber binary (comm == amber) so this script's own shell — whose command line
# contains "amber __vm …" — is not miscounted.
worker_pids() {
  for p in $(pgrep -f "amber (__vm|restore)" 2>/dev/null || true); do
    [ "$(ps -o comm= -p "$p" 2>/dev/null | xargs -r basename 2>/dev/null)" = "amber" ] && echo "$p"
  done
}
daemon_pid() { pgrep -f "amber serve" 2>/dev/null | head -1 || true; }

echo "==> warming daemon + image ($IMG)"
"$BIN" up >/dev/null 2>&1 </dev/null || true
"$BIN" pull "$IMG" >/dev/null 2>&1 </dev/null || true  # pre-pull so spawn latency excludes the fetch

echo "==> spawning $N detached VMs (-- $HOLD)"
ids=(); lat=()
for i in $(seq 1 "$N"); do
  t0=$(now_ms)
  if id=$(AMBER_MEM="${MEM:-}" "$BIN" run -d "$IMG" -- sh -c "$HOLD" 2>/dev/null); then
    t1=$(now_ms)
    ids+=("$id"); lat+=($((t1 - t0)))
  else
    echo "   spawn #$i refused (RAM budget?) — stopping at ${#ids[@]} VMs"
    break
  fi
done
up=${#ids[@]}
sleep 1  # let the last forks settle

echo "==> measuring $up live VMs"
dpid="$(daemon_pid)"
sum_rss=0; sum_thr=0; sum_fds=0; nproc=0
for pid in $(worker_pids); do
  read -r rss thr fds < <(metrics_pid "$pid")
  sum_rss=$((sum_rss + rss)); sum_thr=$((sum_thr + thr)); sum_fds=$((sum_fds + fds))
  nproc=$((nproc + 1))
done
dline="n/a"
[ -n "$dpid" ] && dline="$(metrics_pid "$dpid")"

# Latency percentiles.
pp() { printf '%s\n' "${lat[@]}" | sort -n | awk -v p="$1" '{a[NR]=$1} END{if(NR==0){print 0;exit} print a[int((NR-1)*p)+1]}'; }

echo
echo "================ scale baseline ($OS) ================"
echo "VMs requested:        $N"
echo "VMs up:               $up   (worker procs seen: $nproc)"
echo "spawn latency ms:     p50=$(pp .50)  p99=$(pp .99)  max=$(printf '%s\n' "${lat[@]}" | sort -n | tail -1)"
echo "worker RSS total:     $((sum_rss/1024)) MiB   (avg $(( up>0 ? sum_rss/up/1024 : 0 )) MiB/VM)"
echo "worker host threads:  $sum_thr           (avg $(( up>0 ? sum_thr/up : 0 ))/VM)"
echo "worker fds:           $sum_fds           (avg $(( up>0 ? sum_fds/up : 0 ))/VM)"
echo "daemon (serve) rss/thr/fds: $dline"
echo "======================================================"

echo "==> teardown"
for id in "${ids[@]}"; do "$BIN" rm "$id" >/dev/null 2>&1 || true; done
sleep 1
leaked=$(worker_pids | wc -l | tr -d ' ')
echo "worker procs after teardown: $leaked $([ "$leaked" -eq 0 ] && echo '(clean)' || echo '(LEAK)')"
