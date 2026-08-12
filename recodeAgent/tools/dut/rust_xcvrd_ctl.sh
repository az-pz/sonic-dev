#!/bin/bash
# rust_xcvrd_ctl.sh -- crash-safe inject / restore of a Rust xcvrd into pmon.
#
# Runs ON the DUT (admin@vlab-01) and operates on the local pmon container. It
# reversibly swaps a built Rust `xcvrd-rs` binary in for the supervised Python
# xcvrd, so a test suite (e.g. sonic-mgmt transceiver tests) can exercise the Rust
# daemon, then restores the Python xcvrd. The inject/restore logic mirrors the
# proven harness in recodeAgent/tools/dut/dut_validate.sh (backup-verify + atomic
# shim, so a partial/ENOSPC write can never truncate xcvrd).
#
# Verbs:
#   inject <staged-binary> [dom_update_interval]
#                            back up python xcvrd, install a shim that execv's the
#                            rust binary, then clean-baseline restart: flush stale
#                            TRANSCEIVER_* rows so the injected daemon MUST
#                            repopulate STATE_DB (stale python data can't mask it).
#                            The optional second arg is passed through to the Rust
#                            daemon as --dom_update_interval <secs> (see below)
#   inject-noop              negative control: install a NO-OP xcvrd (stays RUNNING
#                            but never writes STATE_DB) + same clean baseline, so
#                            xcvrd-dependent tests MUST fail (proves they have teeth)
#   restore                  restore the python xcvrd (idempotent; safe to re-run)
#   status                   report which xcvrd is running (python vs rust) + markers
#
# Usage (on the DUT): bash rust_xcvrd_ctl.sh inject /home/admin/xcvrd-rs
#                     bash rust_xcvrd_ctl.sh inject /home/admin/xcvrd-rs 5
#
# DOM poll interval: xcvrd's DOM sensor loop defaults to 60 s, so after the
# clean-baseline flush TRANSCEIVER_DOM_SENSOR / TRANSCEIVER_STATUS stay EMPTY for
# up to a minute (TRANSCEIVER_INFO and _DOM_THRESHOLD are written during per-port
# init and appear immediately). Passing a smaller interval makes the DOM-backed
# tables appear sooner, which both speeds up DOM-cadence tests and shrinks the
# window in which a test can race the first poll. It is OPT-IN: with no value the
# daemon keeps the upstream 60 s default, so the Rust port is never silently
# graded under non-default timing. The Python xcvrd takes the same flag, so the
# value means the same thing for both daemons.
#
# Readiness: supervisor reports RUNNING the moment the process exists, which is
# earlier than the daemon being usable. inject therefore settles for
# RUST_SETTLE_SECS (default 15) after start, before anything asserts on
# STATE_DB. Set RUST_SETTLE_SECS=0 to skip it.
set -uo pipefail

PMON="${PMON:-pmon}"
XBIN=/usr/local/bin/xcvrd            # what supervisor runs: python3 /usr/local/bin/xcvrd
XORIG=/usr/local/bin/xcvrd.pyorig    # backup of the real python xcvrd
XRUST=/usr/local/bin/xcvrd-rs        # the injected Rust binary

# Seconds to let a freshly-started Rust daemon settle before anything reads
# STATE_DB. supervisorctl reports RUNNING as soon as the process exists, which is
# well before xcvrd-rs has opened its DB connections, enumerated the ports and
# completed its first pass -- so "RUNNING" alone is not "ready", and a test that
# starts immediately can read a half-populated STATE_DB. Override with
# RUST_SETTLE_SECS (0 disables).
RUST_SETTLE_SECS="${RUST_SETTLE_SECS:-15}"

wait_running() {
  local i st
  for i in $(seq 1 20); do
    st=$(docker exec "$PMON" supervisorctl status xcvrd 2>/dev/null | awk '{print $2}')
    [ "$st" = "RUNNING" ] && return 0
    sleep 0.3
  done
  return 1
}

# Clean baseline: delete every TRANSCEIVER_* row so a freshly (re)started daemon
# must repopulate STATE_DB from scratch. Without this, stale Python-written rows
# survive an xcvrd restart and can make STATE_DB-reading tests false-pass even if
# the injected daemon does nothing. STATE_DB is reached via sonic-db-cli on the
# DUT host (mirrors recodeAgent xcvrd-tests' flush_transceiver_tables).
flush_baseline() {
  local keys n
  keys=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_*' 2>/dev/null)
  n=$(printf '%s' "$keys" | grep -c .)
  if [ "$n" -gt 0 ]; then
    # shellcheck disable=SC2086
    sonic-db-cli STATE_DB DEL $keys >/dev/null 2>&1
  fi
  echo "[rust-ctl] flushed $n stale TRANSCEIVER_* row(s) from STATE_DB"
}

