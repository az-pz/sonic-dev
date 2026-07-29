"""Forced Tx-disable during CMIS bring-up (forced_tx_disabled path) (B18).

When an admin-up CMIS port loses its bring-up preconditions -- host_tx_ready !=
'true' OR admin_status != 'up' -- xcvrd's CmisManagerTask does more than tear the
datapath down (DataPathDeinit, covered by test_host_tx_ready / test_cmis_reconfig
on 10h:128). It also FORCES the media Tx laser off:
cmis_manager_task.py:935 calls api.tx_disable_channel(media_lanes_mask, True),
which writes OutputDisableTx (page 10h:130) with the media-lane bits SET, and
records forced_tx_disabled=True. On the next bring-up (preconditions restored) the
forced_tx_disabled flag is cleared and the datapath re-activates, re-enabling Tx.

SFF-8636 Tx-disable is covered by test_sff8636.py; this is the CMIS forced-tx path,
a distinct register (OutputDisableTx 10h:130, not DataPathDeinit 10h:128) driven by
the CMIS state machine. We flip admin_status down then up on a spare port and assert
xcvrd ITSELF drives the OutputDisableTx write (bits set on admin-down, cleared as
the port comes back). A reduced daemon that ignores the forced-tx path never issues
the 10h:130 write.

Uses an admin-up port (default Ethernet28) not used by the golden/reconfig gates;
override with XCVRD_FTX_PORT. The fixture always restores admin-up + a re-plug so
the port is left healthy and datapath-activated.
"""
import os

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, T_DOM

pytestmark = pytest.mark.slow

FTX_PORT = os.environ.get("XCVRD_FTX_PORT", "Ethernet28")


def _dp1_activated(emu, idx):
    return cmis.decode_dp_lane_states(
        emu.read(idx, 0, cmis.DP_STATE_PAGE, cmis.DP_STATE_OFFSET, 4, force=True)
    )[0] == cmis.DP_STATE_ACTIVATED


def _txdisable_masks(monitor, idx):
    """Every value written to OutputDisableTx (10h:130) for this module, in order.
    Each byte is a per-media-lane disable mask (bit set = that lane's Tx forced off)."""
    off = cmis.OUTPUT_DISABLE_TX_OFFSET
    out = []
    for e in monitor.writes(index=idx, page=cmis.SCS0_PAGE):
        if e.offset <= off < e.offset + e.length:
            out.append(e.data[off - e.offset])
    return out


@pytest.fixture
def ftx_port(emu, statedb, configdb):
    idx = port_to_index(FTX_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({FTX_PORT})")
    if configdb.hget(f"PORT|{FTX_PORT}", "admin_status") != "up":
        pytest.skip(f"{FTX_PORT} is not admin-up; the forced-tx path only gates admin-up "
                    "ports. Set XCVRD_FTX_PORT to an admin-up port.")
    emu.plug(idx)
    wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM,
               msg=f"{FTX_PORT} datapath activated before forced-tx test")
    yield FTX_PORT, idx
    # Restore admin-up + a clean re-plug so the port is left activated.
    try:
        configdb.hset(f"PORT|{FTX_PORT}", "admin_status", "up")
        emu.unplug(idx)
        wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{FTX_PORT}", "manufacturer"),
                   timeout=T_DOM)
        emu.plug(idx)
        wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM)
    except Exception:  # noqa: BLE001
        pass


def test_forced_tx_disable_on_admin_down_and_reenable(ftx_port, emu, statedb, configdb, monitor):
    """admin-down forces the media Tx off (OutputDisableTx 10h:130 bits set); the
    subsequent admin-up re-enables it (a later 10h:130 write clears those bits)."""
    port, idx = ftx_port
    n = int(statedb.hget(f"TRANSCEIVER_INFO|{port}", "media_lane_count") or 0) \
        or int(statedb.hget(f"TRANSCEIVER_INFO|{port}", "host_lane_count") or 0) or 4
    active = (1 << n) - 1

    monitor.clear()
    configdb.hset(f"PORT|{port}", "admin_status", "down")

    dis = eventually(lambda: _txdisable_masks(monitor, idx) or None, timeout=T_DOM,
                     msg=f"{port} xcvrd OutputDisableTx write (10h:130) after admin-down")
    assert any((m & active) == active for m in dis), (
        f"{port}: OutputDisableTx masks={[hex(m) for m in dis]} do not force the active "
        f"media lanes (0x{active:02x}) off -- xcvrd did not drive the forced-tx-disable path")

    # Bring the port back: forced_tx_disabled is cleared and Tx is re-enabled as the
    # datapath re-activates -- a later 10h:130 write clears the forced bits.
    monitor.clear()
    configdb.hset(f"PORT|{port}", "admin_status", "up")
    en = eventually(lambda: [m for m in _txdisable_masks(monitor, idx) if (m & active) == 0] or None,
                    timeout=T_DOM,
                    msg=f"{port} xcvrd re-enables Tx (10h:130 cleared) after admin-up")
    assert en, f"{port}: no OutputDisableTx write cleared the forced media lanes on recovery"
