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
#   inject <staged-binary>   back up python xcvrd, install a shim that execv's the
#                            rust binary, restart under supervisor, wait RUNNING
#   restore                  restore the python xcvrd (idempotent; safe to re-run)
#   status                   print the supervisor xcvrd status word
#
# Usage (on the DUT): bash rust_xcvrd_ctl.sh inject /home/admin/xcvrd-rs
set -uo pipefail

PMON="${PMON:-pmon}"
XBIN=/usr/local/bin/xcvrd            # what supervisor runs: python3 /usr/local/bin/xcvrd
XORIG=/usr/local/bin/xcvrd.pyorig    # backup of the real python xcvrd
XRUST=/usr/local/bin/xcvrd-rs        # the injected Rust binary

wait_running() {
  local i st
  for i in $(seq 1 20); do
    st=$(docker exec "$PMON" supervisorctl status xcvrd 2>/dev/null | awk '{print $2}')
    [ "$st" = "RUNNING" ] && return 0
    sleep 0.3
  done
  return 1
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
  local staged="${1:?usage: rust_xcvrd_ctl.sh inject <staged-binary>}"
  [ -s "$staged" ] || { echo "[rust-ctl] staged binary missing/empty: $staged" >&2; return 1; }
  # Crash-safe: never truncate xcvrd unless the backup is confirmed and the shim
  # is fully staged. Any failure (e.g. ENOSPC) aborts with xcvrd untouched.
  docker cp "$staged" "$PMON:$XRUST" || { echo "[rust-ctl] docker cp binary failed" >&2; return 1; }
  docker exec "$PMON" chmod +x "$XRUST" || return 1
  # 1) back up the real xcvrd FIRST and verify the backup is non-empty.
  docker exec "$PMON" sh -c "[ -s $XBIN ] || exit 1; [ -e $XORIG ] || cp $XBIN $XORIG" \
      || { echo "[rust-ctl] backup of xcvrd failed" >&2; return 1; }
  docker exec "$PMON" sh -c "[ -s $XORIG ]" || { echo "[rust-ctl] backup empty" >&2; return 1; }
  # 2) stage the shim to a temp file, verify it, then atomically move into place.
  docker exec -i "$PMON" sh -c "cat > $XBIN.new" <<'SHIM'
#!/usr/bin/env python3
import os
os.execv("/usr/local/bin/xcvrd-rs", ["xcvrd-rs"])
SHIM
  docker exec "$PMON" sh -c "[ -s $XBIN.new ] && mv $XBIN.new $XBIN && chmod +x $XBIN" \
      || { echo "[rust-ctl] shim write failed; aborting" >&2; docker exec "$PMON" rm -f "$XBIN.new" 2>/dev/null; return 1; }
  docker exec "$PMON" supervisorctl restart xcvrd >/dev/null 2>&1
  if wait_running; then
    echo "[rust-ctl] injected rust xcvrd; supervisor status: $(status | awk '{print $1, $2}')"
  else
    echo "[rust-ctl] warning: xcvrd not RUNNING after inject" >&2
    return 1
  fi
}

case "${1:-}" in
  inject)  inject "${2:-}" ;;
  restore) restore ;;
  status)  status ;;
  *) echo "usage: rust_xcvrd_ctl.sh {inject <binary>|restore|status}" >&2; exit 2 ;;
esac
