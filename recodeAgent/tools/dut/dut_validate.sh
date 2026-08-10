#!/bin/bash
# Runs ON the DUT (admin@vlab-01). Reversibly swaps the Rust xcvrd into pmon,
# runs the milestone's xcvrd-tests subset, ALWAYS restores the Python xcvrd, and
# writes report.json (authoritative verdict derived from results.xml).
#
# The Rust binary is staged at $STAGE/xcvrd-rs (shipped by run_validate.sh).
# Usage: dut_validate.sh <MILESTONE> [PYTEST_ARGS_B64]
set -uo pipefail
MILESTONE="${1:?milestone id}"
ARGS_B64="${2:-}"

STAGE=/tmp/recode
PMON=pmon
XBIN=/usr/local/bin/xcvrd            # what supervisor runs: python3 /usr/local/bin/xcvrd
XORIG=/usr/local/bin/xcvrd.pyorig    # backup of the real python xcvrd
XRUST=/usr/local/bin/xcvrd-rs        # the injected Rust binary
TESTS=/home/admin/xcvrd-tests
REPORT="$STAGE/report.json"

# --- timing: per-step elapsed in ms, to profile where the harness spends time ---
_now_ms() { date +%s%3N; }
_LAP=$(_now_ms)
lap() { local n; n=$(_now_ms); printf '[dut][t] %-26s %6d ms\n' "$1" "$((n - _LAP))"; _LAP=$n; }

# Bounded poll for xcvrd RUNNING (supervisorctl restart already blocks until the
# process is started, so this usually returns on the first probe -- much better
# than a fixed sleep, and it also catches BACKOFF/FATAL quickly).
wait_running() {
  local i st
  for i in $(seq 1 20); do
    st=$(docker exec "$PMON" supervisorctl status xcvrd 2>/dev/null | awk '{print $2}')
    [ "$st" = "RUNNING" ] && return 0
    sleep 0.3
  done
  return 1
}

_restored=0
restore() {
  # Idempotent: only acts while the backup exists.
  if docker exec "$PMON" sh -c "[ -e $XORIG ]" 2>/dev/null; then
    docker exec "$PMON" sh -c "cp $XORIG $XBIN && rm -f $XORIG $XRUST"
    docker exec "$PMON" supervisorctl restart xcvrd >/dev/null 2>&1
    wait_running || true
    _restored=1
  fi
}
trap restore EXIT   # safety net: never leave the Rust binary in pmon

inject() {
  # Crash-safe: never truncate xcvrd unless the backup is confirmed and the shim
  # is fully staged. Any failure (e.g. ENOSPC) aborts with xcvrd untouched.
  _LAP=$(_now_ms)
  docker cp "$STAGE/xcvrd-rs" "$PMON:$XRUST" || { echo "[dut] docker cp binary failed" >&2; return 1; }
  lap "docker cp binary"
  docker exec "$PMON" chmod +x "$XRUST" || return 1
  lap "chmod xcvrd-rs"
  # 1) back up the real xcvrd FIRST and verify the backup is non-empty.
  docker exec "$PMON" sh -c "[ -s $XBIN ] || exit 1; [ -e $XORIG ] || cp $XBIN $XORIG" \
      || { echo "[dut] backup of xcvrd failed" >&2; return 1; }
  lap "backup xcvrd"
  docker exec "$PMON" sh -c "[ -s $XORIG ]" || { echo "[dut] backup empty" >&2; return 1; }
  lap "verify backup"
  # 2) stage the shim to a temp file, verify it, then atomically move into place
  #    so a partial/ENOSPC write can never leave xcvrd truncated.
  docker exec -i "$PMON" sh -c "cat > $XBIN.new" <<'SHIM'
#!/usr/bin/env python3
import os
os.execv("/usr/local/bin/xcvrd-rs", ["xcvrd-rs"])
SHIM
  lap "write shim.new"
  docker exec "$PMON" sh -c "[ -s $XBIN.new ] && mv $XBIN.new $XBIN && chmod +x $XBIN" \
      || { echo "[dut] shim write failed; aborting" >&2; docker exec "$PMON" rm -f "$XBIN.new" 2>/dev/null; return 1; }
  lap "mv shim -> xcvrd"
  docker exec "$PMON" supervisorctl restart xcvrd >/dev/null 2>&1
  lap "supervisorctl restart"
  wait_running || echo "[dut] warning: xcvrd not RUNNING after restart" >&2
  lap "settle (poll RUNNING)"
}

echo "[dut] injecting Rust xcvrd for milestone $MILESTONE"
_PHASE=$(_now_ms)
if ! inject; then
  echo "[dut] INJECT FAILED — writing fail report, leaving Python xcvrd intact"
  python3 - "$MILESTONE" > "$REPORT" <<'PY'
import json, sys
print(json.dumps({"milestone": sys.argv[1], "passed": False, "tests": {},
                  "failures": [{"test": "<inject>",
                                "msg": "could not inject the Rust binary (see run log; e.g. no space on DUT)"}]}, indent=2))
