#!/usr/bin/env python3
"""Config P -- drive the REAL Python xcvrd DOM sweep. The counterpart of bin/trace.rs.

Runs the genuine `xcvrd.dom.dom_mgr.DomInfoUpdateTask` against:
  * the same pymocks transceiver plant configs A and B use, and
  * the same real swsscommon + the same Redis instance the Rust configs write to.

Both halves matter. A DOM sweep is ~97% DB I/O, so pairing Python with an in-memory
table would hand it a win having nothing to do with the daemon; and using a different
mock plant would reintroduce the MagicMock bias this harness exists to remove.

WHY THE SWEEP IS RE-EXPRESSED HERE. The Rust translation exposes `poll_once`; the
Python reference has no equivalent -- its sweep is inline in `task_worker`, wrapped in
a `while` loop, a port-change subscription, and `check_port_update` interleaving that
all require live APPL_DB notifications. Driving `task_worker` directly would therefore
measure the select loop rather than the sweep. So the per-port body below mirrors
dom_mgr.py:325-400 call for call, and -- crucially -- it is not trusted: run it under
--trace and compare against the Rust trace with equivalence/compare.py. If this driver
drifts from the daemon, the gate says so.

    bench_dom.py --ports 32 --polls 1 --trace out.jsonl
    bench_dom.py --ports 32 --polls 100 --time
"""

import argparse
import json
import os
import sys
import threading
import time


def _pin_db_transport_to_unix_socket():
    """Force the Python daemon onto the same Redis transport the Rust daemon uses.

    The Rust side connects with DbConnector::new_unix (env.rs:48). The Python side goes
    through sonic_py_common.daemon_base.db_connect, which calls
    DBConnector(db_name, timeout, True, namespace) -- and in this swsscommon build that
    third argument is isTcpConn, so it selects TCP, not the unix socket.

    Left alone that would compare a TCP client against a unix-socket client. Since DB
    I/O is ~97% of a DOM sweep, the transport difference would dominate the result and
    be mistaken for a language difference. Only the connection helper is redirected --
    dom_mgr, the db_utils and every posting path run unmodified.
    """
    from sonic_py_common import daemon_base
    from swsscommon import swsscommon

    def db_connect(db_name, namespace=""):
        return swsscommon.DBConnector(db_name, 0, False, namespace)

    daemon_base.db_connect = db_connect
    # xcvr_table_helper imported the name directly, so rebinding the module attribute
    # alone would not reach it.
    try:
        from xcvrd.xcvrd_utilities import xcvr_table_helper
        if hasattr(xcvr_table_helper, "daemon_base"):
            xcvr_table_helper.daemon_base.db_connect = db_connect
    except ImportError:
        pass


def build(ports, fixture, pymocks):
    os.environ.setdefault("XCVRD_BENCH_FIXTURE", fixture)
    os.environ["XCVRD_BENCH_NUM_SFPS"] = str(ports)
    for p in (pymocks,):
        if p not in sys.path:
            sys.path.insert(0, p)

    # Import the package entry point first: dom_mgr does `from xcvrd import xcvrd`
    # while xcvrd.xcvrd pulls DomThermalInfoUpdateTask back out of dom_mgr, so
    # importing dom_mgr first trips the cycle.
    from xcvrd import xcvrd as _xcvrd  # noqa: F401
    from xcvrd.dom.dom_mgr import DomInfoUpdateTask
    from xcvrd.xcvrd_utilities import common
    from xcvrd.xcvrd_utilities.port_event_helper import PortChangeEvent, PortMapping

    from sonic_platform.platform import Platform

    _pin_db_transport_to_unix_socket()

    chassis = Platform().get_chassis()
    # The reference tests patch this global; the daemon's _wrapper_get_presence reads
    # it directly (common.py:126).
    common.platform_chassis = chassis

    port_mapping = PortMapping()
    for i in range(ports):
        port_mapping.handle_port_change_event(
            PortChangeEvent("Ethernet{}".format(i * 4), i, 0, PortChangeEvent.PORT_ADD)
        )

    sfp_obj_dict = {i: chassis.get_sfp(i) for i in range(ports)}
    task = DomInfoUpdateTask(
        [""], port_mapping, sfp_obj_dict, threading.Event(), False, 0
    )
    return task, port_mapping, common


