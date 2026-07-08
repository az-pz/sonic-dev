"""
sonic_platform — a SONiC platform plugin backed by the xcvr-emu CMIS emulator.

xcvrd loads a platform via `import sonic_platform.platform` →
`Platform().get_chassis()`. This package implements just enough of that contract
(Platform → Chassis → Sfp) to drive the *software* transceiver emulator
(xcvr-emu) over gRPC instead of real optics hardware.

It is dev tooling: put this directory on PYTHONPATH and point it at a running
xcvr-emud (default localhost:50051, override with XCVR_EMU_ADDR).
"""
# Bind the submodules on the package so callers that access them as attributes
# (e.g. sfputil does `import sonic_platform` then `sonic_platform.platform.Platform()`)
# work without an explicit `import sonic_platform.platform`. The stock vs stub did
# the equivalent via `__all__ = ["platform"]; from sonic_platform import *`.
from . import platform  # noqa: F401
from . import chassis   # noqa: F401
from . import sfp       # noqa: F401

__all__ = ["platform", "chassis", "sfp"]
