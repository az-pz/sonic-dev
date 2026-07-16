#!/bin/bash
# inject_dummy_xcvrd.sh — negative control for the xcvrd black-box suite.
#
# Swaps the real xcvrd for a DUMMY "fake-healthy" daemon: it writes BOGUS
# TRANSCEIVER_INFO for every port (just enough for the suite's clean baseline to
# accept it as live) and then does NOTHING else — no emulator reads, no presence
# handling, no DOM, no CMIS state, no Monitor traffic. Because the baseline now
# passes, the real test bodies actually RUN and then FAIL on their content and
# behaviour assertions — proving the individual tests catch a broken xcvrd, not
# just the session-level baseline guard. Then restore the real xcvrd.
#
# Runs ON the DUT (admin@vlab-01), where the pmon container lives.
#
# Usage:
#   ./inject_dummy_xcvrd.sh inject     # swap in the fake-healthy xcvrd (default)
#   ./inject_dummy_xcvrd.sh restore    # put the real xcvrd back
#   ./inject_dummy_xcvrd.sh status     # show which xcvrd is active
set -uo pipefail
PMON=pmon
XCVRD=/usr/local/bin/xcvrd
BACKUP=/usr/local/bin/xcvrd.real
MARK=/usr/local/bin/.xcvrd_is_dummy

_in_pmon()   { docker exec "$PMON" "$@"; }
_in_pmon_i() { docker exec -i "$PMON" "$@"; }  # -i so the heredoc reaches the container

inject() {
  if _in_pmon test -e "$MARK"; then
    echo "[dummy] already injected — xcvrd is the no-op. (restore first to re-inject)"
    return 0
  fi
  echo "[dummy] backing up the real xcvrd -> $BACKUP"
  _in_pmon cp "$XCVRD" "$BACKUP"
  echo "[dummy] writing fake-healthy dummy xcvrd (bogus INFO only, no real behaviour)"
  _in_pmon_i sh -c "cat > $XCVRD" <<'DUMMY'
#!/usr/bin/env python3
# DUMMY xcvrd (negative control, "fake-healthy" variant).
#
# Writes BOGUS TRANSCEIVER_INFO for every emulator port so the suite's clean
# baseline -- which only checks that the probe port's manufacturer is non-empty
# -- is satisfied and lets the real test bodies RUN. The dummy then does NOTHING
# else: no emulator reads, no presence handling, no DOM, no CMIS state, no
# Monitor traffic. So every content/behaviour assertion FAILS, proving the
# individual tests catch a broken xcvrd, not just the session-level baseline.
import signal
import subprocess
import time

CLI = "/usr/bin/sonic-db-cli"
FAKE_INFO = {
    "manufacturer": "DUMMY",
    "model": "DUMMY-XCVR",
    "serial": "DUMMYSN0",
    "vendor_rev": "00",
    "vendor_oui": "00-00-00",
    "type": "DUMMYTYPE",
    "ext_identifier": "Power Class 1 (DUMMY)",
}


def populate():
    # emulator modules 0..32 -> Ethernet0..Ethernet128 (index * 4)
    for idx in range(0, 33):
        key = "TRANSCEIVER_INFO|Ethernet%d" % (idx * 4)
        args = [CLI, "STATE_DB", "HSET", key]
        for f, v in FAKE_INFO.items():
            args += [f, v]
        subprocess.run(args, capture_output=True, text=True)


signal.signal(signal.SIGTERM, lambda *_: exit(0))
populate()
while True:
    time.sleep(3600)
DUMMY
  _in_pmon chmod +x "$XCVRD"
  _in_pmon touch "$MARK"
  echo "[dummy] restarting xcvrd (now the no-op)"
  _in_pmon supervisorctl restart xcvrd >/dev/null
  sleep 3
  _in_pmon supervisorctl status xcvrd
  echo
  echo "[dummy] INJECTED. Run the suite now — the real tests should RUN and FAIL:"
  echo "        ./run.sh -m 'not slow'"
  echo "        (the dummy fakes TRANSCEIVER_INFO so the clean baseline passes;"
  echo "         the content/behaviour tests then fail on their real assertions."
  echo "         Expect test_xcvrd_running to PASS and the rest to FAIL.)"
  echo "[dummy] restore with: ./inject_dummy_xcvrd.sh restore"
}

restore() {
  if ! _in_pmon test -e "$BACKUP"; then
    echo "[dummy] no backup at $BACKUP — nothing to restore (is xcvrd already real?)"
    return 1
  fi
  echo "[dummy] restoring the real xcvrd"
  _in_pmon cp "$BACKUP" "$XCVRD"
  _in_pmon rm -f "$BACKUP" "$MARK"
  _in_pmon supervisorctl restart xcvrd >/dev/null
  sleep 3
  _in_pmon supervisorctl status xcvrd
  echo "[dummy] RESTORED. The suite should pass again."
}

status() {
  if _in_pmon test -e "$MARK"; then
    echo "xcvrd = DUMMY (no-op negative control)"
  else
    echo "xcvrd = REAL"
  fi
  _in_pmon supervisorctl status xcvrd
}

case "${1:-inject}" in
  inject)  inject ;;
  restore) restore ;;
  status)  status ;;
  *) echo "usage: $0 {inject|restore|status}"; exit 2 ;;
esac
