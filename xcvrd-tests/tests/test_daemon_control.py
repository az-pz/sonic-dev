"""Daemon-driven module power control (T3.2).

Distinct from test_lpmode_reset.py, where an OPERATOR `sfputil lpmode/reset`
command drives the module: here xcvrd ITSELF drives the module's power state as
part of CMIS bring-up. For an admin-up port, the CmisManagerTask takes the module
OUT of low power (clears LowPwrRequestSW in ModuleGlobalControls 00h:26) to reach
ModuleReady. The emulator initialises every module in low power, so a daemon that
drives bring-up must issue an MGC write clearing that bit -- observable as xcvrd's
own write on the Monitor stream and cross-checked against the emulator module
state machine (GetInfo.msm == MODULE_READY). A reduced daemon that never drives
module power leaves it in low power and fails here.

Reset stays host-driven (xcvrd does not SoftwareReset a module in normal
operation) -- that path is covered by test_lpmode_reset.py.
"""
import os

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, T_FAST, T_DOM

ACTIVATED_PORT = os.environ.get("XCVRD_ACTIVATED_PORT", "Ethernet4")

pytestmark = pytest.mark.slow


@pytest.fixture
def activated(emu, statedb, configdb):
    idx = port_to_index(ACTIVATED_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({ACTIVATED_PORT})")
    if configdb.hget(f"PORT|{ACTIVATED_PORT}", "admin_status") != "up":
        pytest.skip(f"{ACTIVATED_PORT} is not admin-up; the daemon only drives module "
                    "power on admin-up ports. Set XCVRD_ACTIVATED_PORT to an admin-up port.")
    emu.plug(idx)
    yield ACTIVATED_PORT, idx
    emu.plug(idx)


def _mgc_writes(monitor, index):
    """The byte values xcvrd wrote to ModuleGlobalControls (00h:26) for a module."""
    out = []
    for e in monitor.writes(index=index, page=cmis.MGC_PAGE):
        if e.offset <= cmis.MGC_OFFSET < e.offset + e.length:
            out.append(e.data[cmis.MGC_OFFSET - e.offset])
    return out


def test_daemon_drives_module_out_of_low_power(activated, emu, statedb, monitor):
    """On CMIS bring-up xcvrd clears LowPwrRequestSW itself (daemon-driven lpmode
    exit), reaching ModuleReady -- not an operator sfputil command."""
    port, idx = activated
    # Force a fresh bring-up so the daemon's power-control writes are on the trace.
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} removal detected before re-insert")
    monitor.clear()
    emu.plug(idx)

    # xcvrd writes ModuleGlobalControls with the LowPwrRequestSW bit CLEARED. The
    # emulator boots the module in low power (0x10), so a cleared write is the
    # daemon actively driving the module to high power.
    vals = eventually(lambda: _mgc_writes(monitor, idx) or None, timeout=T_DOM,
                      msg=f"{port} xcvrd ModuleGlobalControls write during bring-up")
    assert any((v & cmis.LOW_PWR_REQUEST_BIT) == 0 for v in vals), (
        f"{port}: xcvrd never cleared LowPwrRequestSW (MGC writes="
        f"{[hex(v) for v in vals]}) -- the daemon did not drive the module out of low power")


def test_daemon_low_power_exit_reaches_module_ready(activated, emu, statedb, monitor):
    """The daemon-driven low-power exit actually lands: STATE_DB module_state and
    the emulator's own module state machine both read ModuleReady."""
    port, idx = activated
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} removal detected before re-insert")
    monitor.clear()
    emu.plug(idx)

    wait_until(lambda: statedb.hget(f"TRANSCEIVER_STATUS|{port}", "module_state") == "ModuleReady",
               timeout=T_DOM, msg=f"{port} module_state ModuleReady after daemon low-power exit")

    # Cross-check the emulator's module state machine (needs GetInfo.msm; older
    # emulators leave it unset and this assertion is skipped).
    info = emu.get_info(idx)
    if not info.msm.state:
        pytest.skip("emulator does not expose the module state machine (GetInfo.msm)")
    assert "READY" in info.msm.state, (
        f"{port}: emulator msm.state={info.msm.state!r} (expected MODULE_READY) -- "
        "the daemon did not drive the module to high power")
