//! Port of `dom/dom_mgr.py` — `DomInfoUpdateTask` (the periodic ~60s DOM poll that
//! republishes `TRANSCEIVER_DOM_SENSOR`) and `DomThermalInfoUpdateTask` (the
//! optional fast temperature poll that republishes `TRANSCEIVER_DOM_TEMPERATURE`).
//! Each Python `threading.Thread` becomes a struct with `run(self, stop)` spawned
//! via `std::thread::spawn`.
//!
//! M2 realizes the DOM sensor poll loop end-to-end: the `dom_update_interval`
//! cadence, the `dom_polling` CONFIG_DB enable/disable toggle
//! (`get_dom_polling_from_config_db` / `is_port_dom_monitoring_disabled`), the CMIS
//! bring-up gate (`is_port_in_cmis_initialization_process`), the blocking-error gate
//! (`detect_port_in_error_status`), and the per-port posts via [`DomDbUtils`] /
//! [`StatusDbUtils`]: `TRANSCEIVER_DOM_SENSOR`, `TRANSCEIVER_DOM_FLAG` (+ its
//! change-count / set-time / clear-time metadata), `TRANSCEIVER_STATUS`, and
//! `TRANSCEIVER_STATUS_FLAG` (+ metadata). `TRANSCEIVER_DOM_THRESHOLD` is posted at
//! insert by [`crate::xcvrd::sfp_state_update::SfpStateUpdateTask`], not by this
//! loop. M4 adds the APPL_DB `PORT_TABLE` link-change fast-flag refresh: a
//! `flap_count` bump schedules `update_port_db_diagnostics_on_link_change` ~1s later
//! (off the ~60s DOM cadence), re-reading ONLY the flag tables for that port. M5 adds
//! the VDM real-value (basic+statistic merge under freeze), per-type VDM flag
//! (+metadata), `TRANSCEIVER_PM`, and `TRANSCEIVER_FIRMWARE_INFO` publishes off this
//! loop, and folds the per-type VDM flag tables into the link-change fast re-read.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::db::DbTable;
use crate::dom::utilities::db::{value_to_py_str, DbUtils, Fvs};
use crate::dom::utilities::dom_sensor::DomDbUtils;
use crate::dom::utilities::status::StatusDbUtils;
use crate::dom::utilities::vdm::{VdmDbUtils, VdmFlagTables, VdmUtils};
use crate::hal::{Hal, SfpHandle};
use crate::xcvrd_utilities::common::{
    self, CMIS_STATE_FAILED, CMIS_STATE_READY, CMIS_STATE_REMOVED,
};
use crate::xcvrd_utilities::port_event_helper::{
    PortChangeEvent, PortChangeEventType, PortMapping,
};
use crate::xcvrd_utilities::sfp_status_helper::detect_port_in_error_status;
use crate::xcvrd_utilities::utils::XcvrdUtils;
use crate::xcvrd_utilities::xcvr_table_helper::VDM_THRESHOLD_TYPES;

/// The `dom_polling` field value that disables DOM monitoring for a port.
const DOM_POLLING_DISABLED: &str = "disabled";
/// The default `dom_polling` value (field absent ⇒ enabled).
const DOM_POLLING_ENABLED: &str = "enabled";

/// `PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS` (`dom_mgr.py:36`) — the cap on how long
/// the fast inner loop naps between link-change checks, so a `flap_count` bump is
/// reacted to within ~1s regardless of the ~60s DOM cadence.
const PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS: u64 = 1000;

/// `get_dom_polling_from_config_db` (`dom_mgr.py:76`) — read the `dom_polling` field
/// off the CONFIG_DB `PORT` table for the first subport of `lport`'s breakout group.
/// Returns `"disabled"` iff the field is explicitly `disabled`, else `"enabled"`.
fn get_dom_polling_from_config_db(
    port_mapping: &PortMapping,
    cfg_port_tbl: &dyn DbTable,
    lport: &str,
) -> String {
    // Resolve the physical port, then its first logical (subport-0) port.
    let pport = match port_mapping.get_logical_to_physical(lport).and_then(|l| l.first().copied()) {
        Some(p) => p,
        None => return DOM_POLLING_ENABLED.to_string(),
    };
    let first_logical_port = match port_mapping
        .get_physical_to_logical(pport)
        .and_then(|l| l.first().cloned())
    {
        Some(f) => f,
        None => return DOM_POLLING_ENABLED.to_string(),
    };
    cfg_port_tbl
        .hget(&first_logical_port, "dom_polling")
        .unwrap_or_else(|| DOM_POLLING_ENABLED.to_string())
}

/// `common.CMIS_TERMINAL_STATES = {FAILED, READY, REMOVED}` membership test.
fn is_cmis_terminal_state(state: &str) -> bool {
    matches!(state, CMIS_STATE_FAILED | CMIS_STATE_READY | CMIS_STATE_REMOVED)
}

/// The DOM/hardware-status FLAG tables (each flag value table plus its change-count
/// / set-time / clear-time metadata) and the plain `TRANSCEIVER_STATUS` table — the
/// extra STATE_DB outputs the M2 DOM loop threads through the three flag/status
/// posters (`post_port_dom_flags_to_db`, `post_port_transceiver_hw_status_to_db`,
/// `post_port_transceiver_hw_status_flags_to_db`). Held as an `Option` on the task:
/// the daemon always wires them; gate-only unit tests may leave them unset.
#[derive(Clone)]
pub struct FlagStatusTables {
    pub dom_flag_tbl: Arc<dyn DbTable>,
    pub dom_flag_change_count_tbl: Arc<dyn DbTable>,
    pub dom_flag_set_time_tbl: Arc<dyn DbTable>,
    pub dom_flag_clear_time_tbl: Arc<dyn DbTable>,
    pub status_tbl: Arc<dyn DbTable>,
    pub status_flag_tbl: Arc<dyn DbTable>,
    pub status_flag_change_count_tbl: Arc<dyn DbTable>,
    pub status_flag_set_time_tbl: Arc<dyn DbTable>,
    pub status_flag_clear_time_tbl: Arc<dyn DbTable>,
}

/// The M5 VDM / PM / firmware STATE_DB outputs the DOM loop publishes each periodic
/// cycle (and, for VDM flags, re-reads on link change): the merged
/// `TRANSCEIVER_VDM_REAL_VALUE` value table, the per-type VDM flag value + metadata
/// tables ([`VdmFlagTables`]), `TRANSCEIVER_PM`, and `TRANSCEIVER_FIRMWARE_INFO`.
/// Held as an `Option` on the task — the daemon wires them; gate-only / non-VDM unit
/// tests leave them unset. (VDM *thresholds* are posted at insert by the
/// `SfpStateUpdateTask`, not here.)
#[derive(Clone)]
pub struct VdmPmFirmwareTables {
    pub vdm_real_value_tbl: Arc<dyn DbTable>,
    pub vdm_flag_tables: VdmFlagTables,
    pub pm_tbl: Arc<dyn DbTable>,
    pub firmware_info_tbl: Arc<dyn DbTable>,
}

/// `DomInfoUpdateTask` (`dom_mgr.py:141`).
pub struct DomInfoUpdateTask {
    pub port_mapping: PortMapping,
    pub skip_cmis_mgr: bool,
    /// Resolved DOM poll cadence in seconds (Python `dom_update_interval` with the
    /// `None` → default fallback already applied; `0` is honored as-is).
    pub dom_update_interval: u64,
    /// The transceiver plant (Python `sfp_obj_dict`), shared with the daemon.
    hal: Arc<dyn Hal>,
    /// `TRANSCEIVER_DOM_SENSOR` — the poll loop's output table.
    dom_tbl: Arc<dyn DbTable>,
    /// `TRANSCEIVER_STATUS_SW` — read for the blocking-error and CMIS-state gates.
    status_sw_tbl: Arc<dyn DbTable>,
    /// CONFIG_DB `PORT` — read live each cycle for the `dom_polling` toggle.
    cfg_port_tbl: Arc<dyn DbTable>,
    /// DOM/status flag + `TRANSCEIVER_STATUS` tables (M2 flag path). `None` in the
    /// gate-only unit tests that don't exercise the flag posters.
    flag_status_tables: Option<FlagStatusTables>,
    /// VDM real-value / VDM flag / PM / firmware tables (M5). `None` when unset
    /// (gate-only / pre-M5 unit tests); wired by the daemon.
    vdm_pm_fw: Option<VdmPmFirmwareTables>,
    /// APPL_DB `PORT_TABLE` (colon-separated), watched for `flap_count` bumps that
    /// trigger the M4 fast flag re-read. `None` in unit tests / when APPL_DB is
    /// unreachable (the fast re-read is then simply inactive).
    appl_port_tbl: Option<Arc<dyn DbTable>>,
    /// Last `flap_count` seen per logical port (empty string ⇒ field absent). A port
    /// absent from the map is being observed for the first time and is only seeded
    /// (no re-read), so only genuine post-boot flaps trigger a re-read.
    flap_last: Mutex<HashMap<String, String>>,
    /// Physical ports with a pending post-link-change flag re-read, each mapped to
    /// the [`Instant`] the re-read is due (`now + DIAG_DB_UPDATE_TIME_AFTER_LINK_
    /// CHANGE`). Mirrors `dom_mgr.py:link_change_affected_ports`; interior-mutable so
    /// `task_worker` can stay `&self` across the DOM thread.
    link_change_affected_ports: Mutex<HashMap<usize, Instant>>,
    /// Logical ports torn down by a CONFIG_DB logical-port DEL, shared with
    /// `SfpStateUpdateTask` (which maintains it) so this loop stops re-publishing a
    /// removed port's DOM/status/VDM/PM/firmware rows (this task iterates its OWN
    /// boot-time port-mapping clone and would otherwise resurrect them). `None` in the
    /// unit tests that drive `poll_once` directly.
    deconfigured_ports: Option<Arc<Mutex<BTreeSet<String>>>>,
    /// Absolute `Instant` at which the FIRST periodic DOM poll is due. The daemon sets
    /// this to `boot_instant + dom_update_interval` (see [`Self::set_first_periodic_from_boot`])
    /// so the synchronous boot identity pass + boot DOM prime that run on the MAIN thread
    /// BEFORE this thread spawns do not stack on top of the interval and delay the first
    /// latched `TRANSCEIVER_DOM_FLAG` baseline past the e2e DOM budget. `None` (unit tests
    /// / no daemon wiring) → `now + interval` at `task_worker` start (plain reference
    /// behaviour).
    first_periodic_deadline: Option<Instant>,
    /// Per-logical-port last-observed CMIS *terminal* state (`true` = terminal:
    /// READY/FAILED/REMOVED), tracked by the fast loop's [`Self::prime_flags_on_cmis_gate_release`]
    /// sweep to detect the non-terminal→terminal RISING edge — i.e. the moment the
    /// CMIS-init DOM gate (`is_port_in_cmis_initialization_process`) *releases* a port
    /// after a bring-up (including a mid-session replug). A port absent from the map is
    /// being observed for the first time and is only SEEDED (no publish), so a port that
    /// is already terminal at boot — whose latched flag baseline the boot
    /// `prime_flag_baselines` sweep already published — is never spuriously republished
    /// here. Interior-mutable so the sweep stays `&self` across the DOM thread.
    cmis_terminal_last: Mutex<HashMap<String, bool>>,
}

impl DomInfoUpdateTask {
    pub const DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS: u64 = 60;
    pub const DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE: u64 = 1;

    pub fn new(
        port_mapping: PortMapping,
        skip_cmis_mgr: bool,
        dom_update_interval: Option<u64>,
        hal: Arc<dyn Hal>,
        dom_tbl: Arc<dyn DbTable>,
        status_sw_tbl: Arc<dyn DbTable>,
        cfg_port_tbl: Arc<dyn DbTable>,
    ) -> Self {
        // Mirror dom_mgr.py: None (and, in Python, a negative value) falls back to
        // the default; an explicit value — including 0 — is honored.
        let dom_update_interval =
            dom_update_interval.unwrap_or(Self::DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS);
        DomInfoUpdateTask {
            port_mapping,
            skip_cmis_mgr,
            dom_update_interval,
            hal,
            dom_tbl,
            status_sw_tbl,
            cfg_port_tbl,
            flag_status_tables: None,
            vdm_pm_fw: None,
            appl_port_tbl: None,
            flap_last: Mutex::new(HashMap::new()),
            link_change_affected_ports: Mutex::new(HashMap::new()),
            deconfigured_ports: None,
            first_periodic_deadline: None,
            cmis_terminal_last: Mutex::new(HashMap::new()),
        }
    }

    /// Wire the DOM/status flag + `TRANSCEIVER_STATUS` tables the DOM loop publishes
    /// each cycle (the daemon calls this after construction; unit tests that only
    /// exercise the gates leave them unset).
    pub fn set_flag_status_tables(&mut self, tables: FlagStatusTables) {
        self.flag_status_tables = Some(tables);
    }

    /// Wire the M5 VDM real-value / VDM flag / PM / firmware tables the DOM loop
    /// publishes each periodic cycle (and the link-change VDM flag re-read). The
    /// daemon calls this after construction; pre-M5 / gate-only unit tests leave it
    /// unset (the VDM/PM/firmware block is then simply skipped).
    pub fn set_vdm_pm_firmware_tables(&mut self, tables: VdmPmFirmwareTables) {
        self.vdm_pm_fw = Some(tables);
    }

    /// Wire the APPL_DB `PORT_TABLE` view the DOM loop watches for `flap_count` bumps
    /// (M4 link-change fast flag re-read). The daemon calls this after construction;
    /// unit tests that drive `on_port_update_event` directly leave it unset.
    pub fn set_appl_port_table(&mut self, tbl: Arc<dyn DbTable>) {
        self.appl_port_tbl = Some(tbl);
    }

    /// Wire the cross-thread deconfigured-logical-port set maintained by
    /// `SfpStateUpdateTask` on CONFIG_DB logical-port DEL/ADD. While a port is in the
    /// set the DOM loop stops publishing its rows (and defensively purges any it may
    /// have raced in). Left unset by the unit tests that drive `poll_once` directly.
    pub fn set_deconfigured_ports(&mut self, set: Arc<Mutex<BTreeSet<String>>>) {
        self.deconfigured_ports = Some(set);
    }

    /// Anchor the FIRST periodic DOM poll at `boot_instant + dom_update_interval` rather
    /// than one interval after this thread happens to start. The reference schedules the
    /// first poll one interval after the `DomInfoUpdateTask` thread begins, and there that
    /// is ~boot (its daemon init is light). This Rust daemon runs a synchronous boot
    /// identity pass AND a boot DOM prime on the MAIN thread BEFORE spawning this thread,
    /// so anchoring on this thread's start would stack that boot latency ON TOP of the
    /// interval and delay the first `TRANSCEIVER_DOM_FLAG` baseline past the e2e DOM budget
    /// (`tests/test_dom_flag_meta.py`, which observes the baseline shortly after a fresh
    /// boot). Anchoring on the boot instant restores the reference's ~boot+interval timing.
    /// It does NOT publish a flag at boot: the deadline is still one full interval out (so
    /// the link-change guard window is unaffected), unless the boot pass itself already
    /// exceeded one interval, in which case the deadline is past and the first poll fires
    /// as this thread starts — still well after any early link-change test's flap re-read.
    pub fn set_first_periodic_from_boot(&mut self, boot_instant: Instant) {
        self.first_periodic_deadline =
            Some(boot_instant + Duration::from_secs(self.dom_update_interval));
    }

    /// True while `logical_port` is marked deconfigured (CONFIG_DB DEL not yet followed
    /// by a re-ADD). Always `false` when the set isn't wired (unit tests).
    fn is_deconfigured(&self, logical_port: &str) -> bool {
        match &self.deconfigured_ports {
            Some(set) => set
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(logical_port),
            None => false,
        }
    }

