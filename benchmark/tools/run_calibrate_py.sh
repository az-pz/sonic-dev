#!/usr/bin/env bash
# run_calibrate_py.sh [--num-sfps N] [--out FILE]
#
# Config P: the Python daemon's edge, against the same pymocks plant config B uses.
#
# Runs in the recode-rust-build container on purpose. That image carries the CPython
# that config B embeds via PyO3, so P and B are timed on the same interpreter -- using
# the host python instead would make the B-vs-P difference a mixture of the PyO3
# crossing and an interpreter version change, which is exactly the confound this
# harness exists to avoid.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH="$(cd "$HERE/.." && pwd)"
REPO="$(cd "$BENCH/.." && pwd)"
IMAGE="${RECODE_BUILD_IMAGE:-recode-rust-build}"
FIXTURE="${FIXTURE:-$BENCH/fixtures/cmis_40g_lr4.json}"

docker run --rm \
  -v "$REPO":"$REPO" \
  -w "$BENCH" \
  "$IMAGE" python3 python/calibrate.py \
    --fixture "$FIXTURE" \
    --pymocks "$BENCH/pymocks" \
    "$@"
