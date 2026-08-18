#!/usr/bin/env python3
"""Compare two benchmark call traces and fail loudly if the daemons did different work.

This is the harness's validity gate. Timing numbers taken alongside a FAILED
comparison are meaningless -- they would be timing two programs doing different
amounts of work -- so this is designed to be run first and to exit non-zero.

Usage:
    compare.py rust.jsonl python.jsonl [--strict-order] [--json]

See ../schema/trace.md for the record format and the comparison semantics.
"""
import argparse
import collections
import json
import sys


def load(path):
    """Parse a JSONL trace, tolerating blank lines but not malformed records."""
    records = []
    with open(path) as fh:
        for lineno, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError as exc:
                sys.exit(f"{path}:{lineno}: malformed trace record: {exc}")
    if not records:
        sys.exit(f"{path}: trace is empty -- did the scenario actually run?")
    return records


def summarize(records):
    """Reduce a trace to order-independent multisets.

    Concurrent daemons interleave nondeterministically, so counts -- not sequence --
    are what can be legitimately compared. `seq` is preserved in the raw trace for
    debugging and is only used under --strict-order.
    """
    hal = collections.Counter()
    db = collections.Counter()
    eeprom = collections.Counter()
    db_writes = collections.Counter()

    for r in records:
        kind = r.get("kind")
        if kind == "hal":
            # Global calls (no port) bucket under port -1 so they stay distinguishable.
            hal[(r.get("port", -1), r.get("op"))] += 1
        elif kind == "db":
            db[(r.get("table"), r.get("op"))] += 1
            if r.get("op") in ("set", "hset") and "nfields" in r:
                db_writes[(r.get("table"), r.get("key"), r["nfields"])] += 1
        elif kind in ("eeprom_read", "eeprom_write"):
            eeprom[(r.get("port"), kind, r.get("offset"), r.get("len"))] += 1
        else:
            sys.exit(f"unknown trace record kind {kind!r}")

    return {"hal": hal, "db": db, "db_writes": db_writes, "eeprom": eeprom}


def diff_counter(name, a, b):
    """Return human-readable differences between two Counters."""
    out = []
    for key in sorted(set(a) | set(b), key=repr):
        na, nb = a.get(key, 0), b.get(key, 0)
        if na != nb:
            out.append(f"  {name} {key!r}: A={na} B={nb} (delta {nb - na:+d})")
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("trace_a", help="first trace (conventionally the Rust daemon)")
    ap.add_argument("trace_b", help="second trace (conventionally the Python daemon)")
    ap.add_argument("--strict-order", action="store_true",
                    help="also require identical seq ordering; ONLY valid for "
                         "single-threaded scenarios (threads == 1)")
    ap.add_argument("--json", action="store_true", help="emit a machine-readable verdict")
    args = ap.parse_args()

    a, b = load(args.trace_a), load(args.trace_b)
    sa, sb = summarize(a), summarize(b)

    problems = []
    for section in ("hal", "db", "db_writes", "eeprom"):
        problems += diff_counter(section, sa[section], sb[section])

    if args.strict_order and not problems:
        # Compare the op sequence only; seq numbers themselves are per-process.
        seq_a = [(r.get("kind"), r.get("port"), r.get("op")) for r in a]
        seq_b = [(r.get("kind"), r.get("port"), r.get("op")) for r in b]
        if seq_a != seq_b:
            for i, (x, y) in enumerate(zip(seq_a, seq_b)):
                if x != y:
                    problems.append(f"  order diverges at index {i}: A={x!r} B={y!r}")
                    break
            else:
                problems.append(f"  order: length differs A={len(seq_a)} B={len(seq_b)}")

    totals = {
        "a_records": len(a), "b_records": len(b),
        "a_hal_calls": sum(sa["hal"].values()), "b_hal_calls": sum(sb["hal"].values()),
        "a_db_ops": sum(sa["db"].values()), "b_db_ops": sum(sb["db"].values()),
        "a_eeprom": sum(sa["eeprom"].values()), "b_eeprom": sum(sb["eeprom"].values()),
    }

    if args.json:
        print(json.dumps({"equivalent": not problems, "totals": totals,
                          "differences": problems}, indent=2))
    else:
        print(f"A: {args.trace_a}\nB: {args.trace_b}")
        print(f"  records   A={totals['a_records']:6d}  B={totals['b_records']:6d}")
        print(f"  hal calls A={totals['a_hal_calls']:6d}  B={totals['b_hal_calls']:6d}")
        print(f"  db ops    A={totals['a_db_ops']:6d}  B={totals['b_db_ops']:6d}")
        print(f"  eeprom    A={totals['a_eeprom']:6d}  B={totals['b_eeprom']:6d}")
        if problems:
            print("\nEQUIVALENCE FAILED -- the daemons did different work:")
            print("\n".join(problems))
            print("\nDo NOT compare timings until this is reconciled.")
        else:
            print("\nEQUIVALENT: identical work profile. Timing comparison is valid.")

    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
