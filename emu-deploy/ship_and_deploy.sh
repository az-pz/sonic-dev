#!/bin/bash
# Ship the emulator image + bundle + native deploy/revert scripts to the DUT and
# run the NATIVE deploy:
#   1. HOST sonic_platform := our emulator-backed bridge  (fixes host sfputil/reset)
#   2. flip skip_xcvrd -> false                            (enable xcvrd natively)
#   3. inject the bridge into pmon                         (so native xcvrd imports it)
# plus the standalone xcvr-emu --network host container.
#
# Run this ON the VM (testbed host). Robust to the ssh drop that the pmon restart
# can cause: the deploy runs DETACHED on the DUT and we poll its log for the
# EMU_DEPLOY_DONE marker.
#
# Prereqs (built by build_emu_image.sh + build_bundle.sh):
#   $1 (or /tmp/emu-bundle.tar.gz)   emu-bundle.tar.gz     — bridge + emu_config.yaml
#   $2 (or alongside the bundle)     xcvr-emu-image.tar.gz — the emulator image
# plus deploy_on_dut.sh + revert_on_dut.sh next to this script.
set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUNDLE="${1:-/tmp/emu-bundle.tar.gz}"
IMAGE_TAR="${2:-$(dirname "$BUNDLE")/xcvr-emu-image.tar.gz}"
DEPLOY="$HERE/deploy_on_dut.sh"
REVERT="$HERE/revert_on_dut.sh"
CNAME="${MGMT_CONTAINER:-$(docker ps --format '{{.Names}}' | grep -i mgmt | head -1)}"
# The sonic-mgmt container is created with a user matching the host user (see
# setup-container.sh), so `docker exec --user` must use THAT user, not a
# hardcoded one. Default to the user running this script (overridable via env).
CTR_USER="${CTR_USER:-$(id -un)}"
DUT_IP="${DUT_IP:-10.250.0.101}"
DUT_PASS="${DUT_PASS:-password}"
EMU_TEST_HOOKS="${EMU_TEST_HOOKS:-0}"   # forwarded to deploy_on_dut.sh (error-injection hook)
EMU_DEBUG="${EMU_DEBUG:-0}"             # forwarded to deploy_on_dut.sh (emulator -v)
SSHP="sshpass -p $DUT_PASS"
SSHOPT='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
DUT="admin@$DUT_IP"

[ -f "$IMAGE_TAR" ] || { echo "[ship] ERROR: emulator image tarball not found at $IMAGE_TAR (run build_emu_image.sh)"; exit 1; }
[ -f "$DEPLOY" ]    || { echo "[ship] ERROR: deploy_on_dut.sh not found at $DEPLOY"; exit 1; }

echo "[ship] container=$CNAME  bundle=$BUNDLE  image=$IMAGE_TAR"
docker cp "$BUNDLE"    "$CNAME":/tmp/emu-bundle.tar.gz
docker cp "$IMAGE_TAR" "$CNAME":/tmp/xcvr-emu-image.tar.gz
docker cp "$DEPLOY"    "$CNAME":/tmp/deploy_on_dut.sh
docker cp "$REVERT"    "$CNAME":/tmp/revert_on_dut.sh

echo "[ship] scp image + bundle + scripts to DUT"
docker exec --user "$CTR_USER" "$CNAME" bash -lc "
  $SSHP scp $SSHOPT /tmp/xcvr-emu-image.tar.gz $DUT:/home/admin/xcvr-emu-image.tar.gz
  $SSHP scp $SSHOPT /tmp/emu-bundle.tar.gz     $DUT:/home/admin/emu-bundle.tar.gz
  $SSHP scp $SSHOPT /tmp/deploy_on_dut.sh      $DUT:/home/admin/deploy_on_dut.sh
  $SSHP scp $SSHOPT /tmp/revert_on_dut.sh      $DUT:/home/admin/revert_on_dut.sh
"

echo "[ship] unpack bundle + launch native deploy (detached — survives the pmon-restart ssh drop)"
docker exec --user "$CTR_USER" "$CNAME" bash -lc "
  $SSHP ssh $SSHOPT $DUT 'rm -rf /home/admin/emu-bundle && mkdir -p /home/admin/emu-bundle && tar xzf /home/admin/emu-bundle.tar.gz -C /home/admin/emu-bundle && rm -f /home/admin/native.log && nohup env EMU_TEST_HOOKS=$EMU_TEST_HOOKS EMU_DEBUG=$EMU_DEBUG bash /home/admin/deploy_on_dut.sh > /home/admin/native.log 2>&1 & echo launched'
"

echo "[ship] waiting for deploy to complete (marker EMU_DEPLOY_DONE, up to ~6 min)..."
done=0
for i in $(seq 1 60); do
  sleep 6
  if docker exec --user "$CTR_USER" "$CNAME" bash -lc "$SSHP ssh $SSHOPT $DUT 'grep -q EMU_DEPLOY_DONE /home/admin/native.log 2>/dev/null'" 2>/dev/null; then
    done=1; break
  fi
done

echo "[ship] --- deploy log tail ---"
docker exec --user "$CTR_USER" "$CNAME" bash -lc "$SSHP ssh $SSHOPT $DUT 'tail -25 /home/admin/native.log'" 2>/dev/null || true
[ "$done" = "1" ] || { echo "[ship] ERROR: native deploy did not signal completion in time (see log above)"; exit 1; }

