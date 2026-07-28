"""CMIS datapath bring-up (parity gate for the CMIS manager).

An admin-up CMIS port must be driven to an ACTIVATED datapath by xcvrd's
CmisManagerTask, not merely marked ``cmis_state=READY``:

  * the module is taken to high power  -> TRANSCEIVER_STATUS.module_state=ModuleReady
  * every configured datapath activates -> TRANSCEIVER_STATUS.DP{n}State=DataPathActivated
  * a REAL per-host-lane application-select lands in TRANSCEIVER_INFO
    (active_apsel_hostlane{n} != 'N/A'), with real host_lane_count/media_lane_count

A reduced daemon that just marks cmis_state READY and writes 'N/A' apsel (the
admin-down short-circuit) fails here. We also cross-check the EMULATOR's own view
(GetInfo.dpsms) so the test proves xcvrd actually drove the module's datapath
state machine, not only that it wrote STATE_DB.

The port must be admin-up in CONFIG_DB (default Ethernet4 on the KVM testbed);
override with XCVRD_ACTIVATED_PORT.
"""
import os

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.waits import wait_until, T_FAST, T_DOM

ACTIVATED_PORT = os.environ.get("XCVRD_ACTIVATED_PORT", "Ethernet4")

pytestmark = pytest.mark.slow


@pytest.fixture
def activated(emu, statedb, configdb):
    idx = port_to_index(ACTIVATED_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({ACTIVATED_PORT})")
    if configdb.hget(f"PORT|{ACTIVATED_PORT}", "admin_status") != "up":
        pytest.skip(f"{ACTIVATED_PORT} is not admin-up; CMIS bring-up only runs on "
                    "admin-up ports. Set XCVRD_ACTIVATED_PORT to an admin-up port.")
    emu.plug(idx)
    yield ACTIVATED_PORT, idx
    emu.plug(idx)


def _status(statedb, port):
    return statedb.hgetall(f"TRANSCEIVER_STATUS|{port}")


def test_cmis_datapath_activated(activated, statedb):
    """xcvrd drives the datapath up: ModuleReady + all DP{n} activated + real apsel."""
    port, _ = activated
    wait_until(lambda: (_status(statedb, port).get("module_state") == "ModuleReady"
                        and _status(statedb, port).get("DP1State") == "DataPathActivated"),
               timeout=T_DOM, msg=f"{port} datapath activated (ModuleReady + DP1State)")

    st = _status(statedb, port)
    assert st.get("module_state") == "ModuleReady", f"module_state={st.get('module_state')!r}"

    n = int(statedb.hget(f"TRANSCEIVER_INFO|{port}", "host_lane_count") or 0)
    assert n >= 1, f"host_lane_count={n!r} (expected a real lane count, not N/A)"

    for i in range(1, n + 1):
        assert st.get(f"DP{i}State") == "DataPathActivated", \
            f"DP{i}State={st.get(f'DP{i}State')!r} (expected DataPathActivated)"
        apsel = statedb.hget(f"TRANSCEIVER_INFO|{port}", f"active_apsel_hostlane{i}")
        assert apsel not in (None, "N/A"), \
            f"active_apsel_hostlane{i}={apsel!r} (expected a real appsel; 'N/A' means the " \
            "CMIS datapath was never actually brought up)"


def test_cmis_emulator_datapath_agrees(activated, emu, statedb):
    """Cross-check: xcvrd drove the EMULATOR's datapath state machine to activated
    (GetInfo.dpsms), proving real bring-up rather than just a STATE_DB write."""
    port, idx = activated
    wait_until(lambda: _status(statedb, port).get("DP1State") == "DataPathActivated",
               timeout=T_DOM, msg=f"{port} DP1State activated before emulator cross-check")

    info = emu.get_info(idx)
    assert info.dpsms, f"emulator reports no datapath state machines for {port}"

    def _is_activated(state):
        # str(DPStateHostLane.DPACTIVATED); DPDEACTIVATED also contains 'ACTIVATED'.
        return "ACTIVATED" in state and "DEACTIVATED" not in state

    bad = [(dp.bank, dp.dpid, dp.state) for dp in info.dpsms if not _is_activated(dp.state)]
    assert not bad, (f"{port}: emulator datapath(s) not activated: {bad} -- xcvrd did not "
                     "drive the module's datapath state machine")


def test_cmis_emulator_module_state_agrees(activated, emu, statedb):
    """Cross-check: xcvrd drove the EMULATOR's MODULE state machine to ModuleReady
    (GetInfo.msm), proving real module bring-up (high power), not just a STATE_DB
    write. Complements the datapath cross-check above.

    Requires an emulator that exposes the module state machine via GetInfo.msm;
    older emulators leave it unset and this test skips rather than fails."""
    port, idx = activated
    wait_until(lambda: _status(statedb, port).get("module_state") == "ModuleReady",
               timeout=T_DOM,
               msg=f"{port} module_state ModuleReady before emulator cross-check")

    info = emu.get_info(idx)
    if not info.msm.state:
        pytest.skip("emulator does not expose the module state machine (GetInfo.msm); "
                    "update the emulator (feature/getinfo-msm) to enable this cross-check")
    assert "READY" in info.msm.state, (
        f"{port}: emulator module state machine msm.state={info.msm.state!r} "
        "(expected MODULE_READY) -- xcvrd did not drive the module to high power")


def _first_write_ts(monitor, idx, lo, hi):
    """Earliest Monitor timestamp of a page-10h write touching any offset in
    [lo, hi] for a module (None if no such write was seen)."""
    tss = [e.ts for e in monitor.writes(index=idx, page=cmis.SCS0_PAGE)
           if any(lo <= off <= hi for off in range(e.offset, e.offset + e.length))]
    return min(tss) if tss else None


def test_cmis_bringup_provisioning_write_order(activated, emu, statedb, monitor):
    """xcvrd drives the CMIS bring-up provisioning writes in the correct causal
    order: DataPathDeinit (10h:128) to tear down, THEN the DPConfigLane app-select
    bytes (10h:145-152), THEN the ApplyDPInitLane trigger (10h:143) to provision.
    Asserted on the Monitor stream by first-occurrence timestamp -- proving xcvrd
    runs the real CMIS state machine (DP_DEINIT -> AP_CONF -> DP_INIT), not just
    writes STATE_DB. A daemon that provisions before deinit, or never triggers
    ApplyDPInit, fails here."""
    port, idx = activated
    # Force a fresh bring-up so the whole provisioning sequence lands on the trace.
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} removal detected before re-insert")
    monitor.clear()
    emu.plug(idx)

    # Wait for the LAST milestone (ApplyDPInit) so the full sequence is captured.
    wait_until(
        lambda: _first_write_ts(monitor, idx, cmis.APPLY_DPINIT_OFFSET, cmis.APPLY_DPINIT_OFFSET) is not None,
        timeout=T_DOM, msg=f"{port} ApplyDPInitLane trigger (10h:143) write during bring-up")

    t_deinit = _first_write_ts(monitor, idx, cmis.DPDEINIT_OFFSET, cmis.DPDEINIT_OFFSET)
    t_config = _first_write_ts(monitor, idx, cmis.SCS0_DPCONFIG_RANGE.start,
                               cmis.SCS0_DPCONFIG_RANGE.stop - 1)
    t_apply = _first_write_ts(monitor, idx, cmis.APPLY_DPINIT_OFFSET, cmis.APPLY_DPINIT_OFFSET)

    assert t_deinit is not None, f"{port}: no DataPathDeinit (10h:128) write during bring-up"
    assert t_config is not None, f"{port}: no DPConfigLane (10h:145-152) write during bring-up"
    assert t_deinit <= t_config <= t_apply, (
        f"{port}: CMIS provisioning writes out of order -- DataPathDeinit@{t_deinit:.3f}, "
        f"DPConfigLane@{t_config:.3f}, ApplyDPInit@{t_apply:.3f} (expected deinit <= config "
        "<= apply)")


