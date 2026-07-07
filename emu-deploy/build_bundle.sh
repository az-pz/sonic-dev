#!/bin/bash
# Build the emulator bundle (emu-bundle.tar.gz).
#
# Model: emud runs INSIDE the pmon container (like the original working setup),
# but ALL our python — the emulator (xcvr_emu + cmis) AND the sonic_platform
# bridge — is placed in a single side directory (/opt/xcvr-emu-bridge on the DUT)
# and loaded via PYTHONPATH. pmon's dist-packages is NEVER modified.
#
# Bundle layout:
#   payload/sonic_platform/   - the gRPC bridge (dev/platform/sonic_platform)
#   payload/xcvr_emu/         - the emulator package (xcvr-emu/src/xcvr_emu)
#   payload/cmis/             - CMIS decode used by the emulator (xcvr-emu/src/cmis)
#   emu_config.yaml           - N present CMIS modules
#
# Usage:  ./build_bundle.sh [XCVR_EMU_REPO] [N_MODULES]
set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
XCVR_EMU_REPO="${1:-$HERE/../../xcvr-emu}"
N="${2:-33}"
BRIDGE="$HERE/../platform/sonic_platform"

[ -d "$XCVR_EMU_REPO/src/xcvr_emu" ] || { echo "ERROR: xcvr-emu repo not found at $XCVR_EMU_REPO"; exit 1; }
[ -d "$XCVR_EMU_REPO/src/cmis" ]     || { echo "ERROR: cmis not found in $XCVR_EMU_REPO/src"; exit 1; }
[ -d "$BRIDGE" ] || { echo "ERROR: bridge not found at $BRIDGE"; exit 1; }

echo "[build] generating emu_config.yaml with $N modules"
python3 "$HERE/gen_emu_config.py" "$N" "$HERE/emu_config.yaml"

echo "[build] assembling staging/payload (bridge + emulator, loaded via PYTHONPATH)"
rm -rf "$HERE/staging"
mkdir -p "$HERE/staging/payload"
cp -r "$BRIDGE"                          "$HERE/staging/payload/sonic_platform"
cp -r "$XCVR_EMU_REPO/src/xcvr_emu"      "$HERE/staging/payload/xcvr_emu"
cp -r "$XCVR_EMU_REPO/src/cmis"          "$HERE/staging/payload/cmis"
cp    "$HERE/emu_config.yaml"            "$HERE/staging/emu_config.yaml"
find "$HERE/staging" -name '__pycache__' -type d -prune -exec rm -rf {} + 2>/dev/null || true
find "$HERE/staging" -name '*.pyc' -delete 2>/dev/null || true

# sanity: the get_change_event fix is present, and the emulator daemon is included
grep -q 'def get_change_event' "$HERE/staging/payload/sonic_platform/chassis.py" \
  || { echo "ERROR: bridge missing get_change_event"; exit 1; }
[ -f "$HERE/staging/payload/xcvr_emu/xcvr_emud.py" ] \
  || { echo "ERROR: emulator daemon missing from payload"; exit 1; }

tar czf "$HERE/emu-bundle.tar.gz" -C "$HERE/staging" .
echo "[build] wrote $HERE/emu-bundle.tar.gz"
ls -la "$HERE/emu-bundle.tar.gz"
