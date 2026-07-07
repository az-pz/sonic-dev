#!/bin/bash
# Runs ON the DUT (admin@vlab-01). Deploys the xcvr-emu emulator as its OWN
# standalone Docker container and the sonic_platform bridge into pmon, then
# (re)registers xcvrd with pmon's supervisord.
#
#   * xcvr-emud runs as a standalone container (`docker run --network host
#     --restart unless-stopped`), NOT inside pmon. A plain docker container is
#     not a SONiC "feature", so the `config reload` that sonic-mgmt tests trigger
#     (which restarts pmon/swss/syncd/…) leaves the emulator RUNNING. xcvrd just
#     reconnects to it over gRPC at localhost:50051 (pmon and the emulator
#     container both share the host network namespace).
#   * only xcvrd runs INSIDE pmon, under supervisord (autorestart=true). The
#     bridge (sonic_platform + the xcvr_emu gRPC proto stubs it imports) lives in
#     /opt/xcvr-emu-bridge and is loaded via PYTHONPATH — baked into the
#     supervisord program's environment=, so pmon's dist-packages is never
#     modified and PYTHONPATH persists across every (re)launch.
#   * the supervisor drop-in goes to /etc/supervisor/conf.d/xcvr-emu.conf; pmon's
#     main supervisord includes conf.d/*.conf and only regenerates its own
#     supervisord.conf, so our drop-in survives a container restart.
#
# Expects on the DUT:
#   /home/admin/emu-bundle/            unpacked bundle (payload/, supervisor/, emu_config.yaml)
#   /home/admin/xcvr-emu-image.tar.gz  the emulator image tarball (docker save|gzip)
# Idempotent: safe to re-run.
#
# NB: a full pmon *recreation* (reboot / image change) wipes /opt and the drop-in;
# re-run this deploy after such an event. The emulator container itself survives
# (docker --restart unless-stopped) unless the whole docker/data dir is wiped.
set -uo pipefail

BUNDLE=/home/admin/emu-bundle
IMAGE_TAR=/home/admin/xcvr-emu-image.tar.gz
IMAGE_TAG=xcvr-emu:local
EMU_CTR=xcvr-emu                       # standalone emulator container name
EMU_CFG_HOST=/home/admin/emu_config.yaml   # stable path on the DUT host, bind-mounted into the container
PMON=pmon
OPT=/opt/xcvr-emu-bridge              # bridge lives here in pmon; loaded via PYTHONPATH
SUP_CONF=/etc/supervisor/conf.d/xcvr-emu.conf
EMU_ADDR=localhost:50051
EXPECT_SFPS=33
PYRUN="PYTHONPATH=$OPT XCVR_EMU_ADDR=$EMU_ADDR"

# --- 1. emulator: standalone container on the DUT ---------------------------
echo "[deploy] loading emulator image $IMAGE_TAG from $IMAGE_TAR"
[ -f "$IMAGE_TAR" ] || { echo "[deploy] ERROR: image tarball $IMAGE_TAR not found"; exit 1; }
gunzip -c "$IMAGE_TAR" | docker load

echo "[deploy] installing emu config to $EMU_CFG_HOST (bind-mounted into the container)"
cp "$BUNDLE/emu_config.yaml" "$EMU_CFG_HOST"

echo "[deploy] (re)starting standalone emulator container '$EMU_CTR' (--network host --restart unless-stopped)"
docker rm -f "$EMU_CTR" >/dev/null 2>&1 || true
docker run -d --name "$EMU_CTR" \
  --network host \
  --restart unless-stopped \
  -v "$EMU_CFG_HOST":/emu_config.yaml:ro \
  "$IMAGE_TAG" xcvr-emud -c /emu_config.yaml
echo "[deploy] emulator container status:"
docker ps --filter "name=^/${EMU_CTR}$" --format '  {{.Names}}  {{.Status}}  {{.Image}}'

# --- 2. bridge into pmon (PYTHONPATH — no dist-packages writes) --------------
echo "[deploy] installing bridge (sonic_platform + xcvr_emu proto stubs) into $PMON:$OPT"
docker exec "$PMON" rm -rf "$OPT"
docker exec "$PMON" mkdir -p "$OPT"
for pkg in sonic_platform xcvr_emu; do
  docker cp "$BUNDLE/payload/$pkg" "$PMON:$OPT/$pkg"
done

docker exec "$PMON" bash -c "$PYRUN python3 -c 'import grpc, sonic_platform.platform; from xcvr_emu.proto import emulator_pb2; print(\"[deploy] bridge imports OK\")'" \
  || { echo "[deploy] ERROR: bridge imports failed in pmon"; exit 1; }

# --- 3. register the xcvrd supervisord program (vanilla xcvrd + PYTHONPATH) --
# We do NOT pkill xcvrd and do NOT force a restart. `reread`+`update` starts
# xcvrd only if the program is newly added or its config actually changed; an
# already-running xcvrd with an unchanged config is left exactly as-is. The
# bridge is picked up purely via the PYTHONPATH exported in the program's
# environment= (see supervisor/xcvr-emu.conf) — the command is the stock
# /usr/local/bin/xcvrd, unwrapped.
#
# NOTE: because we never force a restart, updating the bridge code in /opt does
# NOT take effect in an already-running xcvrd until it restarts on its own
# (config reload / pmon restart). Restart it manually only if you want new
# bridge code loaded immediately: `docker exec pmon supervisorctl restart xcvrd`.
echo "[deploy] registering the xcvrd supervisord program (vanilla xcvrd + PYTHONPATH; no kill, no forced restart)"
docker cp "$BUNDLE/supervisor/xcvr-emu.conf" "$PMON:$SUP_CONF"
docker exec "$PMON" supervisorctl reread
docker exec "$PMON" supervisorctl update

echo "[deploy] supervisord program status:"
docker exec "$PMON" supervisorctl status xcvrd 2>&1 | sed 's/^/  /'

# --- 4. wait for the plant to come up ---------------------------------------
echo "[deploy] waiting until the bridge sees $EXPECT_SFPS modules (via the emulator container)..."
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
[ "$ok" = "1" ] || { echo "[deploy] ERROR: emulator not ready"; docker logs --tail 20 "$EMU_CTR" 2>&1; exit 1; }

echo "[deploy] waiting up to 120s for TRANSCEIVER_INFO + DOM to populate..."
for i in $(seq 1 24); do
  sleep 5
  ni=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l)
  nd=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l)
  echo "  [$((i*5))s] TRANSCEIVER_INFO=$ni  DOM_SENSOR=$nd"
  [ "$ni" -ge 28 ] && [ "$nd" -ge 28 ] && break
done

echo "===EMU_CONTAINER==="; docker ps --filter "name=^/${EMU_CTR}$" --format '{{.Names}} {{.Status}}'
echo "===SUPERVISOR_STATUS==="; docker exec "$PMON" supervisorctl status xcvrd 2>&1
echo "===INFO_COUNT==="; sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l
echo "===DOM_COUNT===";  sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l
echo "[deploy] done — emulator is a standalone container (survives config reload); xcvrd supervised in pmon"
