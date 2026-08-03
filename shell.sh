#!/usr/bin/env bash
# Open an interactive shell inside the sonic-xcvrd-dev container.
#
# The repo is bind-mounted read-write at /work and you start in the sonic-xcvrd
# directory, so you can browse code, edit, and run tests. Inside the shell:
#
#   runtests                # run the full suite (artifacts go to /tmp, repo stays clean)
#   runtests -k cmis -x     # forward args to pytest
#   pytest ...              # plain pytest also works (writes gitignored coverage files)
#
# Pass a one-off command to run non-interactively instead of a shell:
#   ./shell.sh python -c "import swsscommon; print('ok')"
set -euo pipefail

# Keep MSYS/Git-Bash from rewriting the container-side paths (e.g. /work).
export MSYS_NO_PATHCONV=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../sonic-platform-daemons" && pwd -W)"

# Image to open (override with IMAGE=... if you retag it).
IMAGE="${IMAGE:-sonic-xcvrd-dev}"

# Only allocate a TTY when stdin/stdout are real terminals.
TTY_FLAGS=()
if [ -t 0 ] && [ -t 1 ]; then TTY_FLAGS=(-it); fi

# Default to an interactive bash shell; otherwise run the passed command.
if [ "$#" -gt 0 ]; then CMD=("$@"); else CMD=(bash); fi

docker run --rm "${TTY_FLAGS[@]}" \
    -v "$REPO:/work" \
    -e COVERAGE_FILE=/tmp/.coverage \
    -w /work/sonic-xcvrd \
    "$IMAGE" \
    "${CMD[@]}"
