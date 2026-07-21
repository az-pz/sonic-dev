#!/bin/bash
# unit_test.sh -- build + run the Rust crate's UNIT tests (cargo test) in the
# Debian-13 trixie container. This is the Part-B counterpart to the e2e black-box
# harness (validate_on_dut.sh): the unit tests use MOCKS for the platform HAL and
# STATE_DB (mirroring the Python xcvrd tests' mock_platform.py / mock_swsscommon.py),
# so they run standalone in the container -- no DUT, emulator, or redis needed.
#
# Builds ${RECODE_CRATE_DIR:-<recodeAgent>/crate} (agents set RECODE_CRATE_DIR to
# the pipeline working copy). Exit code = cargo test's; test output is streamed.
# Usage: bash tools/unit_test.sh
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECODE_DIR="$(cd "$HERE/.." && pwd)"          # dev/recodeAgent
CRATE_DIR="${RECODE_CRATE_DIR:-$RECODE_DIR/crate}"
SD="${RECODE_SSH_HOST:-sonic-dev}"

echo "[unit] shipping crate ($CRATE_DIR) to $SD"
ssh "$SD" "mkdir -p ~/recode/dut ~/recode/crate"
tar -C "$CRATE_DIR" --exclude target -cf - . | ssh "$SD" "tar -C ~/recode/crate -xf -"
scp -q "$HERE/dut/ensure_swsslib.sh" "$SD:/home/sonic/recode/dut/"

# libswsscommon.so for the linker (the crate links it even though unit tests mock it).
ssh "$SD" "bash ~/recode/dut/ensure_swsslib.sh"

echo "[unit] cargo test --release in the trixie container"
ssh "$SD" "docker run --rm --network host -v ~/recode/crate:/src -v ~/recode/swsslib:/swsslib -w /src -e RUSTFLAGS='-L native=/swsslib' recode-rust-build cargo test --release -p xcvrd-rs"
