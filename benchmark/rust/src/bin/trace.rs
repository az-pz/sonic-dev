//! Scenario runner -- drives a real daemon task and records what it did.
//!
//! This is the validity gate's data source. Timing without it would be timing two
//! programs that may be doing different amounts of work, and the per-op edge costs
//! measured by `calibrate` are only correctable when weighted by the per-op call
//! counts `k_op` that this binary produces.
//!
//!   trace --config a|b --ports 32 --polls 1 [--fixture ...] [--pymocks ...]
//!         [--out trace.jsonl] [--dump-db db.json] [--time]
//!
//! Config A wires `BenchHal` (no Python in the process); config B wires the real
//! `BridgeHal` onto `benchmark/pymocks`. Both run the SAME daemon code and the SAME
//! `XcvrTableHelper::with_mock_tables`, so a diff between their traces is a defect in
//! the harness, not a property of either daemon -- which is exactly what makes
//! `compare.py a.jsonl b.jsonl` a meaningful self-check.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use xcvrd_bench::edges::{BenchHal, Fixture};
use xcvrd_bench::{CountingHal, Recorder};
use xcvrd_rs::dom::dom_mgr::DomInfoUpdateTask;
use xcvrd_rs::hal::Hal;
use xcvrd_rs::xcvrd_utilities::port_event_helper::{
    PortChangeEvent, PortChangeEventType, PortMapping,
};
use xcvrd_rs::xcvrd_utilities::xcvr_table_helper::XcvrTableHelper;