    /// Defensively delete every DOM-owned STATE_DB row for a deconfigured `logical_port`
    /// — `TRANSCEIVER_DOM_SENSOR`, the DOM/status flag (+metadata) and `TRANSCEIVER_
    /// STATUS` tables, and the VDM real-value / per-type VDM flag (+metadata) / PM /
    /// firmware tables. Closes the micro-race where a DOM pass already in flight when
    /// `SfpStateUpdateTask::on_remove_logical_port` deleted the tables re-posts a row
    /// afterwards. `TRANSCEIVER_STATUS_SW` is NOT owned by this loop (it belongs to the
    /// state task) and is left untouched here.
    fn purge_deconfigured_port_tables(&self, logical_port: &str) {
        let mut tbls: Vec<&dyn DbTable> = vec![&*self.dom_tbl];
        if let Some(ft) = &self.flag_status_tables {
            tbls.extend([
                &*ft.dom_flag_tbl,
                &*ft.dom_flag_change_count_tbl,
                &*ft.dom_flag_set_time_tbl,
                &*ft.dom_flag_clear_time_tbl,
                &*ft.status_tbl,
                &*ft.status_flag_tbl,
                &*ft.status_flag_change_count_tbl,
                &*ft.status_flag_set_time_tbl,
                &*ft.status_flag_clear_time_tbl,
            ]);
        }
        if let Some(vpf) = &self.vdm_pm_fw {
            tbls.push(&*vpf.vdm_real_value_tbl);
            for t in VDM_THRESHOLD_TYPES {
                if let Some(tbl) = vpf.vdm_flag_tables.flag.get(t) {
                    tbls.push(&**tbl);
                }
                if let Some(tbl) = vpf.vdm_flag_tables.change_count.get(t) {
                    tbls.push(&**tbl);
                }
                if let Some(tbl) = vpf.vdm_flag_tables.set_time.get(t) {
                    tbls.push(&**tbl);
                }
                if let Some(tbl) = vpf.vdm_flag_tables.clear_time.get(t) {
                    tbls.push(&**tbl);
                }
            }
            tbls.push(&*vpf.pm_tbl);
            tbls.push(&*vpf.firmware_info_tbl);
        }
        common::del_port_sfp_dom_info_from_db(logical_port, &self.port_mapping, &tbls);
    }

    /// `get_dom_polling_from_config_db` — live CONFIG_DB read (see the free fn).
    pub fn get_dom_polling_from_config_db(&self, lport: &str) -> String {
        get_dom_polling_from_config_db(&self.port_mapping, &*self.cfg_port_tbl, lport)
    }

    /// `is_port_in_cmis_initialization_process` (`dom_mgr.py:182`) — while the CMIS
    /// datapath bring-up is still non-terminal, the DOM loop skips the port so it
    /// doesn't race the CMIS manager. `skip_cmis_mgr` short-circuits to `false`.
    pub fn is_port_in_cmis_initialization_process(&self, logical_port_name: &str) -> bool {
        if self.skip_cmis_mgr {
            return false;
        }
        if self
            .port_mapping
            .get_asic_id_for_logical_port(logical_port_name)
            .is_none()
        {
            return false;
        }
        let cmis_state =
            common::get_cmis_state_from_state_db(logical_port_name, &*self.status_sw_tbl);
        !is_cmis_terminal_state(&cmis_state)
    }

    /// `is_port_dom_monitoring_disabled` (`dom_mgr.py:198`) — `dom_polling==disabled`
    /// OR the port is still in CMIS initialization.
    pub fn is_port_dom_monitoring_disabled(&self, logical_port_name: &str) -> bool {
        self.get_dom_polling_from_config_db(logical_port_name) == DOM_POLLING_DISABLED
            || self.is_port_in_cmis_initialization_process(logical_port_name)
    }

