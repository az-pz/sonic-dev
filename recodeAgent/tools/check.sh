#!/bin/bash
# check.sh - verify the DETERMINISTIC orchestrator offline (no DUT, no Copilot).
# Exercises BOTH loops end-to-end against the mock agents and prints a summary:
#   * inner loop  (milestone x repair): analyze->scope->plan->[select->translate->validate]
#   * outer loop  (parity coverage):    ...->parity_verify-> (gaps? re-scope : done)
#
#   bash tools/check.sh
#
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"     # -> dev/recodeAgent
cd "$HERE"
export RECODE_MOCK=1
export RECODE_PIPELINE_DIR="$HERE/pipeline"
PY="${PYTHON:-python}"

reset_run() {
  rm -f "$HERE/pipeline/burr.db" "$HERE"/pipeline/.mock_* \
        "$HERE/pipeline/milestones.json" "$HERE/pipeline/parity_report.json" \
        "$HERE/pipeline/skips.json" 2>/dev/null
  unset RECODE_MOCK_FAIL RECODE_CRASH_AT RECODE_MOCK_PARITY_GAPS RECODE_MOCK_RETRY_FAIL \
        RECODE_MOCK_FAIL_STYLE 2>/dev/null || true
}

echo; echo "===== 1) HAPPY PATH - analyze->scope->plan->M0..M6->parity COMPLETE ====="
reset_run; export RECODE_MOCK_PARITY_GAPS=0
"$PY" -m orchestrator.app --app-id chk-happy --mock

echo; echo "===== 2) REPAIR LOOP - M1 fails once, then passes ====="
reset_run; export RECODE_MOCK_FAIL="M1:1" RECODE_MOCK_PARITY_GAPS=0
"$PY" -m orchestrator.app --app-id chk-repair --mock

echo; echo "===== 3) GIVE-UP + RETRY MILESTONE FIXES IT - M2 gives up, parity retries -> pass ====="
reset_run; export RECODE_MOCK_FAIL="M2:99" RECODE_MOCK_PARITY_GAPS=0
"$PY" -m orchestrator.app --app-id chk-giveup --mock --max-iter 3
echo "  skips.json (expect tests_to_skip EMPTY after retry passed; retried records M2's test):"
"$PY" - <<'PY'
import json, os
p = os.path.join(os.environ["RECODE_PIPELINE_DIR"], "skips.json")
try:
    d = json.load(open(p)); print("     tests_to_skip=", d.get("tests_to_skip"), " retried=", d.get("retried"))
except FileNotFoundError:
    print("     MISSING skips.json (FAIL)")
PY
echo "  translate MODE per milestone (regression: M3, the milestone AFTER the skipped M2,"
echo "  must start IMPLEMENT -- not REPAIR on M2's stale report):"
"$PY" - <<'PY'
import os
p = os.path.join(os.environ["RECODE_PIPELINE_DIR"], ".mock_modes")
seen, first = [], {}
for line in open(p):
    mid, mode = line.strip().split(":")
    seen.append(f"{mid}:{mode}")
    first.setdefault(mid, mode)
print("      sequence:", " ".join(seen))
bad = [m for m, mode in first.items() if m not in ("M2",) and mode != "IMPLEMENT"]
print("      first-mode per milestone:", first)
print("      RESULT:", "PASS (every milestone starts IMPLEMENT)" if not bad
      else f"FAIL (started in REPAIR: {bad})")
PY

echo; echo "===== 3b) RETRY STILL FAILS -> PERMANENT SKIP - M2 + retry both give up ====="
reset_run; export RECODE_MOCK_FAIL="M2:99" RECODE_MOCK_RETRY_FAIL=1 RECODE_MOCK_PARITY_GAPS=0
"$PY" -m orchestrator.app --app-id chk-permskip --mock --max-iter 3
echo "  skips.json (expect the test in BOTH tests_to_skip AND retried = permanent):"
"$PY" - <<'PY'
import json, os
p = os.path.join(os.environ["RECODE_PIPELINE_DIR"], "skips.json")
d = json.load(open(p))
perm = [t for t in d.get("tests_to_skip", []) if t in set(d.get("retried", []))]
print("     tests_to_skip=", d.get("tests_to_skip"), " retried=", d.get("retried"), " permanent=", perm)
PY
unset RECODE_MOCK_RETRY_FAIL

echo; echo "===== 3c) SKIPS EXTRACTION IS SHAPE-TOLERANT - validator omits \"layer\" / uses prose ====="
for style in nolayer string; do
  reset_run
  export RECODE_MOCK_FAIL="M2:99" RECODE_MOCK_PARITY_GAPS=0 RECODE_MOCK_RETRY_FAIL=1 \
         RECODE_MOCK_FAIL_STYLE="$style"
  "$PY" -m orchestrator.app --app-id "chk-style-$style" --mock --max-iter 2 >/dev/null 2>&1
  echo "  failures reported as '$style' ->"
  "$PY" - <<'PY'
