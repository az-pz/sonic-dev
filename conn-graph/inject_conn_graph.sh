#!/bin/bash
# Inject the vlab connection graph into the docker-sonic-mgmt container's
# ansible/files/ at test runtime. Nothing is committed into the sonic-mgmt repo;
# the container's copy is disposable and rebuilt on every testbed rebuild.
#
# Usage (on the VM):  bash inject_conn_graph.sh
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CNAME=$(docker ps --format '{{.Names}}' | grep -i mgmt | head -1)
FILES_DIR=/data/sonic-mgmt/ansible/files

echo "[inject_conn_graph] container=$CNAME files_dir=$FILES_DIR"

docker cp "$SCRIPT_DIR/sonic_vlab_devices.csv" "$CNAME:$FILES_DIR/sonic_vlab_devices.csv"
docker cp "$SCRIPT_DIR/sonic_vlab_links.csv"   "$CNAME:$FILES_DIR/sonic_vlab_links.csv"

# Add 'vlab' to graph_groups.yml inside the container (idempotent).
docker exec --user root "$CNAME" bash -c '
  GG=/data/sonic-mgmt/ansible/files/graph_groups.yml
  if ! grep -qE "^[[:space:]]*-[[:space:]]*vlab[[:space:]]*$" "$GG"; then
    echo "  - vlab" >> "$GG"
    echo "[inject_conn_graph] added vlab to graph_groups.yml"
  else
    echo "[inject_conn_graph] vlab already in graph_groups.yml"
  fi
  echo "--- graph_groups.yml ---"; cat "$GG"
'
echo "[inject_conn_graph] done"
