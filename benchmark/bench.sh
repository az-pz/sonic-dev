#!/usr/bin/env bash
# bench.sh -- build a Rust xcvrd translation and benchmark it against the Python
# reference on the live DUT, emitting a single self-describing JSON result.
#
#   ./bench.sh recodeAgent/results/result_4              # build + run everything
#   ./bench.sh result_4                                  # same, shorthand
#   ./bench.sh result_4 --scenario B9                    # one scenario
#   ./bench.sh result_4 --scenario B4,B9                 # just those two
#   ./bench.sh result_4 --build-only                     # build, do not measure
#   ./bench.sh --list                                    # what can be run
#
# Replaces the previous nine scripts under tools/. Those grew one-per-task and left
# the important invariants (provenance, restore-on-exit, interleaving) implemented in
# some paths and not others; folding them into one place makes those invariants
# unconditional rather than per-script.
#
# WHY A CRATE ARGUMENT IS MANDATORY. The old driver defaulted to a staging directory
# that the validation pipeline overwrites, so "the rust variant" was whatever happened
# to be sitting there. That produced two headline findings which did not reproduce on
# a second host, because the hosts had different unnamed crates staged. Every run now
# builds from a named result_N and records what it measured, so a stored number can
# always be traced back to a specific translation.
#
# Options:
#   -s, --scenario IDS    run only these scenarios; comma- or space-separated, and
#                         repeatable (B1 B2 B3 B4 B5 B6 B7 B8 B9 B10 B11 B12).
#                         An unknown id is an error, not a silent skip.
#   -b, --build-only      build the daemon (and harness), then stop
#       --skip-build      measure using the existing binary (NOT recommended; the
#                         result is only as attributable as that binary)
#       --variants LIST   comma list of rust,python          (default: rust,python)
#       --reps N          repetitions per scenario           (default: 1)
#       --dom-interval S  DOM poll interval given to BOTH daemons (default: 5, or
#                         $DOM_UPDATE_INTERVAL -- the same knob setup-sonic-testbed.sh uses).
#                         The reference defaults to 60s and reads this only from argv,
#                         so leaving it unset would compare a 60s Python against a 5s
#                         Rust -- an order of magnitude apart in polling work.
#       --duration S      seconds for soak-style scenarios   (default: 30)
#       --settle S        quiet period before each DUT scenario (default: 20; env
#                         SETTLE_SECS). Do not set to 0 for multi-scenario runs -- the
#                         stimulus scenarios leave the daemon busy and the next one
#                         will measure that instead of what it intends to.
#   -o, --out FILE        output json (default: results/run-<crate>-<ts>.json)
#       --vendor          stage the Python reference runtime from the DUT's pmon
#                         container into vendor/ (needed for the in-process config p)
#   -l, --list            list scenarios and exit
#   -h, --help            this text
set -uo pipefail

BENCH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$BENCH/.." && pwd)"
DUT_IP="${DUT_IP:-10.250.0.101}"
DUT_PW="${DUT_PW:-password}"
STAGE=/tmp/xbench
IMAGE="${RECODE_BUILD_IMAGE:-recode-rust-build}"
SWSSLIB="${SWSSLIB:-$HOME/recode/swsslib}"

# DUT scenarios drive the real supervised daemon and assume only the deployed
# contract (sonic_platform + STATE_DB), so they work for ANY translation.
# In-process scenarios link the target as a library and implement its Hal/SfpHandle
# traits -- private internals that differ per translation -- so they only build
# against result_3/result_4-shaped crates and are skipped otherwise.
DUT_SCENARIOS="B1 B2 B3 B5 B6 B8 B9 B10 B11"
INPROC_SCENARIOS="B4 B7 B12"

CRATE_ARG=""; ONE=""; BUILD_ONLY=0; SKIP_BUILD=0; LIST=0; VENDOR=0
VARIANTS="rust,python"; REPS=1; DURATION=30; OUT=""; DOM_INTERVAL="${DOM_UPDATE_INTERVAL:-5}"

