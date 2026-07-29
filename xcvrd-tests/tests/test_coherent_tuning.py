"""Coherent/ZR laser-tuning control-plane writes: set_tx_power / set_laser_freq (B16).

The PM gate (test_pm.py) proves a coherent module exists and asserts the
TRANSCEIVER_PM table, but NOT the tuning control plane. When a coherent (C-CMIS)
port has a CONFIG_DB laser_freq / tx_power configured, xcvrd's CmisManagerTask
programs the module during bring-up:
  * TX_CONFIG_POWER (12h:200)      via set_tx_power  (DP_PRE_INIT_CHECK)
  * GRID_SPACING (12h:128) +
    LASER_CONFIG_CHANNEL (12h:136) via set_laser_freq (AP_CONF), after
    validate_frequency_and_grid accepts the 75GHz-grid frequency against the
    page-04h capability.

We provision the page-04h tuning capability (lib/coherent), set a valid 75GHz-grid
laser_freq + an in-range tx_power in CONFIG_DB, zero the page-12h tuning registers
so the configure guards trip, then bounce admin_status to force a fresh bring-up
(NOT a re-plug, which would reset the provisioned capability page). We assert xcvrd
ITSELF drives the TX_CONFIG_POWER, GRID_SPACING and LASER_CONFIG_CHANNEL writes on
the Monitor trace. A reduced daemon that ignores the coherent tuning control plane
issues none of them.

Uses the coherent PM port (default Ethernet44); override with XCVRD_COHERENT_PORT.
Skips cleanly if that port is not a coherent module. The fixture always clears the
CONFIG_DB tuning fields, deprovisions the capability, and re-plugs so the port is
left a clean coherent module.
"""
import os
import time

import pytest

from lib import coherent
from lib.emu import port_to_index
from lib.waits import wait_until, eventually, T_FAST, T_DOM, T_BASELINE

pytestmark = pytest.mark.slow

COHERENT_PORT = os.environ.get("XCVRD_COHERENT_PORT", "Ethernet44")
COHERENT_INFO_MARKER = "supported_max_laser_freq"   # CCmisApi-only INFO field
STATUS_SW = "TRANSCEIVER_STATUS_SW"


def _page12_write_offsets(monitor, idx):
    """Set of page-12h tuning offsets xcvrd wrote (across any bank)."""
    wanted = (coherent.GRID_SPACING_OFFSET, coherent.LASER_CONFIG_CHANNEL_OFFSET,
              coherent.TX_CONFIG_POWER_OFFSET)
    hit = set()
    for e in monitor.writes(index=idx, page=coherent.TUNE_PAGE):
        for off in wanted:
            if e.offset <= off < e.offset + e.length:
                hit.add(off)
    return hit


def _cmis_state(statedb, port):
    return statedb.hgetall(f"{STATUS_SW}|{port}").get("cmis_state")


@pytest.fixture
def coherent_port(emu, statedb, configdb):
    port = COHERENT_PORT
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port})")
    if configdb.hget(f"PORT|{port}", "admin_status") != "up":
        pytest.skip(f"{port} is not admin-up; coherent tuning runs during an admin-up "
                    "bring-up. Set XCVRD_COHERENT_PORT to an admin-up coherent port.")
    emu.plug(idx)
    try:
        wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", COHERENT_INFO_MARKER) is not None,
                   timeout=T_DOM, msg=f"{port} coherent (C-CMIS) module")
    except Exception:  # noqa: BLE001
        pytest.skip(f"{port} is not a coherent (C-CMIS) module; configure the emulator to "
                    "advertise a 400GBASE-ZR media interface for the tuning gate")
    yield port, idx
    # Restore a clean coherent module: clear config, deprovision, re-plug.
    try:
        configdb.hdel(f"PORT|{port}", "laser_freq")
        configdb.hdel(f"PORT|{port}", "tx_power")
        configdb.hset(f"PORT|{port}", "admin_status", "up")
        coherent.deprovision(emu, idx)
        emu.unplug(idx)
        wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
                   timeout=T_DOM)
        emu.plug(idx)
        wait_until(lambda: _cmis_state(statedb, port) == "READY", timeout=T_BASELINE)
    except Exception:  # noqa: BLE001
        pass


def test_coherent_tuning_writes(coherent_port, emu, statedb, configdb, monitor):
    """xcvrd programs Tx power (12h:200) and laser frequency (12h:128 + 12h:136)
    on a coherent module configured with a 75GHz-grid laser_freq + tx_power."""
    port, idx = coherent_port

    # Advertise the tuning capability + zero the tuning registers so the guards trip.
    coherent.clear_tuning_registers(emu, idx)
    coherent.provision_tuning_capability(emu, idx)
    configdb.hset(f"PORT|{port}", "laser_freq", str(coherent.LASER_FREQ_GHZ))
    configdb.hset(f"PORT|{port}", "tx_power", str(coherent.TX_POWER_DBM))

    # Force a fresh bring-up via an admin bounce (a re-plug would reset page 04h).
    monitor.clear()
    configdb.hset(f"PORT|{port}", "admin_status", "down")
    time.sleep(3)
    configdb.hset(f"PORT|{port}", "admin_status", "up")

    hit = eventually(lambda: (_page12_write_offsets(monitor, idx)
                              if len(_page12_write_offsets(monitor, idx)) >= 3 else None),
                     timeout=2 * T_DOM,
                     msg=f"{port} coherent tuning writes (12h:128/136/200) during bring-up")
    assert coherent.TX_CONFIG_POWER_OFFSET in hit, \
        f"{port}: xcvrd did not write TX_CONFIG_POWER (12h:200) -- set_tx_power not driven"
    assert coherent.GRID_SPACING_OFFSET in hit, \
        f"{port}: xcvrd did not write GRID_SPACING (12h:128) -- set_laser_freq not driven"
    assert coherent.LASER_CONFIG_CHANNEL_OFFSET in hit, \
        f"{port}: xcvrd did not write LASER_CONFIG_CHANNEL (12h:136) -- set_laser_freq not driven"
