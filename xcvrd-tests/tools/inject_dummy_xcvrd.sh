#!/bin/bash
# inject_dummy_xcvrd.sh — negative control for the xcvrd black-box suite.
#
# Swaps the real xcvrd for a DUMMY that stays alive (so supervisor reports
# RUNNING) but does NOTHING: it never talks to the emulator, never handles
# change events, and never writes STATE_DB. Run the suite against it and it must
# FAIL — that proves the tests actually detect a non-functional xcvrd instead of
# passing on stale data. Then restore the real xcvrd.
#
# Runs ON the DUT (admin@vlab-01), where the pmon container lives.
#
# Usage:
#   ./inject_dummy_xcvrd.sh inject     # swap in the no-op xcvrd (default)
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
  echo "[dummy] writing no-op xcvrd (stays RUNNING, does nothing)"
  _in_pmon_i sh -c "cat > $XCVRD" <<'DUMMY'
#!/usr/bin/env python3
# DUMMY xcvrd (negative control): stay alive so supervisor reports RUNNING, but
# do NOTHING -- no emulator reads, no STATE_DB writes, no change-event handling.
import signal
import time

signal.signal(signal.SIGTERM, lambda *_: exit(0))
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
  echo "[dummy] INJECTED. Run the suite now — it must FAIL:"
  echo "        ./run.sh -m 'not slow'"
  echo "        (the clean baseline flushes TRANSCEIVER_* and requires xcvrd to"
  echo "         repopulate; the no-op can't, so the suite fails fast.)"
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