    /// `task_worker` (`dom_mgr.py:284`) — the periodic DOM monitoring loop. Each
    /// cycle FIRST runs the fast link-change inner loop (`check_port_update`) until
    /// the next periodic poll is due — staying responsive to APPL_DB `PORT_TABLE`
    /// `flap_count` bumps (~1s reaction, off the ~60s DOM cadence) — and only THEN
    /// polls every present, enabled module's DOM monitors and republishes
    /// `TRANSCEIVER_DOM_SENSOR` + the flag/status tables. The next poll is scheduled
    /// from the poll **start** (`loop_start + interval`), so long per-cycle processing
    /// doesn't drift the cadence.
    ///
    /// The first periodic poll is deliberately DELAYED by one full interval
    /// (`next_periodic = now + interval`), faithfully mirroring the Python worker
    /// (`dom_mgr.py:298`, "Adding dom_info_update_periodic_secs to allow xcvrd to
    /// initialize ports before starting the periodic update"): the wait-then-poll
    /// order matches the reference exactly. The prompt first snapshot — DOM_SENSOR,
    /// TRANSCEIVER_STATUS, AND the latched `TRANSCEIVER_DOM_FLAG` / `TRANSCEIVER_STATUS_FLAG`
    /// baselines — is instead published by the synchronous boot prime in `daemon::serve`
    /// (`prime_flag_baselines` for the latched flag tables, then
    /// `poll_once(_, include_flags=false, include_vdm=false)` for DOM_SENSOR/STATUS)
    /// BEFORE this thread is spawned, so a present port's flag baseline exists without
    /// waiting for this delayed first cadence poll (withholding it regressed
    /// `tests/test_dom_flag_meta.py` on a late-index port — see `poll_once` doc). This
    /// thread then refreshes the whole set on each ~60s cadence and adds the deferred
    /// VDM/PM/firmware block (`include_vdm=true`).
    pub fn task_worker(&self, stop: &Arc<AtomicBool>) {
        eprintln!(
            "xcvrd-rs: DomInfoUpdateTask: start DOM monitoring loop (interval={}s)",
            self.dom_update_interval
        );
        let interval = Duration::from_secs(self.dom_update_interval);
        // Mirror dom_mgr.py:298 — the first periodic poll is one interval out; until
        // then only the fast APPL_DB link-change watch runs. When the daemon anchored the
        // deadline on the boot instant (`set_first_periodic_from_boot`), use it so the
        // synchronous boot pass + boot DOM prime that ran before this thread spawned do
        // not stack on top of the interval and delay the first latched-flag baseline.
        let mut next_periodic = self
            .first_periodic_deadline
            .unwrap_or_else(|| Instant::now() + interval);
        while !stop.load(Ordering::Relaxed) {
            // Fast inner loop until the next periodic DOM poll is due: watch APPL_DB
            // PORT_TABLE for flap_count bumps and process any due link-change flag
            // re-reads, capped at PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS per pass so a
            // flap is reacted to within ~1s rather than the ~60s DOM cadence.
            while !stop.load(Ordering::Relaxed) {
                let remaining = next_periodic.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                self.check_port_update(stop);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let remaining = next_periodic.saturating_duration_since(Instant::now());
                let nap =
                    remaining.min(Duration::from_millis(PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS));
                interruptible_sleep(stop, nap);
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Schedule the next poll from THIS poll's start so the cadence stays a
            // consistent `interval` and does not drift with per-pass processing time
            // (dom_mgr.py:419-420).
            let loop_start = Instant::now();
            self.poll_once(stop, true, true);
            next_periodic = loop_start + interval;
        }
        eprintln!("xcvrd-rs: DomInfoUpdateTask: DOM monitoring loop stopped");
    }

    /// One DOM poll pass: for each present physical port's first (subport-0) logical
    /// port, honor the `dom_polling`/CMIS-init/blocking-error gates and, if all pass,
    /// republish `TRANSCEIVER_DOM_SENSOR` and — when the flag/status tables are wired
    /// — `TRANSCEIVER_DOM_FLAG` (+metadata), `TRANSCEIVER_STATUS`, and
    /// `TRANSCEIVER_STATUS_FLAG` (+metadata), in the same order as the per-port body
    /// of `dom_mgr.py:DomInfoUpdateTask.task_worker` (minus the still-later
    /// VDM/PM/firmware posts). Per-port errors are swallowed inside each poster so one
    /// bad module never aborts the pass.
    ///
    /// `include_flags` gates the two LATCHED flag tables (`TRANSCEIVER_DOM_FLAG` and
    /// `TRANSCEIVER_STATUS_FLAG`, each with its change-count / set-time / clear-time
    /// metadata); `include_vdm` gates the heavier VDM/PM/firmware block (a VDM freeze /
    /// unfreeze handshake with per-port settle waits). The periodic thread passes
    /// `(true, true)` — the full reference poll body. The synchronous BOOT prime in
    /// `daemon::serve` passes `(false, false)`: the latched flag baselines are published
    /// FIRST by the dedicated `prime_flag_baselines` sweep (so a present port's
    /// DOM_FLAG/STATUS_FLAG baseline is prompt and index-independent), leaving this boot
    /// poll to publish only DOM_SENSOR/STATUS; the VDM freeze block is likewise deferred to
    /// the periodic thread so the boot prime — which runs on the main thread ahead of the
    /// change-event / error-injection loop — stays fast.
    ///
    /// Why the flag baseline is front-loaded at boot: the first `DomInfoUpdateTask`
    /// periodic poll is delayed one full interval (`dom_mgr.py:298`, ~60s), so a present
    /// port must get its baseline from the boot prime. Interleaving flags behind each
    /// port's DOM_SENSOR+STATUS pushed a late-index port's FIRST `TRANSCEIVER_DOM_FLAG` to
    /// `boot + full-traversal`, which for `Ethernet100` (physical index 25) lands past the
    /// e2e `T_DOM` (80s) budget when `tests/test_dom_flag_meta.py` runs early in the
    /// session — regressing `test_dom_flag_groups_temp_and_vcc` (the whole DOM_FLAG row was
    /// simply late, so BOTH `tempHAlarm` and `vccHAlarm` read absent, not a VCC-specific
    /// decode gap: both derive from the same CMIS byte 00h:9 group). `prime_flag_baselines`
    /// publishes every present port's latched flags in a lean flags-only pass first, so the
    /// baseline the DOM-flag e2e expects is prompt regardless of port index.
    ///
    /// This does mean `tests/test_link_change_flags.py`'s DOM_FLAG can be present before
    /// its first flap. That test is an acknowledged coincident-poll flake ("re-run"): it
    /// also failed with the boot flag WITHHELD, because the ~60s periodic poll pre-
    /// publishes the flag before the (slow) test runs anyway — so withholding it at boot
    /// never actually isolated the flap re-read. The fast link-change re-read path it
    /// exercises (`update_port_db_diagnostics_on_link_change`) is unchanged.
    pub fn poll_once(&self, stop: &AtomicBool, include_flags: bool, include_vdm: bool) {
        let dom_db = DomDbUtils::new();
        let status_db = StatusDbUtils::new();
        for (_physical_port, logical_ports) in &self.port_mapping.physical_to_logical {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let logical_port_name = match logical_ports.first() {
                Some(name) => name,
                None => continue,
            };
            // A CONFIG_DB logical-port DEL marks the port deconfigured; the state task
            // has already torn down its whole table set. This loop iterates its OWN
            // boot-time port-mapping clone (still holding the removed port), so it must
            // NOT re-publish here — and it defensively purges any DOM-owned row a pass
            // already in flight at teardown time may have re-posted. Skip the port until
            // a re-ADD clears the mark.
            if self.is_deconfigured(logical_port_name) {
                self.purge_deconfigured_port_tables(logical_port_name);
                continue;
            }
            if self.is_port_dom_monitoring_disabled(logical_port_name) {
                continue;
            }
            if self
                .port_mapping
                .get_asic_id_for_logical_port(logical_port_name)
                .is_none()
            {
                continue;
            }
            // A port whose EEPROM reads are blocked is skipped entirely (its DOM
            // would be stale/unreadable); the poster's presence check handles absent
            // modules.
            if detect_port_in_error_status(logical_port_name, &*self.status_sw_tbl) {
                continue;
            }
            dom_db.post_port_dom_sensor_info_to_db(
                stop,
                logical_port_name,
                &self.port_mapping,
                &*self.dom_tbl,
                &*self.hal,
                None,
            );
            // The flag / hardware-status path (mirrors the task_worker order after
            // the DOM sensor post): DOM flags → HW status → HW status flags. Skipped
            // when the tables aren't wired (gate-only unit tests). The two LATCHED flag
            // tables (DOM_FLAG / STATUS_FLAG + metadata) are gated on `include_flags`,
            // which BOTH the boot prime and the periodic thread pass as `true` so a
            // present port's baseline is published promptly (a late-index port's first
            // DOM_FLAG must land inside the e2e T_DOM budget — see `poll_once` doc).
            // TRANSCEIVER_STATUS (a non-latched, re-read observable) is always published.
            if let Some(ft) = &self.flag_status_tables {
                if include_flags {
                    dom_db.post_port_dom_flags_to_db(
                        stop,
                        logical_port_name,
                        &self.port_mapping,
                        &*self.hal,
                        &*ft.dom_flag_tbl,
                        &*ft.dom_flag_change_count_tbl,
                        &*ft.dom_flag_set_time_tbl,
                        &*ft.dom_flag_clear_time_tbl,
                        None,
                    );
                }
                status_db.post_port_transceiver_hw_status_to_db(
                    stop,
                    logical_port_name,
                    &self.port_mapping,
                    &*ft.status_tbl,
                    &*self.hal,
                    None,
                );
                if include_flags {
                    status_db.post_port_transceiver_hw_status_flags_to_db(
                        stop,
                        logical_port_name,
                        &self.port_mapping,
                        &*self.hal,
                        &*ft.status_flag_tbl,
                        &*ft.status_flag_change_count_tbl,
                        &*ft.status_flag_set_time_tbl,
                        &*ft.status_flag_clear_time_tbl,
                        None,
                    );
                }
            }
            // M5 VDM real values (basic+statistic merge under freeze) + PM + firmware
            // + VDM flags. Gated on `include_vdm`: the SYNCHRONOUS boot prime
            // (`include_vdm == false`) defers this heavier block — a VDM freeze/unfreeze
            // handshake with per-port settle waits — to the periodic thread so the boot
            // prime stays fast and the change-event / error-injection loop (the main
            // thread, which runs AFTER the prime) starts promptly. The periodic poll
            // (`include_vdm == true`) publishes the full set within the ~60s cadence the
            // e2e T_DOM budget (80s) expects — mirroring the Python DOM worker order
            // (firmware → DOM → status → VDM/PM off the same per-port pass).
            if include_vdm {
                if let Some(vpf) = &self.vdm_pm_fw {
                    self.post_port_vdm_pm_firmware_info(stop, logical_port_name, vpf);
                }
            }
        }
    }

    /// Fast BOOT-only sweep that publishes ONLY the two LATCHED flag baselines —
    /// `TRANSCEIVER_DOM_FLAG` and `TRANSCEIVER_STATUS_FLAG` (each with its change-count /
    /// set-time / clear-time metadata) — for every present, DOM-enabled, non-blocking-error
    /// port, SKIPPING the heavier `TRANSCEIVER_DOM_SENSOR` / `TRANSCEIVER_STATUS` reads.
    ///
    /// Why a dedicated flags-first pass: the e2e begins asserting as soon as
    /// `TRANSCEIVER_INFO|<port>` is healthy — published by the boot IDENTITY pass, BEFORE
    /// this synchronous boot DOM prime — so it RACES the prime. In the old ordering the
    /// prime published each port's flags only AFTER that port's DOM_SENSOR+STATUS, so a
    /// LATE-index port's `TRANSCEIVER_DOM_FLAG` landed behind a full per-port traversal of
    /// every lower-index port; for `Ethernet100` (physical index 25) that pushed its FIRST
    /// flag baseline past the e2e `T_DOM` (80s) budget and regressed
    /// `tests/test_dom_flag_meta.py::test_dom_flag_groups_temp_and_vcc`. The whole DOM_FLAG
    /// row was simply late — both `tempHAlarm` (00h:9.0) and `vccHAlarm` (00h:9.4) derive
    /// from the same CMIS byte-9 group and are published in the SAME row, so it was never a
    /// VCC-specific decode gap. Running this flags-only sweep first, then
    /// `poll_once(_, include_flags=false, include_vdm=false)` for the DOM_SENSOR/STATUS
    /// body, keeps the TOTAL boot work identical (the main change-event loop still starts at
    /// the same instant) while making every present port's latched flag baseline land ~2x
    /// sooner — index-independent. Gate chain mirrors `poll_once` EXCEPT the CMIS-init gate,
    /// which is deliberately dropped here because this prime runs before the CMIS thread is
    /// spawned (every `cmis_state` is still absent → `UNKNOWN`, which the gate would treat as
    /// "in init" and skip every port); see the per-port gate comment for the full rationale
    /// and why it stays safe for `tests/test_dom_gating.py`. No-op when the flag/status
    /// tables are not wired (gate-only unit tests).
    pub fn prime_flag_baselines(&self, stop: &AtomicBool) {
        let ft = match &self.flag_status_tables {
            Some(ft) => ft,
            None => return,
        };
        let dom_db = DomDbUtils::new();
        let status_db = StatusDbUtils::new();
        for (_physical_port, logical_ports) in &self.port_mapping.physical_to_logical {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let logical_port_name = match logical_ports.first() {
                Some(name) => name,
                None => continue,
            };
            // Gate chain mirrors poll_once's per-port body EXCEPT the CMIS-init gate: a
            // CONFIG_DB DEL'd port is purged and skipped; a dom-monitoring-DISABLED /
            // invalid-asic / blocking-error port is skipped.
            //
            // Why the CMIS-init gate is deliberately DROPPED here (and kept in poll_once):
            // this boot prime runs ONCE, synchronously, BEFORE the CMIS bring-up thread is
            // spawned — so at prime time every present port's `cmis_state` is still ABSENT
            // (`get_cmis_state_from_state_db` → `UNKNOWN`, which is non-terminal). Reusing
            // `is_port_dom_monitoring_disabled` (whose `is_port_in_cmis_initialization_process`
            // arm treats UNKNOWN as "in init") would therefore skip EVERY port and publish
            // nothing at boot, defeating the prime's whole purpose and pushing each present
            // port's first DOM_FLAG onto the interval-gated (~60s) periodic poll — which, once
            // the real M8 datapath bring-up floods the PyO3 bridge, lands a late-index port
            // (Ethernet100) past the e2e T_DOM (80s) budget and regressed
            // tests/test_dom_flag_meta.py::test_dom_flag_groups_temp_and_vcc. The latched
            // byte-9 flag baseline is a plain EEPROM read that does not race the datapath
            // registers the CMIS manager drives, so publishing it at boot is safe. The
            // steady-state gate is unchanged: `poll_once` (and the link-change re-read) KEEP
            // the full `is_port_dom_monitoring_disabled` gate, so a port that later enters a
            // genuine, non-null non-terminal CMIS bring-up still has its DOM_FLAG withheld
            // (tests/test_dom_gating.py — whose invariant only applies while cmis_state is a
            // non-null non-terminal value, never at this absent-cmis_state boot instant).
            if self.is_deconfigured(logical_port_name) {
                self.purge_deconfigured_port_tables(logical_port_name);
                continue;
            }
            if self.get_dom_polling_from_config_db(logical_port_name) == DOM_POLLING_DISABLED {
                continue;
            }
            if self
                .port_mapping
                .get_asic_id_for_logical_port(logical_port_name)
                .is_none()
            {
                continue;
            }
            if detect_port_in_error_status(logical_port_name, &*self.status_sw_tbl) {
                continue;
            }
            // TRANSCEIVER_DOM_FLAG (+metadata) then TRANSCEIVER_STATUS_FLAG (+metadata) —
            // the same posters/order as poll_once's flag path, minus the DOM_SENSOR/STATUS
            // reads. Per-port read errors are swallowed inside each poster so one bad
            // module never aborts the sweep.
            dom_db.post_port_dom_flags_to_db(
                stop,
                logical_port_name,
                &self.port_mapping,
                &*self.hal,
                &*ft.dom_flag_tbl,
                &*ft.dom_flag_change_count_tbl,
                &*ft.dom_flag_set_time_tbl,
                &*ft.dom_flag_clear_time_tbl,
                None,
            );
            status_db.post_port_transceiver_hw_status_flags_to_db(
                stop,
                logical_port_name,
                &self.port_mapping,
                &*self.hal,
                &*ft.status_flag_tbl,
                &*ft.status_flag_change_count_tbl,
                &*ft.status_flag_set_time_tbl,
                &*ft.status_flag_clear_time_tbl,
                None,
            );
        }
    }

    /// Sleep between DOM cycles is handled by the module-level `interruptible_sleep`
    /// (shared by both tasks), which wakes every 200 ms to react promptly to `stop`.

    /// Thread entry (`run`): call `task_worker`.
    pub fn run(self, stop: Arc<AtomicBool>) {
        self.task_worker(&stop)
    }

    /// `post_port_pm_info_to_db` (`dom_mgr.py:238`) → `TRANSCEIVER_PM`, keyed by the
    /// module's `physical_port_name`. Skips an absent module, a flat-memory module
    /// (`is_flat_memory == True`), and an empty PM dict (the API not applicable);
    /// otherwise beautifies (stringifies) and posts. No `last_update_time` (matches
    /// Python). Called only inside the VDM statistic freeze window.
    pub fn post_port_pm_info_to_db(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        table: &dyn DbTable,
    ) {
        for (physical_port, physical_port_name) in
            common::get_physical_port_name_dict(logical_port_name, &self.port_mapping)
        {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let sfp = match self.hal.sfp(physical_port) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !sfp.get_presence().unwrap_or(false) {
                continue;
            }
            // `_wrapper_is_flat_memory == True` → skip (flat/SFF module has no PM page).
            let flat = sfp.call_json("is_flat_memory").ok().and_then(|v| v.as_bool());
            if flat == Some(true) {
                continue;
            }
            let mut pm = match sfp.call_json("get_transceiver_pm") {
                Ok(Value::Object(o)) => o,
                _ => continue,
            };
            if pm.is_empty() {
                continue;
            }
            DbUtils::new().beautify_info_dict(&mut pm);
            let fvs: Fvs = pm
                .iter()
                .map(|(k, v)| (k.clone(), value_to_py_str(v)))
                .collect();
            table.set(&physical_port_name, &fvs);
        }
    }

    /// `post_port_sfp_firmware_info_to_db` (`dom_mgr.py:203`) →
    /// `TRANSCEIVER_FIRMWARE_INFO`. Reads `get_transceiver_info_firmware_versions()`
    /// off each present physical port and writes the (raw) `active_firmware` /
    /// `inactive_firmware` row for EVERY logical port of that physical port. An empty
    /// read posts nothing. No `last_update_time` (matches Python).
    pub fn post_port_sfp_firmware_info_to_db(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        table: &dyn DbTable,
    ) {
        for (physical_port, _name) in
            common::get_physical_port_name_dict(logical_port_name, &self.port_mapping)
        {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let sfp = match self.hal.sfp(physical_port) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !sfp.get_presence().unwrap_or(false) {
                continue;
            }
            let fw = match sfp.call_json("get_transceiver_info_firmware_versions") {
                Ok(Value::Object(o)) => o,
                _ => continue,
            };
            if fw.is_empty() {
                continue;
            }
            let fvs: Fvs = fw
                .iter()
                .map(|(k, v)| (k.clone(), value_to_py_str(v)))
                .collect();
            // Firmware info is written to ALL logical ports of the physical port.
            if let Some(logical_ports) = self.port_mapping.get_physical_to_logical(physical_port) {
                for lport in logical_ports {
                    table.set(&lport, &fvs);
                }
            }
        }
    }

    /// The per-port M5 VDM/PM/firmware pass (`dom_mgr.py:350-417`), run off the
    /// periodic DOM poll for a present, non-error port: firmware info first, then —
    /// iff the module is VDM-capable — (a) if statistic observables are supported and
    /// the port is not in low-power mode, freeze the module, capture the statistic
    /// observables + PM info, then unfreeze; (b) capture basic observables, merge with
    /// the statistic set (statistic overrides on key collision) and publish the merged
    /// `TRANSCEIVER_VDM_REAL_VALUE`; (c) publish the per-type VDM flags (read last, as
    /// they are clear-on-read). Per-port read errors are swallowed by the posters so a
    /// single bad module never aborts the pass.
    fn post_port_vdm_pm_firmware_info(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        vpf: &VdmPmFirmwareTables,
    ) {
        self.post_port_sfp_firmware_info_to_db(stop, logical_port_name, &*vpf.firmware_info_tbl);

        let physical_port = match self
            .port_mapping
            .get_logical_to_physical(logical_port_name)
            .and_then(|l| l.first().copied())
        {
            Some(p) => p,
            None => return,
        };
        let sfp = match self.hal.sfp(physical_port) {
            Ok(s) => s,
            Err(_) => return,
        };
        let vdm = VdmUtils::new();
        if !vdm.is_transceiver_vdm_supported(&*sfp) {
            return;
        }

        // Step (a): statistic observables + PM under a freeze, only when supported and
        // the port is admin-up (not in low-power mode). The lpmode gate routes through
        // XCVRDUtils.is_transceiver_lpmode_on (dom_mgr.py:386-387) so a module in low
        // power skips the VDM-statistic freeze + TRANSCEIVER_PM refresh entirely
        // (basic DOM_SENSOR is NOT gated); it resumes once lpmode clears.
        let mut statistic: Map<String, Value> = Map::new();
        let need_freeze = vdm.is_vdm_statistic_supported(&*sfp) && {
            let mut lp_dict: BTreeMap<usize, &dyn SfpHandle> = BTreeMap::new();
            lp_dict.insert(physical_port, &*sfp);
            !XcvrdUtils::new(lp_dict).is_transceiver_lpmode_on(physical_port)
        };
        if need_freeze {
            vdm.with_vdm_freeze(&*sfp, |frozen| {
                if frozen {
                    statistic = vdm.get_vdm_real_values_statistic(&*sfp);
                    self.post_port_pm_info_to_db(stop, logical_port_name, &*vpf.pm_tbl);
                }
            });
        }

        // Step (b): basic observables, merged with statistic (statistic wins), posted
        // to TRANSCEIVER_VDM_REAL_VALUE with one trailing last_update_time.
        let mut merged = vdm.get_vdm_real_values_basic(&*sfp);
        for (k, v) in statistic {
            merged.insert(k, v);
        }
        VdmDbUtils::new().post_port_vdm_real_values_from_dict_to_db(
            stop,
            logical_port_name,
            &self.port_mapping,
            &*vpf.vdm_real_value_tbl,
            &*self.hal,
            merged,
        );

        // Step (c): per-type VDM flags (+metadata), read last (clear-on-read).
        VdmDbUtils::new().post_port_vdm_flags_to_db(
            stop,
            logical_port_name,
            &self.port_mapping,
            &*self.hal,
            &vpf.vdm_flag_tables,
        );
    }

    /// `on_port_update_event` (`dom_mgr.py:424`) — an APPL_DB `PORT_TABLE` `PORT_SET`
    /// (e.g. a `flap_count` bump from a link flap) schedules a fast flag re-read for
    /// the affected physical port `DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE` seconds
    /// later, letting the module settle before the DB is refreshed and consolidating
    /// all subports of a breakout group into one pending re-read. Non-`PORT_SET` or
    /// non-APPL_DB events are ignored. Interior-mutable so it can run off `&self`.
    pub fn on_port_update_event(&self, port_change_event: &PortChangeEvent) {
        if port_change_event.event_type == PortChangeEventType::PortSet
            && port_change_event.db_name.as_deref() == Some("APPL_DB")
        {
            let due = Instant::now()
                + Duration::from_secs(Self::DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE);
            self.link_change_affected_ports
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(port_change_event.port_index as usize, due);
        }
    }

    /// `check_port_update` (`dom_mgr.py:267`) — one pass of the fast link-change loop:
    /// dispatch any pending APPL_DB `PORT_TABLE` `flap_count` changes into
    /// `on_port_update_event`, then run each affected port's flag re-read once its
    /// scheduled time has arrived (dropping it afterward). Breakout consolidation and
    /// the settle delay come from the pending map keyed by physical port.
    pub fn check_port_update(&self, stop: &AtomicBool) {
        self.poll_flap_events();

        let now = Instant::now();
        let due_ports: Vec<usize> = {
            let map = self
                .link_change_affected_ports
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            map.iter()
                .filter(|(_, &due)| due <= now)
                .map(|(&p, _)| p)
                .collect()
        };
        for physical_port in due_ports {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            self.update_port_db_diagnostics_on_link_change(physical_port);
            self.link_change_affected_ports
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&physical_port);
        }

        // Front-load the latched flag baseline for any port whose CMIS bring-up just
        // reached a terminal state (the DOM gate releasing), off the ~60s cadence.
        self.prime_flags_on_cmis_gate_release(stop);
    }

