#!/bin/bash
# Build the emulator deployment bundle (emu-bundle.tar.gz) from local sources.
#
# The bundle contains everything that must be installed into the DUT's pmon
# container to run xcvrd against emulated CMIS optics:
#   sonic_platform/   - the xcvr-emu gRPC bridge (dev/platform/sonic_platform)
#   xcvr_emu/         - the emulator package        (xcvr-emu/src/xcvr_emu)
#   cmis/             - CMIS decode used by emulator (xcvr-emu/src/cmis)
#   emu_config.yaml   - N present QSFP-DD modules    (gen_emu_config.py)
#
# Usage:  ./build_bundle.sh [XCVR_EMU_REPO] [N_MODULES]
# Defaults: XCVR_EMU_REPO=../../../xcvr-emu (relative to this script), N=33.
set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
XCVR_EMU_REPO="${1:-$HERE/../../xcvr-emu}"
N="${2:-33}"
BRIDGE="$HERE/../platform/sonic_platform"

[ -d "$XCVR_EMU_REPO/src/xcvr_emu" ] || { echo "ERROR: xcvr-emu repo not found at $XCVR_EMU_REPO"; exit 1; }
[ -d "$BRIDGE" ] || { echo "ERROR: bridge not found at $BRIDGE"; exit 1; }

echo "[build] generating emu_config.yaml with $N modules"
python3 "$HERE/gen_emu_config.py" "$N" "$HERE/emu_config.yaml"

echo "[build] assembling staging/"
rm -rf "$HERE/staging"
mkdir -p "$HERE/staging"
cp -r "$XCVR_EMU_REPO/src/xcvr_emu" "$HERE/staging/xcvr_emu"
cp -r "$XCVR_EMU_REPO/src/cmis"     "$HERE/staging/cmis"
cp -r "$BRIDGE"                     "$HERE/staging/sonic_platform"
cp "$HERE/emu_config.yaml"          "$HERE/staging/emu_config.yaml"
find "$HERE/staging" -name '__pycache__' -type d -prune -exec rm -rf {} + 2>/dev/null || true
find "$HERE/staging" -name '*.pyc' -delete 2>/dev/null || true

# sanity: the get_change_event fix must be present, and no accidental nesting
grep -q 'def get_change_event' "$HERE/staging/sonic_platform/chassis.py" \
  || { echo "ERROR: bridge missing get_change_event"; exit 1; }
[ -f "$HERE/staging/sonic_platform/chassis.py" ] \
  || { echo "ERROR: sonic_platform not laid out flat"; exit 1; }

tar czf "$HERE/emu-bundle.tar.gz" -C "$HERE/staging" .
echo "[build] wrote $HERE/emu-bundle.tar.gz"
ls -la "$HERE/emu-bundle.tar.gz"
