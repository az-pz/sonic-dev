"""Golden baseline: capture the reference xcvrd's STATE_DB projection and diff.

The current upstream xcvrd is the oracle. We snapshot the STATE_DB rows it
produces for a port into golden/<port>.json, then a compare test asserts a
candidate xcvrd (e.g. a Rust reimplementation) reproduces the same projection.

Only stable tables are goldened; volatile fields (timestamps) are dropped so the
comparison is deterministic. Live DOM sensor values are intentionally excluded
(they are covered by test_dom.py instead).
"""
import json
import os

# Tables whose contents are static for a given module + config.
GOLDEN_TABLES = [
    "TRANSCEIVER_INFO",
    "TRANSCEIVER_STATUS_SW",
    "TRANSCEIVER_DOM_THRESHOLD",
]

# Per-row fields that change every update and must not be goldened.
VOLATILE_KEYS = {"last_update_time"}


def project(statedb, port, tables=GOLDEN_TABLES):
    """Return {table: {field: value}} for ``port``, volatile fields removed."""
    out = {}
    for tbl in tables:
        row = statedb.hgetall(f"{tbl}|{port}")
        row = {k: v for k, v in row.items() if k not in VOLATILE_KEYS}
        out[tbl] = dict(sorted(row.items()))
    return out


def path_for(golden_dir, port, scenario=None):
    """Golden file path. With a scenario -> ``<golden_dir>/<scenario>/<port>.json``;
    without -> the legacy ``<golden_dir>/<port>.json``."""
    if scenario:
        return os.path.join(golden_dir, scenario, f"{port}.json")
    return os.path.join(golden_dir, f"{port}.json")


def save(projection, path):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(projection, f, indent=2, sort_keys=True)
        f.write("\n")


def load(path):
    with open(path) as f:
        return json.load(f)


def diff(current, golden):
    """Return a list of human-readable field differences (empty == match)."""
    diffs = []
    for tbl in sorted(set(current) | set(golden)):
        cur = current.get(tbl, {})
        gold = golden.get(tbl, {})
        for key in sorted(set(cur) | set(gold)):
            if cur.get(key) != gold.get(key):
                diffs.append(
                    f"{tbl}.{key}: golden={gold.get(key)!r} current={cur.get(key)!r}")
    return diffs
