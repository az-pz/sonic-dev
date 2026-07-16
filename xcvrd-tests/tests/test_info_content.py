"""TRANSCEIVER_INFO content correctness (HLD 1.1.1).

The static identity xcvrd publishes must match what the emulator serves. The
emulator's emu_config.yaml defines VendorName=xcvr-emu, VendorPN=EMU-40G-LR4,
VendorSN=0123456789, VendorOUI=0x010203, QSFP-DD / CMIS 5.2, Power Class 8.
These are the oracle values for the fields below.
"""
from lib.waits import T_FAST, T_MULTI


def test_info_has_expected_identity(module):
    module.wait_info_populated(timeout=T_FAST)
    info = module.info()
    assert info.get("manufacturer") == "xcvr-emu"
    assert info.get("model") == "EMU-40G-LR4"
    assert info.get("vendor_oui") == "01-02-03"


def test_info_serial_and_revision(module):
    module.wait_info_populated(timeout=T_FAST)
    info = module.info()
    assert info.get("serial") == "0123456789"
    assert info.get("vendor_rev") in ("01", "1")


def test_info_type_is_qsfp_dd(module):
    module.wait_info_populated(timeout=T_FAST)
    info = module.info()
    type_str = (info.get("type") or "").upper()
    assert "QSFP" in type_str and "DD" in type_str, f"unexpected type={info.get('type')!r}"


def test_info_power_class(module):
    """ext_identifier should reflect the configured Power Class 8."""
    module.wait_info_populated(timeout=T_FAST)
    info = module.info()
    ext = info.get("ext_identifier") or ""
    assert "Power Class 8" in ext, f"unexpected ext_identifier={ext!r}"


def test_all_present_ports_have_info(emu, statedb, configdb):
    """Every emulator module whose logical port exists in CONFIG_DB and is
    admin-up should converge to a populated TRANSCEIVER_INFO row."""
    from lib.emu import index_to_port
    from lib.waits import wait_until
    ports = []
    for idx, is_present in emu.list().items():
        if not is_present:
            continue
        port = index_to_port(idx)
        if configdb.hget(f"PORT|{port}", "admin_status") == "up":
            ports.append(port)
    if not ports:
        import pytest
        pytest.skip("no admin-up emulator-backed ports found in CONFIG_DB")

    def _all_populated():
        return all(statedb.hget(f"TRANSCEIVER_INFO|{p}", "manufacturer") for p in ports)
    wait_until(_all_populated, timeout=T_MULTI,
               msg=f"all {len(ports)} admin-up ports populated")

    for port in ports:
        assert statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer") == "xcvr-emu", \
            f"{port} has unexpected manufacturer"
