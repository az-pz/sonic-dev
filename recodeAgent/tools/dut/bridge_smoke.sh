#!/bin/bash
# bridge_smoke.sh (host side) — build the platform-bridge smoke binary, run it
# inside pmon against the live xcvr-emu, and clean it up. Proves the PyO3 ->
# sonic_platform spine end to end on the DUT: embed CPython, import the plugin,
# gRPC to the emulator, CMIS decode, marshal back to Rust.
#
# Runs on the sonic-dev host. Assumes the crate is staged at ~/recode/crate
# (tools/bridge_smoke.sh ships it first). Leaves xcvrd untouched (the smoke binary
# is a separate /usr/local/bin/bridge-smoke, removed on the way out).
set -uo pipefail
RECODE="$HOME/recode"
CRATE="$RECODE/crate"
IMG=recode-rust-build
BIN="$CRATE/target/release/bridge-smoke"
DUT="admin@10.250.0.101"
SP="sshpass -p password ssh -o StrictHostKeyChecking=no"
SPC="sshpass -p password scp -o StrictHostKeyChecking=no"

if ! docker image inspect "$IMG" >/dev/null 2>&1; then
  echo "[bridge] image $IMG missing; build it: docker build -t $IMG -f dut/Dockerfile.build ." >&2
  exit 2
fi

echo "[bridge] building bridge-smoke in $IMG"
docker run --rm --network host -v "$CRATE":/src -w /src "$IMG" \
  cargo build --release --bin bridge-smoke || exit 2
[ -x "$BIN" ] || { echo "[bridge] no binary at $BIN" >&2; exit 2; }

echo "[bridge] shipping into pmon (mgmt -> vlab -> pmon)"
docker cp "$BIN" mgmt:/tmp/bridge-smoke
docker exec mgmt bash -lc "$SPC /tmp/bridge-smoke $DUT:/tmp/bridge-smoke"
docker exec mgmt bash -lc "$SP $DUT 'docker cp /tmp/bridge-smoke pmon:/usr/local/bin/bridge-smoke && docker exec pmon chmod +x /usr/local/bin/bridge-smoke'"

echo "==================== bridge-smoke output ===================="
docker exec mgmt bash -lc "$SP $DUT 'docker exec pmon /usr/local/bin/bridge-smoke'"
rc=$?
echo "==================== end (rc=$rc) ===================="

echo "[bridge] cleanup"
docker exec mgmt bash -lc "$SP $DUT 'docker exec pmon rm -f /usr/local/bin/bridge-smoke; rm -f /tmp/bridge-smoke'" || true
docker exec mgmt rm -f /tmp/bridge-smoke >/dev/null 2>&1 || true
exit $rc
