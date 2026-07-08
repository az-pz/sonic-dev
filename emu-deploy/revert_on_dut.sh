#!/bin/bash
# Reverts deploy_native.sh: restores stock host sonic_platform, restores
# pmon_daemon_control.json (skip_xcvrd=true), removes the pmon injection, and
# restarts pmon. Leaves the emulator container alone (harmless; remove with
# `docker rm -f xcvr-emu` if desired).
set -uo pipefail
DEVDIR=/usr/share/sonic/device/x86_64-kvm_x86_64-r0
PDC=$DEVDIR/pmon_daemon_control.json
PLATFORM_JSON=$DEVDIR/platform.json
HOST_DP=/usr/lib/python3/dist-packages
PMON_DP=/usr/local/lib/python3.13/dist-packages

echo "[revert] restoring host sonic_platform"
if [ -e "${HOST_DP}/sonic_platform.orig" ]; then
  sudo rm -rf "${HOST_DP}/sonic_platform"
  sudo mv "${HOST_DP}/sonic_platform.orig" "${HOST_DP}/sonic_platform"
  sudo rm -rf "${HOST_DP}/xcvr_emu"
  echo "  restored stock sonic_platform, removed host xcvr_emu"
else
  echo "  no backup found — leaving host as-is"
fi

echo "[revert] restoring pmon_daemon_control.json"
if [ -e "${PDC}.orig" ]; then
  sudo cp "${PDC}.orig" "$PDC"
  echo "  restored (skip_xcvrd back to original)"
fi

echo "[revert] removing platform.json"
if [ -e "${PLATFORM_JSON}.orig" ]; then
  sudo mv "${PLATFORM_JSON}.orig" "$PLATFORM_JSON"
  echo "  restored original platform.json"
elif [ -f "$PLATFORM_JSON" ]; then
  # stock vs ships no platform.json, so ours (no .orig backup) is removed
  sudo rm -f "$PLATFORM_JSON"
  echo "  removed our platform.json (stock vs had none)"
fi

echo "[revert] removing pmon injection + restarting pmon"
docker exec pmon rm -rf "$PMON_DP/sonic_platform" "$PMON_DP/xcvr_emu" 2>/dev/null || true
docker restart pmon >/dev/null 2>&1 || true
echo "[revert] done"
