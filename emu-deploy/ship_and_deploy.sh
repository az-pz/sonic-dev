#!/bin/bash
# Ship the emulator bundle + deploy script to the DUT and run the deploy.
# Run this ON the Azure VM (testbed host). Requires the mgmt container up and the
# DUT (vlab-01) reachable at 10.250.0.101 with admin/password.
#
# Prereq: emu-bundle.tar.gz built by build_bundle.sh and present in /tmp (or pass
# its path as $1), plus deploy_on_dut.sh in the same dir as this script.
set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUNDLE="${1:-/tmp/emu-bundle.tar.gz}"
DEPLOY="$HERE/deploy_on_dut.sh"
CNAME="${MGMT_CONTAINER:-$(docker ps --format '{{.Names}}' | grep -i mgmt | head -1)}"
DUT_IP="${DUT_IP:-10.250.0.101}"
DUT_PASS="${DUT_PASS:-password}"
SSHP="sshpass -p $DUT_PASS"
SSHOPT='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
DUT="admin@$DUT_IP"

echo "[ship] container=$CNAME bundle=$BUNDLE"
docker cp "$BUNDLE" "$CNAME":/tmp/emu-bundle.tar.gz
docker cp "$DEPLOY" "$CNAME":/tmp/deploy_on_dut.sh

echo "[ship] scp to DUT"
docker exec --user azureuser "$CNAME" bash -lc "
  $SSHP scp $SSHOPT /tmp/emu-bundle.tar.gz $DUT:/home/admin/emu-bundle.tar.gz
  $SSHP scp $SSHOPT /tmp/deploy_on_dut.sh   $DUT:/home/admin/deploy_on_dut.sh
"

echo "[ship] unpack + deploy on DUT (starts emud + xcvrd)"
docker exec --user azureuser "$CNAME" bash -lc "
  $SSHP ssh $SSHOPT $DUT 'rm -rf /home/admin/emu-bundle && mkdir -p /home/admin/emu-bundle && tar xzf /home/admin/emu-bundle.tar.gz -C /home/admin/emu-bundle && bash /home/admin/deploy_on_dut.sh'
"
