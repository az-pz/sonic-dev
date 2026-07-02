#!/usr/bin/env bash
# End-to-end demo: drive the real SONiC CmisApi against an emulated CMIS module
# through the sonic_platform bridge (gRPC -> xcvr-emu).
#
# Starts its own xcvr-emud inside the container (using the emulator's bundled
# config.yaml: modules 0-6 present as 400G/200G DR4 optics), builds the bridge
# chassis, and reads transceiver identity + application advertisement.
set -euo pipefail

export MSYS_NO_PATHCONV=1
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEV="$(cd "$SCRIPT_DIR" && pwd -W)"
IMAGE="${IMAGE:-sonic-xcvrd-dev}"

docker run --rm \
    -v "$DEV:/dev_scripts:ro" \
    -e PYTHONPATH=/dev_scripts/platform \
    --entrypoint python3 \
    "$IMAGE" /dev_scripts/emu-demo.py
