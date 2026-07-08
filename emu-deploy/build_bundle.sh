#!/bin/bash
# Build the emulator bundle (emu-bundle.tar.gz).
#
# Model: the emulator (xcvr-emud) runs as its OWN standalone Docker container on
# the DUT — see build_emu_image.sh / deploy_on_dut.sh — so it survives the SONiC
# `config reload` events that restart pmon. Only the sonic_platform BRIDGE (and
# the xcvr_emu gRPC proto stubs it imports as a client) lives inside pmon, in a
# side directory (/opt/xcvr-emu-bridge on the DUT) loaded via PYTHONPATH. pmon's
# dist-packages is NEVER modified. xcvrd is launched directly by deploy_on_dut.sh
# with the env exported (no supervisord).
#
# Bundle layout:
#   payload/sonic_platform/   - the gRPC bridge (dev/platform/sonic_platform)
#   payload/xcvr_emu/         - emulator package, kept ONLY for the gRPC proto
#                               stubs (xcvr_emu.proto) the bridge client imports
#   emu_config.yaml           - N present CMIS modules (mounted into the emulator
#                               container on the DUT)
#   kvm_platform.json         - chassis.sfps inventory installed as the platform's
#                               platform.json (needed by platform_tests/api/test_sfp.py)
#
# Usage:  ./build_bundle.sh [XCVR_EMU_REPO] [N_MODULES]
set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
XCVR_EMU_REPO="${1:-$HERE/../../xcvr-emu}"
N="${2:-33}"
BRIDGE="$HERE/../platform/sonic_platform"

[ -d "$XCVR_EMU_REPO/src/xcvr_emu" ] || { echo "ERROR: xcvr-emu repo not found at $XCVR_EMU_REPO"; exit 1; }
[ -d "$BRIDGE" ] || { echo "ERROR: bridge not found at $BRIDGE"; exit 1; }
[ -f "$HERE/kvm_platform.json" ] || { echo "ERROR: kvm_platform.json not found at $HERE"; exit 1; }

echo "[build] generating emu_config.yaml with $N modules"
python3 "$HERE/gen_emu_config.py" "$N" "$HERE/emu_config.yaml"

echo "[build] assembling staging/payload (bridge + xcvr_emu proto stubs, loaded via PYTHONPATH)"
rm -rf "$HERE/staging"
mkdir -p "$HERE/staging/payload"
cp -r "$BRIDGE"                          "$HERE/staging/payload/sonic_platform"
cp -r "$XCVR_EMU_REPO/src/xcvr_emu"      "$HERE/staging/payload/xcvr_emu"
cp    "$HERE/emu_config.yaml"            "$HERE/staging/emu_config.yaml"
cp    "$HERE/kvm_platform.json"          "$HERE/staging/kvm_platform.json"
find "$HERE/staging" -name '__pycache__' -type d -prune -exec rm -rf {} + 2>/dev/null || true
find "$HERE/staging" -name '*.pyc' -delete 2>/dev/null || true

# sanity: the get_change_event fix is present; the gRPC proto stubs included
grep -q 'def get_change_event' "$HERE/staging/payload/sonic_platform/chassis.py" \
  || { echo "ERROR: bridge missing get_change_event"; exit 1; }
[ -f "$HERE/staging/payload/xcvr_emu/proto/emulator_pb2.py" ] \
  || { echo "ERROR: xcvr_emu.proto stubs missing from payload (bridge client needs them)"; exit 1; }
[ -f "$HERE/staging/kvm_platform.json" ] \
  || { echo "ERROR: kvm_platform.json missing from bundle"; exit 1; }

tar czf "$HERE/emu-bundle.tar.gz" -C "$HERE/staging" .
echo "[build] wrote $HERE/emu-bundle.tar.gz"
ls -la "$HERE/emu-bundle.tar.gz"
