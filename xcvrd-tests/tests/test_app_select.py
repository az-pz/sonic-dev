"""Application selection across speeds / lane-counts (B15).

xcvrd chooses the CMIS application from the module's application advertisement by
matching the port's host lane count + speed (common.get_cmis_application_desired,
228-257) and provisions it. The rest of the suite only ever exercises the single
default 40G application because every emulated module and port is 40G/4-lane.

The multi-app module (emu-deploy/provision_special_modules.sh serves idx14 /
Ethernet56 with app1 = XLAUI 40G and app2 = CAUI-4 100G, both 4-lane) lets us change
the port speed while keeping 4 host lanes: at 40G xcvrd selects app1 (AppSelCode 1),
and reconfiguring the port to 100G it must re-select app2 (AppSelCode 2). Switching
the active application drives the CMIS decommission -> re-provision handshake (reset
AppSel to 0, then provision the new app), so this also exercises that path end to
end. A reduced daemon that always applies the default application never moves the
active AppSel off 1.

Needs the emulator's "ConfigSuccess on decommission" support (xcvr-emu
feature/multi-app-datapath) for the re-provision to converge. Uses the multi-app
module (default Ethernet56); override with XCVRD_APPSEL_PORT. Skips cleanly if the
module does not advertise a second 100G/4-lane app, so the suite stays portable. The
fixture always restores the original port speed so the port returns to its 40G/app1
baseline.
"""
import os

import pytest

from lib.emu import port_to_index
from lib.waits import wait_until, T_DOM, T_BASELINE

pytestmark = pytest.mark.slow

APPSEL_PORT = os.environ.get("XCVRD_APPSEL_PORT", "Ethernet56")
INFO = "TRANSCEIVER_INFO"
STATUS_SW = "TRANSCEIVER_STATUS_SW"
APP2_SPEED = "100000"          # CAUI-4 (app2) advertised speed on the multi-app module
APP2_HOST_EI = 11              # 00h:90 host_electrical_interface_id for app2 (CAUI-4)
ACTIVE_APSEL = "active_apsel_hostlane1"


def _apsel(statedb, port):
    return statedb.hget(f"{INFO}|{port}", ACTIVE_APSEL)


def _cmis(statedb, port):
    return statedb.hgetall(f"{STATUS_SW}|{port}").get("cmis_state")


def _ready_with_apsel(statedb, port, code):
    return _cmis(statedb, port) == "READY" and _apsel(statedb, port) == code


@pytest.fixture
def appsel_port(emu, statedb, configdb):
    port = APPSEL_PORT
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port})")
    emu.plug(idx)
    # Require a second 100G/4-lane app (host EI at 00h:90) or the branch is not exercisable.
    app2 = emu.read(idx, 0, 0, 90, 3, force=True)
    if app2[0] != APP2_HOST_EI:
        pytest.skip(f"{port} does not advertise a second 100G/4-lane app (00h:90={app2[0]:#x}); "
                    "provision it via emu-deploy/provision_special_modules.sh")
    orig_speed = configdb.hget(f"PORT|{port}", "speed")
    if orig_speed != "40000":
        # normalise to the 40G/app1 baseline before the test
        configdb.hset(f"PORT|{port}", "speed", "40000")
    wait_until(lambda: _ready_with_apsel(statedb, port, "1"), timeout=T_BASELINE,
               msg=f"{port} READY with app1 (AppSel 1) at 40G baseline")
    yield port, idx, orig_speed
    # Restore the original port speed + its baseline app.
    try:
        configdb.hset(f"PORT|{port}", "speed", orig_speed or "40000")
        want = "2" if (orig_speed == APP2_SPEED) else "1"
        wait_until(lambda: _ready_with_apsel(statedb, port, want), timeout=T_BASELINE)
    except Exception:  # noqa: BLE001
        pass


def test_app_selection_default_speed(appsel_port, statedb):
    """At 40G the module runs app1 (AppSelCode 1) -- the default application."""
    port, _, _ = appsel_port
    assert _apsel(statedb, port) == "1", (
        f"{port}: active AppSel {_apsel(statedb, port)} at 40G (expected 1 / app1)")


def test_app_selection_follows_port_speed(appsel_port, statedb, configdb):
    """Reconfiguring the port from 40G to 100G makes xcvrd re-select the 100G
    application: the active AppSel moves 1 -> 2 and the datapath reaches READY on
    app2 (via the decommission -> re-provision handshake)."""
    port, _, _ = appsel_port
    assert _apsel(statedb, port) == "1", f"{port} not on app1 at start"

    configdb.hset(f"PORT|{port}", "speed", APP2_SPEED)

    wait_until(lambda: _apsel(statedb, port) == "2", timeout=2 * T_DOM,
               msg=f"{port} active AppSel 1 -> 2 (app2 / CAUI-4 100G) after the 100G reconfig")
    wait_until(lambda: _cmis(statedb, port) == "READY", timeout=T_DOM,
               msg=f"{port} datapath READY on app2 after the 100G reconfig "
                   "(decommission -> re-provision converged)")
