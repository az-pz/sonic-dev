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
from lib.waits import wait_until, T_DOM, T_FAST

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


CHANGE_COUNT = "TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT"
SET_TIME = "TRANSCEIVER_STATUS_FLAG_SET_TIME"
CLEAR_TIME = "TRANSCEIVER_STATUS_FLAG_CLEAR_TIME"
NEVER = "never"


def _count(m, table, field):
    v = m.db.hget(f"{table}|{m.port}", field)
    try:
        return int(v)
    except (TypeError, ValueError):
        return 0


def test_status_flag_change_count_and_times(status_flags):
    """A STATUS flag transition maintains the change-tracking metadata tables:
    raising module_firmware_fault bumps CHANGE_COUNT + stamps SET_TIME, clearing it
    bumps the count again + stamps CLEAR_TIME. The count is cumulative in STATE_DB,
    so we assert a delta. A daemon that publishes the flag but not the metadata
    tables fails here."""
    m = status_flags
    m.plug()
    field = "module_firmware_fault"

    m.emu.write_field(m.index, cmis.MODULE_FLAGS_FW_STATE, bytes([0x00]))
    wait_until(lambda: _flags(m).get(field) == "False", timeout=T_DOM,
               msg=f"{m.port} {field} cleared baseline")
    base = _count(m, CHANGE_COUNT, field)

    m.emu.write_field(m.index, cmis.MODULE_FLAGS_FW_STATE, bytes([cmis.MODULE_FW_FAULT_FLAG]))
    wait_until(lambda: _flags(m).get(field) == "True", timeout=T_DOM,
               msg=f"{m.port} {field} raised")
    wait_until(lambda: _count(m, CHANGE_COUNT, field) == base + 1, timeout=T_DOM,
               msg=f"{m.port} {field} change count {base} -> {base + 1} on raise")
    assert m.db.hget(f"{SET_TIME}|{m.port}", field) not in (None, NEVER), \
        f"{m.port} {field} SET_TIME not stamped on raise"

    m.emu.write_field(m.index, cmis.MODULE_FLAGS_FW_STATE, bytes([0x00]))
    wait_until(lambda: _flags(m).get(field) == "False", timeout=T_DOM,
               msg=f"{m.port} {field} cleared")
    wait_until(lambda: _count(m, CHANGE_COUNT, field) == base + 2, timeout=T_DOM,
               msg=f"{m.port} {field} change count {base + 1} -> {base + 2} on clear")
    assert m.db.hget(f"{CLEAR_TIME}|{m.port}", field) not in (None, NEVER), \
        f"{m.port} {field} CLEAR_TIME not stamped on clear"


def test_datapath_firmware_fault_flag(status_flags):
    """Raising DataPathFirmwareErrorFlag (00h:8.2) surfaces as
    STATUS_FLAG.datapath_firmware_fault, and clears again."""
    m = status_flags
    m.plug()
    wait_until(lambda: _flags(m).get("datapath_firmware_fault") == "False", timeout=T_DOM,
               msg=f"{m.port} datapath_firmware_fault baseline False")
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_FW_STATE, bytes([cmis.DP_FW_FAULT_FLAG]))
    wait_until(lambda: _flags(m).get("datapath_firmware_fault") == "True", timeout=T_DOM,
               msg=f"{m.port} datapath_firmware_fault set after raising DataPathFirmwareErrorFlag")
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_FW_STATE, bytes([0x00]))
    wait_until(lambda: _flags(m).get("datapath_firmware_fault") == "False", timeout=T_DOM,
               msg=f"{m.port} datapath_firmware_fault cleared")


def test_module_state_changed_flag(status_flags):
    """Raising ModuleStateChangedFlag (00h:8.0) surfaces as
    STATUS_FLAG.module_state_changed, and clears again."""
    m = status_flags
    m.plug()
    wait_until(lambda: "module_state_changed" in _flags(m), timeout=T_DOM,
               msg=f"{m.port} STATUS_FLAG published")
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_FW_STATE, bytes([cmis.MODULE_STATE_CHANGED_FLAG]))
    wait_until(lambda: _flags(m).get("module_state_changed") == "True", timeout=T_DOM,
               msg=f"{m.port} module_state_changed set after raising ModuleStateChangedFlag")
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_FW_STATE, bytes([0x00]))
    wait_until(lambda: _flags(m).get("module_state_changed") == "False", timeout=T_DOM,
               msg=f"{m.port} module_state_changed cleared")