fn arg(name: &str, default: &str) -> String {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == name)
        .and_then(|i| a.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn flag(name: &str) -> bool {
    std::env::args().any(|x| x == name)
}

/// One logical port per physical port, `Ethernet<4*i>` -- the uniform T0 shape the
/// testbed presents when EMU_NO_SPECIAL=1. Special modules are deliberately excluded:
/// they have different work profiles and averaging across them would blend distributions.
fn build_port_mapping(ports: usize) -> PortMapping {
    let mut pm = PortMapping::new();
    for i in 0..ports {
        pm.handle_port_change_event(&PortChangeEvent::new(
            format!("Ethernet{}", i * 4),
            Some(i),
            0,
            PortChangeEventType::Add,
            "CONFIG_DB".to_string(),
            "PORT".to_string(),
        ));
    }
    pm
}

fn main() {
    let config = arg("--config", "a").to_lowercase();
    let ports: usize = arg("--ports", "32").parse().expect("--ports");
    let polls: usize = arg("--polls", "1").parse().expect("--polls");
    let fixture_path = arg("--fixture", "../fixtures/cmis_40g_lr4.json");
    let pymocks = arg("--pymocks", "../pymocks");
    let out = arg("--out", "");
    let dump_db = arg("--dump-db", "");
    let timing = flag("--time");

    let fx_abs = std::fs::canonicalize(&fixture_path)
        .unwrap_or_else(|e| panic!("--fixture {fixture_path}: {e}"));

    // Must precede any GIL acquire: sys.path is fixed at interpreter startup, and
    // pyo3 auto-initialize starts CPython lazily on the first Python::with_gil.
    if config == "b" {
        let pm_abs =
            std::fs::canonicalize(&pymocks).unwrap_or_else(|e| panic!("--pymocks {pymocks}: {e}"));
        std::env::set_var("PYTHONPATH", &pm_abs);
        std::env::set_var("XCVRD_BENCH_FIXTURE", &fx_abs);
        std::env::set_var("XCVRD_BENCH_NUM_SFPS", ports.to_string());
        // The Rust decorator already records every call; letting the Python mock record
        // too would double-count and would tax the very path being timed.
        std::env::set_var("XCVRD_BENCH_TRACE", "0");
    }

    let rec = Recorder::new();
    let inner: Arc<dyn Hal> = match config.as_str() {
        "a" => Arc::new(BenchHal::new(
            Fixture::from_path(fx_abs.to_str().unwrap()).expect("load fixture"),
            ports,
        )),
        "b" => Arc::new(xcvrd_rs::hal::BridgeHal::new().expect(
            "BridgeHal::new failed -- needs libpython and an importable sonic_platform",
        )),
        other => panic!("--config must be 'a' or 'b', got {other:?}"),
    };
    // Tracing costs far more than the calls it records, so it is skipped entirely for
    // timing runs. Recording and timing in the same pass would measure the recorder.
    let hal: Arc<dyn Hal> = if timing {
        inner
    } else {
        Arc::new(CountingHal::new(inner, rec.clone()))
    };

    // XcvrTableHelper::with_mock_tables is #[cfg(test)] and build() is private, so an
    // external crate cannot inject in-memory tables. Rather than modify the immutable
    // target, both configs use the REAL swss-common path against a throwaway Redis
    // (env::redis_sock honours $REDIS_SOCK, so no /var/run access is needed). That is
    // strictly more faithful than a mock table -- it is the same C++ library the
    // daemon ships against -- and it keeps the DB edge identical between A and B, so
    // any A-vs-B difference remains attributable to the platform edge alone.
    let table_helper = Arc::new(
        XcvrTableHelper::new(&["".to_string()]).expect(
            "XcvrTableHelper::new failed -- start Redis first (tools/run_trace.sh does this) \
             and point $REDIS_SOCK at its unix socket",
        ),
    );
    let task = DomInfoUpdateTask::new(
        build_port_mapping(ports),
        hal,
        table_helper.clone(),
        false,
        Some(0),
    );

    let stop = AtomicBool::new(false);

    // Reproduce CMIS steady state. poll_port defers on any port whose
    // TRANSCEIVER_STATUS_SW.cmis_state is not terminal (dom_mgr.rs:312-325), and an
    // absent field reads as UNKNOWN -- so against an empty STATE_DB the daemon
    // correctly skips every port and the pass measures nothing. Production reaches
    // READY via CmisManagerTask, which this harness does not run, so seed the same
    // end state rather than setting skip_cmis_mgr and taking a different code path.
    if !flag("--no-seed") {
        use xcvrd_rs::db::DbTable;
        use xcvrd_rs::xcvrd_utilities::common::CMIS_STATE_READY;
        let tbl = table_helper.get_status_sw_tbl(0);
        for i in 0..ports {
            tbl.hset(&format!("Ethernet{}", i * 4), "cmis_state", CMIS_STATE_READY);
        }
    }

    if timing {
        // Discard the first pass: it populates empty rows and takes a different path
        // through the posters than every steady-state pass after it.
        task.poll_once(&stop);
        let mut samples = Vec::with_capacity(polls);
        for _ in 0..polls {
            let t = Instant::now();
            task.poll_once(&stop);
            samples.push(t.elapsed().as_nanos() as f64);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| samples[((samples.len() as f64 - 1.0) * p) as usize];
        // Percentiles, not a mean: poll durations are long-tailed and a mean would
        // hide exactly the tail that matters.
        println!(
            "{{\"config\":\"{config}\",\"ports\":{ports},\"polls\":{polls},\
             \"p50_ns\":{:.0},\"p95_ns\":{:.0},\"max_ns\":{:.0},\"p50_ns_per_port\":{:.0}}}",
            pct(0.50),
            pct(0.95),
            samples[samples.len() - 1],
            pct(0.50) / ports as f64
        );
    } else {
        for _ in 0..polls {
            task.poll_once(&stop);
        }
        let jsonl = rec.to_jsonl();
        if out.is_empty() {
            println!("{jsonl}");
        } else {
            std::fs::write(&out, &jsonl).unwrap_or_else(|e| panic!("--out {out}: {e}"));
            eprintln!(
                "wrote {out} ({} records)",
                jsonl.lines().filter(|l| !l.is_empty()).count()
            );
        }
    }

    // End-state snapshot. The DB seam cannot be decorated (XcvrTableHelper owns its
    // tables and build() is private), so DB-side equivalence is checked as final state
    // rather than as an op stream. Weaker than a call trace, but it still catches a
    // daemon that wrote different rows, different fields, or different values.
    if !dump_db.is_empty() {
        use xcvrd_rs::db::DbTable;
        let mut snap = serde_json::Map::new();
        let tables: Vec<(&str, &dyn DbTable)> = vec![
            ("TRANSCEIVER_DOM_SENSOR", table_helper.get_dom_tbl(0)),
            ("TRANSCEIVER_STATUS", table_helper.get_status_tbl(0)),
            ("TRANSCEIVER_DOM_THRESHOLD", table_helper.get_dom_threshold_tbl(0)),
            ("TRANSCEIVER_DOM_FLAG", table_helper.get_dom_flag_tbl(0)),
            ("TRANSCEIVER_STATUS_FLAG", table_helper.get_status_flag_tbl(0)),
            ("TRANSCEIVER_PM", table_helper.get_pm_tbl(0)),
        ];
        for (name, t) in tables {
            let mut rows = serde_json::Map::new();
            for k in t.get_keys() {
                let fvs = t.get(&k).unwrap_or_default();
                rows.insert(
                    k,
                    serde_json::Value::Object(
                        fvs.into_iter()
                            .map(|(f, v)| (f, serde_json::Value::String(v)))
                            .collect(),
                    ),
                );
            }
            snap.insert(name.to_string(), serde_json::Value::Object(rows));
        }
        std::fs::write(&dump_db, serde_json::to_string_pretty(&snap).unwrap())
            .unwrap_or_else(|e| panic!("--dump-db {dump_db}: {e}"));
        eprintln!("wrote {dump_db}");
    }
}
