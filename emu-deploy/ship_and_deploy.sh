#!/bin/bash
# Ship the emulator image + bundle + deploy script to the DUT and run the deploy.
# Run this ON the Azure VM (testbed host).
#
# The emulator now runs as its OWN standalone Docker container on the DUT, so we
# ship the image tarball (docker save|gzip) too and `docker load` it there. The
# The emulator now runs as its OWN standalone Docker container on the DUT, so we
# ship the image tarball (docker save|gzip) too and `docker load` it there. The
# bridge still goes into pmon (via the bundle) and xcvrd is launched directly with
# the env exported (no supervisord).
#
# Prereqs (built by build_emu_image.sh + build_bundle.sh):
#   $1 (or /tmp/emu-bundle.tar.gz)      emu-bundle.tar.gz     — bridge + emu_config.yaml
#   $2 (or alongside the bundle)        xcvr-emu-image.tar.gz — the emulator image
# plus deploy_on_dut.sh in the same dir as this script.
set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUNDLE="${1:-/tmp/emu-bundle.tar.gz}"
IMAGE_TAR="${2:-$(dirname "$BUNDLE")/xcvr-emu-image.tar.gz}"
DEPLOY="$HERE/deploy_on_dut.sh"
CNAME="${MGMT_CONTAINER:-$(docker ps --format '{{.Names}}' | grep -i mgmt | head -1)}"
DUT_IP="${DUT_IP:-10.250.0.101}"
DUT_PASS="${DUT_PASS:-password}"
SSHP="sshpass -p $DUT_PASS"
SSHOPT='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
DUT="admin@$DUT_IP"

[ -f "$IMAGE_TAR" ] || { echo "[ship] ERROR: emulator image tarball not found at $IMAGE_TAR (run build_emu_image.sh)"; exit 1; }

echo "[ship] container=$CNAME bundle=$BUNDLE image=$IMAGE_TAR"
docker cp "$BUNDLE"    "$CNAME":/tmp/emu-bundle.tar.gz
docker cp "$IMAGE_TAR" "$CNAME":/tmp/xcvr-emu-image.tar.gz
docker cp "$DEPLOY"    "$CNAME":/tmp/deploy_on_dut.sh

echo "[ship] scp image + bundle + deploy script to DUT"
docker exec --user azureuser "$CNAME" bash -lc "
  $SSHP scp $SSHOPT /tmp/xcvr-emu-image.tar.gz $DUT:/home/admin/xcvr-emu-image.tar.gz
  $SSHP scp $SSHOPT /tmp/emu-bundle.tar.gz     $DUT:/home/admin/emu-bundle.tar.gz
  $SSHP scp $SSHOPT /tmp/deploy_on_dut.sh      $DUT:/home/admin/deploy_on_dut.sh
"

echo "[ship] unpack + deploy on DUT (runs emulator container + xcvrd)"
docker exec --user azureuser "$CNAME" bash -lc "
  $SSHP ssh $SSHOPT $DUT 'rm -rf /home/admin/emu-bundle && mkdir -p /home/admin/emu-bundle && tar xzf /home/admin/emu-bundle.tar.gz -C /home/admin/emu-bundle && bash /home/admin/deploy_on_dut.sh'
"
