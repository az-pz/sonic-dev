"""Rich TRANSCEIVER_STATUS table coverage (module + per-lane datapath state).

xcvrd's DomInfoUpdateTask publishes TRANSCEIVER_STATUS with the CMIS module state
and per-host-lane datapath / config / tx-rx state. For the admin-down baseline
test port this reads the DEACTIVATED state (ModuleLowPwr / DataPathDeactivated);
the ACTIVATED case (admin-up port) is covered by test_cmis_datapath.py. A
candidate daemon that only publishes TRANSCEIVER_STATUS_SW (not the rich STATUS)
fails here.
"""
import pytest

from lib.waits import wait_until, T_DOM

pytestmark = pytest.mark.slow

DP_STATES = [f"DP{i}State" for i in range(1, 9)]
CONFIG_STATES = [f"config_state_hostlane{i}" for i in range(1, 9)]


def _status_published(module):
    module.plug()
    wait_until(lambda: module.status().get("module_state"), timeout=T_DOM,
               msg=f"{module.port} TRANSCEIVER_STATUS published")
    return module.status()


def test_status_table_published(module):
    """xcvrd publishes the rich STATUS table with the module + all 8 datapath fields."""
    st = _status_published(module)
    assert st.get("module_state"), f"module_state missing: {st}"
    assert "module_fault_cause" in st, "module_fault_cause missing from TRANSCEIVER_STATUS"
    for f in DP_STATES + CONFIG_STATES:
        assert f in st, f"{f} missing from TRANSCEIVER_STATUS"


def test_status_baseline_deactivated(module, configdb):
    """Admin-down baseline: module in low power, every datapath deactivated."""
    if configdb.hget(f"PORT|{module.port}", "admin_status") == "up":
        pytest.skip(f"{module.port} is admin-up; the deactivated-baseline check is for "
                    "admin-down ports (activated case is in test_cmis_datapath.py)")
    st = _status_published(module)
    assert st.get("module_state") == "ModuleLowPwr", \
        f"module_state={st.get('module_state')!r} (expected ModuleLowPwr for admin-down {module.port})"
    for f in DP_STATES:
        assert st.get(f) == "DataPathDeactivated", f"{f}={st.get(f)!r} (expected DataPathDeactivated)"