def seed_cmis_ready(task, ports):
    """poll_port/`is_port_dom_monitoring_disabled` defers on any port whose
    TRANSCEIVER_STATUS_SW.cmis_state is not terminal, and an absent field reads as
    UNKNOWN -- so against a fresh STATE_DB the daemon correctly skips every port and
    the pass measures nothing. Production reaches READY via CmisManagerTask, which
    this harness does not run. Seeding the end state keeps the code path identical;
    setting skip_cmis_mgr would not."""
    from swsscommon import swsscommon

    tbl = task.xcvr_table_helper.get_status_sw_tbl(0)
    for i in range(ports):
        tbl.set(
            "Ethernet{}".format(i * 4),
            swsscommon.FieldValuePairs([("cmis_state", "READY")]),
        )


def sweep(task, port_mapping, common):
    """One DOM pass. Mirrors dom_mgr.py:325-400, minus the check_port_update
    interleave (which needs live APPL_DB notifications and is absent from the Rust
    poll_once being compared against)."""
    from xcvrd.xcvrd_utilities import sfp_status_helper

    helper = task.xcvr_table_helper
    for physical_port, logical_ports in port_mapping.physical_to_logical.items():
        logical_port_name = logical_ports[0]
        if task.is_port_dom_monitoring_disabled(logical_port_name):
            continue
        asic_index = port_mapping.get_asic_id_for_logical_port(logical_port_name)
        if asic_index is None:
            continue
        if sfp_status_helper.detect_port_in_error_status(
            logical_port_name, helper.get_status_sw_tbl(asic_index)
        ):
            continue
        if not common._wrapper_get_presence(physical_port):
            continue

        task.post_port_sfp_firmware_info_to_db(
            logical_port_name,
            port_mapping,
            helper.get_firmware_info_tbl(asic_index),
            task.task_stopping_event,
        )
        task.dom_db_utils.post_port_dom_sensor_info_to_db(logical_port_name)
        task.dom_db_utils.post_port_dom_flags_to_db(logical_port_name)
        task.status_db_utils.post_port_transceiver_hw_status_to_db(logical_port_name)
        task.status_db_utils.post_port_transceiver_hw_status_flags_to_db(logical_port_name)
        # Fixture reports vdm_supported=false, so the VDM branch is skipped -- but the
        # probe itself is a real HAL call the Rust trace also makes, so it must run.
        task.vdm_utils.is_transceiver_vdm_supported(physical_port)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ports", type=int, default=32)
    ap.add_argument("--polls", type=int, default=1)
    ap.add_argument("--fixture", required=True)
    ap.add_argument("--pymocks", required=True)
    ap.add_argument("--trace", default="", help="write a JSONL call trace here")
    ap.add_argument("--time", action="store_true")
    args = ap.parse_args()

    # Recording costs more than the calls it records, so tracing and timing are never
    # done in the same pass.
    os.environ["XCVRD_BENCH_TRACE"] = "0" if args.time else "1"

    task, port_mapping, common = build(
        args.ports, os.path.abspath(args.fixture), os.path.abspath(args.pymocks)
    )
    seed_cmis_ready(task, args.ports)

    from sonic_platform._bench import RECORDER

    if args.time:
        sweep(task, port_mapping, common)  # discard: first pass populates empty rows
        samples = []
        for _ in range(args.polls):
            t0 = time.perf_counter_ns()
            sweep(task, port_mapping, common)
            samples.append(time.perf_counter_ns() - t0)
        samples.sort()
        pct = lambda p: samples[int((len(samples) - 1) * p)]
        print(json.dumps({
            "config": "p", "ports": args.ports, "polls": args.polls,
            "p50_ns": pct(0.50), "p95_ns": pct(0.95), "max_ns": samples[-1],
            "p50_ns_per_port": pct(0.50) / args.ports,
        }))
    else:
        RECORDER.clear()  # drop construction-time calls; only the sweep is compared
        for _ in range(args.polls):
            sweep(task, port_mapping, common)
        jsonl = RECORDER.to_jsonl()
        if args.trace:
            with open(args.trace, "w") as fh:
                fh.write(jsonl)
            sys.stderr.write("wrote {} ({} records)\n".format(
                args.trace, len([l for l in jsonl.splitlines() if l])))
        else:
            print(jsonl)


if __name__ == "__main__":
    main()
