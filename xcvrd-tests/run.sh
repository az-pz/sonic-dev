#!/bin/bash
# Bootstrap + run the xcvrd black-box tests. RUN ON THE DUT (admin@vlab-01),
# where the emulator (localhost:50051), sonic-db-cli and pmon are all local.
#
# The DUT host already has grpcio + protobuf + pyyaml (system site-packages) but
# no pytest and no internet. We install pytest from the shipped wheels/ into a
# local .pydeps dir (no venv -- ensurepip is unavailable on the image) and put
# it on PYTHONPATH alongside the harness.
#
# Usage:  ./run.sh [pytest args...]
#   ./run.sh                     # full suite (includes slow DOM tests)
#   ./run.sh -m "not slow"       # skip the ~60s DOM refresh tests
#   ./run.sh tests/test_presence.py -k unplug
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PYDEPS="$HERE/.pydeps"

if ! PYTHONPATH="$PYDEPS" python3 -c "import pytest" 2>/dev/null; then
  echo "[run] installing pytest from offline wheels -> $PYDEPS"
  python3 -m pip install --no-index --find-links "$HERE/wheels" \
    --target "$PYDEPS" pytest \
    || { echo "[run] ERROR: offline pytest install failed"; exit 2; }
fi

# Sanity: the deps the harness needs must import on this host.
PYTHONPATH="$PYDEPS:$HERE" python3 - <<'PY' || exit 2
import grpc, google.protobuf, yaml, pytest  # noqa
print("[run] deps OK: grpc", grpc.__version__, "| pytest", pytest.__version__)
PY

export PYTHONPATH="$PYDEPS:$HERE"
exec python3 -m pytest "$HERE/tests" \
  --junitxml="$HERE/results.xml" "$@"
