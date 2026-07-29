"""CMIS FAILED after the retry cap (#9; cmis_manager_task.py CMIS_MAX_RETRIES).

When the CMIS bring-up cannot complete, xcvrd's CmisManagerTask does not hang or
loop forever: each state has a timer, and on repeated timeouts it re-inits and
bumps a retry counter; once retries exceed CMIS_MAX_RETRIES (3) it drives
cmis_state to FAILED. This whole failure branch is otherwise untested. We drive it
with the emulator's FAULT_DP_STALL injection: the module never reaches ModuleReady,
so xcvrd retries and lands in cmis_state=FAILED; clearing the fault + re-plugging
recovers the port to READY.
"""
import pytest

from lib import faults
from lib.waits import wait_until, T_FAST, T_DOM

pytestmark = pytest.mark.slow

TERMINAL = {"READY", "FAILED", "REMOVED"}


def _cmis(statedb, port):
    return statedb.hgetall(f"TRANSCEIVER_STATUS_SW|{port}").get("cmis_state")


def test_cmis_reaches_failed_after_retries(fault_port, emu, statedb):
    port, idx = fault_port

    # Arm the datapath stall, then re-insert so the CMIS bring-up runs and stalls.
    faults.arm(emu, idx, faults.FAULT_DP_STALL)
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} cleared before stall insert")
    emu.plug(idx)

    # xcvrd retries the stalled bring-up and, after CMIS_MAX_RETRIES, sets FAILED.
    wait_until(lambda: _cmis(statedb, port) == "FAILED", timeout=2 * T_DOM,
               msg=f"{port} cmis_state == FAILED after the CMIS retry cap")

    # Recovery: clear the fault + re-plug -> the module reaches READY again.
    faults.clear(emu, idx)
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST)
    emu.plug(idx)
    wait_until(lambda: _cmis(statedb, port) == "READY", timeout=T_DOM,
               msg=f"{port} recovered to cmis_state READY after fault cleared")
