"""lpmode / reset control-plane (write-trace + module-state-machine assertions).

The operator commands `sfputil reset` and `sfputil lpmode` drive the module
through the host sonic_platform bridge, which translates them into CMIS
ModuleGlobalControls (00h:26) writes to the emulator. We assert the exact
register write appears on the Monitor stream:
  - reset       -> 00h:26.3 SoftwareReset bit (0x08)
  - lpmode on   -> 00h:26.4 LowPwrRequestSW bit (0x10)

...and that those commands drive the module itself: the emulator MODULE state
machine (GetInfo.msm) follows lpmode, and a reset re-initialises the module
(datapath torn down) with a subsequent re-plug fully re-provisioning the port.
"""
import os

import pytest

from lib import cmis, sfputil
from lib.emu import port_to_index
from lib.waits import eventually, wait_until, T_FAST, T_BURST, T_DOM

RESET_PORT = os.environ.get("XCVRD_RESET_PORT", "Ethernet16")


def _mgc_writes(monitor, index):
    """Writes that touch ModuleGlobalControls (00h:26) for a module, as the
    byte value written at offset 26."""
    out = []
    for e in monitor.writes(index=index, page=cmis.MGC_PAGE):
        if e.offset <= cmis.MGC_OFFSET < e.offset + e.length:
            out.append(e.data[cmis.MGC_OFFSET - e.offset])
    return out


def _dp1_activated(emu, idx):
    """True iff the module reports host lane 1's datapath activated (page 11h:128)."""
    return cmis.decode_dp_lane_states(
        emu.read(idx, 0, cmis.DP_STATE_PAGE, cmis.DP_STATE_OFFSET, 4, force=True)
    )[0] == cmis.DP_STATE_ACTIVATED


def test_reset_writes_software_reset_bit(monitor, module, sfp_control):
    module.wait_info_populated(timeout=T_FAST)
    monitor.clear()
    rc = sfp_control.reset(module.port)
    assert rc.returncode == 0, f"sfputil reset failed: {rc.stderr or rc.stdout}"

    vals = eventually(lambda: _mgc_writes(monitor, module.index) or None,
                      timeout=T_BURST,
                      msg=f"ModuleGlobalControls write for {module.port} on reset")
    assert any(v & cmis.SOFTWARE_RESET_BIT for v in vals), \
        f"no SoftwareReset bit (0x08) in MGC writes {[hex(v) for v in vals]}"


def test_lpmode_on_writes_lowpwr_bit(monitor, module, sfp_control):
    module.wait_info_populated(timeout=T_FAST)
    monitor.clear()
    rc = sfp_control.lpmode(module.port, on=True)
    assert rc.returncode == 0, f"sfputil lpmode on failed: {rc.stderr or rc.stdout}"

    vals = eventually(lambda: _mgc_writes(monitor, module.index) or None,
                      timeout=T_BURST,
                      msg=f"ModuleGlobalControls write for {module.port} on lpmode on")
    assert any(v & cmis.LOW_PWR_REQUEST_BIT for v in vals), \
        f"no LowPwrRequestSW bit (0x10) in MGC writes {[hex(v) for v in vals]}"


def test_lpmode_reported_on_then_off(module, sfp_control):
    """The lpmode state round-trips through sfputil show."""
    assert sfp_control.lpmode(module.port, on=True).returncode == 0
    assert sfp_control.show_lpmode(module.port) == "On"
    assert sfp_control.lpmode(module.port, on=False).returncode == 0
    assert sfp_control.show_lpmode(module.port) == "Off"


@pytest.fixture
def disruptable(emu, statedb, configdb):
    """An admin-up, datapath-activated port for DISRUPTIVE control tests where the
    command actually tears the module down (lpmode / reset). Restores lpmode-off +
    a clean re-plug + waits for re-activation on teardown so the port is left
    healthy regardless of how the test exited (reset does NOT auto-recover -- it
    needs a presence event)."""
    idx = port_to_index(RESET_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({RESET_PORT})")
    if configdb.hget(f"PORT|{RESET_PORT}", "admin_status") != "up":
        pytest.skip(f"{RESET_PORT} is not admin-up; disruptive control tests need an admin-up "
                    "port. Set XCVRD_RESET_PORT to an admin-up port.")
    emu.plug(idx)
    wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM,
               msg=f"{RESET_PORT} datapath activated before disruptive control test")
    yield RESET_PORT, idx
    try:
        sfputil.lpmode(RESET_PORT, on=False)
        emu.unplug(idx)
        wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{RESET_PORT}", "manufacturer"),
                   timeout=T_DOM)
        emu.plug(idx)
        wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM)
    except Exception:  # noqa: BLE001
        pass


def test_lpmode_on_drives_module_state_machine_low_power(disruptable, emu):
    """sfputil lpmode on drives the MODULE itself to low power: the emulator's
    module state machine (GetInfo.msm) reaches MODULE_LOW_PWR -- not just a
    register write. A bridge/daemon that no-ops lpmode never moves it.

    (Observed via eventually(): on an admin-up port xcvrd may later re-drive the
    module back to high power, but the command drives the emulator to low power
    first, which is the parity signal.)"""
    port, idx = disruptable
    if not emu.get_info(idx).msm.state:
        pytest.skip("emulator does not expose the module state machine (GetInfo.msm)")
    rc = sfputil.lpmode(port, on=True)
    assert rc.returncode == 0, f"sfputil lpmode on failed: {rc.stderr or rc.stdout}"
    eventually(lambda: "LOW_PWR" in emu.get_info(idx).msm.state, timeout=T_BURST,
               msg=f"{port} emulator msm -> MODULE_LOW_PWR after lpmode on")


def test_reset_disrupts_then_replug_recovers(disruptable, emu, statedb):
    """sfputil reset re-initialises the module: its datapath tears down
    (DataPathStateLane -> deactivated on page 11h) and does NOT auto-recover; a
    subsequent re-plug (presence event) makes xcvrd re-drive full CMIS bring-up
    back to an activated datapath."""
    port, idx = disruptable
    assert _dp1_activated(emu, idx), f"{port} not activated at start"

    rc = sfputil.reset(port)
    assert rc.returncode == 0, f"sfputil reset failed: {rc.stderr or rc.stdout}"

    # Module effect: reset re-inits the module, tearing the datapath down.
    wait_until(lambda: not _dp1_activated(emu, idx), timeout=T_DOM,
               msg=f"{port} datapath torn down on the module after reset")

    # Recovery: a re-plug drives xcvrd to re-provision back to activated.
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_DOM, msg=f"{port} removal seen before recovery re-plug")
    emu.plug(idx)
    wait_until(lambda: _dp1_activated(emu, idx), timeout=T_DOM,
               msg=f"{port} datapath re-activated after recovery re-plug")