die() { echo "[bench] $*" >&2; exit 2; }
log() { echo "[bench] $*"; }

usage() { sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    -s|--scenario)  ONE="$ONE ${2:?}"; shift 2 ;;
    -b|--build-only) BUILD_ONLY=1; shift ;;
    --skip-build)   SKIP_BUILD=1; shift ;;
    --variants)     VARIANTS="${2:?}"; shift 2 ;;
    --reps)         REPS="${2:?}"; shift 2 ;;
    --dom-interval) DOM_INTERVAL="${2:?}"; shift 2 ;;
    --duration)     DURATION="${2:?}"; shift 2 ;;
    --settle)       SETTLE_SECS="${2:?}"; export SETTLE_SECS; shift 2 ;;
    -o|--out)       OUT="${2:?}"; shift 2 ;;
    -l|--list)      LIST=1; shift ;;
    --vendor)       VENDOR=1; shift ;;
    -h|--help)      usage; exit 0 ;;
    -*)             die "unknown option $1 (try --help)" ;;
    *)              CRATE_ARG="$1"; shift ;;
  esac
done

# ---------------------------------------------------------------- scenario list
scenario_meta() {  # id -> "harness|description"
  case "$1" in
    B1)  echo "dut|cold start to first TRANSCEIVER_INFO on every port" ;;
    B2)  echo "dut|single-port hot plug / unplug latency" ;;
    B3)  echo "dut|CMIS bring-up: plug to cmis_state READY" ;;
    B4)  echo "inprocess|DOM sweep duration, configs A/B/P" ;;
    B5)  echo "dut|idle soak: RSS, CPU, threads, fds" ;;
    B6)  echo "dut|plug storm: all modules at once" ;;
    B7)  echo "inprocess|fan-out sweep: latency and slope vs port count" ;;
    B8)  echo "dut|fault set/clear latency via the bridge error hook" ;;
    B9)  echo "dut|EEPROM work per cycle (THE VALIDITY GATE)" ;;
    B10) echo "dut|SIGTERM to process exit" ;;
    B11) echo "dut|media-settings notify on insert" ;;
    B12) echo "inprocess|PyO3 boundary cost, ns per call" ;;
    *)   echo "" ;;
  esac
}

# Normalise the requested set: -s/--scenario may be repeated and may carry a
# comma-separated list, so "B4,B9", "-s B4 -s B9" and "-s 'B4 B9'" all mean the same
# thing. Uppercased so "b9" works.
ONE="$(printf '%s' "$ONE" | tr ',' ' ' | tr '[:lower:]' '[:upper:]' | xargs 2>/dev/null || true)"
# Validate every id NOW. Previously an unknown id just failed to match any scenario
# and the run quietly produced a shorter result set that still looked complete --
# exactly how B7 stayed silently unmeasured for several runs.
for s in $ONE; do
  [ -n "$(scenario_meta "$s")" ] \
    || die "unknown scenario '$s' (see --list for the valid ids)"
done

if [ "$LIST" = 1 ]; then
  printf '%-5s %-11s %s\n' ID HARNESS DESCRIPTION
  for s in B1 B2 B3 B4 B5 B6 B7 B8 B9 B10 B11 B12; do
    m="$(scenario_meta "$s")"
    printf '%-5s %-11s %s\n' "$s" "${m%%|*}" "${m#*|}"
  done
  echo
  echo "dut       = real supervised daemon on vlab-01; works for ANY translation."
  echo "inprocess = links the target as a library; needs a result_3/result_4-shaped"
  echo "            crate (Hal + SfpHandle traits) and is skipped otherwise."
  exit 0
fi

