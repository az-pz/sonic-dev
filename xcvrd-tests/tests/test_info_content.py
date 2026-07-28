"""TRANSCEIVER_INFO content correctness (HLD 1.1.1).

The static identity xcvrd publishes must match what the emulator serves. The
emulator's emu_config.yaml defines VendorName=xcvr-emu, VendorPN=EMU-40G-LR4,
VendorSN=0123456789, VendorOUI=0x010203, QSFP-DD / CMIS 5.2, Power Class 8.
These are the oracle values for the fields below.
"""
from lib.waits import T_FAST, T_MULTI, wait_until, T_DOM
import pytest


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


def test_info_extended_identity(module):
    """Rich static identity fields decode to the emulator's oracle values -- not
    just manufacturer/model/serial. A daemon that stubs these fails the parity."""
    module.wait_info_populated(timeout=T_FAST)
    info = module.info()
    assert "MPO" in (info.get("connector") or ""), f"connector={info.get('connector')!r}"
    assert info.get("cable_length") == "100.0", f"cable_length={info.get('cable_length')!r}"
    assert info.get("cmis_rev") == "5.2", f"cmis_rev={info.get('cmis_rev')!r}"
    assert info.get("type_abbrv_name") == "QSFP-DD", f"type_abbrv_name={info.get('type_abbrv_name')!r}"
    assert info.get("specification_compliance") == "sm_media_interface", \
        f"specification_compliance={info.get('specification_compliance')!r}"
    assert info.get("is_replaceable") == "True", f"is_replaceable={info.get('is_replaceable')!r}"
    assert info.get("vdm_supported") == "False", f"vdm_supported={info.get('vdm_supported')!r}"
    assert (info.get("vendor_date") or "").startswith("2024-12-14"), \
        f"vendor_date={info.get('vendor_date')!r}"
    assert info.get("media_interface_technology"), "media_interface_technology missing"


def test_info_application_advertisement(module):
    """application_advertisement is a real, structured CMIS application list (per
    entry: host/media interface ids + host/media lane counts), not empty or N/A."""
    import ast
    module.wait_info_populated(timeout=T_FAST)
    adv = module.info().get("application_advertisement")
    assert adv and adv != "N/A", f"application_advertisement={adv!r}"
    parsed = ast.literal_eval(adv)
    assert isinstance(parsed, dict) and parsed, f"unparseable/empty advertisement: {adv!r}"
    first = next(iter(parsed.values()))
    assert "host_lane_count" in first and "media_lane_count" in first, \
        f"advertisement entry missing lane counts: {first!r}"


def test_info_apsel_and_lane_count_on_activated_port(emu, statedb, configdb):
    """On an admin-up (datapath-activated) port, xcvrd writes REAL host/media lane
    counts + per-host-lane application-select into TRANSCEIVER_INFO (these read N/A
    on the admin-down baseline; the CMIS manager fills them after DP activation)."""
    from lib.emu import index_to_port
    port = None
    for idx, present in sorted(emu.list().items()):
        if present and configdb.hget(f"PORT|{index_to_port(idx)}", "admin_status") == "up":
            port = index_to_port(idx)
            break
    if port is None:
        pytest.skip("no admin-up emulator-backed port")

    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", "host_lane_count") not in (None, "N/A"),
               timeout=T_DOM, msg=f"{port} host_lane_count becomes real after datapath activation")
    n = int(statedb.hget(f"TRANSCEIVER_INFO|{port}", "host_lane_count"))
    assert n >= 1, f"{port} host_lane_count={n!r}"
    assert statedb.hget(f"TRANSCEIVER_INFO|{port}", "media_lane_count") not in (None, "N/A"), \
        f"{port} media_lane_count is N/A on an activated port"
    assert statedb.hget(f"TRANSCEIVER_INFO|{port}", "active_apsel_hostlane1") not in (None, "N/A"), \
        f"{port} active_apsel_hostlane1 is N/A on an activated port"
