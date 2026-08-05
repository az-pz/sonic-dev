//! xcvrd-rs daemon — M1 production entry (presence, identity & liveness).
//!
//! Builds the real daemon over the mockable trait seams ([`Hal`] + [`XcvrTableHelper`]
//! over [`crate::db::DbTable`]) and runs the presence/identity engine
//! ([`SfpStateUpdateTask`]) — the same code path the Part-B unit tests exercise under
//! mocks. Mirrors the reference `DaemonXcvrd.init` → `SfpStateUpdateTask` boot:
//!   1. Build the logical→physical port map from CONFIG_DB (`get_port_mapping`).
//!   2. Purge stale `TRANSCEIVER_INFO` for absent ports (`remove_stale_transceiver_info`).
//!   3. `SfpStateUpdateTask::init` — publish `TRANSCEIVER_INFO` + seed
//!      `TRANSCEIVER_STATUS_SW` (`status`/`error`/`cmis_state=READY`).
//!   4. `SfpStateUpdateTask::task_worker` — react to plug/unplug/error change events,
//!      retrying a failed identity read on the 60 s cadence.
//!
//! CMIS strings are NUL-padded fixed-width; values are written trimmed of trailing
//! NULs (see [`crate::xcvrd::stringify`]), matching the reference xcvrd whose outputs
//! the e2e suite reads with NULs stripped.

use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::cmis::cmis_api::{BridgeCmisApi, CmisApi};
use crate::cmis::cmis_manager_task::{CmisApiFactory, CmisManagerTask};
use crate::dom::dom_mgr::DomInfoUpdateTask;
use crate::env;
use crate::hal::{BridgeHal, Hal};
use crate::sff_mgr::{BridgeSffApi, SffApi, SffApiFactory, SffManagerTask};
use crate::xcvrd::remove_stale_transceiver_info;
use crate::xcvrd::deinit_transceiver_tables;
use crate::xcvrd::sfp_state_update::SfpStateUpdateTask;
use crate::xcvrd_utilities::common::is_fast_reboot_enabled;
use crate::xcvrd_utilities::port_event_helper::get_port_mapping;
use crate::xcvrd_utilities::{media_settings_parser, optics_si_parser};
use crate::xcvrd_utilities::xcvr_table_helper::XcvrTableHelper;

