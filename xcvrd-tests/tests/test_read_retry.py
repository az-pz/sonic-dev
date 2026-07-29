"""EEPROM identity read-retry recovery (#11; xcvrd.py retry_eeprom loop).

On insertion xcvrd reads the module identity; if that read fails it does not give
up -- it adds the port to a retry set and re-reads on a ~60s cadence until it
succeeds, only then publishing TRANSCEIVER_INFO. A daemon that reads identity once
and drops the module on failure never recovers. We drive this with the emulator's
FAULT_READ injection: while armed, identity-page reads fail so TRANSCEIVER_INFO
stays absent; clearing it lets the next retry succeed and INFO appears -- WITHOUT a
re-plug (the recovery is the read-retry loop, not a fresh insertion event).
"""
import pytest

from lib import faults
from lib.waits import wait_until, stays, T_FAST, T_DOM

pytestmark = pytest.mark.slow


def test_eeprom_read_retry_recovers(fault_port, emu, statedb):
    port, idx = fault_port

    # Arm the read fault, then re-insert: xcvrd sees the module present but cannot
    # read its identity, so TRANSCEIVER_INFO must NOT populate.
    faults.arm(emu, idx, faults.FAULT_READ)
    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} INFO cleared before read-fault insert")
    emu.plug(idx)
    assert stays(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
                 duration=25.0), \
        f"{port} TRANSCEIVER_INFO populated despite failing identity reads"

    # Clear the fault: xcvrd's retry-eeprom loop re-reads and publishes INFO,
    # WITHOUT any re-plug (proving it kept retrying rather than dropping the port).
    faults.clear(emu, idx)
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer") == "xcvr-emu",
               timeout=T_DOM,
               msg=f"{port} TRANSCEIVER_INFO recovered via read-retry after fault cleared")
