"""Hardware-error stimulus, via the bridge's gated STATE_DB hook.

The emulator models EEPROM bytes and has no error concept, so a fault cannot be
staged there. The platform bridge instead merges a single STATE_DB hash into what
chassis.get_change_event() returns -- field = physical index, value = SfpBase error
bitmap as a decimal string. From the daemon's side that is indistinguishable from a
real platform reporting a fault, which is what makes it a black-box stimulus rather
than a daemon change.

Inert unless the deploy dropped the bridge's `.test_hooks` marker.
"""
import subprocess

TABLE = "XCVR_EMU_INJECT"
# INSERTED(1) | BLOCKING(2) | I2C_STUCK(8) -- the bitmap xcvrd parses into
# TRANSCEIVER_STATUS_SW.error and gates DOM polling on.
I2C_STUCK_BLOCKING = 11


def _db(*args):
    return subprocess.run(["sonic-db-cli", "STATE_DB", *args],
                          capture_output=True, text=True).stdout.strip()


def set_error(index, bitmap=I2C_STUCK_BLOCKING):
    _db("HSET", TABLE, str(index), str(int(bitmap)))


def clear_error(index):
    _db("HDEL", TABLE, str(index))


def clear_all():
    _db("DEL", TABLE)


def hooks_enabled():
    """Report whether the bridge will actually honour an injection.

    Without this a scenario would stage a fault, observe nothing, and report a
    latency of 'timed out' as though the daemon were at fault.
    """
    r = subprocess.run(
        ["docker", "exec", "pmon", "sh", "-c",
         "ls /usr/local/lib/python3*/dist-packages/sonic_platform/.test_hooks "
         "2>/dev/null || ls /usr/lib/python3*/dist-packages/sonic_platform/.test_hooks 2>/dev/null"],
        capture_output=True, text=True)
    return bool(r.stdout.strip())
