"""Mock chassis: the slot table plus the change-event queue."""

import threading

from ._bench import RECORDER, TRACING, load_fixture, num_sfps
from .sfp import make_sfp


class Chassis(object):
    def __init__(self, fixture=None, count=None):
        self._fixture = fixture if fixture is not None else load_fixture()
        self._num = count if count is not None else num_sfps()
        # Built eagerly so slot construction never lands inside a timed region.
        self._sfps = [make_sfp(i, self._fixture) for i in range(self._num)]
        self._events = []
        self._lock = threading.Lock()

    def get_num_sfps(self):
        if TRACING:
            RECORDER.record(kind="hal", port=-1, op="get_num_sfps")
        return self._num

    def get_sfp(self, index):
        if TRACING:
            RECORDER.record(kind="hal", port=-1, op="get_sfp")
        # The bridge passes the daemon's physical index straight through
        # (platform-bridge/src/lib.rs:139). Tolerate both 0- and 1-based callers
        # rather than silently serving the wrong slot.
        if 0 <= index < self._num:
            return self._sfps[index]
        if 1 <= index <= self._num:
            return self._sfps[index - 1]
        raise IndexError(f"no such sfp index {index} (have {self._num})")

    def queue_change_event(self, sfp=None, sfp_error=None):
        """Stage an event for the next get_change_event. Scenario-driven, so a
        plug storm is reproducible rather than dependent on wall-clock timing."""
        with self._lock:
            self._events.append({"sfp": dict(sfp or {}), "sfp_error": dict(sfp_error or {})})

    def get_change_event(self, timeout_ms=0):
        """Returns (status, {"sfp": {...}, "sfp_error": {...}}) -- the shape the
        bridge destructures at platform-bridge/src/lib.rs:159-166.

        Never blocks. A real plant would sleep up to timeout_ms, but sleeping inside
        a benchmark would measure the sleep; scenarios drive time explicitly instead.
        """
        if TRACING:
            RECORDER.record(kind="hal", port=-1, op="get_change_event")
        with self._lock:
            ev = self._events.pop(0) if self._events else {"sfp": {}, "sfp_error": {}}
        return True, ev
