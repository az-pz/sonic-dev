#!/usr/bin/env bash
# Run the sonic-xcvrd unit tests inside the dev container.
#
# The repo is bind-mounted read-only into the container; all generated test
# artifacts (coverage db/html/xml, junit xml, pytest cache) are redirected to
# the container's /tmp so the repository working tree stays pristine. Coverage
# is still summarized on the terminal.
#
#   ./run-tests.sh                # run full suite
#   ./run-tests.sh -k cmis -x     # forward extra args to pytest
set -euo pipefail

# Keep MSYS/Git-Bash from rewriting the container-side paths (e.g. /work).
export MSYS_NO_PATHCONV=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Windows-style path (C:/...) that Docker Desktop accepts as a bind source.
REPO="$(cd "$SCRIPT_DIR/../sonic-platform-daemons" && pwd -W)"

# Image to run (override with IMAGE=... if you retag it).
IMAGE="${IMAGE:-sonic-xcvrd-dev}"

# Override pytest.ini addopts so nothing is written into the (read-only) repo.
ADDOPTS="--cov=xcvrd --cov-report=term \
--cov-report=xml:/tmp/coverage.xml --cov-report=html:/tmp/htmlcov \
--junitxml=/tmp/test-results.xml -o cache_dir=/tmp/pytest_cache -v"

set -x
docker run --rm \
    -v "$REPO:/work:ro" \
    -e COVERAGE_FILE=/tmp/.coverage \
    "$IMAGE" \
    pytest -o addopts="$ADDOPTS" "$@"
