#!/usr/bin/env bash
# select_target.sh <result-dir> -- point the benchmark harness at a translation.
#
# recodeAgent/results/result_N are recorded pipeline artifacts and MUST stay
# immutable, so the harness never adds itself to their workspace. It depends on
# `benchmark/rust/target-crate`, a symlink this script repoints -- which is what
# lets one harness benchmark result_4, result_5, ... unchanged.
#
#   ./tools/select_target.sh recodeAgent/results/result_4
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
LINK="$HERE/../rust/target-crate"

target="${1:-}"
[ -n "$target" ] || {
  echo "usage: select_target.sh <result-dir>   e.g. recodeAgent/results/result_4" >&2
  echo "available:" >&2
  ls -d "$REPO"/recodeAgent/results/*/ 2>/dev/null | sed "s|$REPO/|  |" >&2
  exit 2
}

# Accept either the result dir or the crate dir itself.
case "$target" in
  /*) abs="$target" ;;
  *)  abs="$REPO/$target" ;;
esac
[ -d "$abs/crate" ] && abs="$abs/crate"

[ -d "$abs" ] || { echo "[select] no such directory: $abs" >&2; exit 2; }
[ -f "$abs/xcvrd-rs/Cargo.toml" ] || {
  echo "[select] $abs is not a target crate (expected xcvrd-rs/Cargo.toml)" >&2; exit 2; }

ln -sfn "$abs" "$LINK"
echo "[select] target-crate -> $(readlink -f "$LINK")"
grep -m1 '^version' "$abs/xcvrd-rs/Cargo.toml" 2>/dev/null | sed 's/^/[select] xcvrd-rs /'

# --- portability preflight -------------------------------------------------
# The IN-PROCESS harness (rust/) links the target as a library and implements its
# Hal / SfpHandle traits. Those are PRIVATE INTERNALS and differ per translation:
# result_3/result_4 expose Hal + SfpHandle + DbTable, while result_5 exposes
# Chassis + Sfp + StateDb + Table. So rust/ only builds against the former, and
# against anything else cargo fails with "no `Hal` in `hal`".
#
# The only contract EVERY implementation honours is the deployed one: a process
# that consumes the Python sonic_platform (Platform -> Chassis -> Sfp) and writes
# STATE_DB. That is what benchmark/dut/ drives, which is why the DUT harness works
# for any translation and the in-process harness does not.
have_traits=1
for t in "pub trait Hal" "pub trait SfpHandle"; do
  grep -rq "$t" "$abs"/xcvrd-rs/src/*.rs 2>/dev/null || have_traits=0
done
if [ "$have_traits" = 1 ]; then
  echo "[select] in-process harness: SUPPORTED (target exposes Hal + SfpHandle)"
else
  echo "[select] in-process harness: NOT SUPPORTED for this target."
  echo "[select]   It exposes:" $(grep -rho "pub trait [A-Za-z]*" "$abs"/xcvrd-rs/src/*.rs 2>/dev/null | sed "s/pub trait //" | sort -u | tr "\n" " ")
  echo "[select]   but rust/ requires Hal + SfpHandle (result_3/result_4 shape)."
  echo "[select]   Use the DUT harness instead - it drives the daemon as a PROCESS and"
  echo "[select]   assumes only sonic_platform + STATE_DB, so it works for any translation:"
  echo "[select]     ./tools/run_dut_bench.sh B9 --variants rust,python"
fi
