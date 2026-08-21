"""Emulator client -- plug/unplug stimulus and the EEPROM interaction stream.

Self-contained: the gRPC stubs are generated from proto/emulator.proto at import
time into a temp dir, so this shares no code with the xcvrd-tests suite (the
pipeline's correctness oracle, which must not be entangled with a benchmark).

Two capabilities matter here:

  UpdateInfo(index, present)  -- the plug/unplug stimulus. The emulator has no
      error concept, so hardware faults are injected separately through the
      bridge's STATE_DB hook (see inject_err.py).

  Monitor(index)              -- a server-streaming RPC emitting one message for
      EVERY EEPROM read and write the emulator serves. Since the daemon reaches
      the modules exclusively through those, this is a complete timestamped trace
      of its interaction with the "hardware", captured with zero daemon changes.
      That makes read-amplification MACHINE-INDEPENDENT: it counts work, not time,
      so it is immune to KVM steal and host noise.
"""

import os
import subprocess
import sys
import tempfile
import threading
import time

TARGET = os.environ.get("XCVR_EMU_TARGET", "localhost:50051")
_HERE = os.path.dirname(os.path.abspath(__file__))
_PROTO = os.path.join(_HERE, "proto", "emulator.proto")


def _load_stubs():
    """Generate + import the gRPC stubs. Cached in a stable temp dir so repeated
    runs on the DUT do not pay protoc each time."""
    out = os.path.join(tempfile.gettempdir(), "xbench_pb")
    os.makedirs(out, exist_ok=True)
    pb_py = os.path.join(out, "emulator_pb2.py")
    if not os.path.exists(pb_py):
        subprocess.run(
            [sys.executable, "-m", "grpc_tools.protoc",
             f"-I{os.path.dirname(_PROTO)}", f"--python_out={out}",
             f"--grpc_python_out={out}", _PROTO],
            check=True, capture_output=True)
    if out not in sys.path:
        sys.path.insert(0, out)
    import emulator_pb2 as pb
    import emulator_pb2_grpc as pb_grpc
    return pb, pb_grpc


pb, pb_grpc = _load_stubs()
import grpc  # noqa: E402  (after stub generation, which needs grpc_tools not grpc)


class Emu:
    def __init__(self, target=TARGET):
        self._ch = grpc.insecure_channel(target)
        self._stub = pb_grpc.SfpEmulatorServiceStub(self._ch)

    def list(self):
        return list(self._stub.List(pb.ListRequest()).infos)

    def present_indices(self):
        return [i.index for i in self.list() if i.present]

    def set_present(self, index, present):
        """The plug / unplug stimulus."""
        self._stub.UpdateInfo(pb.UpdateInfoRequest(index=index, present=present))

    def info(self, index):
        return self._stub.GetInfo(pb.GetInfoRequest(index=index))

    def close(self):
        self._ch.close()


class Monitor(threading.Thread):
    """Records every EEPROM read/write the emulator serves, with local timestamps.

    index=0 means all modules (per the proto's MonitorRequest comment).
    """

    def __init__(self, index=0, target=TARGET):
        super().__init__(daemon=True)
        self.index, self.target = index, target
        self.events = []          # (ts, index, page, offset, length, is_write)
        self._stop = threading.Event()
        self._ready = threading.Event()
        self._ch = None

    def run(self):
        self._ch = grpc.insecure_channel(self.target)
        stub = pb_grpc.SfpEmulatorServiceStub(self._ch)
        try:
            stream = stub.Monitor(pb.MonitorRequest(index=self.index))
            self._ready.set()
            for m in stream:
                if self._stop.is_set():
                    break
                self.events.append((time.time(), m.index, m.page, m.offset,
                                    m.length, bool(m.write)))
        except grpc.RpcError:
            self._ready.set()

    def start_and_wait(self, timeout=5.0):
        self.start()
        self._ready.wait(timeout)
        # The stream is established asynchronously; a brief settle avoids losing the
        # first few events of a stimulus that fires immediately after start().
        time.sleep(0.3)
        return self

    def stop(self):
        self._stop.set()
        if self._ch:
            self._ch.close()

    def clear(self):
        self.events.clear()

    def reads(self, index=None):
        return [e for e in self.events if not e[5] and (index is None or e[1] == index)]

    def writes(self, index=None):
        return [e for e in self.events if e[5] and (index is None or e[1] == index)]

    def summary(self):
        """Work profile: counts and the page histogram, both machine-independent."""
        per_port = {}
        pages = {}
        for _, idx, page, _, _, w in self.events:
            d = per_port.setdefault(idx, {"reads": 0, "writes": 0})
            d["writes" if w else "reads"] += 1
            pages[page] = pages.get(page, 0) + 1
        return {"total": len(self.events), "per_port": per_port,
                "page_histogram": dict(sorted(pages.items()))}
