#!/bin/bash
# Runs ON the DUT (admin@vlab-01). Deploys the xcvr-emu emulator + sonic_platform
# bridge into the pmon container and starts xcvrd — all loaded via PYTHONPATH from
# a side directory, so pmon's dist-packages is NEVER modified.
#
#   * emud runs INSIDE pmon:  PYTHONPATH=/opt/xcvr-emu-bridge python3 -m xcvr_emu.xcvr_emud
#   * bridge is loaded from the same /opt dir via PYTHONPATH
#   * xcvrd runs in pmon with the same PYTHONPATH + XCVR_EMU_ADDR=localhost:50051
#
# Expects the bundle unpacked at /home/admin/emu-bundle/ (payload/ + emu_config.yaml).
# Idempotent: safe to re-run.
#
# NB: no `set -e` — daemon starts + polls can transiently return non-zero; the
# readiness gates below `exit 1` on real failure.
set -uo pipefail

BUNDLE=/home/admin/emu-bundle
PMON=pmon
OPT=/opt/xcvr-emu-bridge              # our python lives here; loaded via PYTHONPATH
EMU_CFG=/etc/emu_config.yaml
EMU_ADDR=localhost:50051
EXPECT_SFPS=33
PYRUN="PYTHONPATH=$OPT XCVR_EMU_ADDR=$EMU_ADDR"

echo "[deploy] installing bridge + emulator into $PMON:$OPT (PYTHONPATH — no dist-packages writes)"
docker exec "$PMON" rm -rf "$OPT"
docker exec "$PMON" mkdir -p "$OPT"
for pkg in sonic_platform xcvr_emu cmis; do
  docker cp "$BUNDLE/payload/$pkg" "$PMON:$OPT/$pkg"
done
docker cp "$BUNDLE/emu_config.yaml" "$PMON:$EMU_CFG"

docker exec "$PMON" bash -c "$PYRUN python3 -c 'import grpc, sonic_platform.platform; from xcvr_emu import xcvr_emud; from xcvr_emu.proto import emulator_pb2; print(\"[deploy] imports OK\")'" \
  || { echo "[deploy] ERROR: bridge/emulator imports failed in pmon"; exit 1; }

echo "[deploy] starting xcvr-emud INSIDE pmon (docker exec -d)"
docker exec "$PMON" bash -c 'pkill -f xcvr_emu.xcvr_emud 2>/dev/null; sleep 1; true'
docker exec -d "$PMON" bash -c "$PYRUN exec python3 -m xcvr_emu.xcvr_emud -c $EMU_CFG >/tmp/xcvr-emud.log 2>&1"
sleep 8   # emud registers all CMIS tables + N transceivers before it serves List()

echo "[deploy] waiting until the bridge sees $EXPECT_SFPS modules..."
ok=0
for i in $(seq 1 40); do
  sleep 2
  n=$(docker exec "$PMON" bash -c "$PYRUN python3 -c '
from sonic_platform.platform import Platform
try:
    print(Platform().get_chassis().get_num_sfps())
except Exception:
    print(0)
'" 2>/dev/null | tail -1)
  echo "  [$((i*2))s] num_sfps=$n"
  [ "$n" = "$EXPECT_SFPS" ] && { ok=1; break; }
done
[ "$ok" = "1" ] || { echo "[deploy] ERROR: emulator not ready"; docker exec "$PMON" tail -20 /tmp/xcvr-emud.log; exit 1; }

echo "[deploy] emulator ready. sample get_transceiver_info via bridge:"
docker exec "$PMON" bash -c "$PYRUN python3 -c '
from sonic_platform.platform import Platform
s = Platform().get_chassis().get_sfp(1)
info = s.get_xcvr_api().get_transceiver_info()
print(\"  present:\", s.get_presence(), \"vendor:\", (info or {}).get(\"manufacturer\"))
'"

echo "[deploy] starting xcvrd in pmon (PYTHONPATH=$OPT, XCVR_EMU_ADDR=$EMU_ADDR)"
docker exec "$PMON" bash -c 'pkill -x xcvrd 2>/dev/null; pkill -f xcvrd/xcvrd 2>/dev/null; sleep 1; true'
docker exec -d "$PMON" bash -c "$PYRUN exec xcvrd >/tmp/xcvrd.log 2>&1"

echo "[deploy] waiting up to 120s for TRANSCEIVER_INFO + DOM to populate..."
for i in $(seq 1 24); do
  sleep 5
  ni=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l)
  nd=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l)
  echo "  [$((i*5))s] TRANSCEIVER_INFO=$ni  DOM_SENSOR=$nd"
  [ "$ni" -ge 28 ] && [ "$nd" -ge 28 ] && break
done

echo "===EMUD_IN_PMON==="; docker exec "$PMON" bash -c 'ps aux | grep xcvr_emu.xcvr_emud | grep -v grep | head -1'
echo "===INFO_COUNT==="; sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l
echo "===DOM_COUNT===";  sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l
echo "[deploy] done"
