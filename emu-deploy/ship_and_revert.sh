#!/bin/bash
# Ship revert_on_dut.sh to the DUT and run it: restore the stock host
# sonic_platform, restore pmon_daemon_control.json (skip_xcvrd), remove the pmon
# injection, and restart pmon. Leaves the xcvr-emu container alone.
#
# Run this ON the VM (testbed host).
set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REVERT="$HERE/revert_on_dut.sh"
CNAME="${MGMT_CONTAINER:-$(docker ps --format '{{.Names}}' | grep -i mgmt | head -1)}"
CTR_USER="${CTR_USER:-$(id -un)}"
DUT_IP="${DUT_IP:-10.250.0.101}"
DUT_PASS="${DUT_PASS:-password}"
SSHP="sshpass -p $DUT_PASS"
SSHOPT='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
DUT="admin@$DUT_IP"

[ -f "$REVERT" ] || { echo "[revert] ERROR: revert_on_dut.sh not found at $REVERT"; exit 1; }

echo "[revert] container=$CNAME — shipping + running revert on $DUT"
docker cp "$REVERT" "$CNAME":/tmp/revert_on_dut.sh
docker exec --user "$CTR_USER" "$CNAME" bash -lc "$SSHP scp $SSHOPT /tmp/revert_on_dut.sh $DUT:/home/admin/revert_on_dut.sh"
docker exec --user "$CTR_USER" "$CNAME" bash -lc "$SSHP ssh $SSHOPT $DUT 'bash /home/admin/revert_on_dut.sh'"
echo "[revert] done — DUT restored to stock platform"
