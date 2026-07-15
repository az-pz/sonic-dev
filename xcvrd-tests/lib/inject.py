"""Error injection through the STATE_DB table the bridge reads.

The emulator itself has no error concept, so error events are injected via a
bridge hook: writing XCVR_EMU_INJECT|<physical_index>.event = <bitmap> in
STATE_DB makes chassis.get_change_event() report that event for the port, the
same way a real platform would surface a hardware error. Clearing the row lets
the port return to its live presence state (a plug-in event that clears the
error). This is a black-box stimulus, not an xcvrd/emulator change.
"""

INJECT_TABLE = "XCVR_EMU_INJECT"


class ErrorInjector:
    def __init__(self, statedb):
        self.db = statedb

    def set(self, index, event_bitmap):
        """Inject an sfp error event (int bitmap) for a physical port index."""
        self.db.hset(f"{INJECT_TABLE}|{index}", "event", str(int(event_bitmap)))

    def clear(self, index):
        self.db.delete(f"{INJECT_TABLE}|{index}")

    def clear_all(self):
        return self.db.delete_pattern(f"{INJECT_TABLE}|*")
