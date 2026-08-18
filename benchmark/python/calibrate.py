#!/usr/bin/env python3
"""Edge calibration, Python side -- config P.

The counterpart of rust/src/bin/calibrate.rs. Same fixture, same operations, same
op labels, same JSON shape, so `compare_edges.py` can line the three configs up:

    A  Rust daemon -> BenchHal      (Rust-native plant, no Python in the process)
    B  Rust daemon -> BridgeHal     -> pymocks/sonic_platform   (the deployed path)
    P  Python daemon                -> pymocks/sonic_platform   (this file)

B and P call literally the same mock, so any B-vs-P difference on the platform edge
is the PyO3 crossing and nothing else. A is the no-Python floor.

The DB edge is NOT shared: `XcvrTableHelper::build` is private so Rust is forced onto
its own MockDbTable, and the real swsscommon Python bindings are absent here. Those
rows are measured so the asymmetry is a known quantity rather than a hidden one.

Run via tools/run_calibrate_py.sh so this executes in the same container (and thus
the same CPython) that config B embeds -- calibrating against the host interpreter
would compare two different Pythons.
"""

import argparse
import json
import os
import sys
import time


def bench(label, fn, out):
    """Auto-scaling timer, mirroring the Rust harness: warm up, then grow the
    iteration count until the sample spans >=200ms so short ops still read stably."""
    for _ in range(2000):
        fn()
    n = 2000
    while True:
        t0 = time.perf_counter_ns()
        for _ in range(n):
            fn()
        el = time.perf_counter_ns() - t0
        if el >= 200_000_000 or n >= 2_000_000:
            ns = el / n
            print(f"  {label:<44} {ns:10.1f} ns/call")
            out[label] = ns
            return
        n *= 4


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fixture", required=True)
    ap.add_argument("--pymocks", required=True)
    ap.add_argument("--num-sfps", type=int, default=32)
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    fixture = os.path.abspath(args.fixture)
    pymocks = os.path.abspath(args.pymocks)

    os.environ["XCVRD_BENCH_FIXTURE"] = fixture
    os.environ["XCVRD_BENCH_NUM_SFPS"] = str(args.num_sfps)
    # Recording costs more than the calls being measured; must stay off here.
    os.environ["XCVRD_BENCH_TRACE"] = "0"
    sys.path.insert(0, pymocks)

    from sonic_platform.platform import Platform
    from mock_swsscommon import Table

    print(f"fixture : {fixture}")
    print(f"pymocks : {pymocks}")
    print(f"num_sfps: {args.num_sfps}")
    print(f"python  : {sys.version.split()[0]}\n")

    chassis = Platform().get_chassis()
    sfp = chassis.get_sfp(0)
    out = {}

    print("config P  (Python daemon -> pymocks, no bridge):")
    bench("num_sfps", chassis.get_num_sfps, out)
    bench("sfp(0)  [handle construction]", lambda: chassis.get_sfp(0), out)
    bench("get_presence  [scalar]", sfp.get_presence, out)
    # Attribute read, not a call: the bridge reads sfp_type via getattr
    # (platform-bridge/src/lib.rs:204), so this measures the same thing B does.
    bench("sfp_type  [string]", lambda: sfp.sfp_type, out)
    bench("get_transceiver_dom_real_value  [27 fields]",
          sfp.get_transceiver_dom_real_value, out)
    bench("get_transceiver_info  [33 fields]", sfp.get_transceiver_info, out)
    bench("get_transceiver_status  [7 fields]", sfp.get_transceiver_status, out)
    bench("call_json(get_transceiver_status_flags)",
          sfp.get_transceiver_status_flags, out)
    bench("read_eeprom(9, 1)", lambda: sfp.read_eeprom(9, 1), out)
    bench("get_change_event(0)", lambda: chassis.get_change_event(0), out)

    print("\nDB edge (mock_swsscommon.Table -- NOT shared with Rust):")
    tbl = Table(None, "TRANSCEIVER_DOM_SENSOR")
    dom = sfp.get_transceiver_dom_real_value()
    fvs = [(k, str(v)) for k, v in dom.items()]
    tbl.set("Ethernet0", fvs)
    bench("db.set  [27 fields]", lambda: tbl.set("Ethernet0", fvs), out)
    bench("db.get", lambda: tbl.get("Ethernet0"), out)
    bench("db.hget", lambda: tbl.hget("Ethernet0", "temperature"), out)
    bench("db.hset", lambda: tbl.hset("Ethernet0", "temperature", "45.0"), out)
    bench("db.get_size_for_key", lambda: tbl.get_size_for_key("Ethernet0"), out)
    bench("db.getKeys", tbl.getKeys, out)

    if args.out:
        with open(args.out, "w") as fh:
            json.dump({"p": out}, fh, indent=2)
        print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