if [ "$VENDOR" = 1 ]; then
  # Pull the exact builds xcvrd runs against out of pmon -- same CPython (3.13), same
  # swsscommon -- rather than rebuilding or substituting, so config p is the reference
  # daemon on its real runtime.
  log "staging python runtime from the DUT's pmon container"
  D="$BENCH/vendor/pydeps"; mkdir -p "$D"
  cmd='rm -rf /tmp/pydeps && mkdir -p /tmp/pydeps'
  for m in swsscommon sonic_py_common natsort yaml sonic_platform_base; do
    cmd="$cmd && (docker cp pmon:/usr/lib/python3/dist-packages/$m /tmp/pydeps/ 2>/dev/null || docker cp pmon:/usr/local/lib/python3.13/dist-packages/$m /tmp/pydeps/ 2>/dev/null || echo MISSING_$m)"
  done
  cmd="$cmd; sudo rm -rf /tmp/pydeps/*/__pycache__; tar czf /tmp/pydeps.tar.gz -C /tmp/pydeps ."
  docker exec mgmt bash -lc "sshpass -p $DUT_PW ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null admin@$DUT_IP '$cmd'" >/dev/null 2>&1
  docker exec mgmt bash -lc "sshpass -p $DUT_PW scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null admin@$DUT_IP:/tmp/pydeps.tar.gz /tmp/" >/dev/null 2>&1
  docker cp mgmt:/tmp/pydeps.tar.gz /tmp/pydeps.tar.gz >/dev/null 2>&1 && tar xzf /tmp/pydeps.tar.gz -C "$D" && log "  pydeps: $(ls "$D" | tr '\n' ' ')"
  # swss lua scripts: ProducerStateTable refuses to construct without them
  S="$BENCH/vendor/swssshare"; mkdir -p "$S"
  docker exec mgmt bash -lc "sshpass -p $DUT_PW ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null admin@$DUT_IP 'rm -rf /tmp/swssshare && mkdir -p /tmp/swssshare && docker cp pmon:/usr/share/swss /tmp/swssshare/ && tar czf /tmp/swssshare.tar.gz -C /tmp/swssshare swss'" >/dev/null 2>&1
  docker exec mgmt bash -lc "sshpass -p $DUT_PW scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null admin@$DUT_IP:/tmp/swssshare.tar.gz /tmp/" >/dev/null 2>&1
  docker cp mgmt:/tmp/swssshare.tar.gz /tmp/swssshare.tar.gz >/dev/null 2>&1 && tar xzf /tmp/swssshare.tar.gz -C "$S" && log "  swss lua: staged"
  # The reference daemon source. Prefer the copy recorded in the repo -- that is the
  # exact tree the translation was produced from, so config p benchmarks the same
  # reference the pipeline translated, not whatever the DUT image happens to ship.
  mkdir -p "$BENCH/vendor/xcvrd"
  if [ -d "$REPO/recodeAgent/source/xcvrd" ]; then
    rm -rf "$BENCH/vendor/xcvrd/xcvrd"
    cp -r "$REPO/recodeAgent/source/xcvrd" "$BENCH/vendor/xcvrd/xcvrd"
    find "$BENCH/vendor/xcvrd" -name __pycache__ -prune -exec rm -rf {} + 2>/dev/null
    log "  xcvrd: recodeAgent/source/xcvrd ($(find "$BENCH/vendor/xcvrd" -name '*.py' | wc -l) files)"
  else
    log "  NOTE: recodeAgent/source/xcvrd not found; config p will be unavailable"
  fi
  log "vendoring done"
  exit 0
fi

[ -n "$CRATE_ARG" ] || die "need a rust result to benchmark, e.g.
    ./bench.sh recodeAgent/results/result_4
    ./bench.sh result_4 --scenario B9
  (--list shows the scenarios)"

# ------------------------------------------------------------- resolve the crate
# Accept result_4, results/result_4, an absolute path, or the crate dir itself.
CRATE=""
for cand in "$CRATE_ARG" "$REPO/$CRATE_ARG" "$REPO/recodeAgent/results/$CRATE_ARG"; do
  [ -d "$cand/crate" ] && { CRATE="$cand/crate"; break; }
  [ -d "$cand/xcvrd-rs" ] && { CRATE="$cand"; break; }
