#!/bin/bash
# Verify the injected vlab graph resolves for vlab-01 via the same code path the
# pytest conn_graph_facts fixture uses (module_utils.graph_utils.find_graph).
set -e
CNAME=$(docker ps --format '{{.Names}}' | grep -i mgmt | head -1)
docker exec --user azureuser "$CNAME" bash -lc '
cd /data/sonic-mgmt/ansible
python3 - <<PY
import sys
sys.path.insert(0, "module_utils")
sys.path.insert(0, "library")
import conn_graph_facts as cgf
cgf.LAB_GRAPHFILE_PATH = "files/"
g = cgf.find_graph(["vlab-01"])
print("find_graph ->", g)
assert g is not None, "vlab-01 not found in any graph group!"
ok, res = g.build_results(["vlab-01"], False)
print("build_results ok:", ok)
dc = res["device_conn"]["vlab-01"]
print("num device_conn ports for vlab-01:", len(dc))
print("sample:", list(dc.items())[:2])
PY
'
