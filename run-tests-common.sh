#!/usr/bin/env bash
# Run the sonic-platform-common test suite inside the dev container.
#
# By default it runs tests/sonic_xcvr -- the CMIS / SFF transceiver API suite
# that sonic-xcvrd is built on (the directly xcvrd-relevant tests). Pass other
# paths/args to override, e.g.:
#
#   ./run-tests-common.sh                         # tests/sonic_xcvr (929 tests)
#   ./run-tests-common.sh tests/sonic_xcvr/test_cmis.py -k VDM
#   ./run-tests-common.sh tests                   # whole suite (see note below)
#
# The repo is mounted read-only and PYTHONPATH points at it, so the *cloned*
# sonic_platform_base source is what gets tested (it shadows the copy baked into
# the image). All generated artifacts go to /tmp so the repo stays pristine.
#
# NOTE: the full `tests` directory also contains storage tests (need `psutil`)
# and sfputilhelper_test (needs the real sonic-config-engine `portconfig`, which
# we deliberately stub). Those are unrelated to xcvrd and are not installed here.
set -euo pipefail

export MSYS_NO_PATHCONV=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../sonic-platform-common" && pwd -W)"

IMAGE="${IMAGE:-sonic-xcvrd-dev}"

# Default test target = the xcvrd-relevant transceiver API suite.
if [ "$#" -gt 0 ]; then TARGETS=("$@"); else TARGETS=(tests/sonic_xcvr); fi

# Override pytest.ini addopts so nothing is written into the (read-only) repo.
ADDOPTS="--cov=sonic_platform_base --cov-report=term \
--cov-report=xml:/tmp/coverage.xml --cov-report=html:/tmp/htmlcov \
--junitxml=/tmp/test-results.xml -o cache_dir=/tmp/pytest_cache -v"

set -x
docker run --rm \
    -v "$REPO:/work:ro" \
    -w /work \
    -e PYTHONPATH=/work \
    -e COVERAGE_FILE=/tmp/.coverage \
    "$IMAGE" \
    pytest -o addopts="$ADDOPTS" "${TARGETS[@]}"
