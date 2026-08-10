#!/bin/bash
# bridge_smoke.sh — one-command sanity check of the PyO3 platform-bridge on the DUT.
#
# Ships the crate to the sonic-dev host, builds the bridge-smoke binary in the
# Debian-13 trixie container, runs it inside pmon against the live xcvr-emu, prints
# the result, and cleans up. Use it to (re)verify the platform boundary whenever
# the bridge changes -- the translation agents rely on this scaffolding.
#
# Usage: tools/bridge_smoke.sh
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECODE_DIR="$(cd "$HERE/.." && pwd)"          # dev/recodeAgent
SD="${RECODE_SSH_HOST:-sonic-dev}"
source "$HERE/lib_remote.sh"

echo "[bridge] staging crate + dut script -> $(r_where)"
r_put_dir "$RECODE_DIR/crate" "~/recode/crate"
r_put_files "~/recode/dut/" "$HERE/dut/bridge_smoke.sh"

echo "[bridge] building + running smoke on the DUT"
r_run "bash ~/recode/dut/bridge_smoke.sh"
