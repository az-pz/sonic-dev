"""CMIS fast-reboot datapath-skip: preserve an active datapath across re-init (B17).

test_warm_reboot.py covers the STATUS-table preservation on the xcvrd side. This is
the CMIS-side counterpart: when fast reboot is enabled and a re-init is triggered on
a port whose datapath is still ACTIVATED, xcvrd must SKIP the DataPathDeinit so the
live datapath is not torn down (cmis_manager_task.py:928):

    if is_fast_reboot and check_datapath_state(['DataPathActivated']):
        # skip datapath re-init in fast-reboot
    else:
        api.set_datapath_deinit(host_lanes_mask)   # 10h:128

is_fast_reboot is read (and cached) from FAST_RESTART_ENABLE_TABLE|system.enable, so
we set the flag BEFORE (re)starting xcvrd. Then a re-init trigger (admin-down) on an
activated port must NOT produce a DataPathDeinit (10h:128) write. The control gate --
same trigger with the flag clear -- MUST produce it, proving the skip is a real
fast-reboot behaviour and not a daemon that simply never deinits.

Uses an admin-up, datapath-activated port (default Ethernet24); override with
XCVRD_FASTREBOOT_PORT. The fixture ALWAYS clears the flag, restores a healthy xcvrd,
and restores admin-up so nothing leaks into the rest of the suite.
"""
import os

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, stays, T_FAST, T_DOM, T_BASELINE

pytestmark = pytest.mark.slow

FR_PORT = os.environ.get("XCVRD_FASTREBOOT_PORT", "Ethernet24")
FAST_RESTART_FLAG = "FAST_RESTART_ENABLE_TABLE|system"
SKIP_GUARD = 25.0     # seconds to confirm NO deinit was issued in the skip case


def _dp1_activated(emu, idx):
    return cmis.decode_dp_lane_states(
        emu.read(idx, 0, cmis.DP_STATE_PAGE, cmis.DP_STATE_OFFSET, 4, force=True)
    )[0] == cmis.DP_STATE_ACTIVATED


def _deinit_masks(monitor, idx):
    out = []
    for e in monitor.writes(index=idx, page=cmis.SCS0_PAGE):
        if e.offset <= cmis.DPDEINIT_OFFSET < e.offset + e.length:
            out.append(e.data[cmis.DPDEINIT_OFFSET - e.offset])
    return out


def _restart_xcvrd_with_flag(xcvrd, statedb, port, enable):
    """Set/clear the fast-reboot flag, then restart xcvrd so its CmisManagerTask
    caches the flag value, and wait until it is healthy again."""
    if enable:
        statedb.hset(FAST_RESTART_FLAG, "enable", "true")
    else:
        statedb.hdel(FAST_RESTART_FLAG, "enable")
    xcvrd.stop()
    xcvrd.start()
    xcvrd.wait_healthy(port, timeout=T_BASELINE)


@pytest.fixture
def fastreboot_port(xcvrd, emu, statedb, configdb):
    idx = port_to_index(FR_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({FR_PORT})")
    if configdb.hget(f"PORT|{FR_PORT}", "admin_status") != "up":
        pytest.skip(f"{FR_PORT} is not admin-up; the fast-reboot datapath-skip only applies "
                    "to an activated port. Set XCVRD_FASTREBOOT_PORT to an admin-up port.")
    statedb.hdel(FAST_RESTART_FLAG, "enable")
    if not xcvrd.is_running():
        xcvrd.start()
    emu.plug(idx)
    wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM,
               msg=f"{FR_PORT} datapath activated before fast-reboot test")
    try:
        yield FR_PORT, idx
    finally:
        statedb.hdel(FAST_RESTART_FLAG, "enable")
        try:
            configdb.hset(f"PORT|{FR_PORT}", "admin_status", "up")
            if not xcvrd.is_running():
                xcvrd.start()
            xcvrd.wait_healthy(FR_PORT, timeout=T_BASELINE)
            emu.plug(idx)
            wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM)
        except Exception:  # noqa: BLE001
            pass


def test_fast_reboot_skips_datapath_deinit(fastreboot_port, xcvrd, emu, statedb, configdb, monitor):
    """With fast reboot enabled, re-initialising an activated port does NOT tear the
    datapath down: xcvrd issues no DataPathDeinit (10h:128) write."""
    port, idx = fastreboot_port

    _restart_xcvrd_with_flag(xcvrd, statedb, port, enable=True)
    wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM,
               msg=f"{port} still activated after fast-reboot xcvrd restart")

    monitor.clear()
    configdb.hset(f"PORT|{port}", "admin_status", "down")
    assert stays(lambda: not _deinit_masks(monitor, idx), duration=SKIP_GUARD), (
        f"{port}: xcvrd issued a DataPathDeinit (10h:128) under fast reboot on an activated "
        f"port -- it must SKIP the deinit to preserve the live datapath "
        f"(masks={[hex(m) for m in _deinit_masks(monitor, idx)]})")


def test_normal_reboot_deinits_datapath(fastreboot_port, xcvrd, emu, statedb, configdb, monitor):
    """Control: with fast reboot disabled, the same admin-down re-init DOES issue the
    DataPathDeinit (10h:128) -- so the skip above is a real fast-reboot behaviour."""
    port, idx = fastreboot_port

    _restart_xcvrd_with_flag(xcvrd, statedb, port, enable=False)
    wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM,
               msg=f"{port} activated after normal xcvrd restart")

    monitor.clear()
    configdb.hset(f"PORT|{port}", "admin_status", "down")
    eventually(lambda: _deinit_masks(monitor, idx) or None, timeout=T_DOM,
               msg=f"{port} DataPathDeinit (10h:128) on a normal (non-fast-reboot) admin-down")
