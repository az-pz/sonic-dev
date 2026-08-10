#!/bin/bash
# Runs on the sonic-dev host. Build the crate, stage it + the xcvrd-tests suite
# on the DUT, run the reversible inject+test+restore there, and fetch report.json
# back here.
# Usage: run_validate.sh <MILESTONE> [PYTEST_ARGS_B64]
set -uo pipefail
MILESTONE="${1:?milestone id}"
ARGS_B64="${2:-}"

RECODE="$HOME/recode"
CRATE="$RECODE/crate"
TESTS_SRC="$RECODE/xcvrd-tests"       # staged by validate_on_dut.sh
TESTS_DUT=/home/admin/xcvrd-tests     # where dut_validate.sh runs run.sh from
BIN="$CRATE/target/release/xcvrd-rs"
DUT="admin@10.250.0.101"
SP="sshpass -p password ssh -o StrictHostKeyChecking=no"
SPC="sshpass -p password scp -o StrictHostKeyChecking=no"

_now_ms() { date +%s%3N; }
_ph() { printf '[run][t] %-20s %6d ms\n' "$1" "$(( $(_now_ms) - $2 ))"; }

echo "[run] building crate"
T=$(_now_ms)
bash "$RECODE/dut/build_crate.sh" "$CRATE" || exit 2
[ -x "$BIN" ] || { echo "[run] no binary at $BIN" >&2; exit 2; }
_ph "build" "$T"

echo "[run] staging binary + dut_validate.sh on the DUT"
T=$(_now_ms)
docker cp "$BIN" mgmt:/tmp/xcvrd-rs
docker cp "$RECODE/dut/dut_validate.sh" mgmt:/tmp/dut_validate.sh
docker exec mgmt bash -lc "$SP $DUT \"mkdir -p /tmp/recode\""
docker exec mgmt bash -lc "$SPC /tmp/xcvrd-rs $DUT:/tmp/recode/xcvrd-rs"
docker exec mgmt bash -lc "$SPC /tmp/dut_validate.sh $DUT:/tmp/recode/dut_validate.sh"
_ph "ship" "$T"

# Ship the xcvrd-tests suite itself. dut_validate.sh runs $TESTS_DUT/run.sh, and
# nothing in this path used to refresh it -- so a validation either exploded with
# "No such file or directory" (fresh DUT) or silently graded the crate against
# whatever tree an earlier `setup-sonic-testbed.sh xcvrd_tests` happened to leave
# behind. Re-ship every run so the suite always matches the checkout.
# .pydeps (the offline pytest install run.sh bootstraps) is preserved across
# re-ships so we don't reinstall from wheels/ on every validation.
echo "[run] shipping xcvrd-tests -> $DUT:$TESTS_DUT"
T=$(_now_ms)
if [ ! -f "$TESTS_SRC/run.sh" ]; then
  echo "[run] xcvrd-tests not staged at $TESTS_SRC (validate_on_dut.sh should have put it there)" >&2
  exit 2
fi
TESTS_TAR=/tmp/xcvrd-tests.tar.gz
tar czf "$TESTS_TAR" -C "$(dirname "$TESTS_SRC")" \
    --exclude='xcvrd-tests/.pydeps' --exclude='xcvrd-tests/results.xml' \
    --exclude='xcvrd-tests/**/__pycache__' xcvrd-tests || exit 2
docker cp "$TESTS_TAR" mgmt:/tmp/xcvrd-tests.tar.gz
docker exec mgmt bash -lc "$SPC /tmp/xcvrd-tests.tar.gz $DUT:/tmp/xcvrd-tests.tar.gz" || exit 2
docker exec mgmt bash -lc "$SP $DUT \"
  set -e
  rm -rf /tmp/xt.new && mkdir -p /tmp/xt.new
  tar xzf /tmp/xcvrd-tests.tar.gz -C /tmp/xt.new
  [ -d $TESTS_DUT/.pydeps ] && mv $TESTS_DUT/.pydeps /tmp/xt.new/xcvrd-tests/.pydeps || true
  rm -rf $TESTS_DUT
  mv /tmp/xt.new/xcvrd-tests $TESTS_DUT
  rm -rf /tmp/xt.new /tmp/xcvrd-tests.tar.gz
  chmod +x $TESTS_DUT/run.sh
\"" || exit 2
_ph "ship tests" "$T"

echo "[run] validating on the DUT (milestone $MILESTONE)"
T=$(_now_ms)
docker exec mgmt bash -lc "$SP $DUT \"bash /tmp/recode/dut_validate.sh $MILESTONE $ARGS_B64\""
_ph "validate (on DUT)" "$T"

echo "[run] fetching report.json"
T=$(_now_ms)
docker exec mgmt bash -lc "$SPC $DUT:/tmp/recode/report.json /tmp/report.json"
docker cp mgmt:/tmp/report.json "$RECODE/report.json"
_ph "fetch" "$T"
echo "[run] report -> $RECODE/report.json"
