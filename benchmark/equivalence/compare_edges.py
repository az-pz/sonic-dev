#!/usr/bin/env python3
"""Line up the three edge calibrations side by side.

    compare_edges.py rust.json python.json

    A  Rust -> BenchHal    Rust-native plant, no Python in the process
    B  Rust -> BridgeHal   the real PyO3 bridge onto pymocks   <- the deployed path
    P  Python              the same pymocks plant, called directly

Two ratios matter, and they answer different questions:

  B/P  the PyO3 crossing, isolated. B and P execute literally the same mock code, so
       whatever separates them is transport and nothing else.
  A/P  the floor: what the Rust edge costs when Python is removed entirely. This is
       the instrument bias that must be calibrated out of config-A daemon timings.

Neither is a daemon result. These numbers describe the harness, not xcvrd -- their
job is to make the correction term in `T_measured = T_orchestration + k * C_edge`
explicit, with k supplied by the equivalence gate.
"""

import argparse
import json
import sys


def load(path, key):
    with open(path) as fh:
        data = json.load(fh)
    if key not in data:
        sys.exit(f"{path}: no '{key}' section (got {sorted(data)})")
    return data[key]


def fmt(v):
    return "-" if v is None else f"{v:9.1f}"


def ratio(a, b):
    if a is None or b is None or b == 0:
        return "-"
    return f"{a / b:7.1f}x"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("rust_json", help="output of tools/run_calibrate.sh --out")
    ap.add_argument("python_json", help="output of tools/run_calibrate_py.sh --out")
    args = ap.parse_args()

    with open(args.rust_json) as fh:
        rust = json.load(fh)
    a, b = rust.get("a", {}), rust.get("b", {})
    p = load(args.python_json, "p")

    labels = list(a) or list(b)
    for k in list(b) + list(p):
        if k not in labels:
            labels.append(k)

    w = max(len(x) for x in labels) + 2
    print(f"{'operation':<{w}}{'A rust':>10}{'B bridge':>10}{'P python':>10}"
          f"{'B/P':>9}{'A/P':>9}")
    print("-" * (w + 48))

    db_started = False
    for k in labels:
        if k.startswith("db.") and not db_started:
            db_started = True
            print(f"{'--- DB edge (not shared; calibrated) ---':<{w}}")
        va, vb, vp = a.get(k), b.get(k), p.get(k)
        print(f"{k:<{w}}{fmt(va):>10}{fmt(vb):>10}{fmt(vp):>10}"
              f"{ratio(vb, vp):>9}{ratio(va, vp):>9}")

    plat = [k for k in labels if not k.startswith("db.")]
    tax = [b[k] - p[k] for k in plat if k in b and k in p]
    if tax:
        print(f"\nPyO3 crossing (B - P), platform edge only:")
        print(f"  mean  {sum(tax) / len(tax):9.1f} ns/call over {len(tax)} ops")
        print(f"  max   {max(tax):9.1f} ns/call")
        print("  All of this is transport: B and P run the same mock code.")


if __name__ == "__main__":
    main()
