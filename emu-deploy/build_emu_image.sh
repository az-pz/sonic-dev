#!/bin/bash
# Build the xcvr-emu emulator Docker image and save it to a tarball so it can be
# shipped to the (offline) DUT and `docker load`-ed there.
#
# The emulator now runs as its OWN standalone container on the DUT — NOT inside
# pmon — so it survives the SONiC `config reload` events that sonic-mgmt tests
# trigger (those restart every SONiC *feature* container, but a plain
# `docker run` container is not a feature and is left untouched).
#
# The image is built from the gsoosk/xcvr-emu fork's `sonic-dev` branch, which
# carries the emulator fixes as real source changes (they used to be applied here
# as build-time patches):
#   1. each transceiver gets its own CMIS MemMap        (src/xcvr_emu/server.py)
#   2. each MemMap gets its own EEPROM byte buffer       (src/cmis/field.py)
#   3. SoftwareReset is Write-Only/Self-Clearing per CMIS
#                                     (src/xcvr_emu/transceiver/transceiver.py)
# The source is cloned/checked-out on demand below, so nothing is patched here.
# Its runtime CMD is `xcvr-emud -c config.yaml` and it serves gRPC on :50051.
#
# Usage:  ./build_emu_image.sh [XCVR_EMU_REPO] [IMAGE_TAG] [OUT_TAR]
# Env:    EMU_REBUILD_IMAGE=1      force a rebuild even if the tarball already exists
#         XCVR_EMU_URL=<git url>   emulator remote (default: gsoosk fork over SSH;
#                                  auto-falls back to HTTPS read-only when SSH auth
#                                  is not configured on this host)
#         XCVR_EMU_BRANCH=<name>   branch to build from (default: sonic-dev)
set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
XCVR_EMU_REPO="${1:-$HERE/../../xcvr-emu}"
IMAGE_TAG="${2:-xcvr-emu:local}"
OUT_TAR="${3:-$HERE/xcvr-emu-image.tar.gz}"
XCVR_EMU_URL="${XCVR_EMU_URL:-git@github.com:gsoosk/xcvr-emu.git}"
XCVR_EMU_URL_HTTPS="${XCVR_EMU_URL_HTTPS:-https://github.com/gsoosk/xcvr-emu.git}"
XCVR_EMU_BRANCH="${XCVR_EMU_BRANCH:-sonic-dev}"

if [ -f "$OUT_TAR" ] && [ "${EMU_REBUILD_IMAGE:-0}" != "1" ]; then
  echo "[image] $OUT_TAR already exists — reusing (set EMU_REBUILD_IMAGE=1 to force rebuild)"
  ls -la "$OUT_TAR"
  exit 0
fi

# --- ensure the xcvr-emu source is the gsoosk fork on the sonic-dev branch ----
# The emulator fixes now live in the branch, so there is nothing to patch here.
# Prefer the SSH remote; fall back to read-only HTTPS when SSH auth isn't set up
# (building the image only needs read access to the repo).
URL="$XCVR_EMU_URL"
if ! GIT_SSH_COMMAND='ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new' \
     git ls-remote "$URL" >/dev/null 2>&1; then
  echo "[image] SSH remote $URL not reachable — falling back to HTTPS read-only ($XCVR_EMU_URL_HTTPS)"
  URL="$XCVR_EMU_URL_HTTPS"
fi

if [ ! -d "$XCVR_EMU_REPO/.git" ]; then
  echo "[image] cloning $URL ($XCVR_EMU_BRANCH) -> $XCVR_EMU_REPO"
  git clone --branch "$XCVR_EMU_BRANCH" "$URL" "$XCVR_EMU_REPO"
else
  # An existing checkout may be a stale upstream clone: point origin at the fork
  # and move onto the sonic-dev branch so we always build the fixed source.
  cur="$(git -C "$XCVR_EMU_REPO" remote get-url origin 2>/dev/null || echo none)"
  if [ "$cur" != "$URL" ]; then
    echo "[image] repointing origin: $cur -> $URL"
    git -C "$XCVR_EMU_REPO" remote set-url origin "$URL"
  fi
  # widen the fetch refspec in case this was a shallow/single-branch clone that
  # only tracked the upstream default branch, then land exactly on the fork branch.
  # -f discards any stale in-tree changes (e.g. the old build-time sed patches).
  git -C "$XCVR_EMU_REPO" config remote.origin.fetch '+refs/heads/*:refs/remotes/origin/*'
  git -C "$XCVR_EMU_REPO" fetch origin "$XCVR_EMU_BRANCH"
  git -C "$XCVR_EMU_REPO" checkout -f -B "$XCVR_EMU_BRANCH" "origin/$XCVR_EMU_BRANCH"
fi

echo "[image] xcvr-emu @ branch $(git -C "$XCVR_EMU_REPO" rev-parse --abbrev-ref HEAD) ($(git -C "$XCVR_EMU_REPO" rev-parse --short HEAD))"
[ -f "$XCVR_EMU_REPO/Dockerfile" ] || { echo "ERROR: Dockerfile not found in $XCVR_EMU_REPO"; exit 1; }

echo "[image] building $IMAGE_TAG from $XCVR_EMU_REPO (branch $XCVR_EMU_BRANCH)"
docker build -t "$IMAGE_TAG" "$XCVR_EMU_REPO"

echo "[image] saving $IMAGE_TAG -> $OUT_TAR"
docker save "$IMAGE_TAG" | gzip > "$OUT_TAR"
echo "[image] wrote $OUT_TAR"
ls -la "$OUT_TAR"
