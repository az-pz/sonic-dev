#!/usr/bin/env bash
# run_calibrate.sh [--config a|b|both] [--num-sfps N] [--out FILE]
#
# Measures what the harness's own edges cost, per config. Timing a daemon yields
#   T_measured = T_orchestration + k * C_edge
# and the equivalence gate supplies k exactly, so this supplies C_edge and makes the
# correction (and its error bar) computable rather than assumed.
#
# Runs in the recode-rust-build container so the interpreter, glibc and libswsscommon
# match the pmon runtime -- calibrating against a different CPython than config B will
# actually use would defeat the purpose.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH="$(cd "$HERE/.." && pwd)"
REPO="$(cd "$BENCH/.." && pwd)"
SWSSLIB="${SWSSLIB:-$HOME/recode/swsslib}"
IMAGE="${RECODE_BUILD_IMAGE:-recode-rust-build}"
FIXTURE="${FIXTURE:-$BENCH/fixtures/cmis_40g_lr4.json}"

BIN="$BENCH/rust/target/release/calibrate"
[ -x "$BIN" ] || { echo "[calib] not built -- run ./tools/build_rust.sh first" >&2; exit 2; }

docker run --rm \
  -v "$REPO":"$REPO" \
  -v "$SWSSLIB":/swsslib \
  -w "$BENCH/rust" \
  -e LD_LIBRARY_PATH=/swsslib \
  "$IMAGE" ./target/release/calibrate \
    --fixture "$FIXTURE" \
    --pymocks "$BENCH/pymocks" \
    "$@"
