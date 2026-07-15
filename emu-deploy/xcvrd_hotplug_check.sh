#!/bin/bash
# xcvrd hotplug check
# ------------------
# Unplug a transceiver in the xcvr-emu emulator, assert that xcvrd clears its
# TRANSCEIVER_INFO from STATE_DB, then replug and assert it is restored. This
# exercises the bridge's get_change_event -> xcvrd SfpStateUpdateTask path.
#
# Run ON the DUT (admin@vlab-01); requires the emulator + sonic_platform bridge
# deployed (i.e. after `setup-sonic-testbed.sh emulator`).
#
# Usage:
#   ./xcvrd_hotplug_check.sh [PORT]            # e.g. Ethernet100 (default)
#   ./xcvrd_hotplug_check.sh --index N         # emulator module index N
# Env: EMU_CTR (default xcvr-emu), TIMEOUT secs (default 30).
# Exit: 0 = pass, 1 = assertion failed, 2 = setup/precondition error.
set -u
if [ "${1:-}" = "--index" ]; then IDX="${2:?index}"; PORT="Ethernet$((IDX * 4))"
else PORT="${1:-Ethernet100}"; IDX=$(( ${PORT#Ethernet} / 4 )); fi
EMU_CTR="${EMU_CTR:-xcvr-emu}"
TIMEOUT="${TIMEOUT:-30}"

emu_set() {  # $1 = 1(insert)|0(remove); prints emulator 'present' afterwards
  docker exec -i "$EMU_CTR" python3 - "$IDX" "$1" <<'PY'
import os, sys, grpc
from xcvr_emu.proto import emulator_pb2 as pb
sys.path.append(os.path.dirname(pb.__file__))
from xcvr_emu.proto import emulator_pb2_grpc as pbg
s = pbg.SfpEmulatorServiceStub(grpc.insecure_channel("localhost:50051"))
s.UpdateInfo(pb.UpdateInfoRequest(index=int(sys.argv[1]), present=sys.argv[2] == "1"))
print(s.GetInfo(pb.GetInfoRequest(index=int(sys.argv[1]))).present)
PY
}
# populated == TRANSCEIVER_INFO has a non-empty manufacturer (real optic identity)
is_populated() { [ -n "$(sonic-db-cli STATE_DB HGET "TRANSCEIVER_INFO|$PORT" manufacturer 2>/dev/null)" ]; }
wait_for() {  # $1 = populated|cleared
  for _ in $(seq 1 "$TIMEOUT"); do
    if [ "$1" = populated ]; then is_populated && return 0; else is_populated || return 0; fi
    sleep 1
  done
  return 1
}

echo "[hotplug] target $PORT (emulator index $IDX), timeout ${TIMEOUT}s"
emu_set 1 >/dev/null
wait_for populated || { echo "[hotplug] SETUP FAIL: $PORT never populated"; exit 2; }
echo "[hotplug] baseline manufacturer=$(sonic-db-cli STATE_DB HGET "TRANSCEIVER_INFO|$PORT" manufacturer)"

rc=0
echo "[hotplug] UNPLUG $PORT"
[ "$(emu_set 0)" = "False" ] || { echo "[hotplug] FAIL: emulator did not report absent"; exit 2; }
if wait_for cleared; then echo "[hotplug] PASS: xcvrd cleared TRANSCEIVER_INFO|$PORT on unplug"
else echo "[hotplug] FAIL: TRANSCEIVER_INFO|$PORT still populated after ${TIMEOUT}s"; rc=1; fi

echo "[hotplug] REPLUG $PORT"
emu_set 1 >/dev/null
if wait_for populated; then echo "[hotplug] PASS: xcvrd restored TRANSCEIVER_INFO|$PORT on replug"
else echo "[hotplug] FAIL: $PORT not restored after ${TIMEOUT}s"; rc=1; fi

[ "$rc" = 0 ] && echo "[hotplug] RESULT: PASS" || echo "[hotplug] RESULT: FAIL"
exit "$rc"
