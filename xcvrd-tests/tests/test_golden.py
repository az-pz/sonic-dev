"""Golden-baseline conformance (HLD end-to-end), one test per scenario.

Each scenario (tests/scenarios.py) brings the module to a reproducible state and
we assert the STATE_DB projection xcvrd produces matches a golden captured from
the reference (upstream **Python**) xcvrd. This is the conformance gate for a
future (e.g. Rust) reimplementation; every scenario has its OWN test function so
it is selectable by pytest function name:

  ./run.sh --capture-golden -k test_activated_datapath   # (re)capture from Python
  ./run.sh -k test_activated_datapath                    # compare the live daemon
  ./run.sh tests/test_golden.py                          # all scenarios

Capture refuses a non-reference (Rust-injected) xcvrd, so a golden can never be
baselined from the candidate it is meant to grade.
"""
import os

import pytest

import scenarios
from scenarios import ScenarioCtx
from lib import golden
from lib.emu import port_to_index, index_to_port

GOLDEN_DIR = os.path.join(os.path.dirname(os.path.dirname(__file__)), "golden")
CAPTURE = os.environ.get("XCVRD_GOLDEN_CAPTURE") == "1"


def _run_golden(scenario, statedb, emu, configdb, xcvrd, test_index):
    """Drive ``scenario`` and capture-or-compare its golden projection."""
    # A scenario may pin its own port (e.g. an admin-up one whose datapath xcvrd
    # actually drives); otherwise use the harness default (TEST_PORT via test_index).
    port = scenario.port or index_to_port(test_index)
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port}) for scenario {scenario.name}")

    ctx = ScenarioCtx(port=port, index=idx, statedb=statedb, emu=emu, configdb=configdb)
    try:
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
                        f"--capture-golden ./run.sh -k test_{scenario.name}")

        diffs = golden.diff(proj, golden.load(path))
        assert not diffs, (
            f"[{scenario.name}] {port}: xcvrd STATE_DB projection diverged from "
            f"golden:\n  " + "\n  ".join(diffs))
    finally:
        # Run the scenario's cleanup (e.g. clear a raised flag) even on
        # skip/fail so its stimulus can't leak into later tests.
        if scenario.teardown:
            scenario.teardown(ctx)


@pytest.mark.slow
def test_steady_state(statedb, emu, configdb, xcvrd, test_index):
    """Admin-down port at rest: identity + SW status + DOM thresholds."""
    _run_golden(scenarios.STEADY_STATE, statedb, emu, configdb, xcvrd, test_index)


@pytest.mark.slow
def test_activated_datapath(statedb, emu, configdb, xcvrd, test_index):
    """Admin-up port with CMIS datapath driven to activated (real active_apsel)."""
    _run_golden(scenarios.ACTIVATED_DATAPATH, statedb, emu, configdb, xcvrd, test_index)


@pytest.mark.slow
def test_dom_flag(statedb, emu, configdb, xcvrd, test_index):
    """Module with a raised DOM alarm flag: TRANSCEIVER_DOM_FLAG projection.

    Stimulus raises TempMonHighAlarm on the emulator; a daemon that doesn't read
    and publish the module's latched monitor flags (e.g. the reduced Rust, which
    emits no flag tables) fails this gate.
    """
    _run_golden(scenarios.DOM_FLAG, statedb, emu, configdb, xcvrd, test_index)