    /// Publish the latched `TRANSCEIVER_DOM_FLAG` / `TRANSCEIVER_STATUS_FLAG` (+ VDM
    /// flag) baseline for a port either (a) whose `cmis_state` just transitioned
    /// non-terminal → terminal — the moment the CMIS-init DOM gate
    /// (`is_port_in_cmis_initialization_process`) RELEASES the port after a bring-up — or
    /// (b) that is already TERMINAL but whose `TRANSCEIVER_DOM_FLAG` row is MISSING from
    /// STATE_DB (a self-healing baseline restore for a boot-prime / edge miss).
    ///
    /// Why this exists (tests/test_dom_flag_meta.py::test_dom_flag_groups_temp_and_vcc):
    /// the boot `prime_flag_baselines` sweep only runs ONCE, before the CMIS thread is
    /// spawned, so it cannot restore the baseline for a port replugged (or first brought
    /// up) mid-session, and it can miss a port whose EEPROM was not yet readable at boot.
    /// While such a port's datapath is re-provisioning its `cmis_state` is non-terminal and
    /// the DOM gate correctly WITHHOLDS `TRANSCEIVER_DOM_FLAG` (tests/test_dom_gating.py).
    /// Once it reaches a terminal state the flag was previously republished ONLY by the
    /// interval-gated (~60s) periodic `poll_once`, which under the real M8 datapath bring-up
    /// floods the PyO3 bridge and lands a late-index port (`Ethernet100`, physical index 25)
    /// past the e2e `T_DOM` (80s) budget. This sweep closes that gap: it runs in the DB-only
    /// fast loop (~1s), is index-independent, and does a single fast, flag-only re-read via
    /// `update_port_db_diagnostics_on_link_change` (which re-applies the FULL gate chain —
    /// deconfigured / dom_polling / asic / blocking-error / CMIS-init / presence — so the
    /// gating invariant still holds even against a racing state write).
    ///
    /// Two triggers, both TERMINAL-only so the CMIS-init gate is never bypassed:
    ///   * the non-terminal→terminal RISING EDGE (edge-only, seed-on-first-sight: a port
    ///     seen for the first time is recorded WITHOUT publishing), and
    ///   * a TERMINAL port whose latched `TRANSCEIVER_DOM_FLAG` row is currently absent
    ///     (checked with a plain STATE_DB read, so the bridge is touched ONLY when the row
    ///     is genuinely missing — never in steady state).
    /// A port already terminal at boot with its baseline present (published by
    /// `prime_flag_baselines`) is therefore never spuriously republished, and a port that
    /// stays terminal with a present row (steady state, e.g. `Ethernet48` throughout
    /// tests/test_link_change_flags.py) never triggers here — so a routine gate-release /
    /// restore can't surface a flag inside that test's pre-flap window. `skip_cmis_mgr`
    /// (no CMIS gate to release) and unwired flag tables short-circuit the sweep.
    fn prime_flags_on_cmis_gate_release(&self, stop: &AtomicBool) {
        if self.skip_cmis_mgr || self.flag_status_tables.is_none() {
            return;
        }
        for (physical_port, logical_ports) in &self.port_mapping.physical_to_logical {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let logical_port_name = match logical_ports.first() {
                Some(name) => name,
                None => continue,
            };
            let cmis_state =
                common::get_cmis_state_from_state_db(logical_port_name, &*self.status_sw_tbl);
            let terminal = is_cmis_terminal_state(&cmis_state);
            let rising_edge = {
                let mut last = self
                    .cmis_terminal_last
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                match last.insert(logical_port_name.clone(), terminal) {
                    // First observation: seed only, never publish.
                    None => false,
                    // Publish only on the non-terminal → terminal rising edge.
                    Some(prev) => !prev && terminal,
                }
            };
            // Self-heal a MISSING latched flag baseline for a present, TERMINAL port whose
            // `TRANSCEIVER_DOM_FLAG` row is absent from STATE_DB — even without a fresh
            // non-terminal→terminal edge. The gate-release edge above only fires when THIS
            // loop actually WITNESSES the transition; the boot `prime_flag_baselines` sweep
            // runs once (before the CMIS thread) and can miss a port whose EEPROM was not
            // yet readable at boot, and the boot-projected READY seeds `cmis_terminal_last=
            // true` so the subsequent real bring-up's INSERTED→READY edge can be entirely
            // missed if a fast-loop pass straddles it under load. In those cases the ONLY
            // remaining republish path was the interval-gated (~60s) periodic `poll_once`,
            // which under the real M8 datapath bring-up floods the PyO3 bridge and lands a
            // late-index port (`Ethernet100`, physical index 25) past the e2e `T_DOM` (80s)
            // budget — regressing test_dom_flag_meta::test_dom_flag_groups_temp_and_vcc
            // (the whole row is late/absent, so BOTH tempHAlarm (00h:9.0) and vccHAlarm
            // (00h:9.4) read absent — never a VCC-specific decode gap; both derive from the
            // same CMIS byte-9 group and post in the same row).
            //
            // Safe by construction: (1) gated on `terminal`, so a genuinely initializing
            // port stays non-terminal and is NOT restored — the CMIS-init DOM gate and
            // test_dom_gating still hold; (2) the row-existence probe is a plain STATE_DB
            // read, so the bridge is touched ONLY when the row is genuinely absent (never in
            // steady state → no added bridge load, and no effect on a port whose baseline is
            // already present, e.g. `Ethernet48` throughout test_link_change_flags); (3)
            // `update_port_db_diagnostics_on_link_change` re-applies the FULL gate chain
            // (deconfigured / dom_polling / asic / blocking-error / CMIS-init / presence), so
            // an absent or racing-state-write port is still skipped and the removal/teardown
            // tests keep their deleted rows deleted.
            let restore_missing_baseline = terminal
                && self
                    .flag_status_tables
                    .as_ref()
                    .map(|ft| ft.dom_flag_tbl.get(logical_port_name).is_none())
                    .unwrap_or(false);
            if rising_edge || restore_missing_baseline {
                self.update_port_db_diagnostics_on_link_change(*physical_port);
            }
        }
    }

    /// Poll the watched APPL_DB `PORT_TABLE` `flap_count` for every logical port and,
    /// on a change from the last observed value, synthesize the APPL_DB `PORT_SET`
    /// event the Python `PortChangeObserver` would deliver. A port seen for the first
    /// time is only seeded (no re-read) so only genuine post-boot flaps trigger. This
    /// polling stands in for the reference's `SubscriberStateTable` select; the
    /// observable behavior (a flap → a fast flag re-read) is identical.
    fn poll_flap_events(&self) {
        let appl_tbl = match &self.appl_port_tbl {
            Some(t) => t,
            None => return,
        };
        for logical_port in &self.port_mapping.logical_port_list {
            let current = appl_tbl
                .hget(logical_port, "flap_count")
                .unwrap_or_default();
            let changed = {
                let mut last = self.flap_last.lock().unwrap_or_else(|e| e.into_inner());
                match last.get(logical_port) {
                    None => {
                        last.insert(logical_port.clone(), current);
                        false
                    }
                    Some(prev) if *prev != current => {
                        last.insert(logical_port.clone(), current);
                        true
                    }
                    Some(_) => false,
                }
            };
            if !changed {
                continue;
            }
            let physical_port = match self
                .port_mapping
                .get_logical_to_physical(logical_port)
                .and_then(|l| l.first().copied())
            {
                Some(p) => p,
                None => continue,
            };
            let asic_id = self
                .port_mapping
                .get_asic_id_for_logical_port(logical_port)
                .unwrap_or(0);
            let mut ev = PortChangeEvent::new(
                logical_port.clone(),
                physical_port as i32,
                asic_id,
                PortChangeEventType::PortSet,
            );
            ev.db_name = Some("APPL_DB".to_string());
            self.on_port_update_event(&ev);
        }
    }

    /// `update_port_db_diagnostics_on_link_change` (`dom_mgr.py:442`) — re-read ONLY
    /// the flag tables for `physical_port`'s first subport, fast, off the DOM cadence.
    /// Mirrors the Python gate chain: skip on stop; log + skip an unknown physical
    /// port; skip a DOM-monitoring-disabled port; log + skip an invalid ASIC; skip a
    /// port in blocking-error status or an absent module. Then republish
    /// `TRANSCEIVER_DOM_FLAG` (+metadata), `TRANSCEIVER_STATUS_FLAG` (+metadata), and
    /// the per-type `TRANSCEIVER_VDM_{TYPE}_FLAG` (+metadata) tables. No-op without the
    /// flag tables wired.
    pub fn update_port_db_diagnostics_on_link_change(&self, physical_port: usize) {
        let logical_port_list = match self.port_mapping.get_physical_to_logical(physical_port) {
            Some(list) if !list.is_empty() => list,
            _ => {
                eprintln!(
                    "xcvrd-rs: DomInfoUpdateTask: Update DB diagnostics during link change: \
                     Unknown physical port index {physical_port}"
                );
                return;
            }
        };
        // First logical port corresponds to the first subport.
        let first_logical_port = &logical_port_list[0];

        // A deconfigured port (CONFIG_DB logical-port DEL) must not have its flag tables
        // re-read/re-published; the state task has torn them down and the periodic
        // `poll_once` gate keeps them gone until a re-ADD.
        if self.is_deconfigured(first_logical_port) {
            return;
        }

        if self.is_port_dom_monitoring_disabled(first_logical_port) {
            return;
        }

        if self
            .port_mapping
            .get_asic_id_for_logical_port(first_logical_port)
            .is_none()
        {
            eprintln!(
                "xcvrd-rs: DomInfoUpdateTask: Update DB diagnostics during link change: \
                 Got invalid asic index for {first_logical_port}, ignored"
            );
            return;
        }

        // Skip a port whose EEPROM reads are blocked (its flags would be stale).
        if detect_port_in_error_status(first_logical_port, &*self.status_sw_tbl) {
            return;
        }

        if !self.get_transceiver_presence(physical_port) {
            return;
        }

        let ft = match &self.flag_status_tables {
            Some(ft) => ft,
            None => return,
        };
        let stop = AtomicBool::new(false);
        let dom_db = DomDbUtils::new();
        let status_db = StatusDbUtils::new();
        // TRANSCEIVER_DOM_FLAG (+metadata) then TRANSCEIVER_STATUS_FLAG (+metadata) —
        // same posters and order as the periodic poll's flag path. Per-port read
        // errors are swallowed inside each poster (the Python KeyError/TypeError
        // guard) so one bad module never aborts the re-read.
        dom_db.post_port_dom_flags_to_db(
            &stop,
            first_logical_port,
            &self.port_mapping,
            &*self.hal,
            &*ft.dom_flag_tbl,
            &*ft.dom_flag_change_count_tbl,
            &*ft.dom_flag_set_time_tbl,
            &*ft.dom_flag_clear_time_tbl,
            None,
        );
        status_db.post_port_transceiver_hw_status_flags_to_db(
            &stop,
            first_logical_port,
            &self.port_mapping,
            &*self.hal,
            &*ft.status_flag_tbl,
            &*ft.status_flag_change_count_tbl,
            &*ft.status_flag_set_time_tbl,
            &*ft.status_flag_clear_time_tbl,
            None,
        );
        // M5: re-read the per-type VDM flags on link change too (Python
        // `update_port_db_diagnostics_on_link_change:485`) — the same fast, flag-only
        // refresh, only when VDM tables are wired AND the module is VDM-capable.
        if let Some(vpf) = &self.vdm_pm_fw {
            if let Ok(sfp) = self.hal.sfp(physical_port) {
                if VdmUtils::new().is_transceiver_vdm_supported(&*sfp) {
                    VdmDbUtils::new().post_port_vdm_flags_to_db(
                        &stop,
                        first_logical_port,
                        &self.port_mapping,
                        &*self.hal,
                        &vpf.vdm_flag_tables,
                    );
                }
            }
        }
    }

    /// `xcvrd_utils.get_transceiver_presence(physical_port)` — read the module's SFP
    /// handle over the HAL; `false` for a missing slot or a failed read.
    fn get_transceiver_presence(&self, physical_port: usize) -> bool {
        self.hal
            .sfp(physical_port)
            .and_then(|s| s.get_presence())
            .unwrap_or(false)
    }
}

/// Loop-start scheduling arithmetic (`next = loop_start + interval`): the time left
/// to wait after a cycle that took `elapsed` is `interval - elapsed`, clamped at 0
/// (a cycle that overran the interval re-polls immediately).
fn schedule_remaining(interval: Duration, elapsed: Duration) -> Duration {
    interval.saturating_sub(elapsed)
}

/// `DomThermalInfoUpdateTask` (`dom_mgr.py:526`) — publishes
/// `TRANSCEIVER_DOM_TEMPERATURE` on the fast temperature cadence. Uses the BASE
/// `is_port_dom_monitoring_disabled` (the `dom_polling` toggle only — it does NOT
/// apply the CMIS-init gate that `DomInfoUpdateTask` adds).
pub struct DomThermalInfoUpdateTask {
    pub port_mapping: PortMapping,
    pub poll_interval: u64,
    hal: Arc<dyn Hal>,
    dom_temperature_tbl: Arc<dyn DbTable>,
    status_sw_tbl: Arc<dyn DbTable>,
    cfg_port_tbl: Arc<dyn DbTable>,
    /// Logical ports torn down by a CONFIG_DB logical-port DEL (shared with
    /// `SfpStateUpdateTask`). While set, this loop stops re-publishing the removed
    /// port's `TRANSCEIVER_DOM_TEMPERATURE` (it iterates its own boot-time mapping
    /// clone and would otherwise resurrect the row the state task deleted). `None` in
    /// the unit tests that drive `poll_once` directly.
    deconfigured_ports: Option<Arc<Mutex<BTreeSet<String>>>>,
}

impl DomThermalInfoUpdateTask {
    pub fn new(
        port_mapping: PortMapping,
        poll_interval: u64,
        hal: Arc<dyn Hal>,
        dom_temperature_tbl: Arc<dyn DbTable>,
        status_sw_tbl: Arc<dyn DbTable>,
        cfg_port_tbl: Arc<dyn DbTable>,
    ) -> Self {
        DomThermalInfoUpdateTask {
            port_mapping,
            poll_interval,
            hal,
            dom_temperature_tbl,
            status_sw_tbl,
            cfg_port_tbl,
            deconfigured_ports: None,
        }
    }

    /// Wire the cross-thread deconfigured-logical-port set maintained by
    /// `SfpStateUpdateTask`. While a port is in the set this loop stops publishing its
    /// temperature row (and defensively purges any it raced in). Left unset by unit
    /// tests that drive `poll_once` directly.
    pub fn set_deconfigured_ports(&mut self, set: Arc<Mutex<BTreeSet<String>>>) {
        self.deconfigured_ports = Some(set);
    }

    /// True while `logical_port` is marked deconfigured. Always `false` when the set
    /// isn't wired (unit tests).
    fn is_deconfigured(&self, logical_port: &str) -> bool {
        match &self.deconfigured_ports {
            Some(set) => set
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(logical_port),
            None => false,
        }
    }

    /// `is_port_dom_monitoring_disabled` — base class semantics: `dom_polling`
    /// toggle only (no CMIS-init gate).
    pub fn is_port_dom_monitoring_disabled(&self, logical_port_name: &str) -> bool {
        get_dom_polling_from_config_db(&self.port_mapping, &*self.cfg_port_tbl, logical_port_name)
            == DOM_POLLING_DISABLED
    }

    /// `task_worker` (`dom_mgr.py:535`) — poll temperature ASAP, then on the
    /// `poll_interval` cadence, republishing `TRANSCEIVER_DOM_TEMPERATURE` for each
    /// present, enabled port. Scheduling is from the loop start.
    pub fn task_worker(&self, stop: &Arc<AtomicBool>) {
        eprintln!(
            "xcvrd-rs: DomThermalInfoUpdateTask: start DOM thermal loop (interval={}s)",
            self.poll_interval
        );
        while !stop.load(Ordering::Relaxed) {
            let loop_start = Instant::now();
            self.poll_once(stop);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let interval = Duration::from_secs(self.poll_interval);
            let remaining = schedule_remaining(interval, loop_start.elapsed());
            interruptible_sleep(stop, remaining);
        }
        eprintln!("xcvrd-rs: DomThermalInfoUpdateTask: DOM thermal loop stopped");
    }

    fn poll_once(&self, stop: &AtomicBool) {
        let dom_db = DomDbUtils::new();
        for (physical_port, logical_ports) in &self.port_mapping.physical_to_logical {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let logical_port_name = match logical_ports.first() {
                Some(name) => name,
                None => continue,
            };
            // Deconfigured (CONFIG_DB logical-port DEL): stop re-posting the removed
            // port's temperature row and defensively purge any this loop may have
            // re-posted after the state task's teardown. Cleared on a re-ADD.
            if self.is_deconfigured(logical_port_name) {
                common::del_port_sfp_dom_info_from_db(
                    logical_port_name,
                    &self.port_mapping,
                    &[&*self.dom_temperature_tbl],
                );
                continue;
            }
            if self.is_port_dom_monitoring_disabled(logical_port_name) {
                continue;
            }
            if self
                .port_mapping
                .get_asic_id_for_logical_port(logical_port_name)
                .is_none()
            {
                continue;
            }
            // A non-errored port that is absent is skipped early (the errored-port
            // path still falls through to the poster, which validates presence).
            if !detect_port_in_error_status(logical_port_name, &*self.status_sw_tbl) {
                let present = self
                    .hal
                    .sfp(*physical_port)
                    .map(|s| s.get_presence().unwrap_or(false))
                    .unwrap_or(false);
                if !present {
                    continue;
                }
            }
            dom_db.post_port_dom_temperature_info_to_db(
                stop,
                logical_port_name,
                &self.port_mapping,
                &*self.dom_temperature_tbl,
                &*self.hal,
                None,
            );
        }
    }

    pub fn run(self, stop: Arc<AtomicBool>) {
        self.task_worker(&stop)
    }
}

