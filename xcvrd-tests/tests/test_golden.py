"""Golden-baseline conformance (HLD end-to-end).

Asserts the STATE_DB projection xcvrd produces for the test port matches a
committed golden captured from the reference (current) xcvrd. This is the
conformance gate for a future xcvrd reimplementation.

Workflow:
  ./run.sh --capture-golden tests/test_golden.py   # (re)capture every scenario from Python
  ./run.sh tests/test_golden.py                    # compare the live daemon
  ./run.sh tests/test_golden.py -k steady_state    # one scenario
Capture refuses a non-reference (Rust-injected) xcvrd, so a golden can never be
baselined from the candidate it is meant to grade.
"""
import os

import pytest

from lib import golden, scenarios
from lib.scenarios import ScenarioCtx
from lib.emu import port_to_index, index_to_port

GOLDEN_DIR = os.path.join(os.path.dirname(os.path.dirname(__file__)), "golden")
CAPTURE = os.environ.get("XCVRD_GOLDEN_CAPTURE") == "1"


def _params():
    for s in scenarios.all_scenarios():
        marks = [pytest.mark.slow] if s.slow else []
        yield pytest.param(s, id=s.name, marks=marks)


@pytest.mark.parametrize("scenario", list(_params()))
def test_state_matches_golden(scenario, statedb, emu, configdb, xcvrd, test_index):
    # A scenario may pin its own port (e.g. an admin-up one whose datapath xcvrd
    # actually drives); otherwise use the harness default (TEST_PORT via test_index).
    port = scenario.port or index_to_port(test_index)
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port}) for scenario {scenario.name}")

    ctx = ScenarioCtx(port=port, index=idx, statedb=statedb, emu=emu, configdb=configdb)
    scenario.prepare(ctx)
    proj = golden.project(statedb, port, scenario.tables)
    path = golden.path_for(GOLDEN_DIR, port, scenario.name)

    if CAPTURE:
        # The golden is the ORACLE: it must come from the reference Python xcvrd,
        # never from an injected Rust candidate (that would defeat the diff).
        if not xcvrd.is_reference_python():
            pytest.fail(
                f"refusing to capture golden [{scenario.name}] from a non-reference "
                "xcvrd: the deployed /usr/local/bin/xcvrd is the Rust shim, not the "
                "stock Python daemon. Restore Python xcvrd, then re-capture.")
        golden.save(proj, path)
        pytest.skip(f"captured golden [{scenario.name}] -> {path}")

    if not os.path.exists(path):
        pytest.skip(f"no golden for [{scenario.name}] {port}; capture with "
                    f"--capture-golden ./run.sh tests/test_golden.py")

    diffs = golden.diff(proj, golden.load(path))
    assert not diffs, (
        f"[{scenario.name}] {port}: xcvrd STATE_DB projection diverged from "
        f"golden:\n  " + "\n  ".join(diffs))
