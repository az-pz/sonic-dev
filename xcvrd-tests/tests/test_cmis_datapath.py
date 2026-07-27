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