/// Shared interruptible sleep (module-level so both tasks use one implementation).
fn interruptible_sleep(stop: &Arc<AtomicBool>, dur: Duration) {
    const STEP: Duration = Duration::from_millis(200);
    let mut slept = Duration::ZERO;
    while slept < dur && !stop.load(Ordering::Relaxed) {
        let step = STEP.min(dur - slept);
        std::thread::sleep(step);
        slept += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDbTable, MockHal, MockSfp};
    use crate::xcvrd_utilities::port_event_helper::PortChangeEventType;
    use crate::xcvrd_utilities::sfp_status_helper::SFP_ERROR_DESCRIPTION_BLOCKING;
    use serde_json::json;

    fn mapping_with(ports: &[(&str, usize)]) -> PortMapping {
        let mut pm = PortMapping::new();
        for (name, phys) in ports {
            pm.handle_port_change_event(&PortChangeEvent::new(
                *name,
                *phys as i32,
                0,
                PortChangeEventType::PortAdd,
            ));
        }
        pm
    }

    fn dom_sensor_values() -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("temperature".into(), json!("22.75"));
        m.insert("voltage".into(), json!("0.5"));
        for i in 1..=8 {
            m.insert(format!("rx{i}power"), json!("0.7"));
            m.insert(format!("tx{i}bias"), json!("0.7"));
            m.insert(format!("tx{i}power"), json!("0.7"));
        }
        Value::Object(m)
    }

    use serde_json::Value;

    struct Tables {
        dom: Arc<MockDbTable>,
        status_sw: Arc<MockDbTable>,
        cfg_port: Arc<MockDbTable>,
    }

    fn tables() -> Tables {
        Tables {
            dom: Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR")),
            status_sw: Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW")),
            cfg_port: Arc::new(MockDbTable::new("PORT")),
        }
    }

    fn dom_task(
        pm: PortMapping,
        skip_cmis: bool,
        interval: Option<u64>,
        hal: Arc<dyn Hal>,
        t: &Tables,
    ) -> DomInfoUpdateTask {
        DomInfoUpdateTask::new(
            pm,
            skip_cmis,
            interval,
            hal,
            t.dom.clone(),
            t.status_sw.clone(),
            t.cfg_port.clone(),
        )
    }

    // The default cadence resolves to 60 s; 0 and explicit values are honored
    // (tests/test_xcvrd.py:test_DomInfoUpdateTask_dom_update_interval_parameter).
    #[test]
    fn test_dom_update_interval_parameter() {
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![]));
        let default = dom_task(PortMapping::new(), true, None, hal.clone(), &t);
        assert_eq!(default.dom_update_interval, 60);
        assert_eq!(
            default.dom_update_interval,
            DomInfoUpdateTask::DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS
        );
        assert_eq!(dom_task(PortMapping::new(), true, Some(0), hal.clone(), &t).dom_update_interval, 0);
        assert_eq!(dom_task(PortMapping::new(), true, Some(120), hal.clone(), &t).dom_update_interval, 120);
        assert_eq!(dom_task(PortMapping::new(), true, Some(1000), hal, &t).dom_update_interval, 1000);
        assert_eq!(DomInfoUpdateTask::DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS, 60);
    }

    // tests/test_xcvrd.py:test_DomInfoUpdateTask_get_dom_polling_from_config_db —
    // the first (natsorted) subport of the breakout group names the dom_polling; an
    // unknown port defaults to enabled.
    #[test]
    fn test_get_dom_polling_from_config_db() {
        // Ethernet0/4/8/12 share physical 1 (first subport = Ethernet0); Ethernet16
        // is alone on physical 2.
        let pm = mapping_with(&[
            ("Ethernet4", 1),
            ("Ethernet12", 1),
            ("Ethernet8", 1),
            ("Ethernet0", 1),
            ("Ethernet16", 2),
        ]);
        let t = tables();
        // dom_polling set only on subport-0 ports.
        t.cfg_port.hset("Ethernet0", "dom_polling", "disabled");
        t.cfg_port.hset("Ethernet4", "dom_polling", "enabled");
        t.cfg_port.hset("Ethernet8", "dom_polling", "enabled");
        t.cfg_port.hset("Ethernet12", "dom_polling", "enabled");
        t.cfg_port.hset("Ethernet16", "dom_polling", "enabled");
        let hal = Arc::new(MockHal::with_sfps(vec![]));
        let task = dom_task(pm, true, None, hal, &t);

        // All of Ethernet0/4/8/12 resolve to first subport Ethernet0 (disabled).
        assert_eq!(task.get_dom_polling_from_config_db("Ethernet0"), "disabled");
        assert_eq!(task.get_dom_polling_from_config_db("Ethernet4"), "disabled");
        assert_eq!(task.get_dom_polling_from_config_db("Ethernet8"), "disabled");
        assert_eq!(task.get_dom_polling_from_config_db("Ethernet12"), "disabled");
        // Ethernet16 is its own group (enabled).
        assert_eq!(task.get_dom_polling_from_config_db("Ethernet16"), "enabled");
        // Unknown port -> default enabled.
        assert_eq!(task.get_dom_polling_from_config_db("Ethernet20"), "enabled");
    }

    // tests/test_xcvrd.py:test_DomInfoUpdateTask_is_port_in_cmis_initialization_process
    // (adapted to the STATUS_SW seam): skip_cmis short-circuits false; else a
    // non-terminal cmis_state -> True, a terminal one -> False, unknown asic -> False.
    #[test]
    fn test_is_port_in_cmis_initialization_process() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp::present()]));

        // skip_cmis_mgr -> always False.
        let t1 = tables();
        let skip = dom_task(pm.clone(), true, None, hal.clone(), &t1);
        assert!(!skip.is_port_in_cmis_initialization_process("Ethernet0"));

        // Not skipping: INSERTED (non-terminal) -> True.
        let t2 = tables();
        t2.status_sw.hset("Ethernet0", "cmis_state", "INSERTED");
        let task = dom_task(pm.clone(), false, None, hal.clone(), &t2);
        assert!(task.is_port_in_cmis_initialization_process("Ethernet0"));

        // READY (terminal) -> False.
        let t3 = tables();
        t3.status_sw.hset("Ethernet0", "cmis_state", "READY");
        let task = dom_task(pm.clone(), false, None, hal.clone(), &t3);
        assert!(!task.is_port_in_cmis_initialization_process("Ethernet0"));

        // Absent cmis_state reads back UNKNOWN (non-terminal) -> True.
        let t4 = tables();
        let task = dom_task(pm.clone(), false, None, hal.clone(), &t4);
        assert!(task.is_port_in_cmis_initialization_process("Ethernet0"));

        // Unknown asic (port not in mapping) -> False.
        let t5 = tables();
        let task = dom_task(pm, false, None, hal, &t5);
        assert!(!task.is_port_in_cmis_initialization_process("INVALID_PORT"));
    }

    // A single poll posts DOM_SENSOR for a present, enabled, non-errored, terminal
    // port — the essence of the DomInfoUpdateTask poll body.
    #[test]
    fn test_poll_once_posts_dom_sensor_when_enabled() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY"); // terminal -> not gated
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
        }]));
        let task = dom_task(pm, false, Some(60), hal, &t);
        let stop = AtomicBool::new(false);

        task.poll_once(&stop, true, true);
        assert_eq!(t.dom.get_size_for_key("Ethernet0"), 27);
    }

    // With the flag/status tables wired, a poll additionally publishes DOM_FLAG
    // (+metadata), TRANSCEIVER_STATUS, and STATUS_FLAG (+metadata) — the M2 flag path
    // the test_dom_flag_meta / test_golden e2e checks depend on.
    #[test]
    fn test_poll_once_posts_flags_and_status() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY"); // terminal -> not gated
        let sfp = MockSfp {
            dom_real_value: dom_sensor_values(),
            status: json!({"status": "1", "cmis_state": "READY"}),
            ..MockSfp::present()
        }
        .with_json("get_transceiver_dom_flags", json!({"tempHAlarm": true}))
        .with_json("get_transceiver_status_flags", json!({"tx_fault": false}));
        let hal = Arc::new(MockHal::with_sfps(vec![sfp]));
        let mut task = dom_task(pm, false, Some(60), hal, &t);

        let dom_flag = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG"));
        let dom_flag_cc = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_CHANGE_COUNT"));
        let dom_flag_st = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_SET_TIME"));
        let dom_flag_ct = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_CLEAR_TIME"));
        let status = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS"));
        let status_flag = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG"));
        let status_flag_cc = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT"));
        let status_flag_st = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG_SET_TIME"));
        let status_flag_ct = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG_CLEAR_TIME"));
        task.set_flag_status_tables(FlagStatusTables {
            dom_flag_tbl: dom_flag.clone(),
            dom_flag_change_count_tbl: dom_flag_cc.clone(),
            dom_flag_set_time_tbl: dom_flag_st.clone(),
            dom_flag_clear_time_tbl: dom_flag_ct.clone(),
            status_tbl: status.clone(),
            status_flag_tbl: status_flag.clone(),
            status_flag_change_count_tbl: status_flag_cc.clone(),
            status_flag_set_time_tbl: status_flag_st.clone(),
            status_flag_clear_time_tbl: status_flag_ct.clone(),
        });

        let stop = AtomicBool::new(false);
        task.poll_once(&stop, true, true);

        // DOM sensor still posted.
        assert_eq!(t.dom.get_size_for_key("Ethernet0"), 27);
        // DOM flags + first-publish metadata (count 0, set/clear 'never').
        assert_eq!(dom_flag.hget("Ethernet0", "tempHAlarm").as_deref(), Some("True"));
        assert!(dom_flag.hget("Ethernet0", "last_update_time").is_some());
        assert_eq!(dom_flag_cc.hget("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(dom_flag_st.hget("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        assert_eq!(dom_flag_ct.hget("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        // Hardware status.
        assert_eq!(status.hget("Ethernet0", "status").as_deref(), Some("1"));
        assert!(status.hget("Ethernet0", "last_update_time").is_some());
        // Status flags + first-publish metadata.
        assert_eq!(status_flag.hget("Ethernet0", "tx_fault").as_deref(), Some("False"));
        assert_eq!(status_flag_cc.hget("Ethernet0", "tx_fault").as_deref(), Some("0"));
    }

    // The synchronous BOOT prime (`daemon::serve` -> `poll_once(_, include_flags=true,
    // include_vdm=false)`) publishes the non-latched observables (TRANSCEIVER_DOM_SENSOR +
    // TRANSCEIVER_STATUS) AND the LATCHED flag baselines (TRANSCEIVER_DOM_FLAG /
    // TRANSCEIVER_STATUS_FLAG + change-count/set-time/clear-time metadata) promptly, so a
    // present port's flag baseline exists without waiting for the delayed first ~60s
    // periodic poll. Withholding the flags at boot pushed a late-index port's first
    // DOM_FLAG baseline past the e2e T_DOM (80s) budget and regressed
    // test_dom_flag_groups_temp_and_vcc; this test locks in that the boot prime latches
    // the full flag table set on the fast path.
    #[test]
    fn test_boot_prime_publishes_latched_flags_for_prompt_baseline() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY"); // terminal -> not gated
        let sfp = MockSfp {
            dom_real_value: dom_sensor_values(),
            status: json!({"status": "1", "cmis_state": "READY"}),
            ..MockSfp::present()
        }
        .with_json("get_transceiver_dom_flags", json!({"tempHAlarm": true}))
        .with_json("get_transceiver_status_flags", json!({"tx_fault": false}));
        let hal = Arc::new(MockHal::with_sfps(vec![sfp]));
        let mut task = dom_task(pm, false, Some(60), hal, &t);

        let dom_flag = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG"));
        let dom_flag_cc = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_CHANGE_COUNT"));
        let dom_flag_st = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_SET_TIME"));
        let dom_flag_ct = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_CLEAR_TIME"));
        let status = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS"));
        let status_flag = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG"));
        let status_flag_cc = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT"));
        let status_flag_st = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG_SET_TIME"));
        let status_flag_ct = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG_CLEAR_TIME"));
        task.set_flag_status_tables(FlagStatusTables {
            dom_flag_tbl: dom_flag.clone(),
            dom_flag_change_count_tbl: dom_flag_cc.clone(),
            dom_flag_set_time_tbl: dom_flag_st.clone(),
            dom_flag_clear_time_tbl: dom_flag_ct.clone(),
            status_tbl: status.clone(),
            status_flag_tbl: status_flag.clone(),
            status_flag_change_count_tbl: status_flag_cc.clone(),
            status_flag_set_time_tbl: status_flag_st.clone(),
            status_flag_clear_time_tbl: status_flag_ct.clone(),
        });

        let stop = AtomicBool::new(false);
        // Boot prime: latched flags ON, VDM deferred.
        task.poll_once(&stop, true, false);

        // Non-latched observables published so DOM presence + status stay prompt.
        assert_eq!(t.dom.get_size_for_key("Ethernet0"), 27);
        assert_eq!(status.hget("Ethernet0", "status").as_deref(), Some("1"));
        // The two LATCHED flag tables (+ first-publish metadata) ARE published at boot.
        assert_eq!(dom_flag.hget("Ethernet0", "tempHAlarm").as_deref(), Some("True"));
        assert_eq!(dom_flag_cc.hget("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(dom_flag_st.hget("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        assert_eq!(dom_flag_ct.hget("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        assert_eq!(status_flag.hget("Ethernet0", "tx_fault").as_deref(), Some("False"));
        assert_eq!(status_flag_cc.hget("Ethernet0", "tx_fault").as_deref(), Some("0"));
    }

    // The boot prime DEFERS the heavier VDM/PM/firmware block (a freeze/unfreeze
    // handshake with per-port settle waits) via `include_vdm=false` so the synchronous
    // prime stays fast and the change-event / error-injection loop (the main thread,
    // which runs AFTER the prime) starts promptly; the periodic poll (`include_vdm=true`)
    // publishes it. Locks in that `include_vdm` gates only the heavy block, independently
    // of the latched DOM/STATUS flags.
    #[test]
    fn test_boot_prime_defers_vdm_periodic_publishes() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY"); // terminal -> not gated
        let hal = Arc::new(MockHal::with_sfps(vec![full_vdm_sfp()]));
        let mut task = dom_task(pm, false, Some(60), hal, &t);
        let (vpf, probe) = vpf_tables();
        task.set_vdm_pm_firmware_tables(vpf);

        let stop = AtomicBool::new(false);
        // Boot prime: VDM deferred -> no VDM real value / PM / firmware yet.
        task.poll_once(&stop, true, false);
        assert!(probe.real_value.get("Ethernet0").is_none());
        assert!(probe.pm.get("Ethernet0").is_none());
        assert!(probe.firmware.get("Ethernet0").is_none());

        // Periodic poll: the VDM/PM/firmware block IS published.
        task.poll_once(&stop, true, true);
        assert!(probe.real_value.hget("Ethernet0", "laser_temperature_media1").is_some());
        assert_eq!(probe.firmware.get_size_for_key("Ethernet0"), 2);
    }

    // The first periodic DOM poll is DELAYED by one full interval (dom_mgr.py:298,
    // "allow xcvrd to initialize ports"): task_worker runs only the fast link-change
    // watch until now+interval, so a routine flag/DOM republish does NOT fire right at
    // thread start (which — landing in a link-change test's short pre-flap guard —
    // caused tests/test_link_change_flags.py to flake). With a 1 s interval, no
    // DOM_SENSOR is posted within the first ~300 ms; it appears only after the interval.
    #[test]
    fn test_task_worker_delays_first_periodic_poll_one_interval() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY"); // terminal -> not gated
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
        }]));
        let task = Arc::new(dom_task(pm, false, Some(1), hal, &t));
        let stop = Arc::new(AtomicBool::new(false));

        let worker = task.clone();
        let worker_stop = stop.clone();
        let handle = std::thread::spawn(move || worker.task_worker(&worker_stop));

        // Well within the 1 s interval: the delayed-first-poll means DOM_SENSOR is not
        // published yet (the pre-fix immediate poll would already have written it here).
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            t.dom.get_size_for_key("Ethernet0"),
            0,
            "first periodic DOM poll must be delayed one interval, not run at thread start"
        );

        // After the interval elapses, the first periodic poll fires and posts DOM_SENSOR.
        std::thread::sleep(Duration::from_millis(1400));
        assert!(
            t.dom.get_size_for_key("Ethernet0") > 0,
            "first periodic DOM poll should have fired after one interval"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }

    // dom_polling=disabled halts DOM_SENSOR posting; re-enabling resumes it — the
    // unit-level analogue of the test_dom_polling e2e gate.
    #[test]
    fn test_poll_once_respects_dom_polling_toggle() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY");
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
        }]));
        // skip_cmis so only the dom_polling toggle gates here.
        let task = dom_task(pm, true, Some(60), hal, &t);
        let stop = AtomicBool::new(false);

        // Disabled -> nothing posted.
        t.cfg_port.hset("Ethernet0", "dom_polling", "disabled");
        task.poll_once(&stop, true, true);
        assert_eq!(t.dom.get_size(), 0);

        // Re-enable (remove the field) -> DOM_SENSOR posted.
        t.cfg_port.hdel("Ethernet0", "dom_polling");
        task.poll_once(&stop, true, true);
        assert_eq!(t.dom.get_size_for_key("Ethernet0"), 27);
    }

    // A port whose STATUS_SW.error carries the blocking description is skipped.
    #[test]
    fn test_poll_once_skips_blocking_error_port() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.set(
            "Ethernet0",
            &[
                ("status".to_string(), "1".to_string()),
                ("error".to_string(), SFP_ERROR_DESCRIPTION_BLOCKING.to_string()),
                ("cmis_state".to_string(), "READY".to_string()),
            ],
        );
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
        }]));
        let task = dom_task(pm, true, Some(60), hal, &t);
        let stop = AtomicBool::new(false);

        task.poll_once(&stop, true, true);
        assert_eq!(t.dom.get_size(), 0);
    }

    // An absent module posts nothing (the poster's presence gate), and a poll never
    // panics on a missing HAL slot — the per-port resilience the daemon relies on.
    #[test]
    fn test_poll_once_skips_absent_and_survives_missing_slot() {
        let pm = mapping_with(&[("Ethernet0", 0), ("Ethernet4", 1)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY");
        t.status_sw.hset("Ethernet4", "cmis_state", "READY");
        // Only one slot present+readable; physical 1 has no slot at all (HAL errors).
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
        }]));
        let task = dom_task(pm, true, Some(60), hal, &t);
        let stop = AtomicBool::new(false);

        task.poll_once(&stop, true, true); // must not panic on the missing physical-1 slot
        assert_eq!(t.dom.get_size_for_key("Ethernet0"), 27);
        assert!(t.dom.get("Ethernet4").is_none());
    }

    // The spawned worker issues at least one poll and stops promptly when the flag
    // is set (tests/test_xcvrd.py:test_DomInfoUpdateTask_task_run_stop analogue). The
    // first periodic poll is delayed one interval (dom_mgr.py:298), so wait past the
    // 1 s interval to observe it, then confirm the worker stops promptly on the flag.
    #[test]
    fn test_task_worker_runs_and_stops() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY");
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
        }]));
        let task = dom_task(pm, true, Some(1), hal, &t);
        let dom_tbl = t.dom.clone();

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = stop.clone();
            std::thread::spawn(move || task.run(stop))
        };
        // Past the 1 s interval so the (delayed) first periodic poll has fired.
        std::thread::sleep(Duration::from_millis(1400));
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("DOM worker joins cleanly");

        assert_eq!(dom_tbl.get_size_for_key("Ethernet0"), 27);
    }

    // tests/test_xcvrd.py:test_DomInfoUpdateTask_task_run_with_exception (Rust-shifted):
    // a per-port HAL failure must NOT crash the worker — it keeps the supervisor
    // RUNNING and simply skips the bad module.
    #[test]
    fn test_task_worker_survives_per_port_error() {
        // physical 0 has no HAL slot at all -> hal.sfp(0) errors inside the poster.
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY");
        let hal = Arc::new(MockHal::with_sfps(vec![])); // empty plant
        let task = dom_task(pm, true, Some(1), hal, &t);

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = stop.clone();
            std::thread::spawn(move || task.run(stop))
        };
        // Past the 1 s interval so the (delayed) first periodic poll actually fires and
        // exercises the per-port error path (hal.sfp(0) errors on the empty plant).
        std::thread::sleep(Duration::from_millis(1400));
        stop.store(true, Ordering::Relaxed);
        // Joins cleanly (no panic propagated) despite the missing module.
        handle.join().expect("worker survived the per-port error");
        assert_eq!(t.dom.get_size(), 0);
    }

    // tests/test_xcvrd.py:test_DomInfoUpdateTask_scheduling_uses_loop_start_time:
    // the wait after a cycle is interval - processing_time (loop-start scheduling),
    // clamped at zero when a cycle overruns the interval.
    #[test]
    fn test_scheduling_uses_loop_start_time() {
        let interval = Duration::from_secs(60);
        // 30 s of processing -> 30 s left (not a fresh 60 s from loop end).
        assert_eq!(
            schedule_remaining(interval, Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        // A cycle that overran the interval re-polls immediately.
        assert_eq!(
            schedule_remaining(interval, Duration::from_secs(90)),
            Duration::ZERO
        );
    }

    // tests/test_xcvrd.py:test_DomInfoUpdateTask_handle_port_change_event (adapted):
    // the task's port_mapping tracks add/remove.
    #[test]
    fn test_handle_port_change_event() {
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![]));
        let mut task = dom_task(PortMapping::new(), true, None, hal, &t);
        task.port_mapping.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet0",
            1,
            0,
            PortChangeEventType::PortAdd,
        ));
        assert!(task.port_mapping.is_logical_port("Ethernet0"));
        assert_eq!(task.port_mapping.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(task.port_mapping.get_logical_to_physical("Ethernet0"), Some(vec![1]));

        task.port_mapping.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet0",
            1,
            0,
            PortChangeEventType::PortRemove,
        ));
        assert!(!task.port_mapping.is_logical_port("Ethernet0"));
        assert!(task.port_mapping.logical_port_list.is_empty());
    }

    // --- M4 link-change fast flag re-read -----------------------------------------

    /// The nine DOM/status flag + metadata mock tables the link-change re-read (and
    /// the periodic poll) publish, wired onto a task via [`FlagTables::wire`].
    struct FlagTables {
        dom_flag: Arc<MockDbTable>,
        dom_flag_cc: Arc<MockDbTable>,
        dom_flag_st: Arc<MockDbTable>,
        dom_flag_ct: Arc<MockDbTable>,
        status: Arc<MockDbTable>,
        status_flag: Arc<MockDbTable>,
        status_flag_cc: Arc<MockDbTable>,
        status_flag_st: Arc<MockDbTable>,
        status_flag_ct: Arc<MockDbTable>,
    }

    impl FlagTables {
        fn new() -> Self {
            FlagTables {
                dom_flag: Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG")),
                dom_flag_cc: Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_CHANGE_COUNT")),
                dom_flag_st: Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_SET_TIME")),
                dom_flag_ct: Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_CLEAR_TIME")),
                status: Arc::new(MockDbTable::new("TRANSCEIVER_STATUS")),
                status_flag: Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG")),
                status_flag_cc: Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT")),
                status_flag_st: Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG_SET_TIME")),
                status_flag_ct: Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG_CLEAR_TIME")),
            }
        }

        fn wire(&self, task: &mut DomInfoUpdateTask) {
            task.set_flag_status_tables(FlagStatusTables {
                dom_flag_tbl: self.dom_flag.clone(),
                dom_flag_change_count_tbl: self.dom_flag_cc.clone(),
                dom_flag_set_time_tbl: self.dom_flag_st.clone(),
                dom_flag_clear_time_tbl: self.dom_flag_ct.clone(),
                status_tbl: self.status.clone(),
                status_flag_tbl: self.status_flag.clone(),
                status_flag_change_count_tbl: self.status_flag_cc.clone(),
                status_flag_set_time_tbl: self.status_flag_st.clone(),
                status_flag_clear_time_tbl: self.status_flag_ct.clone(),
            });
        }
    }

    /// A present module with latched DOM + status flags to re-capture.
    fn flagged_sfp() -> MockSfp {
        MockSfp::present()
            .with_json("get_transceiver_dom_flags", json!({"tempHAlarm": true}))
            .with_json("get_transceiver_status_flags", json!({"tx_fault": false}))
    }

    // tests/test_xcvrd.py:test_update_port_db_diagnostics_on_link_change (Case 1/6): a
    // valid, present, non-error port re-publishes ONLY the flag tables (DOM_FLAG +
    // STATUS_FLAG, each with first-publish metadata: count 0, set/clear 'never').
    #[test]
    fn test_update_port_db_diagnostics_on_link_change_publishes_flags() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, true, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);

        task.update_port_db_diagnostics_on_link_change(0);

        assert_eq!(ft.dom_flag.hget("Ethernet0", "tempHAlarm").as_deref(), Some("True"));
        assert_eq!(ft.dom_flag_cc.hget("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(ft.dom_flag_st.hget("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        assert_eq!(ft.dom_flag_ct.hget("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        assert_eq!(ft.status_flag.hget("Ethernet0", "tx_fault").as_deref(), Some("False"));
        assert_eq!(ft.status_flag_cc.hget("Ethernet0", "tx_fault").as_deref(), Some("0"));
    }

    // Case 2: an unknown physical port index is logged and skipped — nothing posted.
    #[test]
    fn test_update_port_db_diagnostics_on_link_change_unknown_port() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, true, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);

        task.update_port_db_diagnostics_on_link_change(9);

        assert_eq!(ft.dom_flag.get_size(), 0);
        assert_eq!(ft.status_flag.get_size(), 0);
    }

    // Case 3: a logical port with no ASIC mapping is logged and skipped.
    #[test]
    fn test_update_port_db_diagnostics_on_link_change_invalid_asic() {
        // A physical->logical entry with NO asic mapping -> asic lookup returns None.
        let mut pm = PortMapping::new();
        pm.physical_to_logical.insert(3, vec!["Ethernet1".to_string()]);
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![]));
        let mut task = dom_task(pm, true, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);

        task.update_port_db_diagnostics_on_link_change(3);

        assert_eq!(ft.dom_flag.get_size(), 0);
        assert_eq!(ft.status_flag.get_size(), 0);
    }

    // Case 4: a port in blocking-error status is skipped (its flags would be stale).
    #[test]
    fn test_update_port_db_diagnostics_on_link_change_skips_error_status() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.set(
            "Ethernet0",
            &[
                ("status".to_string(), "1".to_string()),
                ("error".to_string(), SFP_ERROR_DESCRIPTION_BLOCKING.to_string()),
            ],
        );
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, true, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);

        task.update_port_db_diagnostics_on_link_change(0);

        assert_eq!(ft.dom_flag.get_size(), 0);
        assert_eq!(ft.status_flag.get_size(), 0);
    }

    // Case 5: an absent module posts nothing (the presence gate).
    #[test]
    fn test_update_port_db_diagnostics_on_link_change_skips_absent() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp::default()])); // presence=false
        let mut task = dom_task(pm, true, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);

        task.update_port_db_diagnostics_on_link_change(0);

        assert_eq!(ft.dom_flag.get_size(), 0);
        assert_eq!(ft.status_flag.get_size(), 0);
    }

    // --- boot flag prime (prime_flag_baselines) -----------------------------------

    // A present, admin-clean module: the platform decode surfaces the FULL module temp/vcc
    // flag group (CmisApi.get_transceiver_dom_flags reads byte 00h:9 via get_module_level_flag
    // and emits the temperature AND voltage halves together, all False here). The M8 boot flag
    // prime must publish that baseline (tempHAlarm AND vccHAlarm as "False") for EVERY present
    // port regardless of its physical index. This is the unit-level lock on
    // tests/test_dom_flag_meta.py::test_dom_flag_groups_temp_and_vcc's baseline
    // (tempHAlarm==False AND vccHAlarm==False), whose e2e regression was a TIMING gap: on
    // Ethernet100 (physical index 25) the whole DOM_FLAG row landed past the 80s T_DOM budget
    // because the flags were published only AFTER every lower-index port's DOM_SENSOR+STATUS
    // (the row was simply late, so BOTH tempHAlarm AND vccHAlarm read absent — not a
    // VCC-specific decode gap, since both derive from the same byte-9 group). The flags-only
    // sweep front-loads them so a late-index port's baseline lands promptly. Here Ethernet0 (0)
    // and Ethernet4 (1) both get the full temp+vcc baseline, and the heavier DOM_SENSOR /
    // TRANSCEIVER_STATUS reads are NOT published by the flag prime (they ride the subsequent
    // poll_once(false, false)). (First-publish flag metadata seeding is asserted in the
    // single-key gate test below: the mock's Table.set REPLACES a row where real swss MERGES,
    // so the per-key metadata writes of a multi-key dict only survive the mock for a single-key
    // module.)
    #[test]
    fn test_prime_flag_baselines_publishes_full_temp_vcc_baseline_for_all_ports() {
        let pm = mapping_with(&[("Ethernet0", 0), ("Ethernet4", 1)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY");
        t.status_sw.hset("Ethernet4", "cmis_state", "READY");
        // Each module's platform decode returns the whole temp+vcc flag group (all False, as
        // CmisApi does from a quiescent byte 00h:9); the flag prime publishes it verbatim.
        // dom_real_value is populated to prove the flag prime does NOT read/publish DOM_SENSOR.
        let sfp = || MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
                .with_json(
                    "get_transceiver_dom_flags",
                    json!({
                        "tempHAlarm": false, "tempLAlarm": false, "tempHWarn": false, "tempLWarn": false,
                        "vccHAlarm": false, "vccLAlarm": false, "vccHWarn": false, "vccLWarn": false,
                    }),
                )
                .with_json("get_transceiver_status_flags", json!({"tx_fault": false}))
        };
        let hal = Arc::new(MockHal::with_sfps(vec![sfp(), sfp()]));
        let mut task = dom_task(pm, true, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);

        task.prime_flag_baselines(&AtomicBool::new(false));

        for p in ["Ethernet0", "Ethernet4"] {
            // The exact e2e baseline predicate: BOTH groups published as "False".
            assert_eq!(ft.dom_flag.hget(p, "tempHAlarm").as_deref(), Some("False"), "{p} tempHAlarm");
            assert_eq!(ft.dom_flag.hget(p, "vccHAlarm").as_deref(), Some("False"), "{p} vccHAlarm");
            assert_eq!(ft.status_flag.hget(p, "tx_fault").as_deref(), Some("False"), "{p} status flag");
        }
        // FLAGS ONLY: DOM_SENSOR and TRANSCEIVER_STATUS are NOT published by the flag prime.
        assert_eq!(t.dom.get_size(), 0, "DOM_SENSOR must not be published by the flag prime");
        assert_eq!(ft.status.get_size(), 0, "TRANSCEIVER_STATUS must not be published by the flag prime");
    }

    // The boot flag prime honors the same gate chain as poll_once: a dom-monitoring-disabled
    // port and a blocking-error port are skipped, while a clean present port is published.
    #[test]
    fn test_prime_flag_baselines_respects_gates() {
        let pm = mapping_with(&[("Ethernet0", 0), ("Ethernet4", 1), ("Ethernet8", 2)]);
        let t = tables();
        // Ethernet4: DOM polling disabled -> skipped. Ethernet8: blocking error -> skipped.
        t.cfg_port.hset("Ethernet4", "dom_polling", "disabled");
        t.status_sw.set(
            "Ethernet8",
            &[
                ("status".to_string(), "1".to_string()),
                ("error".to_string(), SFP_ERROR_DESCRIPTION_BLOCKING.to_string()),
            ],
        );
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp(), flagged_sfp(), flagged_sfp()]));
        let mut task = dom_task(pm, true, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);

        task.prime_flag_baselines(&AtomicBool::new(false));

        assert_eq!(ft.dom_flag.hget("Ethernet0", "tempHAlarm").as_deref(), Some("True"));
        // First-publish flag metadata is seeded for the healthy port (count 0, time 'never').
        assert_eq!(ft.dom_flag_cc.hget("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(ft.dom_flag_st.hget("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        assert!(ft.dom_flag.get("Ethernet4").is_none(), "dom-disabled port must be skipped");
        assert!(ft.dom_flag.get("Ethernet8").is_none(), "blocking-error port must be skipped");
    }

    // REGRESSION LOCK for tests/test_dom_flag_meta.py::test_dom_flag_groups_temp_and_vcc:
    // the BOOT flag prime runs BEFORE the CMIS bring-up thread is spawned, so at prime time
    // every present port's cmis_state is still ABSENT (reads back UNKNOWN, which is
    // non-terminal) — and an admin-up port may even already show a non-terminal INSERTED.
    // The prime must publish the latched DOM_FLAG/STATUS_FLAG baseline for BOTH regardless,
    // because the CMIS-init gate is deliberately DROPPED from the boot prime (keeping it, via
    // is_port_dom_monitoring_disabled, would treat UNKNOWN/INSERTED as "in init" and skip
    // EVERY port at boot, pushing the whole DOM_FLAG row onto the ~60s periodic poll and past
    // the e2e T_DOM budget). A port whose dom_polling is disabled is still skipped.
    #[test]
    fn test_prime_flag_baselines_publishes_when_cmis_state_absent_or_non_terminal() {
        let pm = mapping_with(&[("Ethernet0", 0), ("Ethernet4", 1), ("Ethernet8", 2)]);
        let t = tables();
        // Ethernet0: cmis_state ABSENT (the real boot instant, reads UNKNOWN = non-terminal).
        // Ethernet4: cmis_state INSERTED (an explicit non-terminal bring-up state).
        t.status_sw.hset("Ethernet4", "cmis_state", "INSERTED");
        // Ethernet8: same non-terminal state but dom_polling disabled -> still skipped.
        t.status_sw.hset("Ethernet8", "cmis_state", "INSERTED");
        t.cfg_port.hset("Ethernet8", "dom_polling", "disabled");
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp(), flagged_sfp(), flagged_sfp()]));
        let mut task = dom_task(pm, false, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);

        // Sanity: the (kept) steady-state gate WOULD skip these non-terminal ports — proving
        // the boot prime publishes them precisely because it drops the CMIS-init gate.
        assert!(task.is_port_dom_monitoring_disabled("Ethernet0"), "absent cmis_state gates steady-state");
        assert!(task.is_port_dom_monitoring_disabled("Ethernet4"), "INSERTED gates steady-state");

        task.prime_flag_baselines(&AtomicBool::new(false));

        for p in ["Ethernet0", "Ethernet4"] {
            assert_eq!(ft.dom_flag.hget(p, "tempHAlarm").as_deref(), Some("True"), "{p} flag published at boot");
            assert_eq!(ft.status_flag.hget(p, "tx_fault").as_deref(), Some("False"), "{p} status flag published");
        }
        assert!(
            ft.dom_flag.get("Ethernet8").is_none(),
            "dom-disabled port stays skipped even at boot prime",
        );
    }

    // A no-op without the flag/status tables wired (the gate-only unit-test daemon), and it
    // never panics on an absent module (the poster's presence gate handles it).
    #[test]
    fn test_prime_flag_baselines_noop_without_tables_and_survives_absent() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp::default()])); // presence=false
        let task = dom_task(pm, true, Some(60), hal, &t);
        // No FlagTables wired -> early return, must not panic.
        task.prime_flag_baselines(&AtomicBool::new(false));
        assert_eq!(t.dom.get_size(), 0);
    }

    // REGRESSION LOCK for tests/test_dom_flag_meta.py::test_dom_flag_groups_temp_and_vcc
    // (the mid-session replug case the one-shot boot prime cannot cover): the fast loop
    // must re-publish the latched DOM_FLAG/STATUS_FLAG baseline the MOMENT a port's CMIS
    // bring-up reaches a terminal state (the DOM gate releasing), off the ~60s periodic
    // poll. prime_flags_on_cmis_gate_release fires exactly on the non-terminal->terminal
    // rising edge; while the port is still in CMIS init it stays withheld (test_dom_gating).
    #[test]
    fn test_prime_flags_on_cmis_gate_release_publishes_on_terminal_edge() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        // Bring-up in progress: cmis_state non-terminal, DOM gate CLOSED.
        t.status_sw.hset("Ethernet0", "cmis_state", "INSERTED");
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, false, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);
        let stop = AtomicBool::new(false);

        // First observation while non-terminal: seed only, NOTHING published (the gate is
        // closed and this is the port's first sighting).
        task.prime_flags_on_cmis_gate_release(&stop);
        assert!(ft.dom_flag.get("Ethernet0").is_none(), "no publish while cmis_state non-terminal");
        // Still non-terminal on the next sweep: still withheld (respects test_dom_gating).
        task.prime_flags_on_cmis_gate_release(&stop);
        assert!(ft.dom_flag.get("Ethernet0").is_none(), "still withheld while in CMIS init");

        // Bring-up completes -> cmis_state terminal. The rising edge re-publishes the latched
        // flag baseline (+ first-publish metadata) via the fast loop, index-independently.
        t.status_sw.hset("Ethernet0", "cmis_state", "READY");
        task.prime_flags_on_cmis_gate_release(&stop);
        assert_eq!(ft.dom_flag.hget("Ethernet0", "tempHAlarm").as_deref(), Some("True"));
        assert_eq!(ft.dom_flag_cc.hget("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(ft.status_flag.hget("Ethernet0", "tx_fault").as_deref(), Some("False"));
    }

    // A steady terminal port whose latched baseline is ALREADY PRESENT (published by the
    // boot prime — e.g. Ethernet48, which stays present+READY with its DOM_FLAG row
    // throughout tests/test_link_change_flags.py) is NOT republished by a routine sweep:
    // neither the rising edge (it never left terminal) nor the missing-baseline restore
    // (the row is present) fires, so the sweep can never surface a raised flag inside that
    // test's pre-flap guard window (protects the self-declared coincident-poll flake).
    #[test]
    fn test_prime_flags_on_cmis_gate_release_no_republish_for_steady_terminal() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "READY"); // already terminal
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, false, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);
        let stop = AtomicBool::new(false);

        // The boot prime already latched this port's baseline; model it with a SENTINEL the
        // SFP read ("True") would overwrite IF a sweep spuriously republished.
        ft.dom_flag
            .set("Ethernet0", &[("tempHAlarm".to_string(), "SENTINEL".to_string())]);

        // Seed-on-first-sight (terminal) with a PRESENT row -> NO publish across repeated
        // steady-state sweeps: the value stays the SENTINEL, never re-read from the module.
        task.prime_flags_on_cmis_gate_release(&stop);
        task.prime_flags_on_cmis_gate_release(&stop);
        assert_eq!(
            ft.dom_flag.hget("Ethernet0", "tempHAlarm").as_deref(),
            Some("SENTINEL"),
            "steady-state terminal port with a present baseline must not be republished",
        );
    }

    // REGRESSION LOCK for tests/test_dom_flag_meta.py::test_dom_flag_groups_temp_and_vcc in
    // the boot-prime-miss / edge-miss case: a present, DOM-enabled, TERMINAL port whose
    // TRANSCEIVER_DOM_FLAG row is ABSENT from STATE_DB has its latched baseline RESTORED by
    // the fast loop even WITHOUT a fresh non-terminal->terminal edge (the port was already
    // terminal on first sighting, so no rising edge ever fires). This closes the gap where
    // the boot `prime_flag_baselines` sweep missed the port (EEPROM not yet readable at
    // boot) and the only remaining republish path was the interval-gated (~60s) periodic
    // poll, which under the real M8 datapath bring-up lands a late-index port (Ethernet100,
    // physical index 25) past the e2e T_DOM (80s) budget. The restore is DB-gated (fires
    // only while the row is truly absent) and terminal-only (a still-initializing port stays
    // non-terminal -> withheld, preserving test_dom_gating).
    #[test]
    fn test_prime_flags_on_cmis_gate_release_restores_missing_baseline() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        // Terminal from the first sighting (a boot-prime miss: no rising edge will fire) and
        // the DOM_FLAG row is absent.
        t.status_sw.hset("Ethernet0", "cmis_state", "READY");
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, false, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);
        let stop = AtomicBool::new(false);
        assert!(
            ft.dom_flag.get("Ethernet0").is_none(),
            "precondition: baseline absent (boot prime missed it)",
        );

        // A single fast-loop sweep restores the latched baseline (+ first-publish metadata),
        // index-independently — no rising edge and no ~60s periodic poll required.
        task.prime_flags_on_cmis_gate_release(&stop);
        assert_eq!(ft.dom_flag.hget("Ethernet0", "tempHAlarm").as_deref(), Some("True"));
        assert_eq!(ft.dom_flag_cc.hget("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(ft.status_flag.hget("Ethernet0", "tx_fault").as_deref(), Some("False"));

        // Once present, a subsequent sweep does NOT re-read/republish (DB-gated, self-limiting).
        ft.dom_flag
            .set("Ethernet0", &[("tempHAlarm".to_string(), "SENTINEL".to_string())]);
        task.prime_flags_on_cmis_gate_release(&stop);
        assert_eq!(
            ft.dom_flag.hget("Ethernet0", "tempHAlarm").as_deref(),
            Some("SENTINEL"),
            "restore must not re-fire once the row is present",
        );
    }

    // The missing-baseline restore is TERMINAL-only: a present port that is genuinely still
    // in CMIS init (non-terminal cmis_state) with an ABSENT DOM_FLAG row is NOT restored, so
    // the CMIS-init DOM gate and tests/test_dom_gating.py still hold under the new path.
    #[test]
    fn test_prime_flags_on_cmis_gate_release_does_not_restore_while_non_terminal() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "DP_INIT"); // non-terminal (in CMIS init)
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, false, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);
        let stop = AtomicBool::new(false);

        // Repeated sweeps while non-terminal: the missing row is NEVER restored.
        task.prime_flags_on_cmis_gate_release(&stop);
        task.prime_flags_on_cmis_gate_release(&stop);
        assert!(
            ft.dom_flag.get("Ethernet0").is_none(),
            "DOM_FLAG must stay withheld while cmis_state is non-terminal (DOM gating)",
        );
    }

    // With skip_cmis_mgr there is no CMIS gate to release, so the sweep is inert even across a
    // non-terminal->terminal transition (the boot prime + periodic poll own the flags there).
    #[test]
    fn test_prime_flags_on_cmis_gate_release_inert_when_skip_cmis() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        t.status_sw.hset("Ethernet0", "cmis_state", "INSERTED");
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, true, Some(60), hal, &t); // skip_cmis = true
        let ft = FlagTables::new();
        ft.wire(&mut task);
        let stop = AtomicBool::new(false);

        task.prime_flags_on_cmis_gate_release(&stop);
        t.status_sw.hset("Ethernet0", "cmis_state", "READY");
        task.prime_flags_on_cmis_gate_release(&stop);
        assert!(
            ft.dom_flag.get("Ethernet0").is_none(),
            "skip_cmis_mgr short-circuits the gate-release sweep",
        );
    }

    // on_port_update_event records an APPL_DB PORT_SET (a flap) for a fast re-read
    // ~DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE later; other events are ignored.
    #[test]
    fn test_on_port_update_event_schedules_appl_db_port_set() {
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![]));
        let task = dom_task(mapping_with(&[("Ethernet0", 0)]), true, Some(60), hal, &t);

        // A non-APPL_DB PORT_SET is ignored.
        let mut ev = PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::PortSet);
        ev.db_name = Some("CONFIG_DB".to_string());
        task.on_port_update_event(&ev);
        assert!(task.link_change_affected_ports.lock().unwrap().is_empty());

        // An APPL_DB PORT_SET schedules the physical port, due in the future (the
        // settle delay before the fast re-read).
        let mut ev = PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::PortSet);
        ev.db_name = Some("APPL_DB".to_string());
        task.on_port_update_event(&ev);
        let map = task.link_change_affected_ports.lock().unwrap();
        assert!(map.contains_key(&0));
        assert!(map[&0] > Instant::now());
    }

    // check_port_update runs a due port's flag re-read and drops it from the pending
    // set (poll_flap_events is a no-op with no APPL_DB table wired).
    #[test]
    fn test_check_port_update_processes_due_port() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, true, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);

        // Mark Ethernet0 (phys 0) due now.
        task.link_change_affected_ports
            .lock()
            .unwrap()
            .insert(0, Instant::now());
        task.check_port_update(&AtomicBool::new(false));

        assert_eq!(ft.dom_flag.hget("Ethernet0", "tempHAlarm").as_deref(), Some("True"));
        assert_eq!(ft.status_flag.hget("Ethernet0", "tx_fault").as_deref(), Some("False"));
        assert!(task.link_change_affected_ports.lock().unwrap().is_empty());
    }

    // A flap_count bump in the watched APPL_DB PORT_TABLE schedules a fast re-read;
    // the first observation only seeds the baseline (no spurious re-read).
    #[test]
    fn test_poll_flap_events_schedules_on_flap_count_bump() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let hal = Arc::new(MockHal::with_sfps(vec![flagged_sfp()]));
        let mut task = dom_task(pm, true, Some(60), hal, &t);
        let ft = FlagTables::new();
        ft.wire(&mut task);
        let appl = Arc::new(MockDbTable::new("PORT_TABLE"));
        appl.hset("Ethernet0", "flap_count", "1");
        task.set_appl_port_table(appl.clone());

        let stop = AtomicBool::new(false);
        // First pass only seeds the baseline (Ethernet0 -> "1"); no schedule.
        task.check_port_update(&stop);
        assert!(task.link_change_affected_ports.lock().unwrap().is_empty());

        // Flap: flap_count changes -> scheduled for a fast re-read.
        appl.hset("Ethernet0", "flap_count", "2");
        task.check_port_update(&stop);
        assert!(task.link_change_affected_ports.lock().unwrap().contains_key(&0));
    }

    // --- DomThermalInfoUpdateTask -------------------------------------------------

    fn thermal_task(pm: PortMapping, hal: Arc<dyn Hal>, t: &Tables, temp_tbl: Arc<MockDbTable>) -> DomThermalInfoUpdateTask {
        DomThermalInfoUpdateTask::new(
            pm,
            10,
            hal,
            temp_tbl,
            t.status_sw.clone(),
            t.cfg_port.clone(),
        )
    }

    // tests/test_xcvrd.py:test_DomThermalInfoUpdateTask_task_worker (adapted): a
    // present, enabled port posts TRANSCEIVER_DOM_TEMPERATURE; a dom_polling=disabled
    // port is skipped; an unknown-asic port is skipped.
    #[test]
    fn test_dom_thermal_task_worker() {
        let mut pm = mapping_with(&[("Ethernet0", 1), ("Ethernet4", 2)]);
        // Ethernet8 present on physical 3 but with NO asic id (mirrors the Python
        // logical_to_asic[Ethernet8]=None case): add it only to physical_to_logical.
        pm.physical_to_logical.insert(3, vec!["Ethernet8".to_string()]);

        let t = tables();
        let temp_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_TEMPERATURE"));
        // Ethernet4 disabled; Ethernet0 enabled (default).
        t.cfg_port.hset("Ethernet4", "dom_polling", "disabled");

        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![
            MockSfp::present().with_json("get_temperature", json!("30.0")), // phys 0 (unused)
            MockSfp::present().with_json("get_temperature", json!("31.0")), // phys 1 Ethernet0
            MockSfp::present().with_json("get_temperature", json!("32.0")), // phys 2 Ethernet4
            MockSfp::present().with_json("get_temperature", json!("33.0")), // phys 3 Ethernet8
        ]));
        let task = thermal_task(pm, hal, &t, temp_tbl.clone());
        let stop = AtomicBool::new(false);

        task.poll_once(&stop);
        // Ethernet0 enabled+present -> posted (temperature + last_update_time = 2).
        assert_eq!(temp_tbl.get_size_for_key("Ethernet0"), 2);
        // Ethernet4 dom_polling=disabled -> skipped.
        assert!(temp_tbl.get("Ethernet4").is_none());
        // Ethernet8 has no asic id -> skipped.
        assert!(temp_tbl.get("Ethernet8").is_none());
    }

    #[test]
    fn test_dom_thermal_task_worker_runs_and_stops() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let temp_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_TEMPERATURE"));
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![
            MockSfp::present().with_json("get_temperature", json!("42.0")),
        ]));
        let mut task = thermal_task(pm, hal, &t, temp_tbl.clone());
        task.poll_interval = 1;

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = stop.clone();
            std::thread::spawn(move || task.run(stop))
        };
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("thermal worker joins cleanly");
        assert_eq!(temp_tbl.get_size_for_key("Ethernet0"), 2);
    }

    // ---- M5: PM / firmware / VDM freeze-conditions posters -----------------------

    use crate::xcvrd_utilities::xcvr_table_helper::VDM_THRESHOLD_TYPES;

    fn firmware_versions() -> Value {
        json!({"active_firmware": "1.0.1", "inactive_firmware": "1.0.2"})
    }

    // Six PM observables, mirroring the emulator's get_transceiver_pm (the Python
    // test asserts get_size_for_key == 6).
    fn pm_values() -> Value {
        json!({
            "prefec_ber_avg": 0.001,
            "prefec_ber_min": 0.0008,
            "prefec_ber_max": 0.0012,
            "uncorr_frames_avg": 0.0,
            "uncorr_frames_min": 0.0,
            "uncorr_frames_max": 0.0,
        })
    }

    // tests/test_xcvrd.py:test_post_port_pm_info_to_db — a present, non-flat module
    // with a non-empty PM dict posts one beautified row (6 fields, no
    // last_update_time).
    #[test]
    fn test_post_port_pm_info_to_db() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let pm_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_PM"));
        let sfp = MockSfp::present()
            .with_json("is_flat_memory", json!(false))
            .with_json("get_transceiver_pm", pm_values());
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![sfp]));
        let task = dom_task(pm, true, None, hal, &t);
        let stop = AtomicBool::new(false);

        task.post_port_pm_info_to_db(&stop, "Ethernet0", &*pm_tbl);
        assert_eq!(pm_tbl.get_size_for_key("Ethernet0"), 6);
        // No last_update_time on PM rows (parity with Python).
        assert!(pm_tbl.hget("Ethernet0", "last_update_time").is_none());
    }

    // is_flat_memory == True → the flat/SFF module has no PM page; skip it.
    #[test]
    fn test_post_port_pm_info_to_db_skips_flat_memory() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let pm_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_PM"));
        let sfp = MockSfp::present()
            .with_json("is_flat_memory", json!(true))
            .with_json("get_transceiver_pm", pm_values());
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![sfp]));
        let task = dom_task(pm, true, None, hal, &t);
        let stop = AtomicBool::new(false);
        task.post_port_pm_info_to_db(&stop, "Ethernet0", &*pm_tbl);
        assert!(pm_tbl.get("Ethernet0").is_none());
    }

    // An empty PM dict (API not applicable) and an absent module both post nothing.
    #[test]
    fn test_post_port_pm_info_to_db_skips_empty_and_absent() {
        let pm = mapping_with(&[("Ethernet0", 0), ("Ethernet4", 1)]);
        let t = tables();
        let pm_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_PM"));
        let present_empty = MockSfp::present()
            .with_json("is_flat_memory", json!(false))
            .with_json("get_transceiver_pm", json!({}));
        let absent = MockSfp::default(); // presence == false
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![present_empty, absent]));
        let task = dom_task(pm, true, None, hal, &t);
        let stop = AtomicBool::new(false);
        task.post_port_pm_info_to_db(&stop, "Ethernet0", &*pm_tbl);
        task.post_port_pm_info_to_db(&stop, "Ethernet4", &*pm_tbl);
        assert!(pm_tbl.get("Ethernet0").is_none());
        assert!(pm_tbl.get("Ethernet4").is_none());
    }

    // tests/test_xcvrd.py:test_post_port_sfp_firmware_info_to_db — active/inactive
    // firmware is written to EVERY logical port of the physical port (breakout).
    #[test]
    fn test_post_port_sfp_firmware_info_to_db() {
        // Ethernet0 and Ethernet4 share physical 1 → both get the 2-field row.
        let pm = mapping_with(&[("Ethernet0", 1), ("Ethernet4", 1)]);
        let t = tables();
        let fw_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_FIRMWARE_INFO"));
        let sfp0 = MockSfp::default(); // phys 0 unused
        let sfp1 = MockSfp::present()
            .with_json("get_transceiver_info_firmware_versions", firmware_versions());
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![sfp0, sfp1]));
        let task = dom_task(pm, true, None, hal, &t);
        let stop = AtomicBool::new(false);

        task.post_port_sfp_firmware_info_to_db(&stop, "Ethernet0", &*fw_tbl);
        assert_eq!(fw_tbl.get_size_for_key("Ethernet0"), 2);
        assert_eq!(fw_tbl.get_size_for_key("Ethernet4"), 2);
        assert_eq!(
            fw_tbl.hget("Ethernet0", "active_firmware").as_deref(),
            Some("1.0.1")
        );
        assert_eq!(
            fw_tbl.hget("Ethernet4", "inactive_firmware").as_deref(),
            Some("1.0.2")
        );
    }

    // An absent module posts nothing; a raised stop aborts before any write.
    #[test]
    fn test_post_port_sfp_firmware_info_to_db_absent_and_stop() {
        let t = tables();
        let fw_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_FIRMWARE_INFO"));

        let pm = mapping_with(&[("Ethernet0", 1)]);
        let hal: Arc<dyn Hal> =
            Arc::new(MockHal::with_sfps(vec![MockSfp::default(), MockSfp::default()]));
        let task = dom_task(pm, true, None, hal, &t);
        task.post_port_sfp_firmware_info_to_db(&AtomicBool::new(false), "Ethernet0", &*fw_tbl);
        assert!(fw_tbl.get("Ethernet0").is_none());

        let pm2 = mapping_with(&[("Ethernet0", 1)]);
        let sfp1 = MockSfp::present()
            .with_json("get_transceiver_info_firmware_versions", firmware_versions());
        let hal2: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![MockSfp::default(), sfp1]));
        let task2 = dom_task(pm2, true, None, hal2, &t);
        // stop already raised → break before the write.
        task2.post_port_sfp_firmware_info_to_db(&AtomicBool::new(true), "Ethernet0", &*fw_tbl);
        assert!(fw_tbl.get("Ethernet0").is_none());
    }

    // tests/test_xcvrd.py:test_post_port_sfp_firmware_info_to_db_lport_list_None —
    // when get_physical_to_logical returns None (no logical list for the physical
    // port), the firmware row is not written.
    #[test]
    fn test_post_port_sfp_firmware_info_to_db_lport_list_none() {
        let mut pm = PortMapping::new();
        pm.logical_port_list.push("Ethernet0".to_string());
        pm.logical_to_physical.insert("Ethernet0".to_string(), 1);
        // Deliberately leave physical_to_logical[1] unset → get_physical_to_logical None.
        let t = tables();
        let fw_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_FIRMWARE_INFO"));
        let sfp1 = MockSfp::present()
            .with_json("get_transceiver_info_firmware_versions", firmware_versions());
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![MockSfp::default(), sfp1]));
        let task = dom_task(pm, true, None, hal, &t);
        task.post_port_sfp_firmware_info_to_db(&AtomicBool::new(false), "Ethernet0", &*fw_tbl);
        assert!(fw_tbl.get("Ethernet0").is_none());
    }

    struct VpfProbe {
        real_value: Arc<MockDbTable>,
        pm: Arc<MockDbTable>,
        firmware: Arc<MockDbTable>,
    }

    fn vpf_tables() -> (VdmPmFirmwareTables, VpfProbe) {
        let real_value = Arc::new(MockDbTable::new("TRANSCEIVER_VDM_REAL_VALUE"));
        let pm_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_PM"));
        let firmware = Arc::new(MockDbTable::new("TRANSCEIVER_FIRMWARE_INFO"));
        let mut flag = HashMap::new();
        let mut change_count = HashMap::new();
        let mut set_time = HashMap::new();
        let mut clear_time = HashMap::new();
        for ty in VDM_THRESHOLD_TYPES {
            flag.insert(
                ty.to_string(),
                Arc::new(MockDbTable::new(format!("VDM_{ty}_FLAG"))) as Arc<dyn DbTable>,
            );
            change_count.insert(
                ty.to_string(),
                Arc::new(MockDbTable::new(format!("VDM_{ty}_CC"))) as Arc<dyn DbTable>,
            );
            set_time.insert(
                ty.to_string(),
                Arc::new(MockDbTable::new(format!("VDM_{ty}_ST"))) as Arc<dyn DbTable>,
            );
            clear_time.insert(
                ty.to_string(),
                Arc::new(MockDbTable::new(format!("VDM_{ty}_CT"))) as Arc<dyn DbTable>,
            );
        }
        let real_dyn: Arc<dyn DbTable> = real_value.clone();
        let pm_dyn: Arc<dyn DbTable> = pm_tbl.clone();
        let fw_dyn: Arc<dyn DbTable> = firmware.clone();
        let vpf = VdmPmFirmwareTables {
            vdm_real_value_tbl: real_dyn,
            vdm_flag_tables: VdmFlagTables {
                flag,
                change_count,
                set_time,
                clear_time,
            },
            pm_tbl: pm_dyn,
            firmware_info_tbl: fw_dyn,
        };
        (
            vpf,
            VpfProbe {
                real_value,
                pm: pm_tbl,
                firmware,
            },
        )
    }

    // A fully VDM/statistic-capable module with every VDM getter canned; individual
    // freeze-condition tests tweak one field (statistic support, lpmode, freeze OK).
    fn full_vdm_sfp() -> MockSfp {
        MockSfp::present()
            .with_json("is_transceiver_vdm_supported", json!(true))
            .with_json("is_vdm_statistic_supported", json!(true))
            .with_json("freeze_vdm_stats", json!(true))
            .with_json("get_vdm_freeze_status", json!(true))
            .with_json("unfreeze_vdm_stats", json!(true))
            .with_json("get_vdm_unfreeze_status", json!(true))
            .with_json(
                "get_transceiver_vdm_real_value_statistic",
                json!({"laser_temperature_media1_max": 50.0}),
            )
            .with_json(
                "get_transceiver_vdm_real_value_basic",
                json!({"laser_temperature_media1": 45.0}),
            )
            .with_json(
                "get_transceiver_vdm_flags",
                json!({"laser_temperature_media1_halarm": false}),
            )
            .with_json("is_flat_memory", json!(false))
            .with_json("get_transceiver_pm", pm_values())
            .with_json("get_transceiver_info_firmware_versions", firmware_versions())
    }

    // tests/test_xcvrd.py:test_DomInfoUpdateTask_task_worker_vdm_freeze_conditions
    // (full path) — statistic supported + admin-up: freeze, capture statistic + PM,
    // publish firmware, and the merged (basic+statistic) real value + flags.
    #[test]
    fn test_post_port_vdm_pm_firmware_full_freeze() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let sfp = full_vdm_sfp();
        let log = sfp.call_log.clone();
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![sfp]));
        let task = dom_task(pm, true, None, hal, &t);
        let (vpf, probe) = vpf_tables();
        let stop = AtomicBool::new(false);

        task.post_port_vdm_pm_firmware_info(&stop, "Ethernet0", &vpf);

        // Firmware + PM (under freeze) published.
        assert_eq!(probe.firmware.get_size_for_key("Ethernet0"), 2);
        assert_eq!(probe.pm.get_size_for_key("Ethernet0"), 6);
        // Real value merges basic + statistic (+ last_update_time).
        assert_eq!(
            probe.real_value.hget("Ethernet0", "laser_temperature_media1").as_deref(),
            Some("45.0")
        );
        assert_eq!(
            probe.real_value.hget("Ethernet0", "laser_temperature_media1_max").as_deref(),
            Some("50.0")
        );
        assert!(probe.real_value.hget("Ethernet0", "last_update_time").is_some());
        // Freeze/unfreeze handshake ran.
        let calls = log.lock().unwrap();
        assert!(calls.iter().any(|c| c == "freeze_vdm_stats"));
        assert!(calls.iter().any(|c| c == "unfreeze_vdm_stats"));
    }

    // No statistic support → no freeze, no PM; firmware + basic real value + flags
    // still published.
    #[test]
    fn test_post_port_vdm_pm_firmware_skips_freeze_when_no_statistic() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let sfp = full_vdm_sfp().with_json("is_vdm_statistic_supported", json!(false));
        let log = sfp.call_log.clone();
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![sfp]));
        let task = dom_task(pm, true, None, hal, &t);
        let (vpf, probe) = vpf_tables();

        task.post_port_vdm_pm_firmware_info(&AtomicBool::new(false), "Ethernet0", &vpf);

        assert_eq!(probe.firmware.get_size_for_key("Ethernet0"), 2);
        assert!(probe.pm.get("Ethernet0").is_none());
        // Basic-only real value (no statistic key).
        assert!(probe.real_value.hget("Ethernet0", "laser_temperature_media1").is_some());
        assert!(probe
            .real_value
            .hget("Ethernet0", "laser_temperature_media1_max")
            .is_none());
        assert!(!log.lock().unwrap().iter().any(|c| c == "freeze_vdm_stats"));
    }

    // Low-power mode → skip freeze even when statistic is supported (no PM).
    #[test]
    fn test_post_port_vdm_pm_firmware_skips_freeze_in_lpmode() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let mut sfp = full_vdm_sfp();
        sfp.lpmode = true;
        let log = sfp.call_log.clone();
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![sfp]));
        let task = dom_task(pm, true, None, hal, &t);
        let (vpf, probe) = vpf_tables();

        task.post_port_vdm_pm_firmware_info(&AtomicBool::new(false), "Ethernet0", &vpf);

        assert_eq!(probe.firmware.get_size_for_key("Ethernet0"), 2);
        assert!(probe.pm.get("Ethernet0").is_none());
        assert!(probe.real_value.hget("Ethernet0", "laser_temperature_media1").is_some());
        assert!(!log.lock().unwrap().iter().any(|c| c == "freeze_vdm_stats"));
    }

    // Freeze attempted but not confirmed (action False) → frozen=false: no statistic,
    // no PM, but basic real value + flags + firmware still published, and unfreeze is
    // still issued (the Python `finally`).
    #[test]
    fn test_post_port_vdm_pm_firmware_freeze_failure_keeps_basic() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let t = tables();
        let sfp = full_vdm_sfp().with_json("freeze_vdm_stats", json!(false));
        let log = sfp.call_log.clone();
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![sfp]));
        let task = dom_task(pm, true, None, hal, &t);
        let (vpf, probe) = vpf_tables();

        task.post_port_vdm_pm_firmware_info(&AtomicBool::new(false), "Ethernet0", &vpf);

        assert_eq!(probe.firmware.get_size_for_key("Ethernet0"), 2);
        assert!(probe.pm.get("Ethernet0").is_none());
        assert!(probe.real_value.hget("Ethernet0", "laser_temperature_media1").is_some());
        assert!(probe
            .real_value
            .hget("Ethernet0", "laser_temperature_media1_max")
            .is_none());
        let calls = log.lock().unwrap();
        assert!(calls.iter().any(|c| c == "freeze_vdm_stats"));
        assert!(!calls.iter().any(|c| c == "get_transceiver_pm"));
    }
}
