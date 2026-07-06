#!/bin/bash
# Runs ON the DUT (admin@vlab-01). Deploys the xcvr-emu emulator + sonic_platform
# bridge into pmon, starts xcvr-emud (detached), waits until the emulator reports
# all modules, then launches xcvrd (detached) so it populates TRANSCEIVER_INFO/DOM.
#
# Expects the bundle unpacked at /home/admin/emu-bundle/ (sonic_platform/,
# xcvr_emu/, cmis/, emu_config.yaml). Idempotent: safe to re-run.
set -e

BUNDLE=/home/admin/emu-bundle
PMON=pmon
DP=/usr/local/lib/python3.13/dist-packages
EXPECT_SFPS=33

echo "[deploy] installing python packages into $PMON:$DP"
for pkg in sonic_platform xcvr_emu cmis; do
  docker exec "$PMON" rm -rf "$DP/$pkg"
  docker cp "$BUNDLE/$pkg" "$PMON:$DP/$pkg"
done
docker cp "$BUNDLE/emu_config.yaml" "$PMON:/etc/emu_config.yaml"
docker exec "$PMON" python3 -c 'import grpc, sonic_platform.platform; from xcvr_emu.proto import emulator_pb2; print("[deploy] imports OK")'

echo "[deploy] starting xcvr-emud (docker exec -d)"
docker exec "$PMON" bash -c 'pkill -f xcvr_emu.xcvr_emud 2>/dev/null; sleep 1; true'
docker exec -d "$PMON" bash -c 'exec python3 -m xcvr_emu.xcvr_emud -c /etc/emu_config.yaml >/tmp/xcvr-emud.log 2>&1'

echo "[deploy] waiting until emulator reports $EXPECT_SFPS modules via the bridge..."
ok=0
for i in $(seq 1 30); do
  sleep 2
  n=$(docker exec "$PMON" python3 -c '
from sonic_platform.platform import Platform
try:
    print(Platform().get_chassis().get_num_sfps())
except Exception:
    print(0)
' 2>/dev/null | tail -1)
  echo "  [$((i*2))s] num_sfps=$n"
  if [ "$n" = "$EXPECT_SFPS" ]; then ok=1; break; fi
done
[ "$ok" = "1" ] || { echo "[deploy] ERROR: emulator not ready"; docker exec "$PMON" tail -20 /tmp/xcvr-emud.log; exit 1; }

echo "[deploy] emulator ready. sample get_transceiver_info via bridge:"
docker exec "$PMON" python3 -c '
from sonic_platform.platform import Platform
s = Platform().get_chassis().get_sfp(1)
info = s.get_xcvr_api().get_transceiver_info()
print("  present:", s.get_presence(), "model:", (info or {}).get("model"), "vendor:", (info or {}).get("manufacturer"))
'

echo "[deploy] starting xcvrd (docker exec -d)"
docker exec "$PMON" bash -c 'pkill -x xcvrd 2>/dev/null; pkill -f xcvrd/xcvrd 2>/dev/null; sleep 1; true'
docker exec -d "$PMON" bash -c 'exec xcvrd >/tmp/xcvrd.log 2>&1'

echo "[deploy] waiting up to 120s for TRANSCEIVER_INFO + DOM to populate..."
for i in $(seq 1 24); do
  sleep 5
  ni=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l)
  nd=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l)
  echo "  [$((i*5))s] TRANSCEIVER_INFO=$ni  DOM_SENSOR=$nd"
  [ "$ni" -ge 28 ] && [ "$nd" -ge 28 ] && break
done

echo "===XCVRD_LOG_TAIL==="
docker exec "$PMON" tail -25 /tmp/xcvrd.log 2>/dev/null || true
echo "===INFO_COUNT==="; sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l
echo "===DOM_COUNT===";  sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l
echo "===INFO_ETH4==="; sonic-db-cli STATE_DB HGETALL 'TRANSCEIVER_INFO|Ethernet4' 2>/dev/null
echo "[deploy] done"
