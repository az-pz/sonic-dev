"""Golden-baseline conformance (HLD end-to-end).

Asserts the STATE_DB projection xcvrd produces for the test port matches a
committed golden captured from the reference (current) xcvrd. This is the
conformance gate for a future xcvrd reimplementation.

Workflow:
  # (re)capture the golden from the reference daemon, then commit golden/*.json
  XCVRD_GOLDEN_CAPTURE=1 ./run.sh tests/test_golden.py
  # normal run compares against the committed golden
  ./run.sh tests/test_golden.py
"""
import os

import pytest

from lib import golden
from lib.waits import wait_until, T_FAST

GOLDEN_DIR = os.path.join(os.path.dirname(os.path.dirname(__file__)), "golden")
CAPTURE = os.environ.get("XCVRD_GOLDEN_CAPTURE") == "1"


def _stable_projection(module, statedb):
    """Bring the module to a steady state, then snapshot its projection."""
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    wait_until(lambda: module.status_sw().get("cmis_state") == "READY", timeout=T_FAST,
               msg=f"{module.port} cmis_state READY before golden snapshot")
    return golden.project(statedb, module.port)


def test_state_matches_golden(module, statedb):
    proj = _stable_projection(module, statedb)
    path = golden.path_for(GOLDEN_DIR, module.port)

    if CAPTURE:
        golden.save(proj, path)
        pytest.skip(f"captured golden baseline -> {path}")

    if not os.path.exists(path):
        pytest.skip(f"no golden baseline for {module.port}; capture with "
                    f"XCVRD_GOLDEN_CAPTURE=1 ./run.sh tests/test_golden.py")

    diffs = golden.diff(proj, golden.load(path))
    assert not diffs, (
        f"{module.port}: xcvrd STATE_DB projection diverged from golden:\n  "
        + "\n  ".join(diffs))
