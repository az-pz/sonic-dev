#!/bin/bash
# run_dut_bench.sh <B1|B4|B5|B10> [--variants rust,python] [--reps N] [xbench args...]
#
# Runs from the azure host. Ships benchmark/dut/ to the DUT via the mgmt container,
# then for each variant injects the daemon, runs the scenario in situ, and ALWAYS
# restores the stock Python xcvrd afterwards -- including on interrupt, because a DUT
# left with an injected binary silently poisons every later run on this testbed.
#
# Variants are interleaved rather than batched (rust, python, rust, ...): vlab-01 is a
# KVM guest, so host steal and background load drift over minutes and would otherwise
# be attributed to whichever daemon happened to run second.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH="$(cd "$HERE/.." && pwd)"
DUT_IP="${DUT_IP:-10.250.0.101}"
DUT_PW="${DUT_PW:-password}"
STAGE=/tmp/xbench
RUST_BIN="${RUST_BIN:-$HOME/recode/crate/target/release/xcvrd-rs}"
OUT="$BENCH/results/dut"; mkdir -p "$OUT"

SCEN="${1:?scenario id, e.g. B5}"; shift
VARIANTS="rust,python"; REPS=3; EXTRA=()
while [ $# -gt 0 ]; do
  case "$1" in
    --variants) VARIANTS="$2"; shift 2 ;;
    --reps)     REPS="$2"; shift 2 ;;
    *) EXTRA+=("$1"); shift ;;
  esac
done

dut() { docker exec mgmt bash -lc \
  "sshpass -p $DUT_PW ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 admin@$DUT_IP '$1'"; }

# Restore on ANY exit path, not just the happy one.
cleanup() { echo "[dut-bench] restoring stock python xcvrd"; dut "bash $STAGE/inject.sh python" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

echo "[dut-bench] shipping harness -> $DUT_IP:$STAGE"
tar czf /tmp/xbench.tar.gz -C "$BENCH" dut || exit 2
docker cp /tmp/xbench.tar.gz mgmt:/tmp/xbench.tar.gz >/dev/null
docker exec mgmt bash -lc "sshpass -p $DUT_PW scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null /tmp/xbench.tar.gz admin@$DUT_IP:/tmp/" >/dev/null 2>&1
dut "rm -rf $STAGE && mkdir -p $STAGE && tar xzf /tmp/xbench.tar.gz -C /tmp && cp -r /tmp/dut/* $STAGE/ && chmod +x $STAGE/*.sh $STAGE/*.py" >/dev/null

if [[ ",$VARIANTS," == *,rust,* ]]; then
  if [ -f "$RUST_BIN" ]; then
    echo "[dut-bench] shipping rust binary ($(du -h "$RUST_BIN" | cut -f1))"
    docker cp "$RUST_BIN" mgmt:/tmp/xcvrd-rs >/dev/null
    docker exec mgmt bash -lc "sshpass -p $DUT_PW scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null /tmp/xcvrd-rs admin@$DUT_IP:$STAGE/xcvrd-rs" >/dev/null 2>&1
  else
    echo "[dut-bench] no rust binary at $RUST_BIN - build it first, or pass --variants python" >&2
    exit 2
  fi
fi

RESULT="$OUT/${SCEN}.jsonl"
echo "[dut-bench] $SCEN, variants=$VARIANTS, reps=$REPS -> $RESULT"
for rep in $(seq 1 "$REPS"); do
  IFS=',' read -ra VS <<< "$VARIANTS"
  for v in "${VS[@]}"; do
    echo "--- rep $rep / $v ---"
    dut "bash $STAGE/inject.sh $v" || exit 3
    dut "cd $STAGE && python3 xbench.py $SCEN --reps 1 ${EXTRA[*]:-}" | tee -a "$RESULT"
  done
done

echo
echo "[dut-bench] results -> $RESULT"
python3 "$BENCH/equivalence/summarize_dut.py" "$RESULT" 2>/dev/null || true