# Bounded, NON-fatal wait for the just-started daemon to repopulate
# TRANSCEIVER_INFO. A correct daemon fills it within a few seconds; a no-op never
# does -- which is exactly what the negative control demonstrates via test FAILs.
wait_repopulate() {
  local i n
  for i in $(seq 1 20); do
    n=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | grep -c .)
    [ "${n:-0}" -gt 0 ] && { echo "[rust-ctl] TRANSCEIVER_INFO repopulated ($n port(s))"; return 0; }
    sleep 1
  done
  echo "[rust-ctl] note: TRANSCEIVER_INFO still empty after wait (daemon not populating?)" >&2
  return 0
}

status() {
  # Read-only: report which xcvrd pmon is currently running (stock PYTHON vs an
  # injected RUST xcvrd-rs), the supervisor state, the actually-running process
  # image, and the inject/backup markers. Touches nothing.
  if ! docker exec "$PMON" true 2>/dev/null; then
    echo "[xcvrd] pmon container not available (is the DUT up?)"; return 0
  fi
  local sup pid fl rs bk exe cmd run
  sup=$(docker exec "$PMON" supervisorctl status xcvrd 2>/dev/null)
  pid=$(printf '%s\n' "$sup" | sed -n 's/.*pid \([0-9][0-9]*\).*/\1/p')
  docker exec "$PMON" sh -c "[ -e $XRUST ]" 2>/dev/null && rs=present || rs=none
  docker exec "$PMON" sh -c "[ -e $XORIG ]" 2>/dev/null && bk=present || bk=none
  # The injected shim execs /usr/local/bin/xcvrd-rs; stock python xcvrd never
  # mentions it. The backup (xcvrd.pyorig) only exists while Rust is injected.
  if docker exec "$PMON" grep -q xcvrd-rs "$XBIN" 2>/dev/null; then
    fl="RUST (xcvrd-rs)"
  elif [ "$rs" = present ] && [ "$bk" = present ]; then
    fl="RUST (xcvrd-rs)"
  else
    fl="PYTHON (stock)"
  fi
  # Confirm against the actually-running process image (robust mid-restart).
  run="n/a (not running)"
  if [ -n "$pid" ]; then
    exe=$(docker exec "$PMON" readlink -f "/proc/$pid/exe" 2>/dev/null)
    cmd=$(docker exec "$PMON" cat "/proc/$pid/cmdline" 2>/dev/null | tr '\0' ' ')
    case "$exe $cmd" in
      *xcvrd-rs*) run="xcvrd-rs (native binary)" ;;
      *python*)   run="python (interpreter)" ;;
      *)          run="${exe:-unknown}" ;;
    esac
  fi
  echo "[xcvrd] flavor     : $fl"
  echo "[xcvrd] supervisor : ${sup:-<xcvrd not found in pmon>}"
  echo "[xcvrd] running    : $run"
  echo "[xcvrd] markers    : xcvrd-rs=$rs  py-backup=$bk  (py-backup present => Rust injected)"
}

restore() {
  # Idempotent: only acts while the backup exists.
  if docker exec "$PMON" sh -c "[ -e $XORIG ]" 2>/dev/null; then
    docker exec "$PMON" sh -c "cp $XORIG $XBIN && rm -f $XORIG $XRUST"
    docker exec "$PMON" supervisorctl restart xcvrd >/dev/null 2>&1
    wait_running || true
    echo "[rust-ctl] restored python xcvrd"
  else
    echo "[rust-ctl] nothing to restore (no backup present)"
  fi
}

inject() {
  local staged="${1:?usage: rust_xcvrd_ctl.sh inject <staged-binary> [dom_update_interval]}"
  local ival="${2:-}"
  [ -s "$staged" ] || { echo "[rust-ctl] staged binary missing/empty: $staged" >&2; return 1; }
  # Validate here rather than letting a typo reach the shim: a bad value would
  # make the daemon reject its own argv and fail to start, which looks like a
  # broken Rust build instead of a bad argument.
  if [ -n "$ival" ]; then
    case "$ival" in
      ''|*[!0-9]*) echo "[rust-ctl] dom_update_interval must be a non-negative integer (got '$ival')" >&2; return 1 ;;
    esac
  fi
  # Crash-safe: never truncate xcvrd unless the backup is confirmed and the shim
  # is fully staged. Any failure (e.g. ENOSPC) aborts with xcvrd untouched.
  docker cp "$staged" "$PMON:$XRUST" || { echo "[rust-ctl] docker cp binary failed" >&2; return 1; }
  docker exec "$PMON" chmod +x "$XRUST" || return 1
  _backup_and_shim "$ival" || return 1
  _restart_clean "$RUST_SETTLE_SECS"
  echo "[rust-ctl] injected rust xcvrd${ival:+ (--dom_update_interval=$ival)} (clean baseline); status: $(sup_word)"
}

