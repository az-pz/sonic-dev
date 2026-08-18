#!/usr/bin/env python3
"""Summarise DUT benchmark records (one JSON object per line, from xbench.py).

Groups by scenario and daemon variant and reports medians. Medians rather than
means because the DUT is a KVM guest: an occasional steal-inflated sample would drag
a mean around and be read as a daemon difference.
"""
import json, statistics as st, sys
from collections import defaultdict

def med(v):
    v = [x for x in v if x is not None]
    return st.median(v) if v else None

def main(path):
    rows = [json.loads(l) for l in open(path) if l.strip()]
    by = defaultdict(list)
    for r in rows:
        by[(r["scenario"], r["variant"])].append(r["result"])
    scen = sorted({k[0] for k in by})
    for s in scen:
        print(f"\n=== {s} ===")
        variants = [v for (sc, v) in by if sc == s]
        keys = []
        for v in variants:
            for k, val in by[(s, v)][0].items():
                if isinstance(val, (int, float)) and k not in keys:
                    keys.append(k)
        print(f"{'metric':<22}" + "".join(f"{v:>14}" for v in variants) + f"{'ratio':>10}")
        for k in keys:
            vals = [med([r.get(k) for r in by[(s, v)]]) for v in variants]
            line = f"{k:<22}" + "".join(f"{x:>14.2f}" if isinstance(x, float)
                                        else f"{x:>14}" for x in vals)
            if len(vals) == 2 and all(isinstance(x, (int, float)) for x in vals) and vals[1]:
                line += f"{vals[0]/vals[1]:>10.2f}x"
            print(line)
        print(f"  (n = {', '.join(f'{v}:{len(by[(s,v)])}' for v in variants)})")

if __name__ == "__main__":
    main(sys.argv[1])
