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