inject_noop() {
  # Negative control: install a NO-OP xcvrd-rs that stays RUNNING under supervisor
  # but never touches STATE_DB. With the clean-baseline flush, every xcvrd-dependent
  # test MUST fail -- proof the suite actually exercises the daemon (not stale data
  # or platform-only code paths). Uses the same crash-safe backup+shim as inject.
  docker exec -i "$PMON" sh -c "cat > $XRUST" <<'NOOP'
#!/bin/sh
# no-op xcvrd-rs (negative control): stay alive for supervisor, write nothing.
trap 'exit 0' TERM INT
while true; do sleep 3600; done
NOOP
  docker exec "$PMON" sh -c "[ -s $XRUST ] && chmod +x $XRUST" \
      || { echo "[rust-ctl] no-op write failed" >&2; return 1; }
  _backup_and_shim || return 1
  # No settle: the no-op deliberately never writes STATE_DB, so there is nothing
  # to wait for and the negative control stays fast.
  _restart_clean 0
  echo "[rust-ctl] injected NO-OP xcvrd (negative control, clean baseline); status: $(sup_word)"
}

# --- shared inject internals ------------------------------------------------
sup_word() { docker exec "$PMON" supervisorctl status xcvrd 2>/dev/null | awk '{print $1, $2}'; }

_backup_and_shim() {
  # $1 = optional dom_update_interval (seconds) to hand the Rust daemon.
  local ival="${1:-}"
  # 1) back up the real xcvrd FIRST and verify the backup is non-empty.
  docker exec "$PMON" sh -c "[ -s $XBIN ] || exit 1; [ -e $XORIG ] || cp $XBIN $XORIG" \
      || { echo "[rust-ctl] backup of xcvrd failed" >&2; return 1; }
  docker exec "$PMON" sh -c "[ -s $XORIG ]" || { echo "[rust-ctl] backup empty" >&2; return 1; }
  # 2) build the shim's argv. The daemon reads its options from argv (not the
  # environment), and execv's argv[0] is the program name, so any flag has to be
  # baked in here -- supervisor invokes $XBIN with no arguments.
  local shim_args='"xcvrd-rs"'
  if [ -n "$ival" ]; then
    shim_args="$shim_args, \"--dom_update_interval\", \"$ival\""
  fi
  # 3) stage the shim to a temp file, verify it, then atomically move into place.
  docker exec -i "$PMON" sh -c "cat > $XBIN.new" <<SHIM
#!/usr/bin/env python3
import os
os.execv("/usr/local/bin/xcvrd-rs", [$shim_args])
SHIM
  docker exec "$PMON" sh -c "[ -s $XBIN.new ] && mv $XBIN.new $XBIN && chmod +x $XBIN" \
      || { echo "[rust-ctl] shim write failed; aborting" >&2; docker exec "$PMON" rm -f "$XBIN.new" 2>/dev/null; return 1; }
}

_restart_clean() {
  # $1 = seconds to settle after the daemon reports RUNNING (0 = don't).
  local settle="${1:-0}"
  # Clean-baseline restart: stop the daemon, flush stale TRANSCEIVER_* rows while
  # nothing is writing, start fresh, then (soft) verify repopulation. Stopping
  # first guarantees no daemon can re-add rows between flush and start.
  docker exec "$PMON" supervisorctl stop xcvrd >/dev/null 2>&1
  flush_baseline
  docker exec "$PMON" supervisorctl start xcvrd >/dev/null 2>&1
  wait_running || echo "[rust-ctl] warning: xcvrd not RUNNING after start" >&2
  # Settle AFTER the process is up but BEFORE we assert on STATE_DB, so the
  # daemon gets a head start on its first pass instead of being raced.
  if [ "$settle" -gt 0 ] 2>/dev/null; then
    echo "[rust-ctl] settling ${settle}s for the daemon to come up ..."
    sleep "$settle"
  fi
  wait_repopulate
}

case "${1:-}" in
  inject)      inject "${2:-}" "${3:-}" ;;
  inject-noop) inject_noop ;;
  restore)     restore ;;
  status)      status ;;
  *) echo "usage: rust_xcvrd_ctl.sh {inject <binary>|inject-noop|restore|status}" >&2; exit 2 ;;
esac
