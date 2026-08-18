#!/bin/bash
# inject.sh {rust|python|status} -- swap the xcvrd daemon variant on the DUT.
#
# RUNS ON THE DUT. Supervisor runs /usr/local/bin/xcvrd; injecting means backing
# that up and dropping the Rust binary in its place. Written standalone rather than
# reusing the pipeline's control script so a benchmark change can never perturb the
# validator's inject/restore path.
#
# Crash-safe in the same way: the backup at $XORIG is the sole restore marker, so an
# interrupted run leaves a state that `inject.sh python` still recovers.
set -uo pipefail
PMON=pmon
XBIN=/usr/local/bin/xcvrd
XORIG=/usr/local/bin/xcvrd.pyorig
XRUST=/usr/local/bin/xcvrd-rs
STAGE=/tmp/xbench

wait_running() {
  for _ in $(seq 1 40); do
    [ "$(docker exec $PMON supervisorctl status xcvrd 2>/dev/null | awk '{print $2}')" = RUNNING ] && return 0
    sleep 0.3
  done
  return 1
}

# Supervisor runs `python3 /usr/local/bin/xcvrd --enable_sff_mgr`, so the Rust
# daemon cannot simply be copied over that path -- python3 would try to parse an ELF.
# It is installed as a python shim that execv's the binary, which is also how the
# variant is detected.
variant() {
  if docker exec $PMON sh -c "grep -q xcvrd-rs $XBIN" 2>/dev/null; then echo rust; else echo python; fi
}

case "${1:-status}" in
  status)
    echo "variant : $(variant)"
    docker exec $PMON supervisorctl status xcvrd
    docker exec $PMON sh -c "[ -e $XORIG ] && echo 'backup  : present (rust injected)' || echo 'backup  : absent (stock python)'"
    ;;
  rust)
    [ -f "$STAGE/xcvrd-rs" ] || { echo "[inject] no binary at $STAGE/xcvrd-rs" >&2; exit 2; }
    if [ "$(variant)" = rust ]; then echo "[inject] already rust"; exit 0; fi
    docker cp "$STAGE/xcvrd-rs" $PMON:$XRUST
    docker exec $PMON sh -c "chmod +x $XRUST; [ -s $XBIN ] || exit 1; [ -e $XORIG ] || cp $XBIN $XORIG" \
      || { echo "[inject] backup of the stock xcvrd failed - refusing to inject" >&2; exit 2; }
    # --enable_sff_mgr is baked into argv on purpose. execv replaces argv wholesale,
    # so supervisor's own flag is lost; and without it the Rust daemon RE-EXECS ITSELF
    # to add it (daemon.rs:190), which would put two process starts inside every
    # cold-start measurement and change the pid under the /proc sampler.
    docker exec -i $PMON sh -c "cat > $XBIN.new" <<'SHIM'
#!/usr/bin/env python3
import os
os.execv("/usr/local/bin/xcvrd-rs", ["xcvrd-rs", "--enable_sff_mgr"])
SHIM
    docker exec $PMON sh -c "[ -s $XBIN.new ] && mv $XBIN.new $XBIN && chmod +x $XBIN" \
      || { echo "[inject] shim write failed" >&2; docker exec $PMON rm -f $XBIN.new 2>/dev/null; exit 2; }
    docker exec $PMON supervisorctl restart xcvrd >/dev/null
    wait_running || { echo "[inject] rust xcvrd did not reach RUNNING" >&2; exit 3; }
    echo "[inject] rust injected"
    ;;
  python)
    if docker exec $PMON sh -c "[ -e $XORIG ]" 2>/dev/null; then
      docker exec $PMON sh -c "cp $XORIG $XBIN && rm -f $XORIG $XRUST"
      docker exec $PMON supervisorctl restart xcvrd >/dev/null
      wait_running || { echo "[inject] python xcvrd did not reach RUNNING" >&2; exit 3; }
      echo "[inject] restored python"
    else
      echo "[inject] already python"
    fi
    ;;
  *) echo "usage: inject.sh {rust|python|status}" >&2; exit 2 ;;
esac
