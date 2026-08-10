#!/bin/bash
# env_check.sh -- one-command proof of the agent scaffolding (platform-bridge +
# swss-common) on the DUT.
#
# Ships the crate to the sonic-dev host, builds the swss-smoke + env-smoke bins in
# the Debian-13 trixie container, runs them inside pmon (STATE_DB + xcvr-emu live),
# and cleans up. Use it to (re)verify that the two libraries agents build xcvrd-rs
# on top of compile, link, and run together.
#
# Usage: tools/env_check.sh
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECODE_DIR="$(cd "$HERE/.." && pwd)"          # dev/recodeAgent
SD="${RECODE_SSH_HOST:-sonic-dev}"
source "$HERE/lib_remote.sh"

echo "[env] staging crate + dut scripts -> $(r_where)"
r_put_dir "$RECODE_DIR/crate" "~/recode/crate"
r_put_files "~/recode/dut/" "$HERE/dut/env_check.sh" "$HERE/dut/ensure_swsslib.sh"

echo "[env] building + running smokes on the DUT"
r_run "bash ~/recode/dut/env_check.sh"
