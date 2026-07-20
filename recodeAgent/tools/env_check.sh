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

echo "[env] shipping crate + dut scripts to $SD"
ssh "$SD" "mkdir -p ~/recode/dut ~/recode/crate"
tar -C "$RECODE_DIR/crate" --exclude target -cf - . | ssh "$SD" "tar -C ~/recode/crate -xf -"
scp -q "$HERE/dut/env_check.sh" "$HERE/dut/ensure_swsslib.sh" "$SD:/home/sonic/recode/dut/"

echo "[env] building + running smokes on the DUT"
ssh "$SD" "bash ~/recode/dut/env_check.sh"