# Stamp a non-"vs" asic_type so sonic-mgmt actually RUNS the transceiver/SFP
# suite against the emulator. Current sonic-mgmt marks that whole suite
# skip/xfail when asic_type == "vs" (it assumes a virtual switch has no optics),
# and it resolves asic_type from TWO places:
#   * conditional_mark plugin  -> `dut_basic_facts` -> /etc/sonic/sonic_version.yml
#   * duthost.facts            -> sonic_basic_facts -> platform.json (updates over
#                                 the version info)
# The xcvr-emu emulator DOES provide transceivers, so override asic_type in BOTH
# as part of the emulator setup. Done BEFORE clearing the facts caches below.
# Configurable via EMU_ASIC_TYPE (default broadcom); set to "vs" to keep stock.
# base64 avoids nested ssh/quoting issues.
EMU_ASIC_TYPE="${EMU_ASIC_TYPE:-broadcom}"
echo "[ship] stamping asic_type=$EMU_ASIC_TYPE on the DUT (sonic_version.yml + platform.json) so the vs-gated transceiver suite runs against the emulator"
ASIC_PY='import sys, re, json
val = sys.argv[1]
# 1) sonic_version.yml — authoritative for dut_basic_facts / conditional_mark
vf = "/etc/sonic/sonic_version.yml"
try:
    s = open(vf).read()
    if re.search(r"^asic_type:", s, re.M):
        s = re.sub(r"^asic_type:.*$", "asic_type: %s" % val, s, flags=re.M)
    else:
        s = s.rstrip("\n") + "\nasic_type: %s\n" % val
    open(vf, "w").write(s)
    print("set asic_type=%s in %s" % (val, vf))
except Exception as e:
    print("WARN sonic_version.yml:", e)
# 2) platform.json — for duthost.facts (sonic_basic_facts updates over version info)
try:
    from sonic_py_common import device_info
    pj = "/usr/share/sonic/device/%s/platform.json" % device_info.get_platform()
    try:
        d = json.load(open(pj))
    except FileNotFoundError:
        d = {}
    d["asic_type"] = val
    json.dump(d, open(pj, "w"), indent=2)
    print("set asic_type=%s in %s" % (val, pj))
except Exception as e:
    print("WARN platform.json:", e)'
ASIC_B64="$(printf '%s' "$ASIC_PY" | base64 | tr -d '\n')"
docker exec --user "$CTR_USER" "$CNAME" bash -lc \
  "$SSHP ssh $SSHOPT $DUT \"echo $ASIC_B64 | base64 -d | sudo python3 - $EMU_ASIC_TYPE\"" \
  || echo "[ship] WARN: could not stamp asic_type (transceiver suite may stay vs-skipped)"

# The DUT's platform.json / sonic_version.yml changed. duthost.facts AND the
# conditional_mark plugin cache facts, so clear both so the new asic_type is seen.
echo "[ship] clearing mgmt duthost.facts + conditional_mark BASIC_FACTS cache so the new asic_type is picked up"
docker exec --user "$CTR_USER" "$CNAME" bash -lc "rm -f /data/sonic-mgmt/tests/_cache/*/basic_facts.pickle 2>/dev/null; rm -rf /data/sonic-mgmt/.pytest_cache/v/BASIC_FACTS_* /data/sonic-mgmt/tests/.pytest_cache/v/BASIC_FACTS_* 2>/dev/null; true"

# Install the declarative transceiver inventory (emulator-specific expected-optic
# data) into the mgmt container so the tests/transceiver/ suite can load it. This
# lives beside the emulator config because it MIRRORS what the emulator reports
# (vendor xcvr-emu, PN EMU-40G-LR4, 40G ports — see gen_emu_config.py). It must
# land in the mgmt container's sonic-mgmt repo (the test runner), which the DUT
# cannot write, so it is done here rather than in deploy_on_dut.sh. Idempotent;
# container copy only — nothing is committed into the sonic-mgmt repo.
INV_DIR="$HERE/transceiver-inventory"
if [ -d "$INV_DIR" ]; then
  echo "[ship] installing transceiver inventory into mgmt container"
  INV_DST=/data/sonic-mgmt/ansible/files/transceiver/inventory
  tar czf /tmp/xcvr-inv.tgz -C "$INV_DIR" .
  docker cp /tmp/xcvr-inv.tgz "$CNAME":/tmp/xcvr-inv.tgz
  rm -f /tmp/xcvr-inv.tgz
  docker exec --user root "$CNAME" bash -c \
    "mkdir -p '$INV_DST' && tar xzf /tmp/xcvr-inv.tgz -C '$INV_DST' && rm -f /tmp/xcvr-inv.tgz"
  if docker exec --user root "$CNAME" test -f "$INV_DST/normalization_mappings.json"; then
    echo "[ship] transceiver inventory installed at $INV_DST"
  else
    echo "[ship] ERROR: transceiver inventory install failed (missing files under $INV_DST)"; exit 1
  fi
else
  echo "[ship] no transceiver-inventory beside this script — skipping inventory install"
fi

echo "[ship] native deploy complete"
