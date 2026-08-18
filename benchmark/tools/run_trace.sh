#!/usr/bin/env bash
# run_trace.sh --config a|b [--ports N] [--polls N] [--time] [--out F] [--dump-db F]
#
# Drives DomInfoUpdateTask::poll_once and either records what it did (the equivalence
# gate's input) or times it.
#
# Brings up a throwaway Redis on a unix socket first. The target crate's
# XcvrTableHelper::with_mock_tables is #[cfg(test)], so an external crate cannot inject
# in-memory tables; rather than patch the immutable target we use the REAL swss-common
# path, which is also what the daemon actually ships against. The instance is
# socket-only (--port 0), unsaved, and flushed before every run so each config starts
# from an identical empty STATE_DB.
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

BIN="$BENCH/rust/target/release/trace"
[ -x "$BIN" ] || { echo "[trace] not built -- run ./tools/build_rust.sh first" >&2; exit 2; }

mkdir -p "$SOCKDIR"

# Reuse a running instance only if its socket is actually present. A container can
# survive with its socket deleted underneath it (an earlier version of this script did
# exactly that), leaving a "running" server nothing can reach.
if ! docker ps --format '{{.Names}}' | grep -qx "$CNAME" || [ ! -S "$SOCKDIR/redis.sock" ]; then
  docker rm -f "$CNAME" >/dev/null 2>&1
  rm -f "$SOCKDIR/redis.sock"
  docker run -d --name "$CNAME" -v "$SOCKDIR":/sock "$REDIS_IMAGE" \
    redis-server --unixsocket /sock/redis.sock --unixsocketperm 777 \
                 --port 0 --save '' --appendonly no >/dev/null || {
    echo "[trace] could not start redis" >&2; exit 2; }
fi

for _ in $(seq 1 50); do [ -S "$SOCKDIR/redis.sock" ] && break; sleep 0.2; done
[ -S "$SOCKDIR/redis.sock" ] || { echo "[trace] redis socket never appeared" >&2; exit 2; }

# Identical baseline for every run: a leftover row would change which branch the
# posters take and silently make two configs incomparable.
docker exec "$CNAME" redis-cli -s /sock/redis.sock flushall >/dev/null 2>&1

docker run --rm \
  -v "$REPO":"$REPO" \
  -v "$SWSSLIB":/swsslib \
  -v "$SOCKDIR":/sock \
  -w "$BENCH/rust" \
  -e LD_LIBRARY_PATH=/swsslib \
  -e REDIS_SOCK=/sock/redis.sock \
  "$IMAGE" ./target/release/trace \
    --fixture "$FIXTURE" \
    --pymocks "$BENCH/pymocks" \
    "$@"
