#!/usr/bin/env bash
# Interactive shell with the xcvr-emu emulator running and the sonic_platform
# bridge on PYTHONPATH.
#
# Inside the shell you can, for example:
#   xcvr-emush                       # the emulator's own interactive client
#   python3 -c "from sonic_platform.platform import Platform; \
#               print(Platform().get_chassis().get_sfp(0).get_xcvr_api().get_transceiver_info())"
#
# The emulator (xcvr-emud) is started in the background on :50051 using its
# bundled config.yaml; its log is at /tmp/emud.log. The xcvrd repo is mounted
# read-write at /work so you can also run xcvrd against the emulated optics.
set -euo pipefail

export MSYS_NO_PATHCONV=1
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEV="$(cd "$SCRIPT_DIR" && pwd -W)"
REPO="$(cd "$SCRIPT_DIR/../sonic-platform-daemons" && pwd -W)"
IMAGE="${IMAGE:-sonic-xcvrd-dev}"

TTY_FLAGS=(-i)
if [ -t 0 ]; then TTY_FLAGS+=(-t); fi

BOOTSTRAP='
CFG=$(python3 -c "import os,xcvr_emu;print(os.path.join(os.path.dirname(xcvr_emu.__file__),\"config.yaml\"))")
xcvr-emud -c "$CFG" >/tmp/emud.log 2>&1 &
# Wait until the gRPC port is accepting before handing over, so the first
# command does not race the emulator start-up.
for i in $(seq 1 50); do
  python3 -c "import socket,sys; s=socket.socket(); s.settimeout(0.2); sys.exit(0 if s.connect_ex((\"127.0.0.1\",50051))==0 else 1)" && break
  sleep 0.1
done
echo "xcvr-emud started on :50051 (config: $CFG, log: /tmp/emud.log)"
echo "sonic_platform bridge is on PYTHONPATH; try: xcvr-emush"
exec bash
'

docker run --rm "${TTY_FLAGS[@]}" \
    -v "$DEV:/dev_scripts:ro" \
    -v "$REPO:/work" \
    -e PYTHONPATH=/dev_scripts/platform \
    -e XCVR_EMU_ADDR=localhost:50051 \
    -w /work/sonic-xcvrd \
    "$IMAGE" \
    bash -c "$BOOTSTRAP"
