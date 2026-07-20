#!/bin/bash
# check.sh - verify the DETERMINISTIC orchestrator offline (no DUT, no Copilot).
# Runs the four behaviours end-to-end against the mock agent and prints a summary.
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
  rm -f "$HERE/pipeline/burr.db" "$HERE"/pipeline/.mock_attempts_* 2>/dev/null
  unset RECODE_MOCK_FAIL RECODE_CRASH_AT 2>/dev/null || true
}

echo; echo "===== 1) HAPPY PATH - analyze->plan->M0..M6 all pass ====="
reset_run
"$PY" -m orchestrator.app --app-id chk-happy --mock

echo; echo "===== 2) REPAIR LOOP - M1 fails once, then passes ====="
reset_run; export RECODE_MOCK_FAIL="M1:1"
"$PY" -m orchestrator.app --app-id chk-repair --mock

echo; echo "===== 3) BUDGET EXHAUSTION - M2 always fails (max-iter 3) -> give up ====="
reset_run; export RECODE_MOCK_FAIL="M2:99"
"$PY" -m orchestrator.app --app-id chk-giveup --mock --max-iter 3

echo; echo "===== 4) CRASH-RESUME - crash at M3, resume SAME app-id ====="
reset_run
export RECODE_CRASH_AT="M3"; "$PY" -m orchestrator.app --app-id chk-resume --mock 2>/dev/null
echo "  (process 1 crashed; starting process 2 to resume...)"
unset RECODE_CRASH_AT; "$PY" -m orchestrator.app --app-id chk-resume --mock

reset_run
echo
echo "All orchestrator checks ran. Verify above:"
echo "  1 done=True (all pass)   2 M1 iter1=False then iter2=True   3 done=False (gave up)"
echo "  4 process-2 'loaded state ... milestone_idx=3' => resumed, not restarted"
echo "Artifacts: pipeline/report.json ; traces: ~/.burr/recodeagent-xcvrd"
