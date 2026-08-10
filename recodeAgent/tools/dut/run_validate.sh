#!/bin/bash
# Runs on the sonic-dev host. Build the crate, stage it on the DUT, run the
# reversible inject+test+restore there, and fetch report.json back here.
# Usage: run_validate.sh <MILESTONE> [PYTEST_ARGS_B64]
set -uo pipefail
MILESTONE="${1:?milestone id}"
ARGS_B64="${2:-}"

RECODE="$HOME/recode"
CRATE="$RECODE/crate"
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
