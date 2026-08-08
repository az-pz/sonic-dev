"""SFF-8636 (non-CMIS) daemon control: lpmode-disable + high-power-class enable (C23).

test_sff8636.py already gates the SFF-8636 identity/routing, DOM, and the daemon-driven
TX_DISABLE. This adds the other two SffManagerTask control behaviors, driven on module
insert / admin-up (sff_mgr.py:477-490):

  * lpmode-disable -- set_lpmode(False) takes the module OUT of low power by writing the
    00h:93 Power Control byte (Power_override=1, Power_set=0).
  * enable_high_power_class -- for a power-class >= 5 module, set_high_power_class writes
    the High Power Class Enable bit (00h:93.2). The emulator SFF module ships power class
    4, so the gate provisions a class-5 code (00h:129) + re-plugs first.

Like the TX_DISABLE gate, these assert real SffManagerTask behaviour, which only runs
when xcvrd is launched with --enable_sff_mgr. If it is not, these tests **FAIL** rather
than skip: the SFF control path is part of the parity surface, and skipping would hide
the fact that an entire xcvrd code path went unverified. Every test still skips cleanly
if the port is not an SFF-8636 module, so the suite stays portable.

No emulator change: pure plug / admin toggle + raw-page provisioning + Monitor observation.
"""
import pytest

from lib import sff8636 as sff
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, WaitTimeout, T_FAST, T_DOM

pytestmark = pytest.mark.slow


def _power_ctrl_writes(monitor, idx):
    """Every value written to the 00h:93 Power Control byte on the trace, in order."""
    off = sff.POWER_CTRL_OFFSET
    vals = []
    for e in monitor.writes(index=idx, page=sff.POWER_CTRL_PAGE):
        if e.offset <= off < e.offset + e.length:
            vals.append(e.data[off - e.offset])
    return vals


@pytest.fixture
def sff_module(emu, statedb, configdb):
    """Resolve the SFF-8636 port; skip unless it is really a QSFP28 module. Restores
    admin-up + present on teardown."""
    port = sff.SFF_PORT
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port})")
    emu.plug(idx)
    try:
        wait_until(lambda: "QSFP28" in (statedb.hget(f"TRANSCEIVER_INFO|{port}", "type") or ""),
                   timeout=T_DOM, msg=f"{port} SFF-8636 (QSFP28) identity")
    except WaitTimeout:
        pytest.skip(f"{port} is not an SFF-8636 (QSFP28) module; deploy the SFF emulator config")
    # NOTE: the --enable_sff_mgr requirement is asserted inside each test (via
    # sff.require_sff_mgr()), not here: raising in a fixture produces a setup ERROR,
    # and we want a real test FAILURE.
    yield port, idx
    try:
        configdb.hset(f"PORT|{port}", "admin_status", "up")
        emu.plug(idx)
    except Exception:  # noqa: BLE001
        pass


def test_sff_lpmode_disabled_on_bringup(sff_module, emu, statedb, configdb, monitor):
    """On admin-up bring-up, xcvrd's SffManagerTask takes the SFF module OUT of low
    power: it writes the 00h:93 Power Control byte with Power_override set and
    Power_set clear. A reduced daemon that never manages SFF power leaves 00h:93 alone."""
    sff.require_sff_mgr()      # FAIL (not skip) if SffManagerTask isn't running
    port, idx = sff_module

    # Toggle admin down->up to trigger the insert/admin-up control path fresh.
    configdb.hset(f"PORT|{port}", "admin_status", "down")
    wait_until(lambda: configdb.hget(f"PORT|{port}", "admin_status") == "down",
               timeout=T_FAST, msg=f"{port} admin-down applied")
    monitor.clear()
    configdb.hset(f"PORT|{port}", "admin_status", "up")

    vals = eventually(lambda: _power_ctrl_writes(monitor, idx) or None, timeout=T_DOM,
                      msg=f"{port} xcvrd Power Control write (00h:93) on admin-up bring-up")
    # set_lpmode(False) => Power_override=1, Power_set=0 in at least one write.
    assert any((v & sff.POWER_OVERRIDE_BIT) and not (v & sff.POWER_SET_BIT) for v in vals), (
        f"{port}: no Power Control write took the module out of low power "
        f"(00h:93 writes={[hex(v) for v in vals]}) -- sff_mgr did not disable lpmode")


def test_sff_high_power_class_enabled(sff_module, emu, statedb, configdb, monitor):
    """For a power-class >= 5 module, xcvrd enables High Power Class (00h:93.2) on
    bring-up (sff_mgr.py:enable_high_power_class -> set_high_power_class).

    The emulator SFF module ships power class 4 (00h:129 = 0xC0), for which
    enable_high_power_class is a documented no-op, so the gate PROVISIONS a class-5
    code (0xC1) first. Two details make that work:

      * the trigger is an admin-down/up toggle, NOT a re-plug: the emulator re-applies
        its config on every plug, which would reset 00h:129 straight back to 0xC0 and
        make the gate unexercisable. sff_mgr runs the same control path on either
        event (`xcvr_inserted or (admin_status_changed and admin == up)`,
        sff_mgr.py:477), so the toggle is equivalent and keeps the provisioning.
      * get_power_class() is a live EEPROM read (not a cached-at-insert API), so
        xcvrd sees the class we just wrote.

    The fixture restores admin-up; we restore the original power-class byte.
    """
    sff.require_sff_mgr()      # FAIL (not skip) if SffManagerTask isn't running
    port, idx = sff_module

    # SFF-8636 power class lives in 00h:129: bits 7:6 give classes 1-4, and bits 1:0
    # extend it (01b => class 5). 0xC0 = class 4 (emulator default), 0xC1 = class 5.
    snap_pclass = bytes(emu.read_field(idx, sff.POWER_CLASS))
    try:
        emu.write_field(idx, sff.POWER_CLASS, bytes([sff.POWER_CLASS_5_VALUE]))
        assert emu.read_field(idx, sff.POWER_CLASS)[0] == sff.POWER_CLASS_5_VALUE, (
            f"{port}: could not provision SFF power class 5 (00h:129)")

        # Toggle admin down->up to re-run the insert/admin-up control path against the
        # now class-5 module (a re-plug would wipe the provisioning above).
        configdb.hset(f"PORT|{port}", "admin_status", "down")
        wait_until(lambda: configdb.hget(f"PORT|{port}", "admin_status") == "down",
                   timeout=T_FAST, msg=f"{port} admin-down applied")
        monitor.clear()
        configdb.hset(f"PORT|{port}", "admin_status", "up")

        vals = eventually(lambda: _power_ctrl_writes(monitor, idx) or None, timeout=T_DOM,
                          msg=f"{port} xcvrd Power Control write (00h:93) on class-5 bring-up")
        assert any(v & sff.HIGH_POWER_CLASS_5_7_BIT for v in vals), (
            f"{port}: xcvrd did not set High Power Class Enable (00h:93.2) for a class-5 "
            f"module (00h:93 writes={[hex(v) for v in vals]})")
    finally:
        try:
            emu.write_field(idx, sff.POWER_CLASS, snap_pclass)
        except Exception:  # noqa: BLE001
            pass
