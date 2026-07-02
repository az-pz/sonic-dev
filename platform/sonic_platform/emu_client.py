"""gRPC client helper for the xcvr-emu emulator.

Holds a lazily-created channel/stub so that importing the platform plugin does
not require the emulator to be running yet (xcvrd imports the plugin at start-up,
before it ever reads a port).
"""
import os
import sys

import grpc

from xcvr_emu.proto import emulator_pb2 as pb2

# emulator_pb2_grpc does a non-relative `import emulator_pb2`, so its directory
# must be importable. The emulator's own daemon performs the same workaround.
sys.path.append(os.path.dirname(pb2.__file__))
from xcvr_emu.proto import emulator_pb2_grpc as pb2_grpc  # noqa: E402

DEFAULT_ADDR = "localhost:50051"

_channel = None
_stub = None


def get_stub():
    """Return a cached SfpEmulatorServiceStub, creating the channel on first use."""
    global _channel, _stub
    if _stub is None:
        addr = os.environ.get("XCVR_EMU_ADDR", DEFAULT_ADDR)
        _channel = grpc.insecure_channel(addr)
        _stub = pb2_grpc.SfpEmulatorServiceStub(_channel)
    return _stub


# Re-export the protobuf message module for callers.
pb = pb2
