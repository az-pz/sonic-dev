"""TRANSCEIVER_FIRMWARE_INFO coverage.

xcvrd publishes TRANSCEIVER_FIRMWARE_INFO (active_firmware / inactive_firmware)
from the CMIS API's get_transceiver_info_firmware_versions(). The reduced Rust
daemon publishes no firmware table at all, so asserting the table + its fields is
a parity gate: the candidate must read + publish it.

NOTE: the values read 'N/A' on this testbed because the version comes from a CDB
GetFirmwareInfo command (get_module_fw_info), which the emulator does not serve
(and the CMIS API's CDB handler is off) -- not from the plain byte 39-40
registers. Real version strings would need a CDB feature in the emulator, so this
gate covers table PUBLICATION + fields, matching Python's current output.
"""
import pytest

from lib.waits import wait_until, T_DOM

pytestmark = pytest.mark.slow

FW_FIELDS = ("active_firmware", "inactive_firmware")


def test_firmware_info_published(module):
    """xcvrd publishes TRANSCEIVER_FIRMWARE_INFO with active/inactive firmware fields.

    A daemon that never publishes the firmware table (the reduced Rust) fails here.
    """
    module.plug()
    key = f"TRANSCEIVER_FIRMWARE_INFO|{module.port}"
    wait_until(lambda: module.db.hgetall(key), timeout=T_DOM,
               msg=f"{module.port} TRANSCEIVER_FIRMWARE_INFO published")
    row = module.db.hgetall(key)
    for f in FW_FIELDS:
        assert f in row, f"{f} missing from TRANSCEIVER_FIRMWARE_INFO: {sorted(row)}"
