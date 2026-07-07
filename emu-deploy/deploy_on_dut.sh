#!/bin/bash
# Runs ON the DUT (admin@vlab-01). Deploys the xcvr-emu emulator + sonic_platform
# bridge into the pmon container and registers emud + xcvrd with pmon's
# supervisord so they AUTO-START and AUTO-RESTART — surviving the SONiC config
# reloads that sonic-mgmt tests trigger (which restart the pmon container).
#
#   * emud + xcvrd run INSIDE pmon under supervisord (autorestart=true).
#   * all our python (emulator xcvr_emu + cmis + the sonic_platform bridge) lives
#     in /opt/xcvr-emu-bridge and is loaded via PYTHONPATH — baked into each
#     supervisord program's environment=, so pmon's dist-packages is never
#     modified and PYTHONPATH persists across every (re)launch.
#   * the supervisor drop-in goes to /etc/supervisor/conf.d/xcvr-emu.conf; pmon's
#     main supervisord includes conf.d/*.conf and only regenerates its own
#     supervisord.conf, so our drop-in survives a container restart.
#
# Expects the bundle unpacked at /home/admin/emu-bundle/ (payload/, supervisor/,
# emu_config.yaml). Idempotent: safe to re-run.
#
# NB: a full container *recreation* (reboot / image change) wipes /opt and the
# drop-in; re-run this deploy after such an event.
set -uo pipefail

BUNDLE=/home/admin/emu-bundle
PMON=pmon
OPT=/opt/xcvr-emu-bridge              # our python lives here; loaded via PYTHONPATH
EMU_CFG=/etc/emu_config.yaml
SUP_CONF=/etc/supervisor/conf.d/xcvr-emu.conf
EMU_ADDR=localhost:50051
EXPECT_SFPS=33
PYRUN="PYTHONPATH=$OPT XCVR_EMU_ADDR=$EMU_ADDR"

echo "[deploy] installing bridge + emulator into $PMON:$OPT (PYTHONPATH — no dist-packages writes)"
docker exec "$PMON" rm -rf "$OPT"
docker exec "$PMON" mkdir -p "$OPT"
for pkg in sonic_platform xcvr_emu cmis; do
  docker cp "$BUNDLE/payload/$pkg" "$PMON:$OPT/$pkg"
done
docker cp "$BUNDLE/supervisor/start-xcvrd.sh" "$PMON:$OPT/start-xcvrd.sh"
docker exec "$PMON" chmod +x "$OPT/start-xcvrd.sh"
docker cp "$BUNDLE/emu_config.yaml" "$PMON:$EMU_CFG"

docker exec "$PMON" bash -c "$PYRUN python3 -c 'import grpc, sonic_platform.platform; from xcvr_emu import xcvr_emud; from xcvr_emu.proto import emulator_pb2; print(\"[deploy] imports OK\")'" \
  || { echo "[deploy] ERROR: bridge/emulator imports failed in pmon"; exit 1; }

echo "[deploy] stopping any manually-started emud/xcvrd from a previous run"
docker exec "$PMON" bash -c 'pkill -f xcvr_emu.xcvr_emud 2>/dev/null; pkill -x xcvrd 2>/dev/null; pkill -f start-xcvrd.sh 2>/dev/null; sleep 1; true'

echo "[deploy] installing supervisord drop-in $SUP_CONF and (re)starting programs"
docker cp "$BUNDLE/supervisor/xcvr-emu.conf" "$PMON:$SUP_CONF"
docker exec "$PMON" supervisorctl reread
docker exec "$PMON" supervisorctl update
# reread/update auto-starts new programs; restart to be explicit/idempotent
docker exec "$PMON" supervisorctl restart xcvr-emud xcvrd 2>/dev/null || true

echo "[deploy] supervisord program status:"
docker exec "$PMON" supervisorctl status xcvr-emud xcvrd 2>&1 | sed 's/^/  /'

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

echo "[deploy] waiting up to 120s for TRANSCEIVER_INFO + DOM to populate..."
for i in $(seq 1 24); do
  sleep 5
  ni=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l)
  nd=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l)
  echo "  [$((i*5))s] TRANSCEIVER_INFO=$ni  DOM_SENSOR=$nd"
  [ "$ni" -ge 28 ] && [ "$nd" -ge 28 ] && break
done

echo "===SUPERVISOR_STATUS==="; docker exec "$PMON" supervisorctl status xcvr-emud xcvrd 2>&1
echo "===INFO_COUNT==="; sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l
echo "===DOM_COUNT===";  sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l
echo "[deploy] done — emud + xcvrd are supervised (autorestart) and survive pmon restarts"
