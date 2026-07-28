"""CMIS datapath teardown + re-provision on reconfiguration (T3.4).

The teardown counterpart to test_cmis_datapath.py. test_cmis_datapath asserts
xcvrd drives an admin-up port's datapath UP; here we assert xcvrd also drives it
back DOWN and UP again in response to a *reconfiguration* event -- a port going
admin-down in CONFIG_DB and back up. This is pure daemon-driven behaviour: no
operator sfputil command and no emulator change, just a CONFIG_DB admin_status
flip that xcvrd's CmisManagerTask must react to.

On admin-down the CmisManagerTask forces a CMIS re-init and issues a
DataPathDeinit (page 10h:128) to tear the active host lanes down; on admin-up it
re-provisions the datapath (re-writes the DPConfigLane bytes 10h:145-152) and
drives it back up.

We prove xcvrd itself drove the reconfiguration two ways, both deterministic:
  * the Monitor trace shows xcvrd's own DataPathDeinit / DPConfigLane writes
    (the mechanism), and
  * the module's own DataPathStateLane report (page 11h:128, which xcvrd reads
    back) transitions the active host lanes Activated -> Deactivated -> Activated
    (the effect on the module). The emulator only reaches this state because
    xcvrd drove those writes -- it has no knowledge of admin_status itself.

(We deliberately do NOT gate on the emulator's GetInfo.dpsms objects, which do
not fully follow a raw DPDeinit write, nor on TRANSCEIVER_STATUS.DP1State, whose
refresh cadence on this stimulus is not deterministic. Page 11h is the reliable
module-side truth and is exactly what xcvrd reads.)

A reduced daemon that never reacts to admin_status (leaves the datapath up on
admin-down, or never re-provisions on admin-up) fails here.

Uses Ethernet8 by default (NOT Ethernet4, whose activated-datapath golden must
stay undisturbed); override with XCVRD_RECONFIG_PORT.
"""
import os

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, T_DOM

RECONFIG_PORT = os.environ.get("XCVRD_RECONFIG_PORT", "Ethernet8")

pytestmark = pytest.mark.slow


def _lane_states(emu, index, n_lanes=8):
    """The module's per-host-lane DataPathStateLane codes (page 11h:128)."""
    raw = emu.read(index, 0, cmis.DP_STATE_PAGE, cmis.DP_STATE_OFFSET,
                   (n_lanes + 1) // 2, force=True)
    return cmis.decode_dp_lane_states(raw, n_lanes)


def _active_all(emu, index, n):
    return all(s == cmis.DP_STATE_ACTIVATED for s in _lane_states(emu, index)[:n])


def _deinit_masks(monitor, index):
    """The bytes xcvrd wrote to DataPathDeinit (10h:128) for a module."""
    out = []
    for e in monitor.writes(index=index, page=cmis.SCS0_PAGE):
        if e.offset <= cmis.DPDEINIT_OFFSET < e.offset + e.length:
            out.append(e.data[cmis.DPDEINIT_OFFSET - e.offset])
    return out


def _reprovisioned(monitor, index):
    """True iff xcvrd re-wrote any DPConfigLane byte (10h:145-152)."""
    for e in monitor.writes(index=index, page=cmis.SCS0_PAGE):
        if any(off in cmis.SCS0_DPCONFIG_RANGE
               for off in range(e.offset, e.offset + e.length)):
            return True
    return False


@pytest.fixture
def reconfig(emu, statedb, configdb):
    """An admin-up, datapath-activated port to reconfigure, restored on teardown.

    Skips unless the port is emulator-backed and admin-up. ALWAYS restores
    admin-up and waits for the datapath to re-activate on teardown, so a failure
    mid-cycle never leaves the port administratively down.
    """
    idx = port_to_index(RECONFIG_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({RECONFIG_PORT})")
    if configdb.hget(f"PORT|{RECONFIG_PORT}", "admin_status") != "up":
        pytest.skip(f"{RECONFIG_PORT} is not admin-up; datapath reconfiguration only "
                    "runs on admin-up ports. Set XCVRD_RECONFIG_PORT to an admin-up port.")
    emu.plug(idx)
    wait_until(lambda: _lane_states(emu, idx)[0] == cmis.DP_STATE_ACTIVATED,
               timeout=T_DOM,
               msg=f"{RECONFIG_PORT} datapath activated before reconfiguration test")
    yield RECONFIG_PORT, idx
    # Restore: bring the port back admin-up and wait for re-activation so the
    # testbed is left exactly as found regardless of how the test exited.
    try:
        configdb.hset(f"PORT|{RECONFIG_PORT}", "admin_status", "up")
        emu.plug(idx)
        wait_until(lambda: _lane_states(emu, idx)[0] == cmis.DP_STATE_ACTIVATED,
                   timeout=T_DOM)
    except Exception:  # noqa: BLE001
        pass


def test_daemon_drives_datapath_reconfiguration(reconfig, emu, statedb, configdb, monitor):
    """Full reconfiguration cycle driven by xcvrd: admin-down tears the datapath
    down (DataPathDeinit write + module lanes -> Deactivated), admin-up
    re-provisions it (DPConfigLane re-write + module lanes -> Activated)."""
    port, idx = reconfig
    n = int(statedb.hget(f"TRANSCEIVER_INFO|{port}", "host_lane_count") or 0) or 4
    active = (1 << n) - 1

    # Precondition: the active host lanes really are activated on the module.
    assert _active_all(emu, idx, n), (
        f"{port}: active host lanes not all activated at start "
        f"(states={_lane_states(emu, idx)[:n]})")

    # --- reconfiguration event: admin-down should tear the datapath down --------
    monitor.clear()
    configdb.hset(f"PORT|{port}", "admin_status", "down")

    # xcvrd issues DataPathDeinit itself (10h:128), covering the active host lanes.
    masks = eventually(lambda: _deinit_masks(monitor, idx) or None, timeout=T_DOM,
                       msg=f"{port} xcvrd DataPathDeinit write (10h:128) on admin-down")
    assert any((m & active) == active for m in masks), (
        f"{port}: DataPathDeinit masks={[hex(m) for m in masks]} do not cover the "
        f"active host lanes (0x{active:02x}) -- the daemon did not drive datapath teardown")

    # ...and the module reports the active lanes deactivated (it only does so
    # because xcvrd drove the deinit; the emulator has no admin_status of its own).
    wait_until(lambda: all(s == cmis.DP_STATE_DEACTIVATED for s in _lane_states(emu, idx)[:n]),
               timeout=T_DOM,
               msg=f"{port} active host lanes DataPathDeactivated on the module after admin-down")

    # --- re-provision: admin-up should re-drive the datapath back up ------------
    monitor.clear()
    configdb.hset(f"PORT|{port}", "admin_status", "up")

    # xcvrd re-writes the DPConfigLane control set to re-provision the datapath.
    wait_until(lambda: _reprovisioned(monitor, idx), timeout=T_DOM,
               msg=f"{port} xcvrd DPConfigLane re-provision write (10h:145-152) on admin-up")

    # ...and the module reports the active lanes activated again.
    wait_until(lambda: _active_all(emu, idx, n), timeout=T_DOM,
               msg=f"{port} active host lanes re-activated on the module after admin-up")
