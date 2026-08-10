#!/bin/bash
# validate_on_dut.sh — the Validator's entry point (called from the Windows/
# Git-Bash side where the orchestrator + Copilot run).
#
# Ships the Rust crate + the xcvrd-tests suite + DUT scripts to the sonic-dev
# host, runs the full build -> reversible-inject -> xcvrd-tests -> restore cycle
# on the DUT, and fetches the authoritative report.json into pipeline/.
#
# Usage: tools/validate_on_dut.sh <MILESTONE|--all> [pytest args passed to run.sh...]
#   tools/validate_on_dut.sh M0 -k test_xcvrd_running
#   tools/validate_on_dut.sh M1 tests/test_presence.py -m "not slow"
#   tools/validate_on_dut.sh --all                # run the ENTIRE xcvrd-tests suite
#   tools/validate_on_dut.sh --all -m "not slow"  # whole suite, minus slow tests
#
# Set RECODE_PRINT_GATE=1 to print the resolved milestone + pytest gate and exit
# (no build / inject / DUT run) -- handy for previewing or testing the selection.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECODE_DIR="$(cd "$HERE/.." && pwd)"          # dev/recodeAgent
# The crate to build. Defaults to the immutable input crate/; the agent pipeline
# sets RECODE_CRATE_DIR=<recodeAgent>/pipeline/crate so translation works on a copy
# and the pristine crate/ is never modified.
CRATE_DIR="${RECODE_CRATE_DIR:-$RECODE_DIR/crate}"
# The xcvrd-tests suite that grades the translated crate. It is shipped to the DUT
# on every run (see below) so a validation never grades against a stale tree.
TESTS_DIR="${RECODE_TESTS_DIR:-$(cd "$RECODE_DIR/.." && pwd)/xcvrd-tests}"
MILESTONE="${1:?milestone id (e.g. M0) or --all}"; shift || true

# --all / -a (or a bare "all"/"ALL"): run the ENTIRE xcvrd-tests suite with no
# milestone -k gate -- every test module, including the T-series parity tests
# that are not wired into the milestone matrix (slow tests too, unless you also
# pass -m "not slow"). Reported under the label "ALL".
RUN_ALL=0
case "$MILESTONE" in
  --all|-a|all|ALL) RUN_ALL=1; MILESTONE="ALL" ;;
esac

# Resolve the pytest gate. Explicit args (after the milestone) override; else for
# --all use an empty gate (run.sh runs the whole tests dir); otherwise ask the
# milestone matrix for the CUMULATIVE gate (this milestone + all earlier
# milestones' tests). One arg per line preserves quoting like "not slow".
ARGS=()
if [ "$#" -gt 0 ]; then
  ARGS=("$@")
elif [ "$RUN_ALL" -eq 1 ]; then
  ARGS=()   # empty -> run.sh runs pytest over the entire tests dir
else
  # python may run under Windows (Git Bash) and emit CRLF; strip trailing \r and
  # skip blank lines so args like -k "a or b" survive intact.
  while IFS= read -r line; do
    line="${line%$'\r'}"
    [ -n "$line" ] && ARGS+=("$line")
  done < <(cd "$RECODE_DIR" && python -m orchestrator.milestones --args "$MILESTONE")
fi

if [ "$RUN_ALL" -eq 1 ]; then
  echo "[validate] $MILESTONE: running the ENTIRE xcvrd-tests suite: ${ARGS[*]:-(all modules, incl. slow)}"
else
  echo "[validate] $MILESTONE cumulative gate: ${ARGS[*]:-(deploy-smoke)}"
fi

# Preview/test hook: show the resolved selection and stop before any DUT work.
if [ "${RECODE_PRINT_GATE:-0}" = "1" ]; then
  echo "[validate] RECODE_PRINT_GATE=1 -> milestone=$MILESTONE args=[${ARGS[*]:-}]"
  exit 0
fi

# Encode the args the way run.sh expects (base64 of NUL-delimited argv) so quoting
# survives every ssh/docker hop.
ARGS_B64=""
if [ "${#ARGS[@]}" -gt 0 ]; then
  ARGS_B64="$(printf '%s\0' "${ARGS[@]}" | base64 -w0)"
fi

SD="${RECODE_SSH_HOST:-sonic-dev}"
source "$HERE/lib_remote.sh"

echo "[validate] staging crate ($CRATE_DIR) + xcvrd-tests + dut scripts -> $(r_where)"
[ -f "$TESTS_DIR/run.sh" ] || { echo "[validate] xcvrd-tests not found at $TESTS_DIR (set RECODE_TESTS_DIR)" >&2; exit 2; }
r_put_dir "$CRATE_DIR" "~/recode/crate"
# Ship the tests too: dut_validate.sh runs /home/admin/xcvrd-tests/run.sh on the
# DUT, and nothing else in this path refreshes it. Excludes mirror the ones
# setup-sonic-testbed.sh's xcvrd_tests phase uses (local build artifacts only --
# wheels/ MUST ship, run.sh installs pytest from it offline).
r_put_dir "$TESTS_DIR" "~/recode/xcvrd-tests" .pydeps results.xml __pycache__
r_put_files "~/recode/dut/" "$HERE/dut/"*.sh "$HERE/dut/Dockerfile.build"

echo "[validate] running build+inject+test+restore on the DUT"
r_run "bash ~/recode/dut/run_validate.sh $MILESTONE $ARGS_B64"

echo "[validate] fetching report.json -> pipeline/"
mkdir -p "$RECODE_DIR/pipeline"
r_get "~/recode/report.json" "$RECODE_DIR/pipeline/report.json"
cat "$RECODE_DIR/pipeline/report.json"
