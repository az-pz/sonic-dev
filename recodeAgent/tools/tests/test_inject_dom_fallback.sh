#!/bin/bash
# Exercise dut_validate.sh's inject() fallback WITHOUT a DUT, by evaluating the real
# function bodies with docker/supervisor stubbed. Simulates the cases that matter:
# the crate supports --dom_update_interval, it does not, and none was requested.
#
# This exists because the fallback only fires on a crate that lacks the option, which
# is exactly the situation that is awkward to reproduce on a live DUT on demand.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="$HERE/../dut/dut_validate.sh"
[ -f "$SRC" ] || { echo "cannot find $SRC" >&2; exit 2; }

PMON=pmon XBIN=/usr/local/bin/xcvrd
XRUST=/usr/local/bin/xcvrd-rs
XORIG=/usr/local/bin/xcvrd.pyorig
STAGE=/tmp/recode
SHIM_CAPTURE=""       # last shim body written
SUPPORTS_FLAG=1       # does the simulated daemon accept --dom_update_interval?

# Pull the two functions under test out of the real script, so this test cannot
# drift away from the code it is checking.
eval "$(awk '/^_write_shim\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^inject\(\) \{/,/^\}/'      "$SRC")"

lap() { :; }
_now_ms() { echo 0; }
docker() {   # stub: capture the heredoc the shim writer pipes in
  case "$*" in
    *"cat > $XBIN.new"*) SHIM_CAPTURE="$(cat)" ;;
    *) : ;;
  esac
  return 0
}
wait_running() {
  # The simulated daemon refuses to start when handed an option it lacks.
  if [ "$SUPPORTS_FLAG" -eq 0 ] && printf '%s' "$SHIM_CAPTURE" | grep -q -- '--dom_update_interval'; then
    return 1
  fi
  return 0
}

fails=0
run_case() {
  local name="$1" supports="$2" ival="$3" want_argv="$4" want_applied="$5"
  SUPPORTS_FLAG="$supports"; DOM_IVAL="$ival"; DOM_APPLIED=""; SHIM_CAPTURE=""
  local err rc argv errf
  errf="$(mktemp)"
  # NOT $(inject ...): a command substitution runs in a subshell, so DOM_APPLIED
  # and SHIM_CAPTURE would be discarded before we could assert on them.
  inject 2>"$errf" >/dev/null; rc=$?
  err="$(cat "$errf")"; rm -f "$errf"
  argv="$(printf '%s' "$SHIM_CAPTURE" | sed -n 's/.*xcvrd-rs", \[\(.*\)\])/\1/p')"
  printf '%-32s rc=%d applied=%-6s argv=[%s]\n' \
      "$name" "$rc" "'${DOM_APPLIED:-none}'" "$argv"
  if printf '%s' "$err" | grep -q DOM_INTERVAL_FALLBACK; then
    echo "                                 -> logged DOM_INTERVAL_FALLBACK"
  fi
  if [ "$argv" != "$want_argv" ] || [ "${DOM_APPLIED:-none}" != "$want_applied" ]; then
    echo "    FAIL: expected argv=[$want_argv] applied='$want_applied'"
    fails=$((fails + 1))
  fi
}

echo "=== inject() DOM interval behaviour ==="
#         name                       supports ival   expected argv                             applied
run_case "supports flag, asked 5"        1 5   '"xcvrd-rs", "--dom_update_interval", "5"'   5
run_case "NO support, asked 5"           0 5   '"xcvrd-rs"'                                  none
run_case "no interval requested"         1 ""  '"xcvrd-rs"'                                  none
run_case "no flag, daemon down anyway"   0 ""  '"xcvrd-rs"'                                  none

echo
if [ "$fails" -eq 0 ]; then echo "ALL PASS"; else echo "$fails FAILED"; exit 1; fi
