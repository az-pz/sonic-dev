"""Full table-deletion contract on module removal (HLD; xcvrd.py:587-609).

Presence tests only assert TRANSCEIVER_INFO clears. On a physical unplug (the SFP
removed event) xcvrd actually deletes the WHOLE hardware-info table set for the
port -- INFO, DOM_SENSOR, DOM_FLAG (+ change-count/set-time/clear-time), STATUS,
STATUS_FLAG (+ metadata), FIRMWARE_INFO, and (when present) PM / VDM real+flag
tables -- while the plug-state table TRANSCEIVER_STATUS_SW is PRESERVED and merely
updated to removed (status='0'). A reduced daemon that leaves stale rows behind on
removal fails this.

No emulator change: pure plug/unplug + STATE_DB observation.

Note: the "preserve DOM/VDM THRESHOLD tables" contract is a DIFFERENT code path
(on_remove_logical_port, dom_mgr.py:503, triggered by removing the port from
CONFIG_DB). A physical unplug deletes the threshold tables too, so that
preservation is not asserted here (it would require deconfiguring the port).
"""
import pytest

from lib import cmis
from lib.waits import wait_until, T_FAST, T_DOM

pytestmark = pytest.mark.slow

# Tables deleted for the port on a physical SFP-removed event (xcvrd.py:587-609).
DELETED_ON_REMOVE = [
    "TRANSCEIVER_INFO",
    "TRANSCEIVER_DOM_SENSOR",
    "TRANSCEIVER_DOM_FLAG",
    "TRANSCEIVER_DOM_FLAG_CHANGE_COUNT",
    "TRANSCEIVER_DOM_FLAG_SET_TIME",
    "TRANSCEIVER_DOM_FLAG_CLEAR_TIME",
    "TRANSCEIVER_STATUS",
    "TRANSCEIVER_STATUS_FLAG",
    "TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT",
    "TRANSCEIVER_STATUS_FLAG_SET_TIME",
    "TRANSCEIVER_STATUS_FLAG_CLEAR_TIME",
    "TRANSCEIVER_FIRMWARE_INFO",
]


def test_removal_deletes_full_table_set(module):
    """Unplugging deletes the entire hardware-info table set for the port; the
    plug-state table TRANSCEIVER_STATUS_SW survives (status -> '0')."""
    m = module
    # Bring the port to a rich, fully-populated state first (all the tables the
    # removal must clear). STATUS/STATUS_FLAG land on the DOM poll.
    m.plug()
    m.wait_info_populated(timeout=T_FAST)
    # Raise a DOM flag so the DOM_FLAG (+ metadata) tables exist before removal.
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([cmis.TEMP_HIGH_ALARM_FLAG]))
    for table in ("TRANSCEIVER_STATUS", "TRANSCEIVER_STATUS_FLAG",
                  "TRANSCEIVER_DOM_SENSOR", "TRANSCEIVER_DOM_FLAG",
                  "TRANSCEIVER_FIRMWARE_INFO"):
        wait_until(lambda t=table: m.db.exists(f"{t}|{m.port}"), timeout=2 * T_DOM,
                   msg=f"{m.port} {table} populated before removal")
    # sanity: the metadata side-tables exist too
    wait_until(lambda: m.db.exists(f"TRANSCEIVER_DOM_FLAG_CHANGE_COUNT|{m.port}"),
               timeout=T_DOM, msg=f"{m.port} DOM flag metadata present before removal")

    m.unplug()

    for table in DELETED_ON_REMOVE:
        wait_until(lambda t=table: not m.db.exists(f"{t}|{m.port}"), timeout=T_FAST,
                   msg=f"{m.port} {table} deleted on removal")

    # STATUS_SW is NOT deleted -- it is updated to the removed state.
    assert m.db.exists(f"TRANSCEIVER_STATUS_SW|{m.port}"), \
        f"{m.port} TRANSCEIVER_STATUS_SW must survive removal (holds plug status)"
    wait_until(lambda: m.status_sw().get("status") == "0", timeout=T_FAST,
               msg=f"{m.port} STATUS_SW.status == '0' after removal")
