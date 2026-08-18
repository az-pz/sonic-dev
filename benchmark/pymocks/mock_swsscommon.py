"""In-memory STATE_DB `Table`, the Python side of the DB seam.

Mirrors `xcvrd_rs::mock::MockDbTable` (analysis.md:412 records the two as
counterparts) and the upstream `tests/mock_swsscommon.py` API:
`set`/`hset`/`get`/`hget`/`_del`/`hdel`/`getKeys`/`get_size`/`get_size_for_key`.

IMPORTANT -- unlike the platform plant, this edge CANNOT be shared between the two
daemons. `XcvrTableHelper::build` is private in the target crate, so the Rust side is
forced onto its own `MockDbTable`, and the real `swsscommon` Python bindings are not
present in this environment (only the C++ .so's that Rust links against), so a shared
real-library path is not available either. The two DB edges are therefore separate
implementations and their difference must be CALIBRATED rather than assumed away --
see tools/run_calibrate.sh and equivalence/compare.py.

One known semantic divergence, carried deliberately because it matches both
references: `set` REPLACES a row (the real swss `Table.set` merges). `hset` merges a
single field. Getting this backwards would change field counts, which the reference
tests assert on directly.
"""


class Table(object):
    __slots__ = ("_name", "_rows")

    def __init__(self, db=None, name=""):
        self._name = name
        # key -> {field: value}. A dict, where MockDbTable uses a Vec of pairs with a
        # linear scan for hget (mock.rs:90). That asymmetry is real and is why the DB
        # edge is calibrated; it is not corrected here, because each daemon should run
        # against its own reference mock.
        self._rows = {}

    def set(self, key, fvs):
        # Replaces, matching MockDbTable::set. fvs is a sequence of (field, value).
        self._rows[key] = {f: v for f, v in fvs}

    def hset(self, key, field, value):
        self._rows.setdefault(key, {})[field] = value

    def get(self, key):
        row = self._rows.get(key)
        # swss returns (found, pairs); the mock keeps that shape so callers written
        # against the real Table do not need a special case.
        if row is None:
            return False, []
        return True, list(row.items())

    def hget(self, key, field):
        row = self._rows.get(key)
        if row is None or field not in row:
            return False, ""
        return True, row[field]

    def _del(self, key):
        self._rows.pop(key, None)

    def hdel(self, key, field):
        row = self._rows.get(key)
        if row is None:
            return
        row.pop(field, None)
        if not row:
            # Drop the row once its last field goes, matching MockDbTable::hdel.
            del self._rows[key]

    def getKeys(self):
        return list(self._rows.keys())

    def get_size(self):
        return len(self._rows)

    def get_size_for_key(self, key):
        return len(self._rows.get(key, ()))
