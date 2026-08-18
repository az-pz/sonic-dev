#!/usr/bin/env bash
# run_bench_dom.sh [--ports N] [--polls N] [--trace F] [--time]
#
# Config P: the real Python xcvrd DOM sweep, against the same pymocks plant and the
# SAME Redis instance the Rust configs use -- so the DB work, which dominates a sweep,
# is genuinely shared rather than merely comparable.
#
# Runs in recode-rust-build (CPython 3.13.5, matching the DUT's pmon and the
# interpreter PyO3 embeds) with the swsscommon bindings lifted straight out of pmon.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH="$(cd "$HERE/.." && pwd)"
REPO="$(cd "$BENCH/.." && pwd)"
SWSSLIB="${SWSSLIB:-$HOME/recode/swsslib}"
IMAGE="${RECODE_BUILD_IMAGE:-recode-rust-build}"
REDIS_IMAGE="${REDIS_IMAGE:-redis:7-alpine}"
SOCKDIR="${SOCKDIR:-/tmp/xcvrd-bench-redis}"
CNAME="${CNAME:-xcvrd-bench-redis}"
FIXTURE="${FIXTURE:-$BENCH/fixtures/cmis_40g_lr4.json}"

[ -d "$BENCH/vendor/pydeps/swsscommon" ] || {
  echo "[p] swsscommon not staged -- run ./tools/fetch_python_deps.sh" >&2; exit 2; }
[ -d "$BENCH/vendor/xcvrd/xcvrd" ] || {
  echo "[p] xcvrd not staged in vendor/xcvrd/xcvrd" >&2; exit 2; }

mkdir -p "$SOCKDIR"
if ! docker ps --format '{{.Names}}' | grep -qx "$CNAME" || [ ! -S "$SOCKDIR/redis.sock" ]; then
  docker rm -f "$CNAME" >/dev/null 2>&1
  rm -f "$SOCKDIR/redis.sock"
  docker run -d --name "$CNAME" -v "$SOCKDIR":/sock "$REDIS_IMAGE" \
    redis-server --unixsocket /sock/redis.sock --unixsocketperm 777 \
                 --port 0 --save '' --appendonly no >/dev/null || exit 2
fi
for _ in $(seq 1 50); do [ -S "$SOCKDIR/redis.sock" ] && break; sleep 0.2; done

# Identical baseline for every run, exactly as run_trace.sh does for A and B.
docker exec "$CNAME" redis-cli -s /sock/redis.sock flushall >/dev/null 2>&1

docker run --rm \
  -v "$REPO":"$REPO" \
  -v "$SWSSLIB":/swsslib \
  -v "$SOCKDIR":/sock \
  -v "$BENCH/fixtures/database_config.json":/var/run/redis/sonic-db/database_config.json:ro \
  -v "$BENCH/vendor/swssshare/swss":/usr/share/swss:ro \
  -w "$BENCH" \
  -e LD_LIBRARY_PATH=/swsslib \
  -e PYTHONPATH="$BENCH/vendor/pydeps:$BENCH/vendor/xcvrd:$BENCH/pymocks" \
  "$IMAGE" python3 python/bench_dom.py \
    --fixture "$FIXTURE" \
    --pymocks "$BENCH/pymocks" \
    "$@"
