"""Error injection through the STATE_DB hash the bridge reads.

The emulator itself has no error concept, so error events are injected via a
gated bridge hook: with the bridge's `.test_hooks` marker present, writing a
field in the single STATE_DB hash `XCVR_EMU_INJECT` (field = physical index,
value = SfpBase error bitmap) makes chassis.get_change_event() report that event
for the port, the same way a real platform surfaces a hardware error. Clearing
the field lets the port return to its live presence state (a plug-in event that
clears the error). Using one hash means the bridge reads it with a single
HGETALL (no KEYS scan). This is a black-box stimulus, not an xcvrd/emulator
change, and it is inert unless the deploy enabled the marker.
"""

INJECT_TABLE = "XCVR_EMU_INJECT"


class ErrorInjector:
    def __init__(self, statedb):
        self.db = statedb

    def set(self, index, event_bitmap):
        """Inject an sfp error event (int bitmap) for a physical port index."""
        self.db.hset(INJECT_TABLE, str(index), str(int(event_bitmap)))

    def clear(self, index):
        self.db.hdel(INJECT_TABLE, str(index))

    def clear_all(self):
        return self.db.delete(INJECT_TABLE)

