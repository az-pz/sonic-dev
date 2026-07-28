"""Media / optics SI application (T3.3).

xcvrd's CmisManagerTask applies per-vendor Signal-Integrity settings from
``optics_si_settings.json`` during CMIS bring-up: for each SI parameter the module
advertises support for, it writes the value into the page-10h Staged Control Set
and flips ``ExplicitControl`` so the lane uses those staged values instead of the
application defaults. A reduced daemon that stubs media/SI application never
touches the page-10h SI region -- so observing those writes on the emulator
Monitor stream is a parity gate for "the daemon actively applies optics SI".

This is a pure harness stimulus (no emulator image change): the test
  * provisions an ``optics_si_settings.json`` (vendor XCVR-EMU) into the platform
    dir and restarts xcvrd so it loads it (see lib/optics_si.py),
  * advertises SI-control support (01h:161/162) on the module via the emulator,
  * forces a fresh bring-up and asserts xcvrd stages the CDR-enable SI controls
    (page-10h 153-175) and sets ExplicitControl=1 in the DPConfigLane bytes.

The provisioning restarts xcvrd and drops a platform file, so this test fully
restores the no-SI baseline on teardown. It targets an admin-up port (the only
ports whose datapath xcvrd brings up); the JSON vendor key is a regex because the
emulator returns a NUL-padded vendor name that the parser's ``.strip()`` leaves
intact.
"""
import os

import pytest

from lib import cmis, optics_si
from lib.emu import port_to_index, index_to_port
from lib.waits import wait_until, eventually, T_FAST, T_DOM, T_BASELINE

ACTIVATED_PORT = os.environ.get("XCVRD_ACTIVATED_PORT", "Ethernet4")

pytestmark = pytest.mark.slow


@pytest.fixture
def optics_si_loaded(emu, statedb, configdb, xcvrd, test_index):
    """Provision optics_si_settings.json + restart xcvrd so SI is loaded; restore
    the no-SI baseline (remove file, clear advertisement, restart) on teardown."""
    idx = port_to_index(ACTIVATED_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({ACTIVATED_PORT})")
    if configdb.hget(f"PORT|{ACTIVATED_PORT}", "admin_status") != "up":
        pytest.skip(f"{ACTIVATED_PORT} is not admin-up; xcvrd only applies SI during "
                    "bring-up on admin-up ports. Set XCVRD_ACTIVATED_PORT.")
    if not optics_si.sudo_available():
        pytest.skip("passwordless sudo required to provision optics_si_settings.json")

    src = os.path.join(os.path.dirname(__file__), "data", optics_si.SI_FILENAME)
    try:
        optics_si.provision(src)
    except Exception as e:  # noqa: BLE001
        pytest.skip(f"could not provision optics_si_settings.json: {e}")

    baseline_port = index_to_port(test_index)
    if not xcvrd.wait_healthy(baseline_port, timeout=T_BASELINE):
        optics_si.deprovision()
        xcvrd.restart()
        pytest.fail("xcvrd not healthy after provisioning optics_si_settings.json")

    yield ACTIVATED_PORT, idx

    # Restore: remove the SI file, clear the advertisement, and restart so the
    # rest of the session runs against the stock no-SI baseline.
    optics_si.deprovision()
    try:
        emu.write(idx, 0, 1, cmis.SI_ADV_TX_CDR_OFFSET, bytes([0x00]))
        emu.write(idx, 0, 1, cmis.SI_ADV_RX_CDR_OFFSET, bytes([0x00]))
        emu.plug(idx)
    except Exception:  # noqa: BLE001
        pass
    xcvrd.wait_healthy(baseline_port, timeout=T_BASELINE)


def _page10_writes(monitor, idx, lo, hi):
    return [e for e in monitor.writes(index=idx, page=0x10) if lo <= e.offset <= hi]


def test_xcvrd_applies_optics_si(optics_si_loaded, emu, statedb, monitor):
    """With optics SI provisioned, xcvrd stages the custom SI controls on bring-up.

    Asserts xcvrd writes the CDR-enable SI controls into the page-10h Staged
    Control Set (offsets 153-175) and flips ExplicitControl=1 in the DPConfigLane
    bytes. A daemon that does not apply media/optics SI never writes that region.
    """
    port, idx = optics_si_loaded
    # Advertise SI-control support so xcvrd stages the CDR settings (the emulator
    # config advertises none; xcvrd skips unsupported SI params).
    emu.write(idx, 0, 1, cmis.SI_ADV_TX_CDR_OFFSET, bytes([cmis.TX_CDR_SUPPORTED]))
    emu.write(idx, 0, 1, cmis.SI_ADV_RX_CDR_OFFSET, bytes([cmis.RX_CDR_SUPPORTED]))

    # Force a fresh CMIS bring-up so the SI-staging writes land on the trace.
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} removal detected before re-insert")
    monitor.clear()
    emu.plug(idx)

    lo, hi = cmis.SCS0_SI_CONTROL_RANGE.start, cmis.SCS0_SI_CONTROL_RANGE.stop - 1
    si_writes = eventually(
        lambda: _page10_writes(monitor, idx, lo, hi) or None,
        timeout=T_DOM,
        msg=f"{port} xcvrd staged optics SI controls (page-10h {lo}-{hi}) on bring-up")
    assert si_writes, (
        f"{port}: xcvrd wrote no page-10h SI controls -- it did not apply optics SI")

    # ExplicitControl must be set so the module actually uses the staged SI values.
    # Poll for it rather than reading once: on a busy bring-up (other ports
    # re-provisioning concurrently) the DPConfigLane write can land a beat after the
    # SI-control writes, so a single immediate read can race ahead of it.
    dpc_lo = cmis.SCS0_DPCONFIG_RANGE.start
    dpc_hi = cmis.SCS0_DPCONFIG_RANGE.stop - 1

    def _explicit_control_writes():
        dpc = _page10_writes(monitor, idx, dpc_lo, dpc_hi)
        return dpc if any(e.data and (e.data[0] & cmis.EXPLICIT_CONTROL_BIT) for e in dpc) else None

    dpc = eventually(
        _explicit_control_writes, timeout=T_DOM,
        msg=f"{port} xcvrd set ExplicitControl=1 in a DPConfigLane write "
            f"(page-10h {dpc_lo}-{dpc_hi}) on bring-up")
    assert dpc, (
        f"{port}: no DPConfigLane write set ExplicitControl=1 -- the module would not "
        "use the staged SI values")
