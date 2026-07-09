#!/bin/bash
# Runs ON the DUT (admin@vlab-01). "Systematic-at-runtime" wiring — NO image changes:
#   1. HOST sonic_platform := our emulator-backed bridge  (fixes host sfputil/sfpshow/reset)
#   2. flip skip_xcvrd -> false                            (enable xcvrd natively in pmon)
#   3. inject our bridge into pmon's dist-packages         (so native xcvrd imports it)
# The xcvr-emu emulator runs as a standalone --network host container (reused/started here).
#
# Fully reversible: stock host sonic_platform is backed up to sonic_platform.orig and the
# original pmon_daemon_control.json to *.orig (see revert_on_dut.sh).
set -uo pipefail

BUNDLE=/home/admin/emu-bundle
PAY=$BUNDLE/payload
EMU_CTR=xcvr-emu
IMAGE_TAR=/home/admin/xcvr-emu-image.tar.gz
IMAGE_TAG=xcvr-emu:local
EMU_CFG_HOST=/home/admin/emu_config.yaml
DEVDIR=/usr/share/sonic/device/x86_64-kvm_x86_64-r0
PDC=$DEVDIR/pmon_daemon_control.json
PLATFORM_JSON=$DEVDIR/platform.json     # chassis.sfps inventory the platform SFP-API tests need
HOST_DP=/usr/lib/python3/dist-packages
PMON_DP=/usr/local/lib/python3.13/dist-packages

[ -d "$PAY/sonic_platform" ] || { echo "ERROR: $PAY/sonic_platform missing (bundle not unpacked)"; exit 1; }
[ -d "$PAY/xcvr_emu" ]       || { echo "ERROR: $PAY/xcvr_emu missing";       exit 1; }

# --- 0) (re)load the emulator image + (re)create the container --------------
# Always reload + recreate so a redeploy actually picks up a rebuilt image and a
# regenerated emu_config.yaml (do NOT skip when already running, or patches to
# the image/config would be silently ignored).
echo "[native] (re)loading emulator image + recreating container '$EMU_CTR'"
[ -f "$IMAGE_TAR" ] && gunzip -c "$IMAGE_TAR" | docker load
cp "$BUNDLE/emu_config.yaml" "$EMU_CFG_HOST"
docker rm -f "$EMU_CTR" >/dev/null 2>&1 || true
docker run -d --name "$EMU_CTR" --network host --restart unless-stopped \
  -v "$EMU_CFG_HOST":/emu_config.yaml:ro "$IMAGE_TAG" xcvr-emud -c /emu_config.yaml
sleep 3
docker ps --filter "name=^/${EMU_CTR}$" --format '  emulator: {{.Names}} {{.Status}}'

# --- 1) HOST sonic_platform := our bridge -----------------------------------
echo "[native] STEP 1: replace HOST sonic_platform with our bridge (backup once)"
if [ ! -e "${HOST_DP}/sonic_platform.orig" ]; then
  sudo cp -r "${HOST_DP}/sonic_platform" "${HOST_DP}/sonic_platform.orig"
  echo "  backed up stock host sonic_platform -> sonic_platform.orig"
fi
sudo rm -rf "${HOST_DP}/sonic_platform"
sudo cp -r "$PAY/sonic_platform" "${HOST_DP}/sonic_platform"
sudo rm -rf "${HOST_DP}/xcvr_emu"
sudo cp -r "$PAY/xcvr_emu" "${HOST_DP}/xcvr_emu"
sudo find "${HOST_DP}/sonic_platform" "${HOST_DP}/xcvr_emu" -name '__pycache__' -type d -prune -exec rm -rf {} + 2>/dev/null || true

echo "[native] verify host sfputil now uses the emulator:"
sudo sfputil show presence -p Ethernet4 | sed 's/^/    /'; echo "    (presence rc=${PIPESTATUS[0]})"
echo "    -- reset Ethernet4 --"
sudo sfputil reset Ethernet4 | sed 's/^/    /'; echo "    (reset rc=${PIPESTATUS[0]})"

# --- 1b) install platform.json (chassis.sfps inventory) ---------------------
# The platform SFP-API suite (platform_tests/api/test_sfp.py) reads
# duthost.facts["chassis"]["sfps"] from /usr/share/sonic/device/<platform>/platform.json.
# Stock vs ships none, so its setup fixture errors out. We drop in a chassis.sfps
# inventory (32 x 40G ports). Only "chassis" is set — no "interfaces" — so port
# config still comes from port_config.ini.
if [ -f "$BUNDLE/kvm_platform.json" ]; then
  echo "[native] STEP 1b: install platform.json (chassis.sfps) at $PLATFORM_JSON"
  if [ -f "$PLATFORM_JSON" ] && [ ! -e "${PLATFORM_JSON}.orig" ]; then
    sudo cp "$PLATFORM_JSON" "${PLATFORM_JSON}.orig"
    echo "  backed up existing platform.json -> platform.json.orig"
  fi
  sudo cp "$BUNDLE/kvm_platform.json" "$PLATFORM_JSON"
  n=$(python3 -c "import json;print(len(json.load(open('$PLATFORM_JSON'))['chassis']['sfps']))" 2>/dev/null)
  echo "  installed platform.json with ${n:-?} sfps"
