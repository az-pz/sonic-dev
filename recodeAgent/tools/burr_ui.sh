#!/bin/bash
# burr_ui.sh -- run the Burr pipeline UI LOCALLY, live.
#
# The Burr UI stack (pyarrow) has no ARM64-*Windows* wheel, so it can't run
# natively on this box -- but Docker Desktop's Linux engine (linux/arm64) can,
# and pyarrow ships aarch64 Linux wheels. We run the UI in a local container and
# **bind-mount your real ~/.burr trace directory** into it, so the UI reads the
# exact files the pipeline writes: it updates continuously as runs progress --
# no copying/syncing. Just refresh the browser (the UI also polls on its own).
#
# Usage:
#   bash tools/burr_ui.sh          # build (once) + start the UI, print the URL
#   bash tools/burr_ui.sh --stop   # stop the UI container
#   bash tools/burr_ui.sh --logs   # tail the UI server logs
# Then open  http://localhost:7241   (project: recodeagent-xcvrd).
set -uo pipefail
PORT="${BURR_PORT:-7241}"
NAME=recode-burr-ui
LOCAL_BURR="${BURR_HOME:-$HOME/.burr}"

if ! docker version >/dev/null 2>&1; then
  echo "[burr-ui] Docker isn't reachable. Start Docker Desktop (Linux engine) and retry." >&2
  exit 2
fi

case "${1:-}" in
  --stop) docker rm -f "$NAME" >/dev/null 2>&1 && echo "[burr-ui] stopped" || echo "[burr-ui] not running"; exit 0 ;;
  --logs) exec docker logs -f "$NAME" ;;
esac

# Build the UI image once (linux/arm64; burr[start] pulls fastapi/uvicorn/pyarrow).
if ! docker image inspect "$NAME" >/dev/null 2>&1; then
  echo "[burr-ui] building the UI image (one-time, ~2-3 min)"
  tmp="$(mktemp -d)"
  printf 'FROM python:3.12\nRUN pip install --no-cache-dir "burr[start]"\nEXPOSE %s\n' "$PORT" > "$tmp/Dockerfile"
  docker build -t "$NAME" "$tmp" || { rm -rf "$tmp"; exit 2; }
  rm -rf "$tmp"
fi

mkdir -p "$LOCAL_BURR"
# Docker Desktop on Windows needs a Windows-style source path; cygpath + MSYS_NO_PATHCONV
# keep Git Bash from mangling it. On Linux/macOS use the path as-is.
if command -v cygpath >/dev/null 2>&1; then
  SRC="$(cygpath -w "$LOCAL_BURR")"; export MSYS_NO_PATHCONV=1
else
  SRC="$LOCAL_BURR"
fi

echo "[burr-ui] starting UI with a LIVE bind-mount of $LOCAL_BURR"
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p "$PORT:$PORT" -v "${SRC}:/root/.burr" \
  "$NAME" burr --host 0.0.0.0 --port "$PORT" --no-open --no-copy-demo_data >/dev/null

# Wait for it to answer.
for _ in $(seq 1 15); do
  code="$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$PORT/" 2>/dev/null || true)"
  [ "$code" = "200" ] && break
  sleep 1
done
echo "[burr-ui] ===================================================================="
echo "[burr-ui]  Open  http://localhost:$PORT   (project: recodeagent-xcvrd)"
echo "[burr-ui]  Live: it reads ~/.burr directly -- just refresh after/during a run."
echo "[burr-ui]  Stop with:  bash tools/burr_ui.sh --stop"
echo "[burr-ui] ===================================================================="
