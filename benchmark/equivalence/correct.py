#!/usr/bin/env python3
"""Subtract the harness's own edge cost from a daemon timing.

    T_measured = T_orchestration + sum_op( k_op * C_op )

`k_op` comes from a call trace (bin/trace.rs), `C_op` from the edge calibration
(bin/calibrate.rs). Both are required: a single scalar correction is provably wrong
here, because the edge bias INVERTS by operation -- the Rust edge is ~13x faster than
Python's on scalars and ~11x slower on payload getters -- so an unweighted mean is
wrong in both directions at once.

Measured example (32 ports, 1 poll, 736 HAL calls):
    unweighted mean over 10 ops -> predicted B-A = 2.05 ms   (77% high)
    per-op weighted by k_op     -> predicted B-A = 0.81 ms
    observed                                       1.16 ms

    correct.py --trace results/traces/a.jsonl \
               --calib results/edges/rust.json --config a \
               --measured-ms 35.51
"""

import argparse
import collections
import json
import sys

# Trace op name -> calibration label. call_json:<method> falls back to the calibrated
# status_flags variant; that is an approximation, and the residual it leaves is the
# main reason the prediction above lands ~30% low. Calibrating each call_json variant
# separately would close it.
OPMAP = {
    "num_sfps": "num_sfps",
    "sfp": "sfp(0)  [handle construction]",
    "get_presence": "get_presence  [scalar]",
    "sfp_type": "sfp_type  [string]",
    "get_transceiver_dom_real_value": "get_transceiver_dom_real_value  [27 fields]",
    "get_transceiver_info": "get_transceiver_info  [33 fields]",
    "get_transceiver_status": "get_transceiver_status  [7 fields]",
    "get_change_event": "get_change_event(0)",
    "read_eeprom": "read_eeprom(9, 1)",
}
CALL_JSON_FALLBACK = "call_json(get_transceiver_status_flags)"


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--trace", required=True, help="JSONL trace supplying k_op")
    ap.add_argument("--calib", required=True, help="calibration JSON supplying C_op")
    ap.add_argument("--config", default="a", choices=["a", "b"])
    ap.add_argument("--measured-ms", type=float, required=True)
    ap.add_argument("--polls", type=int, default=1,
                    help="polls the trace covers, if it recorded more than one")
    args = ap.parse_args()

    k = collections.Counter()
    for line in open(args.trace):
        line = line.strip()
        if not line:
            continue
        r = json.loads(line)
        if r.get("kind") == "hal":
            k[r["op"]] += 1
        elif r.get("kind", "").startswith("eeprom"):
            k["read_eeprom"] += 1

    calib = json.load(open(args.calib))
    if args.config not in calib:
        sys.exit(f"{args.calib}: no '{args.config}' section (got {sorted(calib)})")
    C = calib[args.config]

    total_ns = 0.0
    unmapped = []
    print(f"{'op':<46}{'k':>7}{'C ns':>10}{'k*C ms':>10}")
    for op, n in k.most_common():
        label = OPMAP.get(op, CALL_JSON_FALLBACK if op.startswith("call_json") else None)
        c = C.get(label) if label else None
        if c is None:
            unmapped.append(op)
            continue
        total_ns += n * c
        print(f"{op:<46}{n:>7}{c:>10.0f}{n * c / 1e6:>10.3f}")

    edge_ms = total_ns / 1e6 / max(args.polls, 1)
    corrected = args.measured_ms - edge_ms
    print(f"\n  calls            {sum(k.values())}")
    print(f"  measured         {args.measured_ms:8.2f} ms")
    print(f"  edge cost        {edge_ms:8.2f} ms   ({edge_ms / args.measured_ms * 100:.2f}% of measured)")
    print(f"  CORRECTED        {corrected:8.2f} ms")

    if unmapped:
        print(f"\n  WARNING: {len(unmapped)} op(s) had no calibration and were skipped: "
              f"{sorted(set(unmapped))}")
        print("  The correction is therefore an UNDER-estimate.")

    # A correction that rivals the quantity being compared means the instrument is
    # not resolving the daemon. Better to say so than to publish the difference.
    if edge_ms > 0.25 * args.measured_ms:
        print("\n  CAUTION: edge cost exceeds 25% of the measurement. The corrected "
              "figure is a small difference of large numbers; treat it as indicative "
              "only, and prefer a scenario where the edge is a smaller share.")


if __name__ == "__main__":
    main()
