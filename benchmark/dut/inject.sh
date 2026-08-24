#!/bin/bash
# inject.sh {rust|python|restore|status} [dom_interval] -- select the xcvrd variant.
#
# RUNS ON THE DUT. Supervisor runs `python3 /usr/local/bin/xcvrd --enable_sff_mgr`,
# so BOTH variants are installed as a python shim at that path which execv's the real
# daemon with an explicit argv. Written standalone rather than reusing the pipeline's
# control script so a benchmark change can never perturb the validator's inject path.
#
# WHY PYTHON IS ALSO SHIMMED. The reference daemon defaults to a 60s DOM interval and
# takes it only from argv, so leaving it stock would compare a 60s Python against a 5s
# Rust -- an order of magnitude apart in polling work, which is most of what these
# benchmarks measure. Both variants therefore get the SAME --dom_update_interval, and
# the value used is recorded in the run's JSON.
#
# Crash-safe: $XORIG holds the pristine python daemon and is the sole restore marker,
# so an interrupted run always leaves a state `inject.sh restore` can recover.
set -uo pipefail
PMON=pmon
XBIN=/usr/local/bin/xcvrd
XORIG=/usr/local/bin/xcvrd.pyorig
XRUST=/usr/local/bin/xcvrd-rs
STAGE=/tmp/xbench

ACTION="${1:-status}"
IVAL="${2:-}"

cur_pid() { docker exec $PMON supervisorctl status xcvrd 2>/dev/null | grep -o 'pid [0-9]*' | awk '{print $2}'; }

# Wait for RUNNING **with a pid different from $1**. Waiting only for RUNNING is a
# race: supervisorctl keeps reporting the OUTGOING process as RUNNING for a moment
# after `restart`, so a benchmark that starts measuring immediately can sample the
# daemon it just replaced. Observed live -- restore returned on the first poll while
# the previous daemon was still the one running.
wait_running() {
  local was="${1:-}" i st pid
  for i in $(seq 1 60); do
    st="$(docker exec $PMON supervisorctl status xcvrd 2>/dev/null | awk '{print $2}')"
    pid="$(cur_pid)"
    if [ "$st" = RUNNING ] && [ -n "$pid" ] && [ "$pid" != "$was" ]; then
      # Give the new process a moment to get past import/bootstrap before anyone
      # samples it, so early CPU is not attributed to steady state.
      sleep 1
      return 0
    fi
    case "$st" in FATAL|BACKOFF) echo "[inject] xcvrd entered $st" >&2; return 1 ;; esac
    sleep 0.3
  done
  return 1
}

variant() {
  if docker exec $PMON sh -c "grep -q xcvrd-rs $XBIN" 2>/dev/null; then echo rust
  elif docker exec $PMON sh -c "grep -q xcvrd.pyorig $XBIN" 2>/dev/null; then echo python
  else echo stock; fi
}

# Back up the pristine daemon exactly once. Refuses if $XBIN is already a shim, so a
# crashed run can never overwrite the only good copy with a shim.
ensure_backup() {
  if docker exec $PMON sh -c "[ -e $XORIG ]" 2>/dev/null; then return 0; fi
  if docker exec $PMON sh -c "grep -q 'os.execv' $XBIN" 2>/dev/null; then
    echo "[inject] $XBIN is a shim but $XORIG is missing - cannot recover the original" >&2
    return 1
  fi
  docker exec $PMON sh -c "[ -s $XBIN ] && cp $XBIN $XORIG && [ -s $XORIG ]" \
    || { echo "[inject] backup of the stock xcvrd failed" >&2; return 1; }
}

# Write the shim atomically: stage, verify non-empty, then move. A half-written $XBIN
# would leave the daemon unstartable with no backup path.
install_shim() {  # $1 = python source
  printf '%s' "$1" | docker exec -i $PMON sh -c "cat > $XBIN.new" || return 1
  docker exec $PMON sh -c "[ -s $XBIN.new ] && mv $XBIN.new $XBIN && chmod +x $XBIN" \
    || { docker exec $PMON rm -f $XBIN.new 2>/dev/null; return 1; }
}

# argv fragment shared by both variants.
args_py() {
  local a='"--enable_sff_mgr"'
  [ -n "$IVAL" ] && a="$a, \"--dom_update_interval\", \"$IVAL\""
  echo "$a"
}

