"""Fixture loading and optional call recording. Not part of the sonic_platform API."""

import json
import os
import threading

_FIXTURE_ENV = "XCVRD_BENCH_FIXTURE"
_NUM_SFPS_ENV = "XCVRD_BENCH_NUM_SFPS"
_TRACE_ENV = "XCVRD_BENCH_TRACE"

DEFAULT_NUM_SFPS = 32


def load_fixture(path=None):
    """Read the fixture JSON. Fails loudly: a benchmark silently falling back to
    defaults would compare two daemons fed different data."""
    path = path or os.environ.get(_FIXTURE_ENV)
    if not path:
        raise RuntimeError(
            f"{_FIXTURE_ENV} is not set -- the mock plant has no payloads to serve. "
            "Point it at e.g. benchmark/fixtures/cmis_40g_lr4.json"
        )
    with open(path) as fh:
        fx = json.load(fh)
    # Drop comment blocks so they cannot leak into a payload and skew field counts.
    return {k: v for k, v in fx.items() if not k.startswith("_")}


def num_sfps():
    return int(os.environ.get(_NUM_SFPS_ENV, DEFAULT_NUM_SFPS))


class Recorder:
    """Records edge calls for the equivalence gate.

    Deliberately opt-in: appending a record costs more than the mock call itself,
    so leaving this on during a timing run would measure the instrument. Emits the
    same JSONL schema as the Rust harness (see benchmark/schema/trace.md) so the
    two traces are directly comparable.
    """

    __slots__ = ("_records", "_lock", "_seq")

    def __init__(self):
        self._records = []
        self._lock = threading.Lock()
        self._seq = 0

    def record(self, **fields):
        with self._lock:
            fields["seq"] = self._seq
            self._seq += 1
            self._records.append(fields)

    def to_jsonl(self):
        with self._lock:
            return "\n".join(
                json.dumps(r, separators=(",", ":"), sort_keys=True) for r in self._records
            )

    def clear(self):
        with self._lock:
            self._records.clear()
            self._seq = 0


# A module-level singleton so the Rust bridge (which constructs its own Platform)
# and the Python harness share one trace even though they never share an object.
RECORDER = Recorder()

# Resolved once at import: branching on os.environ per call would be a per-call cost.
TRACING = os.environ.get(_TRACE_ENV) == "1"
