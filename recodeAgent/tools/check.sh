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
  rm -f "$HERE/pipeline/burr.db" "$HERE"/pipeline/.mock_attempts_* \
        "$HERE/pipeline/.mock_parity_attempts" \
        "$HERE/pipeline/milestones.json" "$HERE/pipeline/parity_report.json" 2>/dev/null
  unset RECODE_MOCK_FAIL RECODE_CRASH_AT RECODE_MOCK_PARITY_GAPS 2>/dev/null || true
}

echo; echo "===== 1) HAPPY PATH - analyze->scope->plan->M0..M6->parity COMPLETE ====="
reset_run; export RECODE_MOCK_PARITY_GAPS=0
"$PY" -m orchestrator.app --app-id chk-happy --mock

echo; echo "===== 2) REPAIR LOOP - M1 fails once, then passes ====="
reset_run; export RECODE_MOCK_FAIL="M1:1" RECODE_MOCK_PARITY_GAPS=0
"$PY" -m orchestrator.app --app-id chk-repair --mock

echo; echo "===== 3) INNER GIVE-UP SKIPS - M2 always fails (max-iter 3) -> skip M2, continue ====="
reset_run; export RECODE_MOCK_FAIL="M2:99" RECODE_MOCK_PARITY_GAPS=0
"$PY" -m orchestrator.app --app-id chk-giveup --mock --max-iter 3

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

reset_run
echo
echo "All orchestrator checks ran. Verify above:"
echo "  1 done=True parity_complete=True   2 M1 iter1=False then iter2=True, done=True"
echo "  3 M2 GAVE-UP/SKIPPED (3 iters, passed=False), run CONTINUES M3..M6->parity, skipped=[M2]"
echo "  4 proc-2 'milestone_idx=3' => resumed"
echo "  5 ids include appended M7 (origin parity), done=True parity_round=2"
echo "  6 done=False parity_complete=False (outer give-up, no deferral)"
echo "  7 proc-2 resumes at scope, done=True   8 proc-2 resumes at parity, done=True"
echo "Artifacts: pipeline/{milestones,report,parity_report}.json ; traces: ~/.burr/recodeagent-xcvrd"
