"""DOM VDM-statistic freeze is skipped while the module is in low-power mode (DOM-lpmode).

test_dom_gating.py covers one half of the DOM gate (CMIS-init: DOM skipped while the
port is non-terminal). This covers the lpmode half. The only lpmode gate on the DOM
poll is the VDM statistic-freeze block (dom_mgr.py:386-387): xcvrd freezes VDM and
publishes TRANSCEIVER_PM (and the VDM statistic real values) only when the module is
NOT in low power. Put an activated coherent module into lpmode and the freeze -- and
therefore the TRANSCEIVER_PM refresh -- must stop; take it back out of lpmode and PM
resumes.

Built on the coherent PM module (see lib/pm.py + test_pm.py). We provision PM, confirm
it is published, then set lpmode (which holds: cmis_state stays READY and the module
goes ModuleLowPwr), delete the PM row, and assert xcvrd does NOT republish it while in
low power -- proving the freeze is gated -- then clear lpmode and confirm PM comes back.
A reduced daemon that freezes/publishes PM regardless of lpmode republishes the row and
fails. Basic DOM_SENSOR is NOT gated by lpmode, so this isolates the freeze gate.

Uses the coherent PM port (default Ethernet44); override with XCVRD_PM_PORT. Skips if it
is not coherent. The fixture always clears lpmode + deprovisions on teardown.
"""
import os

import pytest

from lib import pm, sfputil
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, stays, T_FAST, T_DOM

pytestmark = pytest.mark.slow

PM_PORT = os.environ.get("XCVRD_PM_PORT", "Ethernet44")
PM_TABLE = "TRANSCEIVER_PM"
COHERENT_INFO_MARKER = "supported_max_laser_freq"


def _pm_present(statedb, port):
    return statedb.exists(f"{PM_TABLE}|{port}")


@pytest.fixture
def pm_lpmode(emu, statedb, configdb):
    """Coherent PM module with PM provisioned; teardown clears lpmode, deprovisions,
    re-plugs and flushes PM/VDM so nothing leaks."""
    port = PM_PORT
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port})")
    if configdb.hget(f"PORT|{port}", "admin_status") != "up":
        pytest.skip(f"{port} is not admin-up; PM freeze only runs on an admin-up port.")
    emu.plug(idx)
    try:
        wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", COHERENT_INFO_MARKER) is not None,
                   timeout=T_DOM, msg=f"{port} coherent (C-CMIS) module")
    except Exception:  # noqa: BLE001
        pytest.skip(f"{port} is not a coherent (C-CMIS) module; the lpmode freeze gate needs the "
                    "PM/coherent module (advertise a 400GBASE-ZR media interface)")
    pm.provision(emu, idx)
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"), timeout=T_FAST)
    emu.plug(idx)
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"), timeout=T_FAST)
    yield port, idx
    try:
        sfputil.lpmode(port, on=False)
        pm.deprovision(emu, idx)
        emu.unplug(idx)
        wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"), timeout=T_FAST)
        emu.plug(idx)
        statedb.delete(f"{PM_TABLE}|{port}")
        statedb.delete_pattern(f"TRANSCEIVER_VDM*{port}*")
    except Exception:  # noqa: BLE001
        pass


def test_lpmode_gates_pm_freeze(pm_lpmode, statedb):
    """PM is published at baseline, stops being refreshed while the module is in
    low power (freeze gated), and resumes once low power is cleared."""
    port, _ = pm_lpmode

    # Baseline: PM published (the freeze runs on an admin-up, non-lpmode module).
    eventually(lambda: _pm_present(statedb, port) or None, timeout=2 * T_DOM,
               msg=f"{port} TRANSCEIVER_PM published at baseline")

    # Enter low power -> the freeze must be skipped, so a deleted PM row must NOT
    # be republished for a full DOM cycle.
    sfputil.lpmode(port, on=True)
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_STATUS|{port}", "module_state") == "ModuleLowPwr",
               timeout=T_DOM, msg=f"{port} module reaches ModuleLowPwr")
    statedb.delete(f"{PM_TABLE}|{port}")
    assert stays(lambda: not _pm_present(statedb, port), duration=T_DOM + 20), (
        f"{port}: TRANSCEIVER_PM was republished while the module is in low power -- the "
        "VDM statistic-freeze must be gated by lpmode (dom_mgr.py:387)")

    # Clear low power -> the freeze resumes and PM comes back.
    sfputil.lpmode(port, on=False)
    wait_until(lambda: _pm_present(statedb, port), timeout=2 * T_DOM,
               msg=f"{port} TRANSCEIVER_PM republished after clearing low power")
