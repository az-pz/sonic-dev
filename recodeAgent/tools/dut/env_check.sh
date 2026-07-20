#!/bin/bash
# env_check.sh (host side) -- build BOTH environment smoke bins in the trixie
# container and run them inside pmon, proving the agent scaffolding end to end:
#   swss-smoke : swss-common Rust bindings  -> STATE_DB (Redis)
#   env-smoke  : platform-bridge (PyO3 HAL) -> STATE_DB via swss-common (the exact
#                read-transceiver-then-publish pattern xcvrd-rs will use)
#
# Runs on the sonic-dev host. Assumes the crate is staged at ~/recode/crate
# (tools/env_check.sh ships it first). Leaves xcvrd + STATE_DB untouched (the smoke
# bins use throwaway keys they delete, and are removed from pmon on the way out).
set -uo pipefail
RECODE="$HOME/recode"
CRATE="$RECODE/crate"
IMG=recode-rust-build
DUT="admin@10.250.0.101"
SP="sshpass -p password ssh -o StrictHostKeyChecking=no"
SPC="sshpass -p password scp -o StrictHostKeyChecking=no"

if ! docker image inspect "$IMG" >/dev/null 2>&1; then
  echo "[env] image $IMG missing; build it: docker build -t $IMG -f dut/Dockerfile.build ." >&2
  exit 2
fi

# libswsscommon.so for the linker (pulled from pmon, matches runtime ABI).
bash "$RECODE/dut/ensure_swsslib.sh"

echo "[env] building swss-smoke + env-smoke in $IMG"
# SWSS_COMMON_REPO + BINDGEN_EXTRA_CLANG_ARGS are baked into the image; we add the
# link search for the mounted libswsscommon.so.
docker run --rm --network host \
  -v "$CRATE":/src -v "$RECODE/swsslib":/swsslib -w /src \
  -e RUSTFLAGS="-L native=/swsslib" \
  "$IMG" cargo build --release -p env-check || exit 2
for b in swss-smoke env-smoke; do
  [ -x "$CRATE/target/release/$b" ] || { echo "[env] missing binary $b" >&2; exit 2; }
done

echo "[env] shipping into pmon"
for b in swss-smoke env-smoke; do
  docker cp "$CRATE/target/release/$b" mgmt:/tmp/$b
  docker exec mgmt bash -lc "$SPC /tmp/$b $DUT:/tmp/$b"
  docker exec mgmt bash -lc "$SP $DUT 'docker cp /tmp/$b pmon:/usr/local/bin/$b && docker exec pmon chmod +x /usr/local/bin/$b'"
done

rc=0
echo "==================== swss-smoke ===================="
docker exec mgmt bash -lc "$SP $DUT 'docker exec pmon /usr/local/bin/swss-smoke'" || rc=$?
echo "==================== env-smoke ====================="
docker exec mgmt bash -lc "$SP $DUT 'docker exec pmon /usr/local/bin/env-smoke'" || rc=$?
echo "==================== end (rc=$rc) ===================="

echo "[env] cleanup"
docker exec mgmt bash -lc "$SP $DUT 'docker exec pmon rm -f /usr/local/bin/swss-smoke /usr/local/bin/env-smoke; rm -f /tmp/swss-smoke /tmp/env-smoke'" || true
docker exec mgmt rm -f /tmp/swss-smoke /tmp/env-smoke >/dev/null 2>&1 || true
exit $rc
