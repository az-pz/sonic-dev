"""Vendored xcvr-emu gRPC stubs.

The generated emulator_pb2_grpc.py uses a flat ``import emulator_pb2`` (as the
grpc plugin emits), so this package prepends its own directory to sys.path to
make that import resolve regardless of how the tests are launched.
"""
import os
import sys

_HERE = os.path.dirname(__file__)
if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

from . import emulator_pb2 as pb          # noqa: E402
from . import emulator_pb2_grpc as pb_grpc  # noqa: E402

__all__ = ["pb", "pb_grpc"]
