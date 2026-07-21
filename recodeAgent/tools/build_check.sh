#!/bin/bash
# build_check.sh -- compile-only check for the Planner/Translator agents.
#
# Ships crate/ to the sonic-dev host and builds xcvrd-rs for pmon in the Debian-13
# trixie container (links libpython + libswsscommon) WITHOUT injecting or running
# any tests. Fast feedback so agents can iterate on "does it compile?" before
# handing off to the Validator (which does the full build->inject->test->restore).
#
# Usage: bash tools/build_check.sh
# Exit 0 = compiles; non-zero = build errors (printed above).
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECODE_DIR="$(cd "$HERE/.." && pwd)"          # dev/recodeAgent
CRATE_DIR="${RECODE_CRATE_DIR:-$RECODE_DIR/crate}"   # immutable crate/ by default; pipeline sets pipeline/crate
SD="${RECODE_SSH_HOST:-sonic-dev}"

echo "[build-check] shipping crate ($CRATE_DIR) to $SD"
ssh "$SD" "mkdir -p ~/recode/dut ~/recode/crate"
tar -C "$CRATE_DIR" --exclude target -cf - . | ssh "$SD" "tar -C ~/recode/crate -xf -"
scp -q "$HERE/dut/build_crate.sh" "$HERE/dut/ensure_swsslib.sh" "$SD:/home/sonic/recode/dut/"

echo "[build-check] compiling xcvrd-rs for pmon (trixie container)"
ssh "$SD" "bash ~/recode/dut/build_crate.sh ~/recode/crate"
