#!/bin/bash
# Provision the two "special" emulator modules the xcvrd-tests suite needs, by
# patching the DEPLOYED emu_config.yaml in place and restarting the emulator +
# xcvrd. Idempotent. Run ON the VM (testbed host). These modules are NOT part of
# gen_emu_config.py, so re-run this after any full emulator redeploy.
#
#   idx10 (Ethernet40)  ->  type: sff8636        (SFF-8636 / QSFP28 -> tests/test_sff8636.py)
#   idx11 (Ethernet44)  ->  MediaInterfaceID 77  (400GBASE-ZR -> coherent C-CMIS -> tests/test_pm.py)
#   idx13 (Ethernet52)  ->  MemoryModel FLAT     (flat memory -> tests/test_flat_memory.py)
#   idx14 (Ethernet56)  ->  2 apps (40G + 100G)  (application selection across speeds -> tests/test_app_select.py)
#
# The emulator serves any page as raw bytes, so the SFF byte image and the C-CMIS
# PM / VDM stimulus are provisioned by the tests themselves; only these two config
# properties (which the emulator resets from config on every plug) must live here.
set -uo pipefail
CNAME="${MGMT_CONTAINER:-$(docker ps --format '{{.Names}}' | grep -i mgmt | head -1)}"
CTR_USER="${CTR_USER:-$(id -un)}"
DUT_IP="${DUT_IP:-10.250.0.101}"; DUT_PASS="${DUT_PASS:-password}"
SSHOPT='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'

read -r -d '' VLAB <<'VLABEOF'
set -uo pipefail
echo "[vlab] patching emu_config.yaml: idx10 -> sff8636, idx11 -> coherent(400GBASE-ZR)"
sudo python3 - <<'PY'
import yaml, copy
p = "/home/admin/emu_config.yaml"
cfg = yaml.safe_load(open(p))
tr = cfg["transceivers"]

# idx10: SFF-8636 (QSFP28) module type.
base10 = copy.deepcopy(tr[0])
base10["type"] = "sff8636"
tr[10] = base10

# idx11: coherent C-CMIS -> advertise a 400GBASE-ZR media interface (code 77 at 00h:87).
base11 = copy.deepcopy(tr[0])
base11["defaults"]["ApplicationDescriptor"][0]["MediaInterfaceID"] = 77
tr[11] = base11

# idx13: flat-memory module -> CmisManagerTask short-circuits to READY (00h:2.7).
base13 = copy.deepcopy(tr[0])
base13["defaults"]["MemoryModel"] = "FLAT"
tr[13] = base13

# idx14: multi-application module -> app1 = XLAUI 40G (default), app2 = CAUI-4 100G,
# both 4-lane. Lets the app-selection test change the port speed 40G<->100G and observe
# xcvrd select the matching CMIS application (AppSelCode 1 vs 2). MediaLaneAssignmentOptions
# is a separate per-app list; both need a non-zero entry or get_cmis_media_lanes_mask == 0.
# The 40G<->100G switch drives the CMIS decommission -> re-provision handshake, which needs
# the emulator's "ConfigSuccess on decommission" fix (xcvr-emu feature/multi-app-datapath).
base14 = copy.deepcopy(tr[0])
app0 = base14["defaults"]["ApplicationDescriptor"][0]
app1 = copy.deepcopy(app0)
app1["HostInterfaceID"] = 11          # CAUI-4 C2M (Annex 83E) -> 100G, 4 host lanes
app1["HostLaneCount"] = 4
app1["MediaLaneCount"] = 4
app1["HostLaneAssignmentOptions"] = 1
base14["defaults"]["ApplicationDescriptor"] = [app0, app1]
mla = list(base14["defaults"].get("MediaLaneAssignmentOptions", [1]))
base14["defaults"]["MediaLaneAssignmentOptions"] = (mla + [1, 1])[:2]
tr[14] = base14

yaml.safe_dump(cfg, open(p, "w"))
print("  idx10 type:", tr[10].get("type"))
print("  idx11 MediaInterfaceID:", tr[11]["defaults"]["ApplicationDescriptor"][0]["MediaInterfaceID"])
print("  idx13 MemoryModel:", tr[13]["defaults"].get("MemoryModel"))
print("  idx14 apps:", [a.get("HostInterfaceID") for a in tr[14]["defaults"]["ApplicationDescriptor"]])
PY

echo "[vlab] restart emulator + xcvrd"
docker restart xcvr-emu >/dev/null; sleep 5
docker exec pmon supervisorctl restart xcvrd >/dev/null 2>&1 || true
for i in $(seq 1 24); do
  sleep 5
  sff=$(sonic-db-cli STATE_DB HGET 'TRANSCEIVER_INFO|Ethernet40' type 2>/dev/null)
  coh=$(sonic-db-cli STATE_DB HGET 'TRANSCEIVER_INFO|Ethernet44' supported_max_laser_freq 2>/dev/null)
  echo "  [$((i*5))s] Ethernet40 type='$sff'  Ethernet44 coherent=$([ -n "$coh" ] && echo yes || echo no)"
  case "$sff" in *QSFP28*) [ -n "$coh" ] && break;; esac
done
echo "[vlab] done. Ethernet40=$(sonic-db-cli STATE_DB HGET 'TRANSCEIVER_INFO|Ethernet40' type) | Ethernet44 coherent marker=$(sonic-db-cli STATE_DB HGET 'TRANSCEIVER_INFO|Ethernet44' supported_max_laser_freq)"
echo "[vlab] Ethernet52 (flat) cmis_state=$(sonic-db-cli STATE_DB HGET 'TRANSCEIVER_STATUS_SW|Ethernet52' cmis_state)"
echo "[vlab] Ethernet56 (multi-app) apps=$(sonic-db-cli STATE_DB HGET 'TRANSCEIVER_INFO|Ethernet56' application_advertisement | head -c 80)"
echo "===SPECIAL_MODULES_DONE==="
VLABEOF

B64=$(printf '%s' "$VLAB" | base64 | tr -d '\n')
docker exec --user "$CTR_USER" "$CNAME" bash -lc \
  "sshpass -p $DUT_PASS ssh $SSHOPT admin@$DUT_IP \"echo $B64 | base64 -d > /home/admin/special_modules.sh && bash /home/admin/special_modules.sh\""
