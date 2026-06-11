#!/usr/bin/env bash
# End-to-end smoke test of the real pipeline: boot, networking, exec exit codes,
# and snapshot/restore. Needs Hypervisor.framework (Apple Silicon) and the guest
# assets in ./assets, so it cannot run in cross-platform CI — that is what the unit
# tests (`make test`) are for. Run after `make` so the binary is built and signed.
#
# Exits non-zero if any check fails. The network check needs outbound internet.
set -uo pipefail

BIN=target/release/amber
IMG="${IMG:-alpine:3}"
pass=0
fail=0
TMP="$(mktemp -d)"
trap '"$BIN" down >/dev/null 2>&1 || true; rm -rf "$TMP"' EXIT

ok()  { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass + 1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail + 1)); }

[ -x "$BIN" ]        || { echo "no signed binary at $BIN — run 'make' first"; exit 1; }
[ -f assets/Image ]  || { echo "no assets/Image — run ./scripts/fetch-assets.sh first"; exit 1; }

echo "== vmm lockdown =="
"$BIN" __lockdown-probe >/dev/null 2>&1 && ok "lockdown denies exec + fs writes" || bad "lockdown denies exec + fs writes"

echo "== boot =="
out="$("$BIN" run "$IMG" -- echo __AMBER_OK__ 2>/dev/null)"
echo "$out" | grep -q __AMBER_OK__ && ok "boot + run echo" || bad "boot + run echo"

echo "== network =="
out="$("$BIN" run "$IMG" -- wget -qO- http://example.com 2>/dev/null)"
echo "$out" | grep -qi "Example Domain" && ok "outbound TCP + DNS by name" || bad "outbound TCP + DNS by name"

echo "== exec (warm fork) =="
"$BIN" up >/dev/null 2>&1
"$BIN" template "$IMG" "$TMP/tpl" >/dev/null 2>&1
val="$("$BIN" exec "$TMP/tpl" -- 'echo $((6*7))' 2>/dev/null)"
echo "$val" | grep -qw 42 && ok "exec runs a command (42)" || bad "exec runs a command (got '$val')"
"$BIN" exec "$TMP/tpl" -- 'exit 7' >/dev/null 2>&1
rc=$?
[ "$rc" -eq 7 ] && ok "exec propagates exit code 7" || bad "exec exit code (got $rc)"
"$BIN" down >/dev/null 2>&1

echo "== snapshot / restore =="
AMBER_SNAPSHOT="$TMP/snap" AMBER_SNAPSHOT_AFTER_MS=1500 \
  "$BIN" run "$IMG" -- sh -c 'i=0; while :; do i=$((i + 1)); echo tick$i; sleep 1; done' >/dev/null 2>&1 || true
if [ -f "$TMP/snap/mem.bin" ]; then
  "$BIN" restore "$TMP/snap" >"$TMP/restore.log" 2>&1 &
  rpid=$!
  sleep 3
  kill "$rpid" 2>/dev/null
  wait "$rpid" 2>/dev/null # absorb the job-terminated notice
  grep -q tick "$TMP/restore.log" && ok "restore resumes the periodic timer" || bad "restore resumes the periodic timer"
else
  bad "snapshot not captured"
fi

echo
echo "== $pass passed, $fail failed =="
[ "$fail" -eq 0 ]