restart_and_verify() {  # $1 = label
  local was; was="$(cur_pid)"
  docker exec $PMON supervisorctl restart xcvrd >/dev/null
  wait_running "$was" || { echo "[inject] $1 xcvrd did not reach RUNNING" >&2; return 3; }
  # Confirm the interval actually reached the process rather than trusting the shim.
  # A silently-ignored flag would make the two variants incomparable while looking fine.
  if [ -n "$IVAL" ]; then
    local pid cmd
    pid="$(docker exec $PMON supervisorctl status xcvrd 2>/dev/null | grep -o 'pid [0-9]*' | awk '{print $2}')"
    cmd="$(docker exec $PMON sh -c "tr '\0' ' ' < /proc/$pid/cmdline" 2>/dev/null)"
    case "$cmd" in
      *"--dom_update_interval $IVAL"*) : ;;
      *) echo "[inject] WARNING: dom_update_interval=$IVAL not visible in the running argv:" >&2
         echo "[inject]   $cmd" >&2 ;;
    esac
  fi
  echo "[inject] $1 active${IVAL:+ (dom_update_interval=$IVAL)}"
}

case "$ACTION" in
  status)
    echo "variant : $(variant)"
    docker exec $PMON supervisorctl status xcvrd
    docker exec $PMON sh -c "[ -e $XORIG ] && echo 'backup  : present' || echo 'backup  : absent (stock)'"
    pid="$(docker exec $PMON supervisorctl status xcvrd 2>/dev/null | grep -o 'pid [0-9]*' | awk '{print $2}')"
    [ -n "$pid" ] && echo "argv    : $(docker exec $PMON sh -c "tr '\0' ' ' < /proc/$pid/cmdline" 2>/dev/null)"
    ;;

  rust)
    [ -f "$STAGE/xcvrd-rs" ] || { echo "[inject] no binary at $STAGE/xcvrd-rs" >&2; exit 2; }
    docker cp "$STAGE/xcvrd-rs" $PMON:$XRUST >/dev/null
    docker exec $PMON chmod +x $XRUST
    ensure_backup || exit 2
    # --enable_sff_mgr is baked into argv on purpose: execv replaces argv wholesale so
    # supervisor's own flag is lost, and without it the Rust daemon RE-EXECS ITSELF to
    # add it (daemon.rs:190) -- two process starts inside every cold-start measurement,
    # and a pid that moves out from under the /proc sampler.
    install_shim "#!/usr/bin/env python3
import os
os.execv(\"$XRUST\", [\"xcvrd-rs\", $(args_py)])
" || { echo "[inject] shim write failed" >&2; exit 2; }
    restart_and_verify rust || exit 3
    ;;

  python)
    ensure_backup || exit 2
    # The reference daemon, re-invoked from the preserved copy with the same argv the
    # Rust variant gets.
    install_shim "#!/usr/bin/env python3
import os, sys
os.execv(sys.executable, [sys.executable, \"$XORIG\", $(args_py)])
" || { echo "[inject] shim write failed" >&2; exit 2; }
    restart_and_verify python || exit 3
    ;;

  restore)
    if docker exec $PMON sh -c "[ -e $XORIG ]" 2>/dev/null; then
      local was; was="$(cur_pid)"
      docker exec $PMON sh -c "cp $XORIG $XBIN && rm -f $XORIG $XRUST" \
        || { echo "[inject] restore FAILED - the DUT is still injected" >&2; exit 3; }
      docker exec $PMON supervisorctl restart xcvrd >/dev/null
      wait_running "$was" || { echo "[inject] stock xcvrd did not reach RUNNING" >&2; exit 3; }
      # Verify rather than assume: a silently-failed restore leaves the next run, and
      # every later user of this testbed, measuring an injected daemon.
      docker exec $PMON sh -c "grep -q 'os.execv' $XBIN" 2>/dev/null \
        && { echo "[inject] restore did not take - $XBIN is still a shim" >&2; exit 3; }
      echo "[inject] restored stock python xcvrd"
    else
      echo "[inject] already stock"
    fi
    ;;

  *) echo "usage: inject.sh {rust|python|restore|status} [dom_interval]" >&2; exit 2 ;;
esac
