#!/bin/bash
# env_check.sh (host side) -- build the xcvrd-rs binding examples in the trixie
# container and run them inside pmon, proving the bootstrap scaffolding end to end:
#   statedb_probe  : swss-common Rust bindings  -> STATE_DB (Redis)
#   hal_to_statedb : platform-bridge (PyO3 HAL) -> STATE_DB via swss-common (the
#                    exact read-transceiver-then-publish pattern xcvrd-rs will use)
#
# Runs on the sonic-dev host. Assumes the crate is staged at ~/recode/crate
# (tools/env_check.sh ships it first). Leaves xcvrd + STATE_DB untouched (the
# examples use throwaway keys they delete, and are removed from pmon on the way out).
set -uo pipefail
RECODE="$HOME/recode"
CRATE="$RECODE/crate"
IMG=recode-rust-build
DUT="admin@10.250.0.101"
SP="sshpass -p password ssh -o StrictHostKeyChecking=no"
SPC="sshpass -p password scp -o StrictHostKeyChecking=no"
EXAMPLES="statedb_probe hal_to_statedb"

if ! docker image inspect "$IMG" >/dev/null 2>&1; then
  echo "[env] image $IMG missing; build it: docker build -t $IMG -f dut/Dockerfile.build ." >&2
  exit 2
fi

# libswsscommon.so for the linker (pulled from pmon, matches runtime ABI).
bash "$RECODE/dut/ensure_swsslib.sh"

echo "[env] building xcvrd-rs examples ($EXAMPLES) in $IMG"
# SWSS_COMMON_REPO + BINDGEN_EXTRA_CLANG_ARGS are baked into the image; add the
# link search for the mounted libswsscommon.so.
docker run --rm --network host \
  -v "$CRATE":/src -v "$RECODE/swsslib":/swsslib -w /src \
  -e RUSTFLAGS="-L native=/swsslib" \
  "$IMG" cargo build --release -p xcvrd-rs --examples || exit 2
for e in $EXAMPLES; do
  [ -x "$CRATE/target/release/examples/$e" ] || { echo "[env] missing example $e" >&2; exit 2; }
done

echo "[env] shipping examples into pmon"
for e in $EXAMPLES; do
  docker cp "$CRATE/target/release/examples/$e" mgmt:/tmp/$e
  docker exec mgmt bash -lc "$SPC /tmp/$e $DUT:/tmp/$e"
  docker exec mgmt bash -lc "$SP $DUT 'docker cp /tmp/$e pmon:/usr/local/bin/$e && docker exec pmon chmod +x /usr/local/bin/$e'"
done

rc=0
for e in $EXAMPLES; do
  echo "==================== $e ===================="
  docker exec mgmt bash -lc "$SP $DUT 'docker exec pmon /usr/local/bin/$e'" || rc=$?
done
echo "==================== end (rc=$rc) ===================="

echo "[env] cleanup"
for e in $EXAMPLES; do
  docker exec mgmt bash -lc "$SP $DUT 'docker exec pmon rm -f /usr/local/bin/$e; rm -f /tmp/$e'" || true
  docker exec mgmt rm -f /tmp/$e >/dev/null 2>&1 || true
done
exit $rc
