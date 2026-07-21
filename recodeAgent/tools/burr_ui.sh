#!/bin/bash
# burr_ui.sh -- open the Burr pipeline UI in your local browser.
#
# The Burr UI server stack (pyarrow) has NO ARM64-Windows wheel, so the UI can't
# run natively on this dev box. Instead it runs on the sonic-dev host (Linux x64)
# and you view it through an SSH tunnel. This script:
#   1. builds the UI image on sonic-dev (one-time),
#   2. syncs your local Burr traces (~/.burr) up to the host,
#   3. (re)starts the UI container there, and
#   4. forwards the port so you can open it locally.
#
# Usage:
#   bash tools/burr_ui.sh          # sync + serve + tunnel (Ctrl-C ends the tunnel)
#   bash tools/burr_ui.sh --stop   # stop the remote UI container
# Then open  http://localhost:7241   (project: recodeagent-xcvrd).
#
# Re-run to refresh traces after a pipeline run. The container keeps running after
# Ctrl-C (only the tunnel closes); re-running re-syncs + reconnects.
set -uo pipefail
SD="${RECODE_SSH_HOST:-sonic-dev}"
PORT="${BURR_PORT:-7241}"
LOCAL_BURR="${BURR_HOME:-$HOME/.burr}"
REMOTE_BURR="recode/burr"
NAME=recode-burr-ui

if [ "${1:-}" = "--stop" ]; then
  ssh "$SD" "docker rm -f $NAME >/dev/null 2>&1 && echo '[burr-ui] stopped' || echo '[burr-ui] not running'"
  exit 0
fi

if ! ssh "$SD" "docker image inspect $NAME >/dev/null 2>&1"; then
  echo "[burr-ui] building the UI image on $SD (one-time, ~1-2 min)"
  ssh "$SD" "printf 'FROM python:3.12\nRUN pip install --no-cache-dir \"burr[start]\"\nEXPOSE 7241\n' > /tmp/burrui.Dockerfile && docker build -t $NAME -f /tmp/burrui.Dockerfile /tmp"
fi

echo "[burr-ui] syncing traces $LOCAL_BURR -> $SD:~/$REMOTE_BURR"
ssh "$SD" "mkdir -p ~/$REMOTE_BURR"
if [ -d "$LOCAL_BURR" ]; then
  tar -C "$LOCAL_BURR" -cf - . 2>/dev/null | ssh "$SD" "tar -C ~/$REMOTE_BURR -xf -"
else
  echo "[burr-ui] no local traces at $LOCAL_BURR yet -- run the pipeline first, then re-run this."
fi

echo "[burr-ui] (re)starting the UI container on $SD"
ssh "$SD" "docker rm -f $NAME >/dev/null 2>&1; docker run -d --name $NAME -p $PORT:$PORT -v ~/$REMOTE_BURR:/root/.burr $NAME burr --host 0.0.0.0 --port $PORT --no-open --no-copy-demo_data >/dev/null; echo '[burr-ui] container started'"

echo "[burr-ui] ===================================================================="
echo "[burr-ui]  Open  http://localhost:$PORT   (project: recodeagent-xcvrd)"
echo "[burr-ui]  Ctrl-C closes the tunnel (UI keeps running; re-run to refresh)."
echo "[burr-ui] ===================================================================="
ssh -N -L "$PORT:localhost:$PORT" "$SD"
