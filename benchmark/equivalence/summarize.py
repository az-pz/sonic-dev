#!/usr/bin/env python3
"""Summarise timing JSONL emitted by the trace/bench runners.

Reports medians rather than means: sweep durations are long-tailed, and a mean hides
exactly the tail that matters operationally. With --by-ports it also reports per-port
cost, which is what separates fixed overhead from per-port work in a fan-out sweep.
"""
import argparse, json, statistics as st

NAMES = {"a": "A rust (native edge)", "b": "B rust (real bridge)", "p": "P python daemon"}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("jsonl")
    ap.add_argument("--by-ports", action="store_true")
    a = ap.parse_args()
    rows = [json.loads(l) for l in open(a.jsonl) if l.strip()]
    if not rows:
        raise SystemExit(f"{a.jsonl}: empty")

    if not a.by_ports:
        print(f"\n{'config':<24}{'n':>3}{'median ms':>11}{'min':>9}{'max':>9}{'stdev':>8}")
        med = {}
        for c in "abp":
            v = [r["p50_ns"] / 1e6 for r in rows if r["config"] == c]
            if not v:
                continue
            med[c] = st.median(v)
            sd = st.stdev(v) if len(v) > 1 else 0.0
            print(f"{NAMES[c]:<24}{len(v):>3}{med[c]:>11.2f}{min(v):>9.2f}{max(v):>9.2f}{sd:>8.2f}")
        if "a" in med and "b" in med:
            print(f"\nB - A = {med['b']-med['a']:6.2f} ms   PyO3 crossing at daemon level")
        if "p" in med and "b" in med:
            print(f"P / B = {med['p']/med['b']:6.2f}x   deployed Rust vs the Python reference")
        return

    ns = sorted({r["ports"] for r in rows})
    print(f"\n{'N':>4}" + "".join(f"{NAMES[c][:14]:>16}" for c in "abp"))
    print(f"{'':>4}" + "".join(f"{'ms (us/port)':>16}" for _ in "abp"))
    for n in ns:
        line = f"{n:>4}"
        for c in "abp":
            v = [r["p50_ns"] / 1e6 for r in rows if r["config"] == c and r["ports"] == n]
            line += f"{st.median(v):>9.2f} ({st.median(v)*1000/n:5.0f})" if v else f"{'-':>16}"
        print(line)
    # Slope separates per-port work from fixed overhead; a scenario whose cost is mostly
    # intercept is not telling you anything about per-port efficiency.
    print("\nslope (us/port, from the two largest N):")
    for c in "abp":
        pts = [(n, st.median([r["p50_ns"] / 1e6 for r in rows if r["config"] == c and r["ports"] == n]))
               for n in ns if any(r["config"] == c and r["ports"] == n for r in rows)]
        if len(pts) >= 2:
            (n0, t0), (n1, t1) = pts[-2], pts[-1]
            print(f"  {NAMES[c]:<24}{(t1-t0)*1000/(n1-n0):8.1f}   intercept {t1 - (t1-t0)/(n1-n0)*n1:7.2f} ms")


if __name__ == "__main__":
    main()
