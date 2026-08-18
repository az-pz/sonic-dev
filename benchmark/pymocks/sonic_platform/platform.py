"""`sonic_platform.platform` -- the entry point the bridge imports.

platform-bridge/src/lib.rs:118-120 does exactly:
    py.import_bound("sonic_platform.platform")
      .getattr("Platform")().call_method0("get_chassis")
so this module's contract is just those three steps.
"""

from .chassis import Chassis

_CHASSIS = None


class Platform(object):
    def get_chassis(self):
        # One chassis process-wide. The bridge constructs its own Platform, and a
        # scenario that stages change events or writes EEPROM through the Python
        # side must be observed by the Rust side -- separate chassis objects would
        # silently split that state in two.
        global _CHASSIS
        if _CHASSIS is None:
            _CHASSIS = Chassis()
        return _CHASSIS


def reset_chassis():
    """Drop the cached chassis so the next get_chassis() rebuilds from the fixture.
    Used between scenario repetitions to restore a clean baseline."""
    global _CHASSIS
    _CHASSIS = None
