"""Shared `sonic_platform` mock for the xcvrd benchmark harness.

This package is the SINGLE transceiver-plant implementation used by both daemons
in config B:

  * the Python xcvrd imports it directly, and
  * the Rust xcvrd reaches it through the real `platform-bridge`, which does a
    plain `py.import_bound("sonic_platform.platform")` (platform-bridge/src/lib.rs:118) --
    so putting this package first on `sys.path` substitutes it with no code change
    to the daemon or the bridge.

Because both sides execute literally this code, the mock's own cost is identical
by construction rather than merely specified to match. What is NOT equal in config B
is the transport: Rust additionally pays the PyO3 crossing (measured at ~7.9us per
HAL call, 97.6% of it `json.dumps` + `serde_json` parse). That cost is real -- the
deployed daemon pays it -- so config B measures the shipping architecture, and the
config A -> B delta isolates the crossing itself.

Design constraints, in priority order:

1. Be fast enough not to matter. Every payload is decoded from the fixture ONCE at
   construction; per-call work is a single dict copy (~102ns) and nothing else. No
   MagicMock: it costs ~4.7us per call, 162x a hand-written fake, which was large
   enough to dominate the very difference we are trying to measure.
2. Match the Rust bench edge's semantics exactly -- see edges.rs.
3. Stay silent unless asked. Call recording is opt-in via XCVRD_BENCH_TRACE, so
   timing runs never pay for instrumentation.

Configuration (environment):
    XCVRD_BENCH_FIXTURE   path to the fixture JSON (required)
    XCVRD_BENCH_NUM_SFPS  slot count to present (default 32)
    XCVRD_BENCH_TRACE     "1" to record every call for the equivalence gate
"""

from .platform import Platform  # noqa: F401

__all__ = ["Platform"]
