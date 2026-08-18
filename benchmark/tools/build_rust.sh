#!/usr/bin/env bash
# build_rust.sh [cargo args...] -- build/test the benchmark crate.
#
# Runs inside the recode-rust-build container (Debian 13 / py3.13 / glibc 2.41 --
# the pmon runtime) with libswsscommon on the linker path, mirroring
# recodeAgent/tools/unit_test.sh. The target crate links swss-common and pyo3 even
# when every seam is mocked, so those are link-time requirements regardless of the
# fact that nothing calls into Python under MockHal (pyo3 auto-initializes the
# interpreter lazily on the first Python::with_gil, which never happens).
#
#   ./tools/build_rust.sh                 # cargo build --release
#   ./tools/build_rust.sh test            # cargo test  --release
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_RUST="$(cd "$HERE/../rust" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SWSSLIB="${SWSSLIB:-$HOME/recode/swsslib}"
IMAGE="${RECODE_BUILD_IMAGE:-recode-rust-build}"

[ -e "$BENCH_RUST/target-crate" ] || {
  echo "[build] no target selected -- run: ./tools/select_target.sh recodeAgent/results/result_4" >&2
  exit 2; }
docker image inspect "$IMAGE" >/dev/null 2>&1 || {
  echo "[build] missing build image '$IMAGE' -- run recodeAgent/tools/build_check.sh once to create it" >&2
  exit 2; }
[ -d "$SWSSLIB" ] || {
  echo "[build] libswsscommon not staged at $SWSSLIB (override with SWSSLIB=...)" >&2
  exit 2; }

cmd="${1:-build}"; shift 2>/dev/null || true

# The whole repo is mounted (not just benchmark/) because target-crate is a symlink
# into recodeAgent/results/... -- a narrower mount would dangle it.
docker run --rm \
  -v "$REPO":"$REPO" \
  -v "$SWSSLIB":/swsslib \
  -w "$BENCH_RUST" \
  -e RUSTFLAGS='-L native=/swsslib' \
  -e CARGO_TERM_COLOR=never \
  "$IMAGE" cargo "$cmd" --release "$@"
