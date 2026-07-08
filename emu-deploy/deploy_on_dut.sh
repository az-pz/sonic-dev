#!/bin/bash
# Runs ON the DUT (admin@vlab-01). Deploys the xcvr-emu emulator as its OWN
# standalone Docker container and the sonic_platform bridge into pmon, then
# launches xcvrd directly with the needed environment exported (no supervisord).
#
#   * xcvr-emud runs as a standalone container (`docker run --network host
#     --restart unless-stopped`), NOT inside pmon. A plain docker container is
#     not a SONiC "feature", so the `config reload` that sonic-mgmt tests trigger
#     (which restarts pmon/swss/syncd/…) leaves the emulator RUNNING. xcvrd just
#     reconnects to it over gRPC at localhost:50051 (pmon and the emulator
#     container both share the host network namespace).
#   * xcvrd is launched INSIDE pmon via `docker exec -d` with PYTHONPATH and
#     XCVR_EMU_ADDR exported into its environment, so it imports the bridge
#     (sonic_platform + the xcvr_emu gRPC proto stubs) from /opt/xcvr-emu-bridge
#     without touching pmon's system dist-packages. No supervisord program is
#     installed.
#
# Expects on the DUT:
#   /home/admin/emu-bundle/            unpacked bundle (payload/, emu_config.yaml)
#   /home/admin/xcvr-emu-image.tar.gz  the emulator image tarball (docker save|gzip)
# Idempotent: safe to re-run (stops any running xcvrd first, then relaunches).
#
# NB: because xcvrd is a plain process (no supervisord), it does NOT survive a
# `config reload` / pmon restart — re-run this deploy to bring it back. The
# emulator container itself survives (docker --restart unless-stopped) unless the
# whole docker/data dir is wiped. A full pmon *recreation* also wipes /opt.
set -uo pipefail

BUNDLE=/home/admin/emu-bundle
IMAGE_TAR=/home/admin/xcvr-emu-image.tar.gz
IMAGE_TAG=xcvr-emu:local
EMU_CTR=xcvr-emu                       # standalone emulator container name
EMU_CFG_HOST=/home/admin/emu_config.yaml   # stable path on the DUT host, bind-mounted into the container
PMON=pmon
OPT=/opt/xcvr-emu-bridge              # bridge lives here in pmon; loaded via PYTHONPATH
SUP_CONF=/etc/supervisor/conf.d/xcvr-emu.conf   # legacy drop-in from older deploys; removed if present
XCVRD_BIN=/usr/local/bin/xcvrd
XCVRD_LOG=/tmp/xcvrd.log
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

# --- 3. launch xcvrd directly with the env exported (NO supervisord) ---------
# If an older deploy left a supervisord drop-in, remove it so supervisord stops
# managing/relaunching its own xcvrd (which would fight the one we start here).
if docker exec "$PMON" test -f "$SUP_CONF" 2>/dev/null; then
  echo "[deploy] removing legacy supervisord xcvrd drop-in ($SUP_CONF)"
  docker exec "$PMON" rm -f "$SUP_CONF"
  docker exec "$PMON" supervisorctl reread  >/dev/null 2>&1 || true
  docker exec "$PMON" supervisorctl update  >/dev/null 2>&1 || true   # unregisters + stops the old supervised xcvrd
fi

echo "[deploy] stopping any running xcvrd, then launching it with PYTHONPATH + XCVR_EMU_ADDR exported"
docker exec "$PMON" bash -c 'pkill -x xcvrd 2>/dev/null; sleep 1; true'
docker exec -d "$PMON" bash -c "PYTHONPATH='$OPT' XCVR_EMU_ADDR='$EMU_ADDR' exec $XCVRD_BIN >$XCVRD_LOG 2>&1"

echo "[deploy] xcvrd process (should show PYTHONPATH=$OPT):"
docker exec "$PMON" bash -c '
  sleep 2
  pid=$(pgrep -x xcvrd | head -1)
  if [ -n "$pid" ]; then
    ps -o pid,cmd -p "$pid" | tail -1 | sed "s/^/  /"
    tr "\0" "\n" < /proc/$pid/environ | grep -i pythonpath | sed "s/^/  /"
  else
    echo "  (xcvrd not running yet — check '"$XCVRD_LOG"')"
  fi'

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
echo "===XCVRD_PROC==="; docker exec "$PMON" bash -c 'pgrep -x xcvrd >/dev/null && echo "xcvrd RUNNING (pid $(pgrep -x xcvrd|head -1))" || echo "xcvrd NOT running"'
echo "===INFO_COUNT==="; sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l
echo "===DOM_COUNT===";  sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l
echo "[deploy] done — emulator is a standalone container; xcvrd launched directly with env (no supervisord)"
echo "[deploy] NOTE: xcvrd will NOT survive a config reload / pmon restart — re-run this deploy to bring it back"
