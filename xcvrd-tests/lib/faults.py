"""Emulator fault injection (reserved control page 0xFE).

The emulator's fault-injection feature (xcvr-emu feature/fault-injection) lets the
harness drive xcvrd's error/retry paths. Writes to (0xFE, 0) set a per-module fault
bitmap that is never stored in the EEPROM:

  * FAULT_READ (0x01)     -- non-forced identity-page (page 0) reads fail, so xcvrd's
                             insertion identity read fails and it enters the
                             retry-eeprom loop. Clearing it lets the read recover.
  * FAULT_DP_STALL (0x02) -- the module never reaches ModuleReady, so xcvrd's
                             CmisManagerTask keeps timing out and, after
                             CMIS_MAX_RETRIES, drives cmis_state to FAILED. While it
                             retries the state stays non-terminal (DOM is gated).

Faults persist across plug/unplug until cleared, so a test can arm a fault then
insert the module. ``supported()`` detects (behaviorally) whether the deployed
emulator implements the feature, so the tests skip cleanly on an older emulator.
"""
import grpc

FAULT_PAGE = 0xFE
FAULT_READ = 0x01
FAULT_DP_STALL = 0x02


def arm(emu, index, bits):
    emu.write(index, 0, FAULT_PAGE, 0, bytes([bits]))


def clear(emu, index):
    emu.write(index, 0, FAULT_PAGE, 0, bytes([0x00]))


def supported(emu, index):
    """True iff the deployed emulator implements fault injection.

    Detected behaviorally: on a supporting emulator, arming FAULT_READ makes a
    non-forced page-0 read fail (raises). On an older emulator the write to the
    reserved page is just stored as EEPROM and the read still succeeds. Always
    clears the fault afterwards.
    """
    try:
        arm(emu, index, FAULT_READ)
        try:
            emu.read(index, 0, 0, 0, 1, force=False)
            return False  # read succeeded -> fault not honored
        except grpc.RpcError:
            return True   # read failed -> fault injection active
        except Exception:  # noqa: BLE001 - any read failure means it's honored
            return True
    finally:
        clear(emu, index)
