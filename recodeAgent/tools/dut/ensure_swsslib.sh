#!/bin/bash
# ensure_swsslib.sh -- make sure ~/recode/swsslib holds pmon's libswsscommon.so
# (plus the unversioned dev symlink) so the build container can resolve
# `-lswsscommon` when linking the swss-common crate.
#
# The .so is pulled from the LIVE pmon so its ABI matches the runtime; the built
# binary needs libswsscommon.so.0 in pmon (already present). Runs on the sonic-dev
# host. Idempotent: does nothing if the lib is already staged (pass --force to
# re-pull).
set -uo pipefail
LIBDIR="$HOME/recode/swsslib"
SO="libswsscommon.so.0.0.0"
DUT="admin@10.250.0.101"
SP="sshpass -p password ssh -o StrictHostKeyChecking=no"
SPC="sshpass -p password scp -o StrictHostKeyChecking=no"

mkdir -p "$LIBDIR"
if [ "${1:-}" != "--force" ] && [ -e "$LIBDIR/libswsscommon.so" ]; then
  echo "[swsslib] already staged at $LIBDIR"
  exit 0
fi

echo "[swsslib] pulling $SO from pmon (mgmt -> vlab -> host)"
docker exec mgmt bash -lc "$SP $DUT 'docker cp pmon:/usr/lib/x86_64-linux-gnu/$SO /tmp/$SO'"
docker exec mgmt bash -lc "$SPC $DUT:/tmp/$SO /tmp/$SO"
docker cp "mgmt:/tmp/$SO" "$LIBDIR/$SO"
ln -sf "$SO" "$LIBDIR/libswsscommon.so"
ln -sf "$SO" "$LIBDIR/libswsscommon.so.0"
docker exec mgmt rm -f "/tmp/$SO" >/dev/null 2>&1 || true
docker exec mgmt bash -lc "$SP $DUT 'rm -f /tmp/$SO'" >/dev/null 2>&1 || true
echo "[swsslib] staged:"; ls -l "$LIBDIR"