fi

# --- 3) inject our bridge into pmon -----------------------------------------
echo "[native] STEP 3: inject bridge into pmon dist-packages"
docker exec pmon rm -rf "$PMON_DP/sonic_platform" "$PMON_DP/xcvr_emu"
docker cp "$PAY/sonic_platform" "pmon:$PMON_DP/sonic_platform"
docker cp "$PAY/xcvr_emu"       "pmon:$PMON_DP/xcvr_emu"
docker exec pmon python3 -c "import sonic_platform.platform, xcvr_emu.proto.emulator_pb2; print('    pmon import OK')" \
  || { echo "[native] ERROR: pmon bridge import failed"; exit 1; }

# --- 2) flip skip_xcvrd -> false, restart pmon so supervisord regenerates ---
echo "[native] STEP 2: flip skip_xcvrd -> false"
[ -e "${PDC}.orig" ] || sudo cp "$PDC" "${PDC}.orig"
sudo python3 - "$PDC" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d['skip_xcvrd'] = False
json.dump(d, open(p, 'w'), indent=4)
print("    pmon_daemon_control.json ->", d)
PY

echo "[native] restarting pmon so it regenerates supervisord with [program:xcvrd]"
docker restart pmon >/dev/null
sleep 8

echo "[native] ensuring xcvrd is RUNNING (autostart may be false in the template)"
for i in $(seq 1 12); do
  st=$(docker exec pmon supervisorctl status xcvrd 2>/dev/null | awk '{print $2}')
  echo "    [$((i*3))s] xcvrd=${st:-<not-registered>}"
  [ "$st" = "RUNNING" ] && break
  case "${st:-}" in
    STOPPED|EXITED|FATAL|BACKOFF|"") docker exec pmon supervisorctl start xcvrd 2>/dev/null || true ;;
  esac
  sleep 3
done

echo "[native] waiting up to 120s for TRANSCEIVER_INFO + DOM..."
for i in $(seq 1 24); do
  sleep 5
  ni=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l)
  nd=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l)
  echo "    [$((i*5))s] INFO=$ni DOM=$nd"
  [ "$ni" -ge 28 ] && [ "$nd" -ge 28 ] && break
done

# --- 5) force host_tx_ready=true so xcvrd activates the CMIS datapath ---------
# The emulator advertises a 40G app (matches the ports), so CmisManager reaches
# cmis_state=READY. But on the software-SAI vs the ASIC never asserts
# host_tx_ready, so xcvrd "Forces Tx laser OFF" and the datapath stays
# DataPathDeactivated (module stuck ModuleLowPwr). Setting host_tx_ready=true in
# STATE_DB lets CmisManager finish: ModuleReady + DataPathActivated + error OK.
echo "[native] STEP 5: forcing host_tx_ready=true on all ports (vs SAI doesn't assert it)"
nport=0
for p in $(sonic-db-cli CONFIG_DB KEYS 'PORT|Ethernet*' 2>/dev/null | sed 's/PORT|//'); do
  sonic-db-cli STATE_DB HSET "PORT_TABLE|$p" host_tx_ready true >/dev/null 2>&1 && nport=$((nport+1))
done
echo "    set host_tx_ready=true on $nport ports; waiting 30s for CmisManager to activate datapaths"
sleep 30
echo "    sample: $(sonic-db-cli STATE_DB HGET 'TRANSCEIVER_STATUS_SW|Ethernet8' module_state 2>/dev/null) / DP1=$(sonic-db-cli STATE_DB HGET 'TRANSCEIVER_STATUS_SW|Ethernet8' DP1State 2>/dev/null)"

echo "===EMU===";   docker ps --filter "name=^/${EMU_CTR}$" --format '{{.Names}} {{.Status}}'
echo "===XCVRD==="; docker exec pmon supervisorctl status xcvrd 2>&1
echo "===INFO==="; sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l
echo "===DOM===";  sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l
echo "[native] done — host sfputil + pmon xcvrd both use the emulator; skip_xcvrd=false; datapaths activated"
echo "===EMU_DEPLOY_DONE==="
