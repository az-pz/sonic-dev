#!/bin/bash
# Build the xcvr-emu emulator Docker image and save it to a tarball so it can be
# shipped to the (offline) DUT and `docker load`-ed there.
#
# The emulator now runs as its OWN standalone container on the DUT — NOT inside
# pmon — so it survives the SONiC `config reload` events that sonic-mgmt tests
# trigger (those restart every SONiC *feature* container, but a plain
# `docker run` container is not a feature and is left untouched).
#
# The image is built straight from the upstream repo's Dockerfile (unmodified);
# its runtime CMD is `xcvr-emud -c config.yaml` and it serves gRPC on :50051.
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

echo "[image] building $IMAGE_TAG from $XCVR_EMU_REPO (upstream Dockerfile, unmodified)"
docker build -t "$IMAGE_TAG" "$XCVR_EMU_REPO"

echo "[image] saving $IMAGE_TAG -> $OUT_TAR"
docker save "$IMAGE_TAG" | gzip > "$OUT_TAR"
echo "[image] wrote $OUT_TAR"
ls -la "$OUT_TAR"
