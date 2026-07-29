"""TRANSCEIVER_VDM_REAL_VALUE statistic (min/max/avg) observables (B12).

test_vdm.py provisions only BASIC (instantaneous) VDM observables. The statistic
observables -- Pre-FEC BER minimum / maximum / average -- are published by a
distinct xcvrd code path: the VDM freeze/unfreeze block in dom_mgr's DOM poll
(post_port_pm_info_to_db neighbour), which freezes VDM, reads the statistic
observables and merges them into the SAME TRANSCEIVER_VDM_REAL_VALUE table as the
basic values. That block only runs on an admin-up (non-lpmode) module that
advertises a statistic observable and reports freeze/unfreeze DONE.

The freeze mechanism is exercised indirectly by the PM gate (test_pm.py), but the
min/max/avg VDM_REAL_VALUE fields themselves are asserted nowhere -- test_vdm
still only provisions "basic". This gate provisions the three statistic
observables (lib/vdm.provision_statistic: descriptor + values on the raw pages
the emulator already serves + pre-set freeze-done bits) and asserts xcvrd
publishes prefec_ber_{min,max,avg}_media_input with the real decoded values and
in the correct min < avg < max ordering. A reduced daemon that skips the VDM
freeze/statistic path publishes none of these fields and fails here.

Uses an admin-up CMIS port (default Ethernet36) not used by the golden gates;
override with XCVRD_VDM_STAT_PORT. Skips cleanly if that port is missing or
admin-down (the freeze path is a no-op in low power).
"""
import os

import pytest

from lib import vdm
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, T_FAST, T_DOM

pytestmark = pytest.mark.slow

STAT_PORT = os.environ.get("XCVRD_VDM_STAT_PORT", "Ethernet36")
REAL_VALUE_TABLE = "TRANSCEIVER_VDM_REAL_VALUE"


def _real(statedb, port):
    return statedb.hgetall(f"{REAL_VALUE_TABLE}|{port}")


@pytest.fixture
def vdm_stat(emu, statedb, configdb):
    """Resolve an admin-up port, provision VDM statistic observables, re-plug so
    xcvrd reads the advertisement. Teardown deprovisions + re-plugs
    (vdm_supported -> False) and flushes the VDM tables so no statistic values
    leak into the INFO / golden tests."""
    port = STAT_PORT
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port})")
    if configdb.hget(f"PORT|{port}", "admin_status") != "up":
        pytest.skip(f"{port} is not admin-up; the VDM statistic freeze path only "
                    "runs on an admin-up (non-lpmode) port. Set XCVRD_VDM_STAT_PORT "
                    "to an admin-up port.")
    emu.plug(idx)
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} present before VDM statistic provisioning")

    vdm.provision_statistic(emu, idx)
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} removed before re-insert")
    emu.plug(idx)
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} re-inserted before VDM statistic")
    yield port, idx
    try:
        vdm.deprovision_statistic(emu, idx)
        emu.unplug(idx)
        wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
                   timeout=T_FAST)
        emu.plug(idx)
        statedb.delete_pattern(f"TRANSCEIVER_VDM*{port}*")
    except Exception:  # noqa: BLE001
        pass


def test_vdm_statistic_supported_advertised(vdm_stat, statedb):
    """Advertising a statistic observable makes xcvrd report the module VDM-capable."""
    port, _ = vdm_stat
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", "vdm_supported") == "True",
               timeout=T_DOM, msg=f"{port} INFO.vdm_supported True once VDM is advertised")


def test_vdm_statistic_real_values_published(vdm_stat, statedb):
    """xcvrd's VDM freeze path captures the statistic (min/max/avg) observables and
    publishes them in TRANSCEIVER_VDM_REAL_VALUE with the real decoded values."""
    port, _ = vdm_stat
    first = next(iter(vdm.EXPECTED_STAT))
    # The freeze/statistic capture runs on the DOM poll, so allow a DOM cycle.
    eventually(lambda: _real(statedb, port).get(first) not in (None, "N/A"),
               timeout=2 * T_DOM, msg=f"{port} VDM statistic real values published")
    row = _real(statedb, port)
    for field, expected in vdm.EXPECTED_STAT.items():
        got = row.get(field)
        assert got not in (None, "N/A"), \
            f"{port} {field} missing/NA in VDM_REAL_VALUE (row={sorted(row)})"
        g = float(got)
        assert abs(g - expected) <= abs(expected) * 0.2 + 1e-12, \
            f"{port} VDM_REAL_VALUE {field}={got} (expected ~{expected})"


def test_vdm_statistic_min_avg_max_ordering(vdm_stat, statedb):
    """The three statistic fields keep the provisioned min < avg < max ordering --
    proving xcvrd reads each distinct observable rather than one repeated value."""
    port, _ = vdm_stat
    eventually(lambda: _real(statedb, port).get("prefec_ber_min_media_input1")
               not in (None, "N/A"), timeout=2 * T_DOM,
               msg=f"{port} VDM statistic values published")
    row = _real(statedb, port)
    lo = float(row["prefec_ber_min_media_input1"])
    avg = float(row["prefec_ber_avg_media_input1"])
    hi = float(row["prefec_ber_max_media_input1"])
    assert lo < avg < hi, \
        f"{port} VDM statistic ordering min={lo} avg={avg} max={hi} (expected min<avg<max)"
