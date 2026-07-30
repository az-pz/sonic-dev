"""CONFIG_DB logical-port add/remove: full table teardown + repopulation (C22).

Distinct from a physical SFP unplug (test_removal_tables). Removing a logical port from
CONFIG_DB (a PORT_DEL, e.g. deconfiguring or a dynamic-port-breakout step) makes xcvrd's
on_remove_logical_port tear down the ENTIRE per-port table set -- INFO, DOM_SENSOR, DOM
flag tables, the DOM/VDM THRESHOLD tables, STATUS, STATUS_FLAG, FIRMWARE_INFO, PM AND
the plug-state table TRANSCEIVER_STATUS_SW (xcvrd.py:731-764). Two things make this a
different, still-uncovered branch from the physical-unplug removal:

  * a physical unplug PRESERVES TRANSCEIVER_STATUS_SW (test_removal_tables) and the
    DOM/VDM THRESHOLD tables; a logical-port removal deletes them all -- it is a full
    deconfiguration, not just "the module went away".
  * re-adding the port (PORT_SET) repopulates the tables and re-seeds
    STATE_DB PORT_TABLE.NPU_SI_SETTINGS_SYNC_STATUS = NPU_SI_SETTINGS_DEFAULT
    (xcvrd.py:794 / on_add_logical_port).

Pure CONFIG_DB stimulus + STATE_DB observation, no emulator change. The fixture snapshots
the port's full CONFIG_DB PORT hash and ALWAYS restores it (so the port and its tables come
back) even if the test fails mid-way. Uses a spare admin-up port (default Ethernet60) not
used by the golden/special gates; override with XCVRD_LPORT.
"""
import os

import pytest

from lib.emu import port_to_index
from lib.waits import wait_until, T_FAST, T_DOM, T_BASELINE

pytestmark = pytest.mark.slow

LPORT = os.environ.get("XCVRD_LPORT", "Ethernet60")
SYNC_STATUS_KEY = "NPU_SI_SETTINGS_SYNC_STATUS"
DEFAULT_VALUE = "NPU_SI_SETTINGS_DEFAULT"

# The complete per-port table set on_remove_logical_port deletes -- including the
# THRESHOLD tables and STATUS_SW that a physical unplug preserves.
FULL_TABLE_SET = [
    "TRANSCEIVER_INFO",
    "TRANSCEIVER_DOM_SENSOR",
    "TRANSCEIVER_DOM_THRESHOLD",
    "TRANSCEIVER_VDM_HALARM_THRESHOLD",
    "TRANSCEIVER_STATUS",
    "TRANSCEIVER_STATUS_SW",
    "TRANSCEIVER_FIRMWARE_INFO",
]


def _cfg_key(port):
    return f"PORT|{port}"


@pytest.fixture
def lport(emu, statedb, configdb):
    """A spare admin-up port whose CONFIG_DB PORT hash is snapshotted and always
    restored on teardown (bringing the port + its tables back)."""
    port = LPORT
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port})")
    snap = configdb.hgetall(_cfg_key(port))
    if not snap or snap.get("admin_status") != "up":
        pytest.skip(f"{port} is not an admin-up configured port; C22 needs a live logical port")
    emu.plug(idx)
    # Ensure the full table set exists before we tear it down.
    wait_until(lambda: statedb.exists(f"TRANSCEIVER_INFO|{port}")
               and statedb.exists(f"TRANSCEIVER_DOM_THRESHOLD|{port}"),
               timeout=T_DOM, msg=f"{port} INFO + DOM_THRESHOLD populated before C22")
    try:
        yield port, idx, snap
    finally:
        # Restore the CONFIG_DB PORT hash (re-adds the logical port) + repopulation.
        try:
            if not configdb.exists(_cfg_key(port)):
                for k, v in snap.items():
                    configdb.hset(_cfg_key(port), k, v)
            emu.plug(idx)
            wait_until(lambda: statedb.exists(f"TRANSCEIVER_INFO|{port}"),
                       timeout=T_BASELINE, msg=f"{port} INFO restored after C22")
        except Exception:  # noqa: BLE001
            pass


def test_logical_port_remove_deletes_full_table_set(lport, statedb, configdb):
    """Removing the port from CONFIG_DB deletes the entire per-port table set --
    including the DOM/VDM THRESHOLD tables and TRANSCEIVER_STATUS_SW that a physical
    unplug preserves."""
    port, _, _ = lport
    assert statedb.exists(f"TRANSCEIVER_STATUS_SW|{port}"), f"{port} STATUS_SW missing at start"

    configdb.delete(_cfg_key(port))

    for table in FULL_TABLE_SET:
        wait_until(lambda t=table: not statedb.exists(f"{t}|{port}"), timeout=T_DOM,
                   msg=f"{port} {table} deleted on CONFIG_DB logical-port removal")


def test_logical_port_readd_repopulates_and_reseeds_sync_status(lport, statedb, configdb):
    """Re-adding the port to CONFIG_DB repopulates the tables and re-seeds
    NPU_SI_SETTINGS_SYNC_STATUS = NPU_SI_SETTINGS_DEFAULT."""
    port, idx, snap = lport

    # Remove, then re-add from the snapshot.
    configdb.delete(_cfg_key(port))
    wait_until(lambda: not statedb.exists(f"TRANSCEIVER_INFO|{port}"), timeout=T_DOM,
               msg=f"{port} INFO cleared on removal before re-add")

    for k, v in snap.items():
        configdb.hset(_cfg_key(port), k, v)

    # Tables repopulate...
    for table in ("TRANSCEIVER_INFO", "TRANSCEIVER_DOM_THRESHOLD", "TRANSCEIVER_STATUS_SW"):
        wait_until(lambda t=table: statedb.exists(f"{t}|{port}"), timeout=T_BASELINE,
                   msg=f"{port} {table} repopulated on CONFIG_DB logical-port re-add")

    # ...and the NPU SI sync status is re-seeded to DEFAULT for the fresh port.
    wait_until(lambda: statedb.hget(f"PORT_TABLE|{port}", SYNC_STATUS_KEY) == DEFAULT_VALUE,
               timeout=T_FAST, msg=f"{port} NPU_SI_SETTINGS_SYNC_STATUS re-seeded to DEFAULT on re-add")
