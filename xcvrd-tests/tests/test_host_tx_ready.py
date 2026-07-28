"""Daemon reacts to host_tx_ready (T3-family: daemon must actively drive).

host_tx_ready is the host/ASIC's signal that it is driving a valid Tx electrical
signal into the module. xcvrd's CmisManagerTask reads it from STATE_DB
PORT_TABLE (get_host_tx_status) and, when it is not 'true' on an admin-up port,
forces a CMIS re-init that tears the datapath down (DataPathDeinit, 10h:128)
before it would re-provision -- it must not drive the media-side datapath while
the host side has no good Tx signal (cmis_manager_task.py:926, 1199).

We flip host_tx_ready on an admin-up, datapath-activated port and assert xcvrd
ITSELF issues the DataPathDeinit write on the Monitor stream -- the daemon
actively reacting to host_tx_ready, distinct from the admin_status trigger in
test_cmis_reconfig.py. A reduced daemon that ignores host_tx_ready never issues
that write.

Note: on the live testbed another daemon reconciles host_tx_ready back to 'true'
shortly after we clear it, so the durable signal is xcvrd's DataPathDeinit write
on the Monitor trace (deterministic), not a persistently-down datapath. The
fixture always restores host_tx_ready + re-plugs so the port is left healthy.

Uses Ethernet24 by default (an admin-up port not used by the golden/reconfig
gates); override with XCVRD_HTR_PORT.
"""
import os

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, T_DOM

HTR_PORT = os.environ.get("XCVRD_HTR_PORT", "Ethernet24")
STATE_PORT_TABLE = "PORT_TABLE"  # STATE_DB PORT_TABLE, where host_tx_ready lives

pytestmark = pytest.mark.slow


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


@pytest.fixture
def htr_port(emu, statedb, configdb):
    idx = port_to_index(HTR_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({HTR_PORT})")
    if configdb.hget(f"PORT|{HTR_PORT}", "admin_status") != "up":
        pytest.skip(f"{HTR_PORT} is not admin-up; host_tx_ready only gates admin-up ports. "
                    "Set XCVRD_HTR_PORT to an admin-up port.")
    emu.plug(idx)
    wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM,
              msg=f"{HTR_PORT} datapath activated before host_tx_ready test")
    yield HTR_PORT, idx
    # Restore: host_tx_ready true + a clean re-plug so the port is left activated.
    try:
        statedb.hset(f"{STATE_PORT_TABLE}|{HTR_PORT}", "host_tx_ready", "true")
        emu.unplug(idx)
        wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{HTR_PORT}", "manufacturer"),
                   timeout=T_DOM)
        emu.plug(idx)
        wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM)
    except Exception:  # noqa: BLE001
        pass


def test_daemon_reacts_to_host_tx_ready_not_ready(htr_port, emu, statedb, monitor):
    """Clearing host_tx_ready makes xcvrd drive a DataPathDeinit (10h:128) itself."""
    port, idx = htr_port
    n = int(statedb.hget(f"TRANSCEIVER_INFO|{port}", "host_lane_count") or 0) or 4
    active = (1 << n) - 1

    monitor.clear()
    # host_tx_ready lives in STATE_DB PORT_TABLE; xcvrd reacts to the change.
    statedb.hset(f"{STATE_PORT_TABLE}|{port}", "host_tx_ready", "false")

    masks = eventually(lambda: _deinit_masks(monitor, idx) or None, timeout=T_DOM,
                       msg=f"{port} xcvrd DataPathDeinit write (10h:128) after host_tx_ready=false")
    assert any((m & active) == active for m in masks), (
        f"{port}: DataPathDeinit masks={[hex(m) for m in masks]} do not cover the active host "
        f"lanes (0x{active:02x}) -- the daemon did not react to host_tx_ready by tearing the "
        "datapath down")