import json, os
p = os.path.join(os.environ["RECODE_PIPELINE_DIR"], "skips.json")
try:
    d = json.load(open(p))
    got = d.get("tests_to_skip") or d.get("retried")
    print("     recorded:", got, "" if got else " <-- FAIL (nothing deferred)")
except FileNotFoundError:
    print("     MISSING skips.json (FAIL - extraction found no e2e test)")
PY
done
unset RECODE_MOCK_FAIL_STYLE RECODE_MOCK_RETRY_FAIL

echo; echo "===== 4) CRASH-RESUME (inner) - crash at M3, resume SAME app-id ====="
reset_run; export RECODE_MOCK_PARITY_GAPS=0
export RECODE_CRASH_AT="M3"; "$PY" -m orchestrator.app --app-id chk-resume --mock 2>/dev/null
echo "  (process 1 crashed; starting process 2 to resume...)"
unset RECODE_CRASH_AT; "$PY" -m orchestrator.app --app-id chk-resume --mock

echo; echo "===== 5) PARITY FEEDBACK LOOP - gaps once -> re-scope appends milestone -> COMPLETE ====="
reset_run; export RECODE_MOCK_PARITY_GAPS=1
"$PY" -m orchestrator.app --app-id chk-parity --mock
echo "  milestone ids after re-scope:"
"$PY" - <<'PY'
import json, os
p = os.path.join(os.environ["RECODE_PIPELINE_DIR"], "milestones.json")
ms = json.load(open(p))["milestones"]
print("    ", [m["id"] for m in ms], "  (expect a parity-origin M7 appended)")
print("    ", [m["origin"] for m in ms])
PY

echo; echo "===== 6) OUTER BUDGET EXHAUSTION - parity never completes (max-parity-rounds 2) -> FAIL ====="
reset_run; export RECODE_MOCK_PARITY_GAPS=99
"$PY" -m orchestrator.app --app-id chk-outer --mock --max-parity-rounds 2

echo; echo "===== 7) CRASH-RESUME at SCOPE - crash in scoper, resume SAME app-id ====="
reset_run; export RECODE_MOCK_PARITY_GAPS=0
export RECODE_CRASH_AT="SCOPE"; "$PY" -m orchestrator.app --app-id chk-rscope --mock 2>/dev/null
echo "  (process 1 crashed in scope; starting process 2 to resume...)"
unset RECODE_CRASH_AT; "$PY" -m orchestrator.app --app-id chk-rscope --mock

echo; echo "===== 8) CRASH-RESUME at PARITY - crash in parity_verify, resume SAME app-id ====="
reset_run; export RECODE_MOCK_PARITY_GAPS=0
export RECODE_CRASH_AT="PARITY"; "$PY" -m orchestrator.app --app-id chk-rparity --mock 2>/dev/null
echo "  (process 1 crashed in parity; starting process 2 to resume...)"
unset RECODE_CRASH_AT; "$PY" -m orchestrator.app --app-id chk-rparity --mock

echo; echo "===== 9) EXISTING PIPELINE START - skip analyze/scope/plan, begin at M3 ====="
reset_run; export RECODE_MOCK_PARITY_GAPS=0
START_DIR="$HERE/pipeline_start_check"
rm -rf "$START_DIR"
mkdir -p "$START_DIR"
# Seed realistic artifacts with a normal mock run, then reuse them with a fresh DB/id.
RECODE_PIPELINE_DIR="$START_DIR" "$PY" -m orchestrator.app \
  --pipeline-dir "$START_DIR" --app-id chk-start-seed \
  --db "$START_DIR/seed.db" --mock >/dev/null
mkdir -p "$START_DIR/crate/xcvrd-rs"
rm -f "$START_DIR"/.mock_* "$START_DIR/report.json" "$START_DIR/parity_report.json"
RECODE_PIPELINE_DIR="$START_DIR" "$PY" -m orchestrator.app \
  --pipeline-dir "$START_DIR" --start-milestone M3 \
  --app-id chk-start-m3 --db "$START_DIR/start.db" --mock
rm -rf "$START_DIR"
export RECODE_PIPELINE_DIR="$HERE/pipeline"

reset_run
echo
echo "All orchestrator checks ran. Verify above:"
echo "  1 done=True parity_complete=True   2 M1 iter1=False then iter2=True, done=True"
echo "  3 M2 gives up -> parity appends a RETRY milestone -> retry PASSES -> tests_to_skip empty, done=True"
echo "  3b M2 + retry both give up -> test in tests_to_skip AND retried = PERMANENTLY SKIPPED, done=True"
echo "  4 proc-2 'milestone_idx=3' => resumed"
echo "  5 ids include appended M7 (origin parity), done=True parity_round=2"
echo "  6 done=False parity_complete=False (outer give-up, no deferral)"
echo "  7 proc-2 resumes at scope, done=True   8 proc-2 resumes at parity, done=True"
echo "  9 starts at M3 (history begins M3; no M0/M1/M2, analyze/scope/plan skipped)"
echo "Artifacts: pipeline/{milestones,report,parity_report,skips}.json ; traces: ~/.burr/recodeagent-xcvrd"
