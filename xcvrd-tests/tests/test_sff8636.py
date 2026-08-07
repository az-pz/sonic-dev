"""SFF-8636 (QSFP28) parity gates.

The emulator serves ONE module (XCVRD_SFF_PORT, default Ethernet40 = emu_config
index 10) as SFF-8636 instead of CMIS. SONiC picks the Sff8636Api purely from
EEPROM byte 0 (0x11), which routes the port through xcvrd's SffManagerTask rather
than CmisManagerTask -- an entire xcvrd code path that no CMIS test exercises.

These lock: (1) the identity/routing (only non-CMIS module in the suite) and
(2) the SFF-8636 DOM sensor + threshold projection -- both decoded by Sff8636Api,
which no other test exercises and which a Rust reimplementation must reproduce.

(3) The daemon-driven TX_DISABLE gate is the SFF-8636 analogue of the CMIS
datapath-teardown gate, and SffManagerTask only runs when xcvrd is launched with
--enable_sff_mgr. If it is not, that test **FAILS** rather than skipping: the SFF
control path is part of the parity surface, so skipping it would silently hide an
entire unverified xcvrd code path.

Requires the SFF emulator deploy; every test skips cleanly if the port is not an
SFF-8636 module, so the suite stays portable to a plain all-CMIS testbed.
"""
import pytest

from lib import sff8636 as sff
from lib.emu import port_to_index
from lib.waits import eventually, wait_until, WaitTimeout, T_FAST, T_DOM

pytestmark = pytest.mark.slow

STATE_PORT_TABLE = "PORT_TABLE"  # STATE_DB PORT_TABLE, where host_tx_ready lives


@pytest.fixture
def sff_module(emu, statedb, configdb):
    """Resolve the SFF-8636 port; skip unless it is really a QSFP28 module."""
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
    yield port, idx
    # Leave the port healthy for the next test/user: admin-up + Tx re-enabled + present.
    try:
        configdb.hset(f"PORT|{port}", "admin_status", "up")
        statedb.hset(f"{STATE_PORT_TABLE}|{port}", "host_tx_ready", "true")
        emu.plug(idx)
    except Exception:  # noqa: BLE001
        pass


def _tx_disable_bytes(monitor, idx):
    """The 00h:86 byte value from every TX_DISABLE write xcvrd issued on the trace."""
    vals = []
    for e in monitor.writes(index=idx, page=sff.TX_DISABLE_PAGE):
        if e.offset <= sff.TX_DISABLE_OFFSET < e.offset + e.length:
            vals.append(e.data[sff.TX_DISABLE_OFFSET - e.offset])
    return vals


def test_sff8636_identity_routing(sff_module, emu, statedb):
    """xcvrd decodes the module via Sff8636Api: QSFP28 identity, byte0 = 0x11.

    Reaching QSFP28 at all proves XcvrApiFactory picked Sff8636Api (from byte 0)
    and that SffManagerTask -- not CmisManagerTask -- owns the port."""
    port, idx = sff_module
    assert emu.read_field(idx, sff.IDENTIFIER)[0] == sff.QSFP28_ID
    info = statedb.hgetall(f"TRANSCEIVER_INFO|{port}")
    assert "QSFP28" in info.get("type", ""), info
    assert info.get("manufacturer") == "xcvr-emu", info
    assert info.get("model"), info
    # SFF-8636 identity carries fields the projection wouldn't have if the module
    # were still being decoded as CMIS.
    assert info.get("encoding"), info
    assert info.get("connector"), info


def test_sff8636_dom(sff_module, statedb):
    """xcvrd publishes SFF-8636 DOM sensors + thresholds decoded from the module."""
    port, _ = sff_module
    dom = eventually(lambda: statedb.hgetall(f"TRANSCEIVER_DOM_SENSOR|{port}") or None,
                     timeout=2 * T_DOM, msg=f"{port} DOM_SENSOR populated")
    assert float(dom["temperature"]) == pytest.approx(45.0, abs=0.5)
    assert float(dom["voltage"]) == pytest.approx(3.3, abs=0.05)
    for lane in range(1, 5):
        assert float(dom[f"tx{lane}bias"]) == pytest.approx(6.0, abs=0.2)

    thr = eventually(lambda: statedb.hgetall(f"TRANSCEIVER_DOM_THRESHOLD|{port}") or None,
                     timeout=2 * T_DOM, msg=f"{port} DOM_THRESHOLD populated")
    assert float(thr["temphighalarm"]) == pytest.approx(75.0, abs=0.5)
    assert float(thr["templowalarm"]) == pytest.approx(-5.0, abs=0.5)
    assert float(thr["vcchighalarm"]) == pytest.approx(3.6, abs=0.05)
    assert float(thr["txbiashighalarm"]) == pytest.approx(13.0, abs=0.2)


def test_sff8636_daemon_drives_tx_disable(sff_module, emu, statedb, configdb, monitor):
    """Admin-down makes xcvrd's SffManagerTask write TX_DISABLE (00h:86).

    The SFF-8636 counterpart to the CMIS datapath-teardown gate: xcvrd ITSELF
    reacts to the port going down by disabling the module's Tx on the active lanes
    (00h:86), observed on the Monitor trace, and the disable is durable
    (target = not(host_tx_ready and admin_up)). A reduced daemon that never drives
    the SFF Tx-disable register fails this.

    FAILS (does not skip) unless xcvrd runs SffManagerTask (--enable_sff_mgr): the SFF
    control path is part of the parity surface, so a disabled task is an unverified
    code path, not an "n/a" environment. Admin-status is used (not
    host_tx_ready) because a background keeper continuously re-asserts
    host_tx_ready=true; admin-down is stable + keeper-immune. Restores admin-up."""
    sff.require_sff_mgr()      # FAIL (not skip) if SffManagerTask isn't running
    port, idx = sff_module
    wait_until(lambda: emu.read_field(idx, sff.TX_DISABLE)[0] == 0, timeout=T_DOM,
               msg=f"{port} Tx enabled (00h:86 == 0) at baseline")

    monitor.clear()
    configdb.hset(f"PORT|{port}", "admin_status", "down")
    try:
        vals = eventually(lambda: _tx_disable_bytes(monitor, idx) or None, timeout=T_DOM,
                          msg=f"{port} xcvrd TX_DISABLE write (00h:86) after admin-down")
        assert any(v != 0 for v in vals), (
            f"{port}: SffManagerTask did not disable Tx (00h:86 writes={[hex(v) for v in vals]}) "
            "-- the daemon did not react to the port going down")
        # durable: the module's TX_DISABLE register now reflects disabled Tx.
        wait_until(lambda: emu.read_field(idx, sff.TX_DISABLE)[0] != 0, timeout=T_FAST,
                   msg=f"{port} 00h:86 reflects disabled Tx")
    finally:
        configdb.hset(f"PORT|{port}", "admin_status", "up")
    # admin-up: xcvrd re-enables Tx (00h:86 back to 0).
    wait_until(lambda: emu.read_field(idx, sff.TX_DISABLE)[0] == 0, timeout=T_DOM,
               msg=f"{port} Tx re-enabled (00h:86 == 0) after admin-up")