@pytest.fixture
def perlane_flags(module):
    """``module`` set up to expose the per-host-lane Tx flags: advertise Tx/Rx flag
    support (01h:157/158) and re-plug so xcvrd re-reads the advertisement at
    insertion (it is cached there, like SI/thresholds). Restores the advertisement
    + the page-11h flag byte + a clean module on teardown."""
    snap_adv_tx = module.emu.read_field(module.index, cmis.FLAG_ADV_TX)
    snap_adv_rx = module.emu.read_field(module.index, cmis.FLAG_ADV_RX)
    snap_flag = module.emu.read_field(module.index, cmis.LANE_TX_FAULT)

    module.emu.write_field(module.index, cmis.FLAG_ADV_TX, bytes([cmis.FLAG_ADV_ALL_TX]))
    module.emu.write_field(module.index, cmis.FLAG_ADV_RX, bytes([cmis.FLAG_ADV_RX_LOS_CDR]))
    module.emu.write_field(module.index, cmis.LANE_TX_FAULT, bytes([0x00]))
    # Re-plug so the advertisement is re-read at insertion.
    module.unplug()
    module.wait_info_cleared(timeout=T_FAST)
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    yield module
    try:
        module.emu.write_field(module.index, cmis.LANE_TX_FAULT, snap_flag)
        module.emu.write_field(module.index, cmis.FLAG_ADV_TX, snap_adv_tx)
        module.emu.write_field(module.index, cmis.FLAG_ADV_RX, snap_adv_rx)
        module.unplug()
        module.wait_info_cleared(timeout=T_FAST)
        module.plug()
        module.wait_info_populated(timeout=T_FAST)
    except Exception:  # noqa: BLE001
        pass


def test_per_lane_tx_fault_flag(perlane_flags):
    """With Tx-fault support advertised (01h:157.0), raising FailureFlagTx for host
    lane 1 (page 11h:135 bit0) surfaces as STATUS_FLAG.tx1fault and is isolated to
    that lane (tx2fault stays False); clearing it drops tx1fault back to False.

    Per-lane flags read 'N/A' until the module advertises support, so this gate
    proves xcvrd both honours the advertisement and decodes the per-lane page-11h
    flag register. No emulator change (advertisement + flag are register writes the
    emulator already serves)."""
    m = perlane_flags
    # Advertised now, so the per-lane fields are real booleans (not N/A).
    wait_until(lambda: _flags(m).get("tx1fault") in ("True", "False"), timeout=T_DOM,
               msg=f"{m.port} tx1fault becomes a real boolean once Tx-fault support is advertised")
    wait_until(lambda: _flags(m).get("tx1fault") == "False", timeout=T_DOM,
               msg=f"{m.port} tx1fault baseline False")

    m.emu.write_field(m.index, cmis.LANE_TX_FAULT, bytes([cmis.LANE_TX_FAULT_BIT0]))
    wait_until(lambda: _flags(m).get("tx1fault") == "True", timeout=T_DOM,
               msg=f"{m.port} tx1fault set after raising FailureFlagTx lane 1 (11h:135.0)")
    assert _flags(m).get("tx2fault") == "False", (
        f"{m.port}: tx2fault={_flags(m).get('tx2fault')!r} -- a lane-1 fault leaked to lane 2 "
        "(per-lane flags are not isolated)")

    m.emu.write_field(m.index, cmis.LANE_TX_FAULT, bytes([0x00]))
    wait_until(lambda: _flags(m).get("tx1fault") == "False", timeout=T_DOM,
               msg=f"{m.port} tx1fault cleared after clearing FailureFlagTx")
