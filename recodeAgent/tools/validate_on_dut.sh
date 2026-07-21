#!/bin/bash
# validate_on_dut.sh — the Validator's entry point (called from the Windows/
# Git-Bash side where the orchestrator + Copilot run).
#
# Ships the Rust crate + DUT scripts to the sonic-dev host, runs the full
# build -> reversible-inject -> xcvrd-tests -> restore cycle on the DUT, and
# fetches the authoritative report.json into pipeline/.
#
# Usage: tools/validate_on_dut.sh <MILESTONE> [pytest args passed to run.sh...]
#   tools/validate_on_dut.sh M0 -k test_xcvrd_running
#   tools/validate_on_dut.sh M1 tests/test_presence.py -m "not slow"
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECODE_DIR="$(cd "$HERE/.." && pwd)"          # dev/recodeAgent
# The crate to build. Defaults to the immutable input crate/; the agent pipeline
# sets RECODE_CRATE_DIR=<recodeAgent>/pipeline/crate so translation works on a copy
# and the pristine crate/ is never modified.
CRATE_DIR="${RECODE_CRATE_DIR:-$RECODE_DIR/crate}"
MILESTONE="${1:?milestone id (e.g. M0)}"; shift || true

# Resolve the pytest gate. Explicit args (after the milestone) override; otherwise
# ask the milestone matrix for the CUMULATIVE gate (this milestone + all earlier
# milestones' tests). One arg per line preserves quoting like "not slow".
ARGS=()
if [ "$#" -gt 0 ]; then
  ARGS=("$@")
else
  # python may run under Windows (Git Bash) and emit CRLF; strip trailing \r and
  # skip blank lines so args like -k "a or b" survive intact.
  while IFS= read -r line; do
    line="${line%$'\r'}"
    [ -n "$line" ] && ARGS+=("$line")
  done < <(cd "$RECODE_DIR" && python -m orchestrator.milestones --args "$MILESTONE")
fi
echo "[validate] $MILESTONE cumulative gate: ${ARGS[*]:-(deploy-smoke)}"

# Encode the args the way run.sh expects (base64 of NUL-delimited argv) so quoting
# survives every ssh/docker hop.
ARGS_B64=""
if [ "${#ARGS[@]}" -gt 0 ]; then
  ARGS_B64="$(printf '%s\0' "${ARGS[@]}" | base64 -w0)"
fi

SD="${RECODE_SSH_HOST:-sonic-dev}"

echo "[validate] shipping crate ($CRATE_DIR) + dut scripts to $SD"
ssh "$SD" "mkdir -p ~/recode/dut ~/recode/crate"
# Ship crate SOURCE (build happens on the DUT side; keep sonic-dev's target/ cache).
tar -C "$CRATE_DIR" --exclude target -cf - . | ssh "$SD" "tar -C ~/recode/crate -xf -"
scp -q "$HERE/dut/"*.sh "$HERE/dut/Dockerfile.build" "$SD:/home/sonic/recode/dut/"

echo "[validate] running build+inject+test+restore on the DUT"
ssh "$SD" "bash ~/recode/dut/run_validate.sh $MILESTONE $ARGS_B64"

echo "[validate] fetching report.json -> pipeline/"
mkdir -p "$RECODE_DIR/pipeline"
scp -q "$SD:/home/sonic/recode/report.json" "$RECODE_DIR/pipeline/report.json"
cat "$RECODE_DIR/pipeline/report.json"
