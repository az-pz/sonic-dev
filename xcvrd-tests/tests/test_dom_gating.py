"""DOM gating while the CMIS datapath is initializing (#7; dom_mgr.py:198-200).

xcvrd's DomInfoUpdateTask skips DOM publication for a port whose CMIS bring-up has
not reached a terminal state (is_port_dom_monitoring_disabled ->
is_port_in_cmis_initialization_process). So while cmis_state is non-terminal, the
DOM tables it owns -- TRANSCEIVER_DOM_FLAG in particular -- must NOT be published;
they appear only once the module reaches a terminal state (READY). (The separate
DomThermalInfoUpdateTask is NOT gated, which is why we assert on DOM_FLAG, a
DomInfoUpdateTask-owned table, rather than DOM_SENSOR temperature.)

A healthy module blows through CMIS init in ~9s, too fast to observe reliably, so
we hold it non-terminal with the emulator's FAULT_DP_STALL injection and assert the
invariant across the window: whenever cmis_state is non-terminal, DOM_FLAG is
absent. Clearing the fault lets the module reach READY and DOM_FLAG then appears --
proving the gate, not a mere timing coincidence.
"""
import time

import pytest

from lib import faults
from lib.waits import wait_until, T_FAST, T_DOM, T_BASELINE

pytestmark = pytest.mark.slow

DOM_FLAG = "TRANSCEIVER_DOM_FLAG"
TERMINAL = {"READY", "FAILED", "REMOVED"}


def _cmis(statedb, port):
    return statedb.hgetall(f"TRANSCEIVER_STATUS_SW|{port}").get("cmis_state")


def test_dom_gated_during_cmis_init(fault_port, emu, statedb):
    port, idx = fault_port

    # Arm the stall, clear DOM_FLAG, and re-insert so the module enters CMIS init
    # and stays non-terminal (retrying) for a meaningful window.
    faults.arm(emu, idx, faults.FAULT_DP_STALL)
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} cleared before stall insert")
    statedb.delete(f"{DOM_FLAG}|{port}")
    emu.plug(idx)

    # Sample (cmis_state, DOM_FLAG present) across the stall window. The invariant:
    # DOM_FLAG must be absent in EVERY non-terminal sample.
    non_terminal_seen = 0
    deadline = time.time() + 2 * T_DOM
    while time.time() < deadline:
        state = _cmis(statedb, port)
        flag_present = statedb.exists(f"{DOM_FLAG}|{port}")
        if state not in TERMINAL and state is not None:
            non_terminal_seen += 1
            assert not flag_present, (
                f"{port} TRANSCEIVER_DOM_FLAG published while cmis_state={state} "
                "(non-terminal) -- DOM not gated during CMIS init")
        if state == "FAILED":
            break
        time.sleep(1.0)

    assert non_terminal_seen >= 5, (
        f"{port} never held a non-terminal CMIS state long enough to observe the "
        f"gate (saw {non_terminal_seen} non-terminal samples); stall fault ineffective?")

    # Clear the fault -> module reaches READY -> the gate releases and DOM_FLAG
    # is published (confirming the absence above was the gate, not missing data).
    faults.clear(emu, idx)
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST)
    emu.plug(idx)
    wait_until(lambda: _cmis(statedb, port) == "READY", timeout=T_BASELINE,
               msg=f"{port} reached READY after fault cleared")
    wait_until(lambda: statedb.exists(f"{DOM_FLAG}|{port}"), timeout=2 * T_DOM,
               msg=f"{port} DOM_FLAG published once CMIS init completed (gate released)")
