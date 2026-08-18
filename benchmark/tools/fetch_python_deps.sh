#!/usr/bin/env bash
# fetch_python_deps.sh [module ...] -- stage the Python daemon's runtime deps.
#
# Config P must run the REAL Python xcvrd against the REAL swsscommon, otherwise it is
# not comparable with the Rust configs: a DOM sweep is ~97% DB I/O, so pairing Python
# with an in-memory table would hand it a win that has nothing to do with the daemon.
#
# The bindings are pulled straight out of the DUT's pmon container, which is where
# xcvrd actually runs -- so they are the exact build, for the exact CPython (3.13.5),
# that the recode-rust-build image also carries. Nothing is rebuilt or substituted.
#
# Staged into vendor/pydeps, which is gitignored: this is third-party runtime, not
# repo content. Re-run to refresh after a DUT image change.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH="$(cd "$HERE/.." && pwd)"
DEST="$BENCH/vendor/pydeps"
DUT_IP="${DUT_IP:-10.250.0.101}"
DUT_PW="${DUT_PW:-password}"

MODULES=("$@")
[ ${#MODULES[@]} -gt 0 ] || MODULES=(swsscommon sonic_py_common natsort yaml)

dut() {
  docker exec mgmt bash -lc \
    "sshpass -p $DUT_PW ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
     -o ConnectTimeout=10 admin@$DUT_IP '$1'" 2>/dev/null
}

echo "[fetch] resolving ${MODULES[*]} inside pmon"
# Resolve each module's on-disk location from inside pmon rather than assuming a
# dist-packages path: swsscommon lives in /usr/lib, the pip-installed ones in
# /usr/local/lib, and that split is not stable across images.
resolve="import importlib,os,sys
for m in ${MODULES[*]@Q}.split():
    try:
        mod = importlib.import_module(m)
        p = os.path.dirname(mod.__file__)
        print(m + ' ' + p)
    except Exception as e:
        print(m + ' MISSING ' + repr(e), file=sys.stderr)"

paths="$(dut "docker exec pmon python3 -c \"$(echo "$resolve" | tr '\n' ';' | sed 's/"/\\\\\"/g')\"" 2>/dev/null)"

# Simpler and more robust than shipping a python one-liner through three shells:
# copy the well-known locations and let the tar fail loudly if one is absent.
cmd='rm -rf /tmp/pydeps && mkdir -p /tmp/pydeps'
for m in "${MODULES[@]}"; do
  cmd="$cmd && (docker cp pmon:/usr/lib/python3/dist-packages/$m /tmp/pydeps/ 2>/dev/null \
       || docker cp pmon:/usr/local/lib/python3.13/dist-packages/$m /tmp/pydeps/ 2>/dev/null \
       || echo \"MISSING $m\")"
done
cmd="$cmd && sudo rm -rf /tmp/pydeps/*/__pycache__; tar czf /tmp/pydeps.tar.gz -C /tmp/pydeps . && ls -l /tmp/pydeps.tar.gz"

out="$(dut "$cmd")"
echo "$out" | grep -q MISSING && { echo "[fetch] $out" >&2; }

docker exec mgmt bash -lc \
  "sshpass -p $DUT_PW scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
   admin@$DUT_IP:/tmp/pydeps.tar.gz /tmp/pydeps.tar.gz" >/dev/null 2>&1 || {
  echo "[fetch] could not copy from DUT" >&2; exit 2; }
docker cp mgmt:/tmp/pydeps.tar.gz /tmp/pydeps.tar.gz >/dev/null || exit 2

mkdir -p "$DEST"
tar xzf /tmp/pydeps.tar.gz -C "$DEST"
echo "[fetch] staged into $DEST:"
ls -1 "$DEST"