PY
  cat "$REPORT"
  exit 1
fi
echo "[dut] xcvrd status after inject:"
printf '[dut][t] %-26s %6d ms  <== INJECT PHASE TOTAL\n' "inject() total" "$(( $(_now_ms) - _PHASE ))"
docker exec "$PMON" supervisorctl status xcvrd || true

# M0 is a DEPLOY-SMOKE gate: because the suite's clean-baseline requires a daemon
# that repopulates TRANSCEIVER_INFO, no pytest passes on a bare skeleton. So M0's
# gate is simply "the injected binary is RUNNING under supervisor" -- no pytest.
if [ "$MILESTONE" = "M0" ]; then
  sleep 2
  ST="$(docker exec "$PMON" supervisorctl status xcvrd | awk '{print $2}')"
  echo "[dut] M0 smoke: supervisor xcvrd status=$ST"
  echo "[dut] restoring Python xcvrd"
  restore
  docker exec "$PMON" supervisorctl status xcvrd || true
  python3 - "$MILESTONE" "$ST" > "$REPORT" <<'PY'
import json, sys
mid, st = sys.argv[1], sys.argv[2]
ok = (st == "RUNNING")
print(json.dumps({"milestone": mid, "passed": ok,
                  "tests": {"total": 1, "passed": 1 if ok else 0, "failed": 0 if ok else 1},
                  "failures": [] if ok else [{"test": "<smoke>",
                      "msg": f"injected xcvrd-rs not RUNNING (status={st})"}]}, indent=2))
PY
  echo "[dut] report:"; cat "$REPORT"
  exit 0
fi

echo "[dut] running xcvrd-tests (args_b64=${ARGS_B64:-<none>})"
# Explicit guard: the suite is shipped by run_validate.sh. If it is missing we'd
# otherwise fail deep inside with "No such file or directory" and then report a
# generic "<harness> results.xml parse failed", which hides the real cause.
if [ ! -f "$TESTS/run.sh" ]; then
  echo "[dut] xcvrd-tests missing at $TESTS -- restoring Python xcvrd and bailing" >&2
  restore
  python3 - "$MILESTONE" "$TESTS" > "$REPORT" <<'PY'
import json, sys
print(json.dumps({"milestone": sys.argv[1], "passed": False, "tests": {},
                  "failures": [{"test": "<harness>",
                                "msg": f"xcvrd-tests not present on the DUT at {sys.argv[2]}"}]}, indent=2))
PY
  cat "$REPORT"
  exit 1
fi
export PYTEST_ARGS_B64="$ARGS_B64"
_PHASE=$(_now_ms)
echo "==================== run.sh / pytest output ===================="
bash "$TESTS/run.sh" 2>&1 | tee "$STAGE/run.log"
RC=${PIPESTATUS[0]}
echo "================== end run.sh / pytest output =================="
printf '[dut][t] %-26s %6d ms  <== TESTS PHASE TOTAL\n' "run.sh total" "$(( $(_now_ms) - _PHASE ))"
cp "$TESTS/results.xml" "$STAGE/results.xml" 2>/dev/null || true
echo "[dut] run.sh exit=$RC"

echo "[dut] restoring Python xcvrd"
_PHASE=$(_now_ms)
restore
printf '[dut][t] %-26s %6d ms  <== RESTORE PHASE TOTAL\n' "restore total" "$(( $(_now_ms) - _PHASE ))"
echo "[dut] xcvrd status after restore:"
docker exec "$PMON" supervisorctl status xcvrd || true

# results.xml (JUnit) -> report.json. `passed` requires rc==0 AND zero
# failures/errors AND at least one test executed -- so a broken daemon or a
# broken harness both read as NOT passed.
python3 - "$MILESTONE" "$STAGE/results.xml" "$RC" > "$REPORT" <<'PY'
import json, sys, os, xml.etree.ElementTree as ET
mid, path, rc = sys.argv[1], sys.argv[2], int(sys.argv[3])
tot = fai = 0
failures = []
try:
    if not os.path.exists(path):
        raise FileNotFoundError(path)
    root = ET.parse(path).getroot()
    suites = [root] if root.tag == "testsuite" else root.findall(".//testsuite")
    for s in suites:
        tot += int(s.get("tests", 0))
        fai += int(s.get("failures", 0)) + int(s.get("errors", 0))
        for tc in s.findall("testcase"):
            bad = tc.find("failure")
            if bad is None:
                bad = tc.find("error")
            if bad is not None:
                failures.append({"test": tc.get("name"),
                                 "msg": (bad.get("message") or "")[:600]})
    passed = (rc == 0 and fai == 0 and tot > 0)
except Exception as e:  # noqa: BLE001
    passed = False
    failures.append({"test": "<harness>", "msg": f"results.xml parse failed: {e}"})
print(json.dumps({
    "milestone": mid,
    "passed": passed,
    "tests": {"total": tot, "passed": tot - fai, "failed": fai},
    "failures": failures,
}, indent=2))
PY

echo "[dut] report:"
cat "$REPORT"
