#!/bin/bash
# Wrapper for xcvrd under supervisord: wait until the emulator (reached via the
# sonic_platform bridge) reports all its modules, THEN exec the real xcvrd.
#
# Rationale: supervisord starts programs by priority but does not wait for
# readiness. If xcvrd starts before xcvr-emud is serving gRPC, the bridge's
# Chassis falls back to a small placeholder SFP count and xcvrd crashes on the
# first missing get_sfp(). We poll here so xcvrd only starts once the emulated
# plant is fully up. Best-effort: after the timeout we exec anyway (autorestart
# will retry if it's still not ready).
export PYTHONPATH="${PYTHONPATH:-/opt/xcvr-emu-bridge}"
export XCVR_EMU_ADDR="${XCVR_EMU_ADDR:-localhost:50051}"
EXPECT_SFPS="${EXPECT_SFPS:-33}"

for i in $(seq 1 60); do
  n=$(python3 -c 'from sonic_platform.platform import Platform
try:
    print(Platform().get_chassis().get_num_sfps())
except Exception:
    print(0)' 2>/dev/null | tail -1)
  if [ "${n:-0}" -ge "$EXPECT_SFPS" ] 2>/dev/null; then
    echo "[start-xcvrd] emulator ready ($n modules) — launching xcvrd"
    break
  fi
  sleep 2
done

exec xcvrd
