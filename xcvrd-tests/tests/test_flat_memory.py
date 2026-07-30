"""Flat-memory modules short-circuit the CMIS state machine to READY (B19).

xcvrd's CmisManagerTask runs its datapath bring-up only for a paged CMIS module.
For a **flat-memory** module (`api.is_flat_memory()`, CMIS 00h:2.7) it short-circuits
straight to `cmis_state=READY` without ever driving the datapath (cmis_manager_task.py:
1287-1290) — the same skip a non-CMIS module type takes (1293-1296). No existing test
exercises this branch: the SFF-8636 module routes to `SffManagerTask`, and every other
module is a paged CMIS optic that runs the full machine.

`is_flat_memory()` is a `@read_only_cached_api_return` property read once at module
insertion, and the flat bit lives in page-00h lower memory which the emulator restores
from config on every plug, so the module must be served flat by the emulator config
(emu-deploy/provision_special_modules.sh sets `MemoryModel: FLAT` on idx13/Ethernet52).
We assert the module reaches READY but its datapath is never activated and xcvrd issues
no `ApplyDPInitLane` (10h:143) on a re-plug. A reduced daemon that runs the state machine
regardless of flat memory would activate the datapath and issue that write.

Uses the flat module (default Ethernet52); override with XCVRD_FLATMEM_PORT. Skips
cleanly if that module is not served flat (portable to a plain testbed).
"""
import os

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.waits import wait_until, stays, T_FAST, T_DOM

pytestmark = pytest.mark.slow

FLATMEM_PORT = os.environ.get("XCVRD_FLATMEM_PORT", "Ethernet52")
STATUS_SW = "TRANSCEIVER_STATUS_SW"
BRINGUP_GUARD = 20.0     # seconds to confirm NO datapath bring-up write is issued


def _cmis_state(statedb, port):
    return statedb.hgetall(f"{STATUS_SW}|{port}").get("cmis_state")


def _dp_lane1(emu, idx):
    return cmis.decode_dp_lane_states(
        emu.read(idx, 0, cmis.DP_STATE_PAGE, cmis.DP_STATE_OFFSET, 4, force=True))[0]


def _apply_dpinit_writes(monitor, idx):
    """ApplyDPInitLane (10h:143) writes with any lane bit set -- the datapath
    provisioning trigger xcvrd drives for a paged module but must skip for a flat one."""
    out = []
    off = cmis.APPLY_DPINIT_OFFSET
    for e in monitor.writes(index=idx, page=cmis.SCS0_PAGE):
        if e.offset <= off < e.offset + e.length and e.data[off - e.offset]:
            out.append(e.data[off - e.offset])
    return out


@pytest.fixture
def flat_port(emu, statedb):
    port = FLATMEM_PORT
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port})")
    emu.plug(idx)
    # The module must be served flat (00h:2.7 set); otherwise this testbed has no flat
    # module and the branch is not exercisable here.
    if not (emu.read(idx, 0, 0, 2, 1, force=True)[0] & cmis.FLAT_MEM_BIT):
        pytest.skip(f"{port} is not served as a flat-memory module (00h:2.7 clear); provision it "
                    "via emu-deploy/provision_special_modules.sh (MemoryModel: FLAT)")
    yield port, idx
    try:
        emu.plug(idx)
    except Exception:  # noqa: BLE001
        pass


def test_flat_memory_reaches_ready_without_datapath(flat_port, emu, statedb):
    """A flat-memory module reaches cmis_state=READY but its datapath is never
    activated -- xcvrd skipped the state machine (DataPathDeactivated, not Activated)."""
    port, idx = flat_port
    wait_until(lambda: _cmis_state(statedb, port) == "READY", timeout=T_DOM,
               msg=f"{port} flat-memory module reaches cmis_state=READY (skip branch)")
    # The skip means the datapath was never brought up: it stays DEACTIVATED, unlike a
    # paged module which xcvrd activates.
    assert _dp_lane1(emu, idx) == cmis.DP_STATE_DEACTIVATED, (
        f"{port}: flat-memory datapath lane-1 state={_dp_lane1(emu, idx):#x} (expected "
        f"DataPathDeactivated {cmis.DP_STATE_DEACTIVATED:#x}) -- xcvrd should not bring up a "
        "flat-memory module's datapath")


def test_flat_memory_skips_datapath_bringup_on_replug(flat_port, emu, statedb, monitor):
    """Re-plugging the flat-memory module drives no ApplyDPInitLane (10h:143) -- the
    datapath provisioning is skipped entirely."""
    port, idx = flat_port
    emu.unplug(idx)
    wait_until(lambda: _cmis_state(statedb, port) in (None, "REMOVED")
               or not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_DOM, msg=f"{port} removed before flat re-insert")
    monitor.clear()
    emu.plug(idx)
    wait_until(lambda: _cmis_state(statedb, port) == "READY", timeout=T_DOM,
               msg=f"{port} flat-memory module back to READY after re-plug")
    assert stays(lambda: not _apply_dpinit_writes(monitor, idx), duration=BRINGUP_GUARD), (
        f"{port}: xcvrd drove ApplyDPInitLane (10h:143) on a FLAT-MEMORY module "
        f"(should skip the datapath bring-up): {[hex(m) for m in _apply_dpinit_writes(monitor, idx)]}")
