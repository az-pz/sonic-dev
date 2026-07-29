"""TRANSCEIVER_PM (performance monitoring) parity gate for a coherent module.

No existing test references PM. It is published only for a COHERENT (C-CMIS) module
and only via xcvrd's VDM statistic-freeze path -- see lib/pm.py for the full
mechanism. The emulator serves one module (XCVRD_PM_PORT, default Ethernet44) as
coherent by advertising a 400GBASE-ZR media interface in its config; the rest of the
stimulus (VDM statistic advertisement, pre-set freeze-done bits, PM register values)
is provisioned by the harness on raw pages the emulator already serves.

A reduced daemon that publishes no PM table fails here (parallel to the
firmware-info / VDM gates). Every test skips cleanly if XCVRD_PM_PORT is not a
coherent module, so the suite stays portable to a plain all-CMIS testbed.
"""
import os

import pytest

from lib import pm
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, WaitTimeout, T_FAST, T_DOM

pytestmark = pytest.mark.slow

PM_PORT = os.environ.get("XCVRD_PM_PORT", "Ethernet44")
PM_TABLE = "TRANSCEIVER_PM"
# CCmisApi.get_transceiver_info adds these coherent-only fields; their presence in
# TRANSCEIVER_INFO is a black-box marker that xcvrd classified the module as C-CMIS.
COHERENT_INFO_MARKER = "supported_max_laser_freq"


def _pm(statedb, port):
    return statedb.hgetall(f"{PM_TABLE}|{port}")


@pytest.fixture
def pm_module(emu, statedb, configdb):
    """Resolve a coherent PM port, provision PM, re-plug; skip if not coherent."""
    port = PM_PORT
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port})")
    if configdb.hget(f"PORT|{port}", "admin_status") != "up":
        pytest.skip(f"{port} is not admin-up; PM freeze only runs on an admin-up port. "
                    "Set XCVRD_PM_PORT to an admin-up coherent port.")
    emu.plug(idx)
    try:
        wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", COHERENT_INFO_MARKER) is not None,
                   timeout=T_DOM, msg=f"{port} coherent (C-CMIS) module")
    except WaitTimeout:
        pytest.skip(f"{port} is not a coherent (C-CMIS) module; configure the emulator to "
                    "advertise a 400GBASE-ZR media interface on this module for the PM gate")

    pm.provision(emu, idx)
    # advertisement is cached at insertion -> re-plug so xcvrd reads it.
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} removed before re-insert")
    emu.plug(idx)
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} re-inserted before PM")
    yield port, idx
    # Teardown: deprovision + re-plug + flush PM/VDM so nothing leaks.
    try:
        pm.deprovision(emu, idx)
        emu.unplug(idx)
        wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
                   timeout=T_FAST)
        emu.plug(idx)
        statedb.delete(f"{PM_TABLE}|{port}")
        statedb.delete_pattern(f"TRANSCEIVER_VDM*{port}*")
    except Exception:  # noqa: BLE001
        pass


def test_pm_published(pm_module, statedb):
    """xcvrd publishes TRANSCEIVER_PM for the coherent module with the full field set."""
    port, _ = pm_module
    row = eventually(lambda: _pm(statedb, port) or None, timeout=2 * T_DOM,
                     msg=f"{port} TRANSCEIVER_PM published")
    # The C-CMIS PM projection is broad; assert a representative core is present.
    for field in ("prefec_ber_avg", "cd_avg", "dgd_avg", "osnr_avg",
                  "tx_power_avg", "rx_tot_power_avg"):
        assert field in row, f"{port} TRANSCEIVER_PM missing {field} (row={sorted(row)})"


def test_pm_real_values(pm_module, statedb):
    """The published PM values decode from the provisioned page 34h/35h registers."""
    port, _ = pm_module
    eventually(lambda: _pm(statedb, port).get("osnr_avg") not in (None, "0.0", "0"),
               timeout=2 * T_DOM, msg=f"{port} real PM values published")
    row = _pm(statedb, port)
    for field, expected in pm.EXPECTED_PM.items():
        got = row.get(field)
        assert got is not None, f"{port} PM missing {field} (row={sorted(row)})"
        g = float(got)
        if abs(expected) < 1e-2:      # BER: wide-range, use relative tolerance
            assert abs(g - expected) <= abs(expected) * 0.2 + 1e-12, \
                f"{port} PM {field}={got} (expected ~{expected})"
        else:
            assert abs(g - expected) < 0.5, f"{port} PM {field}={got} (expected ~{expected})"
