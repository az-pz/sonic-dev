"""MonitorRecorder: capture the xcvrd <-> emulator interaction trace.

The emulator implements a server-streaming ``Monitor`` RPC that emits one
message for *every* EEPROM Read and Write it serves. Since xcvrd reaches the
emulated transceivers exclusively through those reads/writes (via the bridge),
subscribing to this stream gives us a complete, timestamped trace of xcvrd's
interaction with the "hardware" -- with zero changes to xcvrd or the emulator.

Usage:
    rec = MonitorRecorder(); rec.start()
    ...stimulus...
    reads = rec.reads(index=25)          # all reads of module 25 so far
    rec.clear()                          # scope the next assertion window
"""
import threading
import time

import grpc

from .proto import pb, pb_grpc

DEFAULT_TARGET = "localhost:50051"


class Event:
    __slots__ = ("ts", "index", "bank", "page", "offset", "length",
                 "data", "present", "write")

    def __init__(self, ts, msg):
        self.ts = ts
        self.index = msg.index
        self.bank = msg.bank
        self.page = msg.page
        self.offset = msg.offset
        self.length = msg.length
        self.data = bytes(msg.data)
        self.present = bool(msg.present)
        self.write = bool(msg.write)

    @property
    def is_read(self):
        return not self.write

    def __repr__(self):
        kind = "W" if self.write else "R"
        return (f"<{kind} idx={self.index} p{self.page:02x}h:"
                f"{self.offset}+{self.length} data={self.data.hex()}>")


class MonitorRecorder:
    def __init__(self, target=DEFAULT_TARGET, index=0):
        # index=0 subscribes to ALL modules (the emulator filters on non-zero).
        self._target = target
        self._index = index
        self._events = []
        self._lock = threading.Lock()
        self._thread = None
        self._channel = None
        self._call = None
        self._stopping = False
        self._ready = threading.Event()

    # --- lifecycle ----------------------------------------------------------
    def start(self, ready_timeout=5.0):
        self._thread = threading.Thread(target=self._run, name="mon-rec",
                                        daemon=True)
        self._thread.start()
        # Give the stream a moment to establish so early events aren't missed.
        self._ready.wait(timeout=ready_timeout)
        time.sleep(0.3)
        return self

    def stop(self):
        self._stopping = True
        try:
            if self._call is not None:
                self._call.cancel()
        except Exception:  # noqa: BLE001
            pass
        try:
            if self._channel is not None:
                self._channel.close()
        except Exception:  # noqa: BLE001
            pass
        if self._thread is not None:
            self._thread.join(timeout=3.0)

    def _run(self):
        self._channel = grpc.insecure_channel(self._target)
        stub = pb_grpc.SfpEmulatorServiceStub(self._channel)
        self._call = stub.Monitor(pb.MonitorRequest(index=self._index))
        self._ready.set()
        try:
            for msg in self._call:
                ev = Event(time.time(), msg)
                with self._lock:
                    self._events.append(ev)
        except grpc.RpcError:
            pass  # cancelled on stop() or emulator restart

    # --- query API ----------------------------------------------------------
    def clear(self):
        with self._lock:
            self._events.clear()

    def all(self):
        with self._lock:
            return list(self._events)

    def _filter(self, want_write, index=None, page=None, since=None):
        with self._lock:
            evs = list(self._events)
        out = []
        for e in evs:
            if e.write != want_write:
                continue
            if index is not None and e.index != index:
                continue
            if page is not None and e.page != page:
                continue
            if since is not None and e.ts < since:
                continue
            out.append(e)
        return out

    def reads(self, index=None, page=None, since=None):
        return self._filter(False, index=index, page=page, since=since)

    def writes(self, index=None, page=None, since=None):
        return self._filter(True, index=index, page=page, since=since)

    def count(self, index=None):
        with self._lock:
            if index is None:
                return len(self._events)
            return sum(1 for e in self._events if e.index == index)
