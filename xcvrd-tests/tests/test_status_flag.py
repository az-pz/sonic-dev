"""TRANSCEIVER_STATUS_FLAG coverage (CMIS module + lane latched flags).

xcvrd's DomInfoUpdateTask reads the module's latched fault/state flags and
publishes TRANSCEIVER_STATUS_FLAG. The reduced Rust daemon publishes no flag
tables at all, so this is a hard parity gate: we raise the module-level
ModuleFirmwareErrorFlag (CMIS v5.2 8.9, lower page byte 8) on the emulator and
assert xcvrd reflects it as STATUS_FLAG.module_firmware_fault, then that it
clears again. No golden -- the stimulus/observe is a self-contained assertion,
and it needs no emulator change (the emulator holds the flag register with no
clear-on-read, like the DOM flags).

The per-host-lane flags (tx fault / rx los, page 11h) read 'N/A' on this module
because its config does not advertise lane-flag support (01h:157/158); covering
those would require the emulator to advertise + serve the page 11h lane flags.
"""
import pytest

from lib import cmis
from lib.waits import wait_until, T_DOM

pytestmark = pytest.mark.slow

MODULE_FIELDS = ("module_firmware_fault", "datapath_firmware_fault", "module_state_changed")


@pytest.fixture
def status_flags(module):
    """``module`` handle whose byte-8 module flags are restored on teardown so a
    raised fault flag can't leak into later tests / the next user."""
    snap = module.emu.read_field(module.index, cmis.MODULE_FLAGS_FW_STATE)
    yield module
    try:
        module.emu.write_field(module.index, cmis.MODULE_FLAGS_FW_STATE, snap)
    except Exception:  # noqa: BLE001
        pass


def _flags(module):
    return module.db.hgetall(f"TRANSCEIVER_STATUS_FLAG|{module.port}")


def test_status_flag_table_published(status_flags):
    """xcvrd publishes TRANSCEIVER_STATUS_FLAG with the module-level fault/state fields.

    A daemon that never publishes the flag table (the reduced Rust) fails here.
    """
    m = status_flags
    m.plug()
    wait_until(lambda: _flags(m), timeout=T_DOM,
               msg=f"{m.port} TRANSCEIVER_STATUS_FLAG published")
    row = _flags(m)
    for f in MODULE_FIELDS:
        assert f in row, f"{f} missing from TRANSCEIVER_STATUS_FLAG: {sorted(row)}"


def test_module_firmware_fault_flag_reported(status_flags):
    """Raising ModuleFirmwareErrorFlag (byte 8 bit1) surfaces as
    STATUS_FLAG.module_firmware_fault, and clearing it drops it back to False."""
    m = status_flags
    m.plug()
    # baseline: the table is published and not faulted.
    wait_until(lambda: _flags(m).get("module_firmware_fault") == "False", timeout=T_DOM,
               msg=f"{m.port} module_firmware_fault baseline False")

    m.emu.write_field(m.index, cmis.MODULE_FLAGS_FW_STATE, bytes([cmis.MODULE_FW_FAULT_FLAG]))
    wait_until(lambda: _flags(m).get("module_firmware_fault") == "True", timeout=T_DOM,
               msg=f"{m.port} module_firmware_fault set after raising ModuleFirmwareErrorFlag")

    m.emu.write_field(m.index, cmis.MODULE_FLAGS_FW_STATE, bytes([0x00]))
    wait_until(lambda: _flags(m).get("module_firmware_fault") == "False", timeout=T_DOM,
               msg=f"{m.port} module_firmware_fault cleared after clearing the flag")
