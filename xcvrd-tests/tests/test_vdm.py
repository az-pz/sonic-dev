"""TRANSCEIVER_VDM_* real values, thresholds, and flags (Versatile Diagnostics Monitoring).

VDM is provisioned as pure harness stimulus (lib/vdm.py): the emulator serves the
raw VDM pages, so writing the descriptor / value / threshold pages and advertising
VDM_SUPPORTED (01h:142.6) makes xcvrd read and publish REAL VDM output -- no
emulator change. A reduced daemon that does not read VDM publishes none of these
tables and fails every test here.

Only BASIC (instantaneous) observables are provisioned; statistic (min/max/avg)
observables would require xcvrd's VDM freeze/unfreeze path, which only runs on a
non-low-power (admin-up) port -- and provisioning on an admin-up port flips its
TRANSCEIVER_INFO.vdm_supported and disturbs the datapath goldens. The basic set
covers every field the VDM parity gate needs (laser temperature, eSNR, PAM4 level
transition, current pre-FEC BER, current errored frames).

The fixture fully deprovisions + flushes the VDM tables on teardown so
vdm_supported returns to False and no real VDM values leak into the INFO / golden
tests.
"""
import pytest

from lib import vdm
from lib.waits import wait_until, T_FAST, T_DOM

pytestmark = pytest.mark.slow

REAL_VALUE_TABLE = "TRANSCEIVER_VDM_REAL_VALUE"
THRESHOLD_TABLES = {
    "halarm": "TRANSCEIVER_VDM_HALARM_THRESHOLD",
    "lalarm": "TRANSCEIVER_VDM_LALARM_THRESHOLD",
    "hwarn": "TRANSCEIVER_VDM_HWARN_THRESHOLD",
    "lwarn": "TRANSCEIVER_VDM_LWARN_THRESHOLD",
}
HALARM_FLAG = "TRANSCEIVER_VDM_HALARM_FLAG"
HALARM_FLAG_CHANGE_COUNT = "TRANSCEIVER_VDM_HALARM_FLAG_CHANGE_COUNT"
HALARM_FLAG_SET_TIME = "TRANSCEIVER_VDM_HALARM_FLAG_SET_TIME"


def _close(got, expected):
    """VDM values span exact-ish S16/U16 and wide-range F16 (BER / errored frames);
    use a relative tolerance for the small-magnitude F16 values, absolute otherwise."""
    g = float(got)
    if abs(expected) < 1e-2:
        return abs(g - expected) <= abs(expected) * 0.15 + 1e-12
    return abs(g - expected) < 0.5


@pytest.fixture
def vdm_provisioned(module):
    """Provision VDM on the test module + re-plug so xcvrd reads it. Teardown
    deprovisions, re-plugs (vdm_supported -> False), then flushes the VDM tables so
    no real VDM values leak into later tests or the goldens."""
    vdm.provision(module.emu, module.index)
    module.unplug()
    module.wait_info_cleared(timeout=T_FAST)
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    yield module
    try:
        vdm.deprovision(module.emu, module.index)
        module.unplug()
        module.wait_info_cleared(timeout=T_FAST)
        module.plug()
        module.wait_info_populated(timeout=T_FAST)
        module.db.delete_pattern(f"TRANSCEIVER_VDM*{module.port}*")
    except Exception:  # noqa: BLE001
        pass


def _real(m):
    return m.db.hgetall(f"{REAL_VALUE_TABLE}|{m.port}")


def test_vdm_supported_advertised(vdm_provisioned):
    """Provisioning VDM makes xcvrd report the module as VDM-capable in INFO."""
    m = vdm_provisioned
    wait_until(lambda: m.db.hget(f"TRANSCEIVER_INFO|{m.port}", "vdm_supported") == "True",
               timeout=T_DOM, msg=f"{m.port} INFO.vdm_supported True once VDM is advertised")


def test_vdm_real_values_published(vdm_provisioned):
    """xcvrd publishes TRANSCEIVER_VDM_REAL_VALUE with the real decoded observable
    values (not N/A) for every provisioned basic observable."""
    m = vdm_provisioned
    wait_until(lambda: _real(m).get("laser_temperature_media1") not in (None, "N/A"),
               timeout=T_DOM, msg=f"{m.port} VDM real values published")
    row = _real(m)
    for field, expected in vdm.EXPECTED_REAL.items():
        got = row.get(field)
        assert got not in (None, "N/A"), f"{field} missing/NA in VDM_REAL_VALUE (row={sorted(row)})"
        assert _close(got, expected), f"VDM_REAL_VALUE {field}={got} (expected ~{expected})"


def test_vdm_threshold_values_published(vdm_provisioned):
    """xcvrd publishes the four VDM threshold tables (HALARM/LALARM/HWARN/LWARN)
    with the real threshold values for every observable."""
    m = vdm_provisioned
    wait_until(lambda: m.db.hget(f"{THRESHOLD_TABLES['halarm']}|{m.port}", "laser_temperature_media1")
               not in (None, "N/A"), timeout=T_DOM, msg=f"{m.port} VDM thresholds published")
    for ttype, table in THRESHOLD_TABLES.items():
        row = m.db.hgetall(f"{table}|{m.port}")
        assert row, f"{table}|{m.port} is empty"
        for field, expected in vdm.EXPECTED_THRESH[ttype].items():
            got = row.get(field)
            assert got not in (None, "N/A"), f"{field} missing/NA in {table}"
            assert _close(got, expected), f"{table} {field}={got} (expected ~{expected})"


def test_vdm_high_alarm_flag_and_metadata(vdm_provisioned):
    """Raising the VDM high-alarm flag for laser temperature surfaces in
    TRANSCEIVER_VDM_HALARM_FLAG and bumps the flag change-count + set-time metadata
    (the same change-tracking machinery as the DOM/STATUS flags)."""
    m = vdm_provisioned
    field = "laser_temperature_media1"

    wait_until(lambda: m.db.hget(f"{HALARM_FLAG}|{m.port}", field) == "False",
               timeout=T_DOM, msg=f"{m.port} VDM halarm baseline False")

    def _count():
        v = m.db.hget(f"{HALARM_FLAG_CHANGE_COUNT}|{m.port}", field)
        return int(v) if v and v.isdigit() else 0
    base = _count()

    vdm.raise_flag(m.emu, m.index, field, "halarm")
    wait_until(lambda: m.db.hget(f"{HALARM_FLAG}|{m.port}", field) == "True",
               timeout=T_DOM, msg=f"{m.port} VDM halarm set after raising the flag")
    wait_until(lambda: _count() >= base + 1, timeout=T_DOM,
               msg=f"{m.port} VDM halarm change count {base} -> {base + 1} on raise")
    assert m.db.hget(f"{HALARM_FLAG_SET_TIME}|{m.port}", field) not in (None, "never"), \
        f"{m.port} VDM halarm SET_TIME not stamped on raise"
