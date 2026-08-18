//! Edge calibration -- measures what the *instrument* costs, per config.
//!
//! Timing the daemon tells you `T_measured = T_orchestration + k * C_edge`. The
//! equivalence gate already yields `k` exactly (it counts every call), so this
//! binary supplies `C_edge` and lets `analyze.py` report a corrected figure plus an
//! explicit error bar. If the correction turns out to be a large fraction of the
//! A/B delta, the result is not publishable -- same halt rule as read amplification.
//!
//!   config A: BenchHal        -- Rust-native plant, no Python in the process
//!   config B: BridgeHal       -- the real PyO3 bridge onto benchmark/pymocks
//!
//! Run:
//!   calibrate --fixture ../fixtures/cmis_40g_lr4.json --pymocks ../pymocks [--config a|b|both]

use std::time::Instant;

use xcvrd_bench::edges::{BenchHal, Fixture};
use xcvrd_rs::hal::{Hal, SfpHandle};

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// Time `f`, auto-scaling the iteration count so short ops still get a stable
/// reading and slow ones (config B is ~17x slower) do not stall the run.
fn bench<F: FnMut()>(label: &str, out: &mut Vec<(String, f64)>, mut f: F) {
    for _ in 0..2_000 {
        f();
    }
    let mut n = 2_000u32;
    loop {
        let t = Instant::now();
        for _ in 0..n {
            f();
        }
        let el = t.elapsed();
        if el.as_millis() >= 200 || n >= 2_000_000 {
            let ns = el.as_nanos() as f64 / n as f64;
            println!("  {label:<44} {ns:10.1} ns/call");
            out.push((label.to_string(), ns));
            return;
        }
        n *= 4;
    }
}

fn exercise(hal: &dyn Hal, tag: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    println!("config {tag}:");

    let sfp: Box<dyn SfpHandle> = hal.sfp(0).expect("sfp(0)");

    bench("num_sfps", &mut out, || {
        std::hint::black_box(hal.num_sfps().unwrap());
    });
    bench("sfp(0)  [handle construction]", &mut out, || {
        std::hint::black_box(hal.sfp(0).unwrap());
    });
    bench("get_presence  [scalar]", &mut out, || {
        std::hint::black_box(sfp.get_presence().unwrap());
    });
    bench("sfp_type  [string]", &mut out, || {
        std::hint::black_box(sfp.sfp_type().unwrap());
    });
    bench("get_transceiver_dom_real_value  [27 fields]", &mut out, || {
        std::hint::black_box(sfp.get_transceiver_dom_real_value().unwrap());
    });
    bench("get_transceiver_info  [33 fields]", &mut out, || {
        std::hint::black_box(sfp.get_transceiver_info().unwrap());
    });
    bench("get_transceiver_status  [7 fields]", &mut out, || {
        std::hint::black_box(sfp.get_transceiver_status().unwrap());
    });
    bench("call_json(get_transceiver_status_flags)", &mut out, || {
        std::hint::black_box(sfp.call_json("get_transceiver_status_flags").unwrap());
    });
    bench("read_eeprom(9, 1)", &mut out, || {
        std::hint::black_box(sfp.read_eeprom(9, 1).unwrap());
    });
    bench("get_change_event(0)", &mut out, || {
        std::hint::black_box(hal.get_change_event(0).unwrap());
    });
    out
}

fn to_map(rows: &[(String, f64)]) -> serde_json::Value {
    let m: serde_json::Map<String, serde_json::Value> = rows
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::from(*v)))
        .collect();
    serde_json::Value::Object(m)
}

fn main() {
    let fixture_path = arg("--fixture", "../fixtures/cmis_40g_lr4.json");
    let pymocks = arg("--pymocks", "../pymocks");
    let which = arg("--config", "both");
    let num_sfps: usize = arg("--num-sfps", "32").parse().expect("--num-sfps");

    // Set before ANY GIL acquire: pyo3's auto-initialize starts CPython lazily on the
    // first `Python::with_gil`, and sys.path is fixed at interpreter startup, so
    // exporting this later would silently import the real plugin (or fail).
    let abs = std::fs::canonicalize(&pymocks).unwrap_or_else(|e| panic!("--pymocks {pymocks}: {e}"));
    let fx_abs = std::fs::canonicalize(&fixture_path)
        .unwrap_or_else(|e| panic!("--fixture {fixture_path}: {e}"));
    std::env::set_var("PYTHONPATH", &abs);
    std::env::set_var("XCVRD_BENCH_FIXTURE", &fx_abs);
    std::env::set_var("XCVRD_BENCH_NUM_SFPS", num_sfps.to_string());
    // Tracing must stay off here: recording costs more than the calls being measured.
    std::env::set_var("XCVRD_BENCH_TRACE", "0");

    println!("fixture : {}", fx_abs.display());
    println!("pymocks : {}", abs.display());
    println!("num_sfps: {num_sfps}\n");

    let fx = Fixture::from_path(fx_abs.to_str().unwrap()).expect("load fixture");

    let mut results = serde_json::Map::new();

    if which == "a" || which == "both" {
        let hal = BenchHal::new(fx.clone(), num_sfps);
        let a = exercise(&hal, "A  (BenchHal -- Rust-native, no Python)");
        results.insert("a".into(), to_map(&a));
        println!();
    }

    if which == "b" || which == "both" {
        match xcvrd_rs::hal::BridgeHal::new() {
            Ok(hal) => {
                let b = exercise(&hal, "B  (BridgeHal -- real PyO3 onto pymocks)");
                results.insert("b".into(), to_map(&b));
            }
            Err(e) => {
                // Loud, not fatal: config A's numbers are still valid on their own.
                eprintln!("config B unavailable: {e}");
                eprintln!("  (needs libpython + an importable sonic_platform on PYTHONPATH)");
            }
        }
    }

    // Per-op A->B ratio: this IS the PyO3 marshalling tax, per call, by operation.
    if let (Some(a), Some(b)) = (results.get("a"), results.get("b")) {
        println!("\nPyO3 tax (B / A), per operation:");
        let (a, b) = (a.as_object().unwrap(), b.as_object().unwrap());
        for (k, va) in a {
            if let (Some(va), Some(vb)) = (va.as_f64(), b.get(k).and_then(|v| v.as_f64())) {
                println!("  {k:<44} {:8.1}x   (+{:.0} ns)", vb / va, vb - va);
            }
        }
    }

    let out = arg("--out", "");
    if !out.is_empty() {
        std::fs::write(&out, serde_json::to_string_pretty(&results).unwrap())
            .unwrap_or_else(|e| panic!("--out {out}: {e}"));
        println!("\nwrote {out}");
    }
}
