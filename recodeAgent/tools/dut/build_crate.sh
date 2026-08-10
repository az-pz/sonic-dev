#!/bin/bash
# Build the xcvrd-rs crate for pmon, inside the Debian-13 build container.
# Runs on the sonic-dev host (has docker + internet). Produces a glibc-2.41
# binary that links libpython3.13 (for the PyO3 platform-bridge).
#
# Usage: build_crate.sh <CRATE_DIR>
set -uo pipefail
CRATE_DIR="${1:-$HOME/recode/crate}"
IMG=recode-rust-build
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! docker image inspect "$IMG" >/dev/null 2>&1; then
  echo "[build] image $IMG missing; build it first: docker build -t $IMG -f dut/Dockerfile.build ." >&2
  exit 2
fi

# Ensure libswsscommon.so is staged for the linker. xcvrd-rs will depend on
# swss-common (STATE_DB) once the agents wire it in; staging now is harmless for
# the current skeleton and makes that build "just work".
bash "$HERE/ensure_swsslib.sh" || true

echo "[build] cargo build --release in $IMG (crate=$CRATE_DIR)"
# Mount the crate; keep target/ in it so the cargo cache persists across runs.
# Mount swsslib + set RUSTFLAGS so `-lswsscommon` resolves (SWSS_COMMON_REPO +
# BINDGEN_EXTRA_CLANG_ARGS are baked into the image).
# --network host so cargo can fetch crates.io + git deps (pyo3, swss-common, ...).
docker run --rm --network host \
  -v "$CRATE_DIR":/src -v "$HOME/recode/swsslib":/swsslib -w /src \
  -e RUSTFLAGS="-L native=/swsslib" \
  "$IMG" \
  cargo build --release --bin xcvrd-rs
rc=$?
BIN="$CRATE_DIR/target/release/xcvrd-rs"
if [ $rc -ne 0 ] || [ ! -x "$BIN" ]; then
  echo "[build] FAILED (rc=$rc, binary present: $([ -x "$BIN" ] && echo yes || echo no))" >&2
  exit 2
fi
echo "[build] OK -> $BIN"
