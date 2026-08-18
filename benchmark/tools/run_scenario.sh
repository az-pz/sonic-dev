#!/usr/bin/env bash
# run_scenario.sh <B-id | scenario-id> [--configs abp]  |  --list
#
# Single entry point for the B1..B12 suite. Reads the scenario artifact, refuses to
# run anything marked blocked (printing WHY, not just failing), and otherwise
# dispatches to the right instrument.
#
# Roughly half the suite is blocked here by design: anything measuring a PROCESS
# (RSS, threads, SIGTERM) or needing the emulator Monitor stream belongs to the live
# DUT harness, not this in-process one. Saying so out loud beats a scenario that
# looks runnable and silently measures the harness instead of the daemon.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH="$(cd "$HERE/.." && pwd)"
SCEN="$BENCH/scenarios"
OUT="$BENCH/results/scenarios"
mkdir -p "$OUT"

jq_get() { python3 -c "import json,sys;print(json.load(open(sys.argv[1])).get(sys.argv[2],''))" "$1" "$2"; }

list() {
  printf '%-5s %-12s %-10s %-28s %s\n' ID STATUS HARNESS SCENARIO METRIC
  for f in "$SCEN"/*.json; do
    python3 - "$f" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
print('%-5s %-12s %-10s %-28s %s' % (d.get('bench_id','-'), d.get('status','-'),
      d.get('harness','-'), d['id'], d.get('metric','')[:60]))
PY
  done
}

[ "${1:-}" = "--list" ] && { list; exit 0; }
[ -n "${1:-}" ] || { echo "usage: run_scenario.sh <B-id|scenario-id> | --list" >&2; list >&2; exit 2; }

want="$1"; shift
file=""
for f in "$SCEN"/*.json; do
  id="$(jq_get "$f" id)"; bid="$(jq_get "$f" bench_id)"
  if [ "$want" = "$id" ] || [ "$want" = "$bid" ]; then file="$f"; break; fi
done
[ -n "$file" ] || { echo "[scenario] no such scenario: $want" >&2; list >&2; exit 2; }

id="$(jq_get "$file" id)"; bid="$(jq_get "$file" bench_id)"
status="$(jq_get "$file" status)"; harness="$(jq_get "$file" harness)"
ports="$(jq_get "$file" ports)"; iters="$(jq_get "$file" iterations)"

echo "=== $bid  $id ==="
echo "harness : $harness"
notes="$(jq_get "$file" notes)"; [ -n "$notes" ] && echo "notes   : $notes"

if [ "$status" = "blocked" ]; then
  echo
  echo "BLOCKED - not runnable in this harness."
  echo "reason  : $(jq_get "$file" blocked_by)"
  exit 3
fi

case "$bid" in
  B4)
    echo "--- pass duration, 3 configs, interleaved ---"
    for rep in 1 2 3; do
      "$BENCH/tools/run_trace.sh"    --config a --ports "$ports" --polls "$iters" --time 2>/dev/null | grep '^{'
      "$BENCH/tools/run_trace.sh"    --config b --ports "$ports" --polls "$iters" --time 2>/dev/null | grep '^{'
      "$BENCH/tools/run_bench_dom.sh"            --ports "$ports" --polls "$iters" --time 2>/dev/null | grep '^{'
    done | tee "$OUT/$id.jsonl"
    python3 "$BENCH/equivalence/summarize.py" "$OUT/$id.jsonl"
    ;;
  B7)
    sweep="$(python3 -c "import json;print(' '.join(map(str,json.load(open('$file'))['port_sweep'])))")"
    echo "--- fan-out sweep: N = $sweep ---"
    : > "$OUT/$id.jsonl"
    for n in $sweep; do
      "$BENCH/tools/run_trace.sh"    --config a --ports "$n" --polls "$iters" --time 2>/dev/null | grep '^{' >> "$OUT/$id.jsonl"
      "$BENCH/tools/run_trace.sh"    --config b --ports "$n" --polls "$iters" --time 2>/dev/null | grep '^{' >> "$OUT/$id.jsonl"
      "$BENCH/tools/run_bench_dom.sh"            --ports "$n" --polls "$iters" --time 2>/dev/null | grep '^{' >> "$OUT/$id.jsonl"
    done
    python3 "$BENCH/equivalence/summarize.py" "$OUT/$id.jsonl" --by-ports
    ;;
  B9)
    echo "--- work profile (the validity gate) ---"
    "$BENCH/tools/run_trace.sh"    --config a --ports "$ports" --polls 1 --out "$OUT/${id}_a.jsonl" >/dev/null 2>&1
    "$BENCH/tools/run_trace.sh"    --config b --ports "$ports" --polls 1 --out "$OUT/${id}_b.jsonl" >/dev/null 2>&1
    "$BENCH/tools/run_bench_dom.sh"            --ports "$ports" --polls 1 --trace "$OUT/${id}_p.jsonl" >/dev/null 2>&1
    echo "[A vs B] rust native edge vs rust over the real bridge - MUST be identical:"
    python3 "$BENCH/equivalence/compare.py" "$OUT/${id}_a.jsonl" "$OUT/${id}_b.jsonl" | tail -3
    echo "[A vs P] rust vs the python reference:"
    python3 "$BENCH/equivalence/compare.py" "$OUT/${id}_a.jsonl" "$OUT/${id}_p.jsonl" | tail -12
    echo "--- Redis commands per sweep ---"
    for c in a p; do
      docker exec xcvrd-bench-redis redis-cli -s /sock/redis.sock flushall >/dev/null 2>&1
      docker exec xcvrd-bench-redis redis-cli -s /sock/redis.sock config resetstat >/dev/null 2>&1
      if [ "$c" = p ]; then "$BENCH/tools/run_bench_dom.sh" --ports "$ports" --polls 1 --time >/dev/null 2>&1
      else "$BENCH/tools/run_trace.sh" --config a --ports "$ports" --polls 1 --time >/dev/null 2>&1; fi
      printf 'config %s: ' "$c"
      docker exec xcvrd-bench-redis redis-cli -s /sock/redis.sock info commandstats \
        | grep -oP 'cmdstat_\K(hset|hgetall|hget)[^:]*:calls=[0-9]+' | tr '\n' ' '; echo
    done
    ;;
  B12)
    echo "--- per-call edge cost, all three configs ---"
    "$BENCH/tools/run_calibrate.sh"    --out "$OUT/${id}_rust.json"   >/dev/null 2>&1
    "$BENCH/tools/run_calibrate_py.sh" --out "$OUT/${id}_python.json" >/dev/null 2>&1
    python3 "$BENCH/equivalence/compare_edges.py" "$OUT/${id}_rust.json" "$OUT/${id}_python.json"
    ;;
  *)
    echo "[scenario] $bid is marked implemented but has no dispatcher entry" >&2; exit 4 ;;
esac