/// Set by the SIGTERM/SIGINT handler so the daemon can shut down gracefully (run `deinit`)
/// instead of being SIGKILLed. The reference `DaemonXcvrd` installs signal handlers that
/// set its stop event and then calls `deinit`; we mirror that here.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe handler: only touches an `AtomicBool` (no allocation / no locks).
extern "C" fn handle_shutdown_signal(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Install the SIGTERM/SIGINT handler (idempotent). `supervisorctl stop xcvrd` sends
/// SIGTERM; a graceful shutdown must run `deinit` (which clears TRANSCEIVER_STATUS on a
/// normal shutdown) before the supervisor's stopwaitsecs elapses and SIGKILL lands.
fn install_signal_handlers() {
    // SAFETY: `handle_shutdown_signal` is async-signal-safe (atomic store only). The fn
    // item is coerced to an `extern "C"` fn pointer, then to `sighandler_t` (a usize).
    unsafe {
        let handler = handle_shutdown_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

/// Entry point: run the daemon forever. On any setup/serve error we log and retry
/// rather than exit, so the pmon supervisor keeps the daemon RUNNING (and the M0
/// deploy-smoke stays green) even if the emulator or Redis is briefly unavailable.
pub fn run() -> ! {
    eprintln!("xcvrd-rs: starting (M2: presence, identity, liveness & DOM monitoring)");
    loop {
        if let Err(e) = serve() {
            eprintln!("xcvrd-rs: serve error: {e}; retrying in 3s");
            std::thread::sleep(Duration::from_secs(3));
        }
    }
}

fn serve() -> Result<(), Box<dyn Error>> {
    // Single-ASIC testbed: one (empty) namespace.
    let namespaces = vec![String::new()];

    let hal: Arc<dyn Hal> = Arc::new(BridgeHal::new()?); // PyO3 -> sonic_platform -> xcvr-emu
    let table_helper = Arc::new(XcvrTableHelper::new(&namespaces)?); // swss-common -> STATE_DB
    let config = env::open_config_db()?; // swss-common -> CONFIG_DB

    let num_sfps = hal.num_sfps()?;
    let port_mapping = get_port_mapping(&config, num_sfps)?;
    eprintln!(
        "xcvrd-rs: {} configured ports discovered",
        port_mapping.logical_port_list().len()
    );

    // Cold-start correctness: purge TRANSCEIVER_INFO left in STATE_DB for a module that
    // was unplugged while xcvrd was down (STATE_DB survives the daemon).
    remove_stale_transceiver_info(hal.as_ref(), table_helper.as_ref(), &port_mapping);

    let stop = Arc::new(AtomicBool::new(false));
    let sfp_error_event = Arc::new(AtomicBool::new(false));

    // Graceful shutdown: a SIGTERM/SIGINT (e.g. `supervisorctl stop xcvrd`) sets a global
    // flag; a lightweight watcher propagates it to `stop` so every task loop exits and the
    // main thread runs `deinit` before the supervisor SIGKILLs us. Mirrors the reference
    // `DaemonXcvrd.signal_handler` -> stop_event -> `deinit()`.
    install_signal_handlers();
    let sig_stop = stop.clone();
    let sig_watcher = std::thread::Builder::new()
        .name("shutdown-watcher".to_string())
        .spawn(move || {
            while !sig_stop.load(Ordering::Relaxed) {
                if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                    sig_stop.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })?;

    // The DOM poll task shares the HAL / table-helper / port-map with the SFP task
    // (SfpStateUpdateTask::new consumes the originals, so clone first). `skip_cmis_mgr`
    // is now false: the CmisManagerTask below drives CMIS modules through datapath
    // bring-up, so DOM polling must gate itself while a port is still in CMIS init
    // (non-terminal `cmis_state`) and only flow once the module reaches READY.
    let dom_task = DomInfoUpdateTask::new(
        port_mapping.clone(),
        hal.clone(),
        table_helper.clone(),
        false,
        None,
    );

    // Load media_settings.json + optics_si_settings.json once at startup, skipped on a
    // fast-reboot (xcvrd.py:1056-1060) — the ASIC/module SI state survives a fast-reboot,
    // so re-publishing it would be redundant and could disrupt an in-service datapath.
    let (media_settings, optics_si_settings) =
        if is_fast_reboot_enabled(table_helper.get_fast_restart_enable_tbl(0)) {
            eprintln!(
                "xcvrd-rs: fast-reboot — skip loading media_settings.json / optics_si_settings.json"
            );
            (serde_json::json!({}), serde_json::json!({}))
        } else {
            (
                media_settings_parser::load_media_settings(),
                optics_si_parser::load_optics_si_settings(),
            )
        };

    // CMIS datapath bring-up state machine. Production wires each port's control/decode
    // surface to a `BridgeCmisApi` over the real SfpHandle; unit tests inject MockCmisApi.
    let cmis_factory: CmisApiFactory =
        Box::new(|sfp| Some(Box::new(BridgeCmisApi::new(sfp)) as Box<dyn CmisApi>));
    let mut cmis_task = CmisManagerTask::new(
        namespaces.clone(),
        port_mapping.clone(),
        hal.clone(),
        table_helper.clone(),
        cmis_factory,
        false,
    );
    cmis_task.set_optics_si_settings(optics_si_settings);

    // Optional SFF (non-CMIS) deterministic link bring-up. Off by default; enabled via
    // the `--enable_sff_mgr` platform flag (xcvrd.py forwards it to the SffManagerTask).
    // Spawned before the CMIS manager, mirroring the reference thread start order
    // (xcvrd.py:1148). Production wires each port's SFF-8636 control surface to a
    // `BridgeSffApi` over the real SfpHandle; unit tests inject MockSffApi.
    let enable_sff_mgr = std::env::args().any(|a| a == "--enable_sff_mgr");
    let sff_handle = if enable_sff_mgr {
        eprintln!("xcvrd-rs: --enable_sff_mgr set; starting SffManagerTask");
        let sff_factory: SffApiFactory =
            Box::new(|sfp| Some(Box::new(BridgeSffApi::new(sfp)) as Box<dyn SffApi>));
        let sff_task = SffManagerTask::new(
            namespaces.clone(),
            hal.clone(),
            table_helper.clone(),
            sff_factory,
        );
        let sff_stop = stop.clone();
        Some(
            std::thread::Builder::new()
                .name("SffManagerTask".to_string())
                .spawn(move || {
                    // `run` wraps each bring-up sweep in catch_unwind so a per-port panic
                    // restarts the loop rather than tearing the daemon down.
                    sff_task.run(sff_stop);
                })?,
        )
    } else {
        None
    };

    let mut task = SfpStateUpdateTask::new(namespaces, port_mapping, hal, table_helper.clone());
    task.set_media_settings(media_settings);

    // Boot publish: INFO + STATUS_SW (status/error/cmis_state) + DOM thresholds.
    task.init(&stop);
    eprintln!("xcvrd-rs: initial sync complete; watching for change events");

    // No boot-prime DOM pass. The reference `DomInfoUpdateTask` delays its FIRST poll a
    // full interval (`next_periodic_db_update_time = loop_start + period`) and never
    // publishes the DOM/STATUS diagnostic tables at boot; `task_worker` below matches
    // that (`next = now + interval`). Priming a pass here would diverge from that cadence
    // and, critically, pre-populate the *latched* flag tables (TRANSCEIVER_DOM_FLAG /
    // TRANSCEIVER_STATUS_FLAG) at boot. An off-cadence boot publish makes a freshly-raised
    // latched flag observable before any link-change re-read, breaking the link-change
    // contract (a raised flag must surface only on the ~60s periodic pass or a genuine
    // flap re-read — see `on_port_update_event`). TRANSCEIVER_INFO / STATUS_SW / DOM
    // thresholds still appear promptly: they are published by `SfpStateUpdateTask::init`
    // above, not this loop.

    let dom_stop = stop.clone();
    let dom_handle = std::thread::Builder::new()
        .name("DomInfoUpdateTask".to_string())
        .spawn(move || {
            // Keep the DOM loop resilient: a panic in one pass restarts the loop rather
            // than tearing the daemon down (the supervisor must stay RUNNING).
            while !dom_stop.load(Ordering::Relaxed) {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    dom_task.task_worker(&dom_stop);
                }));
                if outcome.is_ok() {
                    break; // clean exit — stop was set
                }
                eprintln!("xcvrd-rs: DOM task panicked; restarting DOM monitoring loop");
            }
        })?;

    let cmis_stop = stop.clone();
    let cmis_handle = std::thread::Builder::new()
        .name("CmisManagerTask".to_string())
        .spawn(move || {
            // `run` seeds every port to cmis_state=UNKNOWN then drives the bring-up
            // sweeps, wrapping each pass in catch_unwind so a panic restarts the loop
            // rather than killing the daemon.
            cmis_task.run(cmis_stop);
        })?;

    // Presence/identity/error state machine — runs until a SIGTERM/SIGINT sets `stop`
    // (via the shutdown watcher) or a fatal system-not-ready timeout (STATE_EXIT).
    task.task_worker(&stop, &sfp_error_event);

    // Shutdown: stop every task, join, then run the graceful table teardown. On a NORMAL
    // shutdown this deletes TRANSCEIVER_STATUS/_STATUS_SW; a warm/fast reboot preserves them
    // (deinit_transceiver_tables gates on the reboot flags). Use the task's LIVE port
    // mapping (mutated by CONFIG_DB add/remove) so a port added/removed at runtime is
    // handled correctly.
    stop.store(true, Ordering::Relaxed);
    let _ = dom_handle.join();
    let _ = cmis_handle.join();
    if let Some(sff_handle) = sff_handle {
        let _ = sff_handle.join();
    }
    let _ = sig_watcher.join();
    deinit_transceiver_tables(table_helper.as_ref(), task.port_mapping());
    Ok(())
}
