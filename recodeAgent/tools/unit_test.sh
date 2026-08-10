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
source "$HERE/lib_remote.sh"

echo "[unit] staging crate ($CRATE_DIR) -> $(r_where)"
r_put_dir "$CRATE_DIR" "~/recode/crate"
r_put_files "~/recode/dut/" "$HERE/dut/ensure_swsslib.sh"

# libswsscommon.so for the linker (the crate links it even though unit tests mock it).
r_run "bash ~/recode/dut/ensure_swsslib.sh"

echo "[unit] cargo test --release in the trixie container"
r_run "docker run --rm --network host -v ~/recode/crate:/src -v ~/recode/swsslib:/swsslib -w /src -e RUSTFLAGS='-L native=/swsslib' recode-rust-build cargo test --release -p xcvrd-rs"