done
[ -n "$CRATE" ] || die "no crate found for '$CRATE_ARG'. Available:
$(ls -d "$REPO"/recodeAgent/results/*/ 2>/dev/null | sed 's|.*/results/|    |;s|/$||')"
CRATE="$(cd "$CRATE" && pwd)"
CRATE_NAME="$(basename "$(dirname "$CRATE")")"
RUST_BIN="$CRATE/target/release/xcvrd-rs"

RUN_TS="$(date -Is)"
RUN_ID="${CRATE_NAME}-$(date +%Y%m%d-%H%M%S)"
[ -n "$OUT" ] || OUT="$BENCH/results/run-${RUN_ID}.json"
mkdir -p "$(dirname "$OUT")" "$BENCH/results/raw"
RAW="$BENCH/results/raw/${RUN_ID}.jsonl"; : > "$RAW"

log "crate    : $CRATE_NAME  ($CRATE)"
log "run id   : $RUN_ID"
log "output   : $OUT"

# ------------------------------------------------------------------------ build
INPROC_OK=0
if [ "$SKIP_BUILD" = 0 ]; then
  docker image inspect "$IMAGE" >/dev/null 2>&1 \
    || die "build image '$IMAGE' missing -- create it once with recodeAgent/tools/build_check.sh"
  [ -d "$SWSSLIB" ] || die "libswsscommon not staged at $SWSSLIB (override with SWSSLIB=...)"

  log "building daemon (cargo build --release --bin xcvrd-rs)"
  docker run --rm --network host \
    -v "$CRATE":/src -v "$SWSSLIB":/swsslib -w /src \
    -e RUSTFLAGS="-L native=/swsslib" -e CARGO_TERM_COLOR=never \
    "$IMAGE" cargo build --release --bin xcvrd-rs || die "daemon build FAILED"
  [ -x "$RUST_BIN" ] || die "build reported success but no binary at $RUST_BIN"
  log "  -> $RUST_BIN"

  # The in-process harness is optional: it only compiles against crates exposing the
  # Hal + SfpHandle traits. Probe rather than assume, so an unsupported target degrades
  # to DUT-only instead of failing the whole run.
  if grep -rqs "pub trait Hal" "$CRATE"/xcvrd-rs/src/*.rs \
     && grep -rqs "pub trait SfpHandle" "$CRATE"/xcvrd-rs/src/*.rs; then
    ln -sfn "$CRATE" "$BENCH/rust/target-crate"
    log "building in-process harness (target exposes Hal + SfpHandle)"
    if docker run --rm -v "$REPO":"$REPO" -v "$SWSSLIB":/swsslib \
         -w "$BENCH/rust" -e RUSTFLAGS='-L native=/swsslib' -e CARGO_TERM_COLOR=never \
         "$IMAGE" cargo build --release >/dev/null 2>&1; then
      INPROC_OK=1; log "  -> trace + calibrate"
    else
      log "  in-process harness build FAILED; DUT scenarios only"
    fi
  else
    log "in-process harness: target does not expose Hal + SfpHandle; DUT scenarios only"
    log "  (it exposes: $(grep -rhos 'pub trait [A-Za-z]*' "$CRATE"/xcvrd-rs/src/*.rs | sed 's/pub trait //' | sort -u | tr '\n' ' '))"
  fi
else
  [ -x "$RUST_BIN" ] || die "--skip-build but no binary at $RUST_BIN"
  log "SKIPPING build -- measuring the existing binary, whose provenance is only as"
  log "  good as whatever last wrote it"
  [ -x "$BENCH/rust/target/release/trace" ] && INPROC_OK=1
fi

# ------------------------------------------------------------------- provenance
SHA="$(sha256sum "$RUST_BIN" | cut -c1-16)"
BUILT="$(date -r "$RUST_BIN" -Is)"
log "binary   : sha256 $SHA  built $BUILT"

[ "$BUILD_ONLY" = 1 ] && { log "build-only: done"; exit 0; }

# --------------------------------------------------------------- DUT plumbing
dut() { docker exec mgmt bash -lc \
  "sshpass -p $DUT_PW ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 admin@$DUT_IP '$1'" 2>/dev/null; }

# A DUT left with an injected binary silently poisons every later run on this
# testbed, so restore on EVERY exit path -- including interrupt and error.
RESTORED=0
cleanup() {
  [ "$RESTORED" = 1 ] && return
  RESTORED=1
  log "restoring the stock python xcvrd"
  dut "bash $STAGE/inject.sh restore" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

want_dut=0
for s in ${ONE:-$DUT_SCENARIOS}; do
  [[ " $DUT_SCENARIOS " == *" $s "* ]] && want_dut=1
done

if [ "$want_dut" = 1 ]; then
  docker ps --format '{{.Names}}' 2>/dev/null | grep -qx mgmt \
    || die "the 'mgmt' container is not running -- the DUT is unreachable without it"
  dut "true" || die "DUT $DUT_IP unreachable through mgmt.
  After a host reboot the DUT tap is often not enslaved to the bridge; try:
      sudo ip link set vlab-01-0 master br1"

  log "shipping harness -> $DUT_IP:$STAGE"
  tar czf /tmp/xbench.tar.gz -C "$BENCH" dut || die "could not package benchmark/dut"
  docker cp /tmp/xbench.tar.gz mgmt:/tmp/xbench.tar.gz >/dev/null
  docker exec mgmt bash -lc "sshpass -p $DUT_PW scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null /tmp/xbench.tar.gz admin@$DUT_IP:/tmp/" >/dev/null 2>&1
  dut "rm -rf $STAGE && mkdir -p $STAGE && tar xzf /tmp/xbench.tar.gz -C /tmp && cp -r /tmp/dut/* $STAGE/ && chmod +x $STAGE/*.sh $STAGE/*.py" >/dev/null

  if [[ ",$VARIANTS," == *,rust,* ]]; then
    docker cp "$RUST_BIN" mgmt:/tmp/xcvrd-rs >/dev/null
    docker exec mgmt bash -lc "sshpass -p $DUT_PW scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null /tmp/xcvrd-rs admin@$DUT_IP:$STAGE/xcvrd-rs" >/dev/null 2>&1
  fi
fi

# ----------------------------------------------------------------- environment
ENV_PORTS="$(dut 'sonic-db-cli STATE_DB KEYS "TRANSCEIVER_INFO|*" | wc -l' | tr -d '\r' | tail -1)"
ENV_SPECIALS="$(dut 'cat /tmp/emu_specials 2>/dev/null' | tr -d '\r' | tail -1)"
log "dut      : ${ENV_PORTS:-?} transceiver rows, specials='${ENV_SPECIALS:-none}'"

# ------------------------------------------------------------------- run them
emit() { printf '%s\n' "$1" >> "$RAW"; }

# Return the plant to a known state and let the daemon go quiet BEFORE the next
# measurement. Without this the suite measures its own aftermath: the stimulus
# scenarios (B2/B3/B6/B8) plug, unplug and fault modules, and the daemon is still
# working through that when the next scenario starts. A full-suite run showed exactly
# that -- B5 read 42% CPU straight after B3's plug cycles versus 3.65% standalone, and
# B9 recorded 19809 EEPROM events for rust while python recorded none. Both were
# artefacts of ordering, not properties of either daemon.
SETTLE_SECS="${SETTLE_SECS:-20}"
settle_plant() {
  [ "$want_dut" = 1 ] || return 0
  dut "bash -c 'sonic-db-cli STATE_DB DEL XCVR_EMU_INJECT >/dev/null 2>&1; true'" >/dev/null 2>&1
  # Re-present every module: a scenario that timed out mid-storm can leave some absent.
  dut "cd $STAGE && python3 -c \"import emu; e=emu.Emu(); [e.set_present(i.index, True) for i in e.list()]; e.close()\"" >/dev/null 2>&1
  local i n prev=-1 stable=0
  for i in $(seq 1 30); do
    n="$(dut 'sonic-db-cli STATE_DB KEYS "TRANSCEIVER_INFO|*" | wc -l' | tr -d "\r" | tail -1)"
    [ "$n" = "$prev" ] && stable=$((stable+1)) || stable=0
    prev="$n"
    [ "$stable" -ge 3 ] && break
    sleep 2
  done
  sleep "$SETTLE_SECS"
}

run_dut_scenario() {
  local s="$1" extra=""
  case "$s" in
    B5|B9) extra="--duration $DURATION" ;;
    B4)    extra="--duration $DURATION" ;;
  esac
  # Interleave variants within each rep rather than batching them: vlab-01 is a KVM
  # guest, so host steal drifts over minutes and would otherwise be attributed to
  # whichever daemon happened to run second.
  local rep v out
  for rep in $(seq 1 "$REPS"); do
    IFS=',' read -ra VS <<< "$VARIANTS"
    for v in "${VS[@]}"; do
      echo "    rep $rep / $v"
      # Capture the inject result: a variant that silently failed to start would
      # otherwise be measured as though it were the other one, since supervisor keeps
      # running whatever was there before.
      local ij
      ij="$(dut "bash $STAGE/inject.sh $v $DOM_INTERVAL" 2>&1)"
      case "$ij" in
        *"$v active"*) ;;
        *) echo "    inject $v FAILED:"; echo "$ij" | sed 's/^/      /' | tail -3; continue ;;
      esac
      echo "$ij" | grep -a WARNING | sed 's/^/    /' || true
      out="$(dut "cd $STAGE && python3 xbench.py $s --reps 1 --timeout 180 $extra" | grep -a '^{' | tail -1)"
      [ -n "$out" ] && emit "$out" || echo "    no result from $s/$v"
    done
  done
}

# --- in-process runners -----------------------------------------------------
# These drive the daemon as a LIBRARY inside the build container: config a wires a
# Rust-native plant, config b the real PyO3 bridge onto pymocks, and config p the
# reference Python daemon. All three write to one throwaway Redis so the DB edge is
# genuinely shared and an a-vs-b difference is attributable to the platform edge alone.
REDIS_SOCKDIR=/tmp/xcvrd-bench-redis
REDIS_CNAME=xcvrd-bench-redis

ensure_redis() {
  mkdir -p "$REDIS_SOCKDIR"
  # Reuse only if the socket is really there: a container can survive with its socket
  # deleted underneath it, leaving a "running" server nothing can reach.
  if ! docker ps --format '{{.Names}}' | grep -qx "$REDIS_CNAME" || [ ! -S "$REDIS_SOCKDIR/redis.sock" ]; then
    docker rm -f "$REDIS_CNAME" >/dev/null 2>&1
    rm -f "$REDIS_SOCKDIR/redis.sock"
    docker run -d --name "$REDIS_CNAME" -v "$REDIS_SOCKDIR":/sock "${REDIS_IMAGE:-redis:7-alpine}" \
      redis-server --unixsocket /sock/redis.sock --unixsocketperm 777 \
                   --port 0 --save '' --appendonly no >/dev/null || return 1
  fi
  local i
  for i in $(seq 1 50); do [ -S "$REDIS_SOCKDIR/redis.sock" ] && break; sleep 0.2; done
  [ -S "$REDIS_SOCKDIR/redis.sock" ] || return 1
  # Identical baseline per run: a leftover row changes which branch the posters take.
  docker exec "$REDIS_CNAME" redis-cli -s /sock/redis.sock flushall >/dev/null 2>&1
}

inproc_rust() {   # <args...> -> runs the trace binary
  docker run --rm -v "$REPO":"$REPO" -v "$SWSSLIB":/swsslib -v "$REDIS_SOCKDIR":/sock \
    -w "$BENCH/rust" -e LD_LIBRARY_PATH=/swsslib -e REDIS_SOCK=/sock/redis.sock \
    "$IMAGE" ./target/release/trace --fixture "$BENCH/fixtures/cmis_40g_lr4.json" \
    --pymocks "$BENCH/pymocks" "$@" 2>/dev/null
}

inproc_python() { # <args...> -> runs the reference python daemon sweep
  docker run --rm -v "$REPO":"$REPO" -v "$SWSSLIB":/swsslib -v "$REDIS_SOCKDIR":/sock \
    -v "$BENCH/fixtures/database_config.json":/var/run/redis/sonic-db/database_config.json:ro \
    -v "$BENCH/vendor/swssshare/swss":/usr/share/swss:ro \
    -w "$BENCH" -e LD_LIBRARY_PATH=/swsslib \
    -e PYTHONPATH="$BENCH/vendor/pydeps:$BENCH/vendor/xcvrd:$BENCH/pymocks" \
    "$IMAGE" python3 python/bench_dom.py --fixture "$BENCH/fixtures/cmis_40g_lr4.json" \
    --pymocks "$BENCH/pymocks" "$@" 2>/dev/null
}

run_inproc_scenario() {
  local s="$1"
  if [ "$INPROC_OK" != 1 ]; then
    echo "    skipped: in-process harness unavailable for this crate"
    emit "{\"scenario\":\"$s\",\"harness\":\"inprocess\",\"skipped\":\"needs Hal + SfpHandle; $CRATE_NAME does not expose them\"}"
    return
  fi
  if ! ensure_redis; then
    echo "    skipped: could not start the throwaway redis"
    emit "{\"scenario\":\"$s\",\"harness\":\"inprocess\",\"skipped\":\"redis unavailable\"}"
    return
  fi
  # The reference Python daemon needs swsscommon + xcvrd vendored from the DUT's pmon
  # container. vendor/ is gitignored (third-party runtime), so a fresh checkout has no
  # config-p side. Say so rather than quietly emitting a two-config result that looks
  # like a three-config one.
  local have_py=1
  [ -d "$BENCH/vendor/pydeps/swsscommon" ] && [ -d "$BENCH/vendor/xcvrd/xcvrd" ] || have_py=0
  if [ "$have_py" = 0 ]; then
    echo "    NOTE: python reference unavailable (vendor/pydeps + vendor/xcvrd not staged);"
    echo "          reporting rust configs only. Stage them with: $BENCH/bench.sh --vendor"
    emit "{\"scenario\":\"$s\",\"harness\":\"inprocess\",\"warning\":\"python reference not vendored; rust-only comparison\"}"
  fi

  case "$s" in
    B4)  # DOM sweep duration, interleaved across the configs
      local rep c out
      for rep in $(seq 1 "$REPS"); do
        for c in a b; do
          out="$(inproc_rust --config $c --ports 28 --polls 50 --time | grep -a '^{' | tail -1)"
          [ -n "$out" ] && { echo "    $c: $out"; emit "{\"scenario\":\"B4\",\"harness\":\"inprocess\",\"variant\":\"$c\",\"result\":$out}"; }
        done
        if [ "$have_py" = 1 ]; then
          out="$(inproc_python --ports 28 --polls 50 --time | grep -a '^{' | tail -1)"
          [ -n "$out" ] && { echo "    p: $out"; emit "{\"scenario\":\"B4\",\"harness\":\"inprocess\",\"variant\":\"p\",\"result\":$out}"; }
        fi
      done ;;
    B7)  # fan-out sweep: the slope separates per-port cost from fixed overhead
      local n c out
      for n in 1 4 8 16 28; do
        for c in a b; do
          out="$(inproc_rust --config $c --ports $n --polls 30 --time | grep -a '^{' | tail -1)"
          [ -n "$out" ] && emit "{\"scenario\":\"B7\",\"harness\":\"inprocess\",\"variant\":\"$c\",\"result\":$out}"
        done
        if [ "$have_py" = 1 ]; then
          out="$(inproc_python --ports $n --polls 30 --time | grep -a '^{' | tail -1)"
          [ -n "$out" ] && emit "{\"scenario\":\"B7\",\"harness\":\"inprocess\",\"variant\":\"p\",\"result\":$out}"
        fi
        echo "    N=$n done"
      done ;;
    B12) # per-call edge cost; no daemon involved
      local tmp="$BENCH/results/raw/${RUN_ID}-b12"
      docker run --rm -v "$REPO":"$REPO" -v "$SWSSLIB":/swsslib -w "$BENCH/rust" \
        -e LD_LIBRARY_PATH=/swsslib "$IMAGE" ./target/release/calibrate \
        --fixture "$BENCH/fixtures/cmis_40g_lr4.json" --pymocks "$BENCH/pymocks" \
        --out "${tmp}-rust.json" >/dev/null 2>&1
      if [ "$have_py" = 1 ]; then
        docker run --rm -v "$REPO":"$REPO" -w "$BENCH" "$IMAGE" python3 python/calibrate.py \
          --fixture "$BENCH/fixtures/cmis_40g_lr4.json" --pymocks "$BENCH/pymocks" \
          --out "${tmp}-python.json" >/dev/null 2>&1
      fi
      python3 - "$tmp" >> "$RAW" <<'PYCAL'
import json, sys, os
t = sys.argv[1]
doc = {"scenario": "B12", "harness": "inprocess", "result": {}}
for tag, suf in (("rust", "-rust.json"), ("python", "-python.json")):
    p = t + suf
    if os.path.exists(p):
        doc["result"].update(json.load(open(p)))
print(json.dumps(doc))
PYCAL
      echo "    edge costs captured" ;;
  esac
}

SCENARIOS="${ONE:-$DUT_SCENARIOS $INPROC_SCENARIOS}"
START=$(date +%s)
for s in $SCENARIOS; do
  m="$(scenario_meta "$s")"
  [ -n "$m" ] || { log "unknown scenario $s -- skipping"; continue; }
  echo
  log "=== $s  ${m#*|}"
  if [ "${m%%|*}" = dut ]; then
    settle_plant
    run_dut_scenario "$s"
  else
    run_inproc_scenario "$s"
  fi
done
cleanup
ELAPSED=$(( $(date +%s) - START ))

# --------------------------------------------------------------- assemble json
# One self-describing artifact: provenance + environment + every record. Assembled
# in python because the records are already JSON and shell string-concatenation of
# JSON is how malformed results happen.
python3 - "$OUT" "$RAW" <<PYEOF
import json, sys, collections
out_path, raw_path = sys.argv[1], sys.argv[2]
records = []
for line in open(raw_path):
    line = line.strip()
    if line:
        try: records.append(json.loads(line))
        except json.JSONDecodeError: pass

by = collections.defaultdict(lambda: collections.defaultdict(list))
for r in records:
    if "scenario" in r:
        by[r["scenario"]][r.get("variant", "-")].append(r.get("result", r))

doc = {
  "run": {
    "id": "$RUN_ID", "started": "$RUN_TS", "elapsed_s": $ELAPSED,
    "host": "$(hostname)", "reps": $REPS, "duration_s": $DURATION,
    "variants": "$VARIANTS".split(","),
    "dom_update_interval_s": $DOM_INTERVAL,
    "scenarios_requested": "$SCENARIOS".split(),
  },
  "provenance": {
    "crate": "$CRATE_NAME", "crate_path": "$CRATE",
    "binary": "$RUST_BIN", "sha256_16": "$SHA", "built": "$BUILT",
    "built_this_run": "$SKIP_BUILD" == "0",
    "inprocess_harness": "$INPROC_OK" == "1",
  },
  "environment": {
    "dut": "$DUT_IP",
    "transceiver_rows": "${ENV_PORTS:-unknown}",
    "emulator_specials": "${ENV_SPECIALS:-none}",
  },
  "scenarios": {k: dict(v) for k, v in by.items()},
  "records": records,
}
json.dump(doc, open(out_path, "w"), indent=2)
print()
print("[bench] %d records across %d scenarios" % (len(records), len(by)))
for s in sorted(by):
    vs = by[s]
    print("  %-5s %s" % (s, "  ".join("%s:%d" % (k, len(v)) for k, v in vs.items())))
PYEOF

log "wrote $OUT"
log "raw   $RAW"
