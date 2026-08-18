# Call-trace record format (v1)

One JSON object per line (JSONL). Both the Rust and the Python harness emit this
**identical** format so `equivalence/compare.py` can diff them without knowing
which daemon produced which file.

A trace answers one question: *did the two daemons do the same work?* It records
**what was called**, never how long it took. Timing lives in the criterion /
pytest-benchmark outputs, separately, and is only trustworthy once a trace
comparison passes.

## Record

```json
{"seq": 0, "kind": "hal", "port": 0, "op": "get_presence"}
{"seq": 1, "kind": "hal", "port": 0, "op": "get_transceiver_dom_real_value"}
{"seq": 2, "kind": "db",  "table": "TRANSCEIVER_DOM_SENSOR", "key": "Ethernet0", "op": "set", "nfields": 27}
{"seq": 3, "kind": "eeprom_read",  "port": 0, "offset": 9,   "len": 1}
{"seq": 4, "kind": "eeprom_write", "port": 0, "offset": 130, "len": 1}
```

| field | kinds | meaning |
|---|---|---|
| `seq` | all | monotonic per-process counter, assigned at record time |
| `kind` | all | `hal` \| `db` \| `eeprom_read` \| `eeprom_write` |
| `port` | hal, eeprom_* | physical port index (**not** the logical name) |
| `op` | hal, db | HAL method name, or DB op (`set`/`hset`/`get`/`hget`/`del`/`hdel`/`get_keys`/`get_size`/`get_size_for_key`) |
| `table` | db | STATE_DB table, e.g. `TRANSCEIVER_DOM_SENSOR` |
| `key` | db | row key, e.g. `Ethernet0` |
| `nfields` | db | field count written (`set`) or present (`get_size_for_key`) |
| `offset`, `len` | eeprom_* | flat linear EEPROM offset and byte count |

`op` uses the **Rust `SfpHandle`/`DbTable` method names** as the canonical
vocabulary. The Python harness maps its own call names onto these; the mapping
lives in `equivalence/opmap.py` so it is reviewable rather than implicit.

## Comparison semantics

`seq` is recorded for debuggability but is **not** compared by default: the Rust
daemon runs four concurrent tasks, so interleaving is nondeterministic and an
exact-sequence diff would be flaky by construction.

The gate compares **aggregated multisets**:

- HAL: count per `(port, op)`
- DB: count per `(table, op)`, plus the set of `(table, key, nfields)` for writes
- EEPROM: count per `(port, kind)` and the set of `(port, offset, len)`

`--strict-order` additionally compares the `seq` ordering, and is only meaningful
for single-threaded scenarios (`"threads": 1`).

Field *counts* rather than field *values* are compared, matching the reference
unit tests (`dom_tbl.get_size_for_key("Ethernet0") == 27`). Value-level parity is
already covered by the `xcvrd-tests` behavioural suite; duplicating it here would
make the gate brittle without adding signal.

## Why counts, not time

These are the only metrics in the whole harness that are independent of machine,
language runtime and scheduler. A read-amplification gap (e.g. Rust issuing 40
EEPROM reads per DOM cycle where Python issues 25) is a real fidelity finding on
its own, and it invalidates every timing number taken alongside it.
