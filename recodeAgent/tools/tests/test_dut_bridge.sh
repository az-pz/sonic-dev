#!/bin/bash
# Exercise _ensure_dut_bridged() WITHOUT a testbed, by evaluating the real function
# body with ip/sudo stubbed. Covers the states a post-reboot host can actually be
# in: bridge missing, tap missing, tap already correct, tap unattached, and tap
# attached to the WRONG bridge.
#
# This exists because the failure it fixes ("No route to host" to the DUT) is only
# reproducible by breaking a live testbed, which is exactly what you do not want to
# do to check an error path.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="$HERE/../../../setup-sonic-testbed.sh"
[ -f "$SRC" ] || { echo "cannot find $SRC" >&2; exit 2; }

DUT=vlab-01

# Simulated host state, set per case.
HAS_BR=1; HAS_TAP=1; TAP_MASTER=""
LAST_SET=""          # what `ip link set ... master ...` was asked to do

# Pull the function out of the real script so this test cannot drift from it.
eval "$(awk '/^_ensure_dut_bridged\(\) \{/,/^\}/' "$SRC")"

ok()   { echo "    [ok] $*"; }
warn() { echo "    [warn] $*"; }

ip() {
  case "$*" in
    "link show br1")          [ "$HAS_BR"  = 1 ] && return 0 || return 1 ;;
    "link show $DUT-0")       [ "$HAS_TAP" = 1 ] && return 0 || return 1 ;;
    "-o link show $DUT-0")
      [ "$HAS_TAP" = 1 ] || return 1
      [ -n "$TAP_MASTER" ] && echo "3: $DUT-0: <BROADCAST> master $TAP_MASTER state UP"
      return 0 ;;
    "link set $DUT-0 master br1") LAST_SET="master br1"; TAP_MASTER=br1; return 0 ;;
    "link set $DUT-0 up")         return 0 ;;
    *) return 0 ;;
  esac
}
sudo() { "$@"; }        # run the stubbed ip directly

fails=0
run() {   # name has_br has_tap master  want_rc want_set
  local name="$1"; HAS_BR="$2"; HAS_TAP="$3"; TAP_MASTER="$4"
  local want_rc="$5" want_set="$6"
  LAST_SET=""
  echo "  $name"
  _ensure_dut_bridged; local rc=$?
  if [ "$rc" != "$want_rc" ] || [ "$LAST_SET" != "$want_set" ]; then
    echo "    FAIL: rc=$rc (want $want_rc), set='$LAST_SET' (want '$want_set')"
    fails=$((fails + 1))
  fi
}

echo "=== _ensure_dut_bridged ==="
#    name                          br tap master   rc  expected action
run "br1 missing"                    0 1 ""        1  ""
run "tap missing (VM down)"          1 0 ""        1  ""
run "already enslaved (no-op)"       1 1 br1       0  ""
run "unattached -> attach"           1 1 ""        0  "master br1"
run "attached to WRONG bridge"       1 1 docker0   0  "master br1"

echo
echo "=== idempotence: run twice from the broken state ==="
HAS_BR=1; HAS_TAP=1; TAP_MASTER=""; LAST_SET=""
_ensure_dut_bridged >/dev/null; first="$LAST_SET"
LAST_SET=""
_ensure_dut_bridged >/dev/null; second="$LAST_SET"
echo "  first run acts: '$first'   second run acts: '$second'"
[ "$first" = "master br1" ] && [ -z "$second" ] || { echo "  FAIL: not idempotent"; fails=$((fails+1)); }

echo
if [ "$fails" -eq 0 ]; then echo "ALL PASS"; else echo "$fails FAILED"; exit 1; fi
