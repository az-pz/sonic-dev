#!/bin/bash
# Build the xcvr-emu emulator Docker image and save it to a tarball so it can be
# shipped to the (offline) DUT and `docker load`-ed there.
#
# The emulator now runs as its OWN standalone container on the DUT — NOT inside
# pmon — so it survives the SONiC `config reload` events that sonic-mgmt tests
# trigger (those restart every SONiC *feature* container, but a plain
# `docker run` container is not a feature and is left untouched).
#
# The image is built from the upstream repo's Dockerfile, with TWO local patches
# (see below): (1) each transceiver gets its own CMIS MemMap object, and (2) each
# MemMap gets its own EEPROM byte buffer (upstream shares one via a mutable
# default argument). Together they give all 32 emulated modules independent state.
# Its runtime CMD is `xcvr-emud -c config.yaml` and it serves gRPC on :50051.
#
# Usage:  ./build_emu_image.sh [XCVR_EMU_REPO] [IMAGE_TAG] [OUT_TAR]
# Env:    EMU_REBUILD_IMAGE=1  force a rebuild even if the tarball already exists.
set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
XCVR_EMU_REPO="${1:-$HERE/../../xcvr-emu}"
IMAGE_TAG="${2:-xcvr-emu:local}"
OUT_TAR="${3:-$HERE/xcvr-emu-image.tar.gz}"

[ -f "$XCVR_EMU_REPO/Dockerfile" ] || { echo "ERROR: Dockerfile not found in $XCVR_EMU_REPO"; exit 1; }

if [ -f "$OUT_TAR" ] && [ "${EMU_REBUILD_IMAGE:-0}" != "1" ]; then
  echo "[image] $OUT_TAR already exists — reusing (set EMU_REBUILD_IMAGE=1 to force rebuild)"
  ls -la "$OUT_TAR"
  exit 0
fi

# --- local patch: give each transceiver its OWN MemMap ----------------------
# Upstream server.py shares a single self._cmis_mem_map across ALL transceivers,
# so every emulated module aliases the same CMIS registers. That's fine for one
# active module but breaks multi-module operation: when xcvrd brings up 32 ports
# concurrently they race on the shared ModuleState/DPState/LowPwr registers ->
# "timeout for ModuleReady" -> cmis_state=FAILED, datapath never activates.
# CMISTransceiver.__init__ already does `MemMap() if mem_map is None else mem_map`,
# so simply not passing the shared map gives each module its own. Idempotent.
SRV="$XCVR_EMU_REPO/src/xcvr_emu/server.py"
if grep -q "CMISTransceiver(k, v, self._cmis_mem_map)" "$SRV" 2>/dev/null; then
  echo "[image] patching server.py: per-transceiver MemMap (upstream shares one)"
  sed -i 's/CMISTransceiver(k, v, self\._cmis_mem_map)/CMISTransceiver(k, v)/' "$SRV"
  sed -i 's/CMISTransceiver(req\.index, {}, self\._cmis_mem_map)/CMISTransceiver(req.index, {})/' "$SRV"
fi
grep -q "CMISTransceiver(k, v, self._cmis_mem_map)" "$SRV" 2>/dev/null \
  && { echo "ERROR: per-transceiver MemMap patch failed to apply"; exit 1; } || true

# --- local patch: per-instance EEPROM backing store (mutable-default bug) ----
# BaseMemMap.__init__ (cmis/field.py) declares:
#     def __init__(self, remote=EEPROM("remote"), local=EEPROM("local"), ...):
# Those EEPROM(...) defaults are evaluated ONCE at class-definition time, so
# EVERY MemMap() built without explicit remote/local shares the SAME two EEPROM
# byte buffers. With the per-transceiver MemMap patch above each module gets its
# own MemMap object, but they'd STILL alias the same bytes through these shared
# defaults -> writing module 0's registers changes all 32 modules, and 32
# concurrent bring-ups mutually reset each other (SoftwareReset on any module
# wipes the shared buffer). The emulator was only ever exercised with a single
# module, so this classic Python mutable-default pitfall never surfaced upstream.
# Fix: default to None and allocate a fresh EEPROM per instance. Idempotent.
FLD="$XCVR_EMU_REPO/src/cmis/field.py"
if grep -q 'remote: MemoryAccessor = EEPROM("remote")' "$FLD" 2>/dev/null; then
  echo "[image] patching field.py: per-instance EEPROM (fix shared mutable default)"
  sed -i 's/remote: MemoryAccessor = EEPROM("remote"),/remote: MemoryAccessor | None = None,/' "$FLD"
  sed -i 's/local: MemoryAccessor = EEPROM("local"),/local: MemoryAccessor | None = None,/' "$FLD"
  sed -i 's/^        self.remote = remote$/        self.remote = remote if remote is not None else EEPROM("remote")/' "$FLD"
  sed -i 's/^        self.local = local$/        self.local = local if local is not None else EEPROM("local")/' "$FLD"
fi
grep -q 'remote: MemoryAccessor = EEPROM("remote")' "$FLD" 2>/dev/null \
  && { echo "ERROR: per-instance EEPROM patch failed to apply"; exit 1; } || true

# --- local patch: make SoftwareReset self-clearing (WO/SC per CMIS spec) -----
# The CMIS SoftwareReset trigger (00h:26.3) is Write-Only / Self-Clearing: after
# the module acts on it, the bit must read back 0. The emulator stores the
# written bit in its EEPROM and never clears it, so byte 26 keeps bit3=1. xcvrd's
# CmisManager brings a module to high power with set_lpmode(False), which does a
# READ-MODIFY-WRITE of ModuleGlobalControls: it reads 0x18 (LowPwr + stale
# SoftwareReset), clears the LowPwr bit and writes back 0x08 -- which re-triggers
# SoftwareReset, bouncing the module back to ModuleLowPwr. The datapath can then
# never stay initialized: is_cmis_application_update_required() keeps seeing
# active appsel 0 -> "force Datapath reinit" forever. Clearing the bit right after
# the reset is handled makes it self-clearing and breaks the loop. Idempotent.
TRX="$XCVR_EMU_REPO/src/xcvr_emu/transceiver/transceiver.py"
python3 - "$TRX" <<'PYPATCH'
import sys
f = sys.argv[1]
s = open(f).read()
old = ('                if software_reset.value == software_reset.RESET:\n'
       '                    logger.info("Software reset")\n'
       '                    self._init()')
new = old + ('\n                    self.mem_map.SoftwareReset.value = '
             'self.mem_map.SoftwareReset.NO_ACTION')
if "SoftwareReset.NO_ACTION" in s:
    print("[image] transceiver.py already has SoftwareReset self-clear")
elif old in s:
    open(f, "w").write(s.replace(old, new))
    print("[image] patched transceiver.py: SoftwareReset self-clear (WO/SC)")
else:
    sys.exit("ERROR: SoftwareReset self-clear patch anchor not found")
PYPATCH

echo "[image] building $IMAGE_TAG from $XCVR_EMU_REPO (with per-transceiver MemMap + per-instance EEPROM patches)"
docker build -t "$IMAGE_TAG" "$XCVR_EMU_REPO"

echo "[image] saving $IMAGE_TAG -> $OUT_TAR"
docker save "$IMAGE_TAG" | gzip > "$OUT_TAR"
echo "[image] wrote $OUT_TAR"
ls -la "$OUT_TAR"