def test_activated_per_lane_status_invariants(activated, statedb):
    """The per-lane TRANSCEIVER_STATUS fields on an activated port are mutually
    consistent (a cross-field invariant the golden value-match does not check):
      * tx_disabled_channel is the bitmask of the per-lane tx{n}disable booleans,
      * every active host lane (1..host_lane_count) is DataPathActivated and NOT
        dpdeinit, while unused lanes are dpdeinit.
    A daemon that publishes inconsistent per-lane status (e.g. a tx_disabled_channel
    mask that disagrees with the tx{n}disable flags) fails here."""
    port, _ = activated
    wait_until(lambda: _status(statedb, port).get("DP1State") == "DataPathActivated",
               timeout=T_DOM, msg=f"{port} DP1 activated before invariant check")
    st = _status(statedb, port)
    n = int(statedb.hget(f"TRANSCEIVER_INFO|{port}", "host_lane_count") or 0)
    assert n >= 1, f"{port} host_lane_count={n!r}"

    tdc = int(st.get("tx_disabled_channel") or 0)
    for i in range(1, 9):
        bit = bool((tdc >> (i - 1)) & 1)
        flag = st.get(f"tx{i}disable") == "True"
        assert bit == flag, (
            f"{port}: tx_disabled_channel bit {i - 1} ({bit}) disagrees with "
            f"tx{i}disable ({st.get(f'tx{i}disable')!r}) -- per-lane tx-disable state is inconsistent")

    for i in range(1, n + 1):
        assert st.get(f"DP{i}State") == "DataPathActivated", \
            f"{port}: active lane {i} DP{i}State={st.get(f'DP{i}State')!r} (expected DataPathActivated)"
        assert st.get(f"dpdeinit_hostlane{i}") == "False", \
            f"{port}: active lane {i} dpdeinit_hostlane{i}={st.get(f'dpdeinit_hostlane{i}')!r} (expected False)"
    for i in range(n + 1, 9):
        assert st.get(f"dpdeinit_hostlane{i}") == "True", \
            f"{port}: unused lane {i} dpdeinit_hostlane{i}={st.get(f'dpdeinit_hostlane{i}')!r} (expected True)"
