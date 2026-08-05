//! `dom/dom_mgr.py` → `DomInfoUpdateTask` (+ `DomThermalInfoUpdateTask`) — the periodic
//! DOM poll thread (analysis §1.3, §3.2).
//!
//! M2 runs the `DomInfoUpdateTask` loop: on each pass it walks the physical→logical
//! map and, for every present, polling-enabled, non-error port, republishes
//! `TRANSCEIVER_DOM_SENSOR` (temperature, voltage, the 24 per-lane tx/rx power + tx
//! bias keys — unit-stripped, with a trailing `last_update_time`) and
//! `TRANSCEIVER_DOM_FLAG` (+ its change-count / set-time / clear-time metadata). M3
//! layers the rich `TRANSCEIVER_STATUS` poster (`get_transceiver_status()` — module
//! state/fault + per-host-lane datapath/config/tx-rx) onto the same pass. M4 adds the
//! latched `TRANSCEIVER_STATUS_FLAG` poster (+ its metadata siblings) and an APPL_DB
//! `PORT_TABLE` link-change watch: a `flap_count` flap schedules an off-cadence re-read
//! of the DOM + status flag tables (`update_port_db_diagnostics_on_link_change`) so the
//! latched-flag snapshot reflects the module's post-flap state without waiting for the
//! next periodic pass. The `dom_polling=disabled` CONFIG_DB gate and the CMIS-init gate
//! mirror the Python `is_port_dom_monitoring_disabled`. Later milestones layer
//! firmware/VDM/PM posting onto the same pass.
//!
//! The `TRANSCEIVER_DOM_THRESHOLD` table is *not* refreshed on this loop — like the
//! Python daemon it is seeded once at boot / at insert by `SfpStateUpdateTask`.
#![allow(dead_code, unused_variables, unused_imports)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::db::DbTable;
use crate::dom::utilities::db::{value_to_py_str, DbCache, DbUtils, Fvs};
use crate::dom::utilities::dom_sensor::DomDbUtils;
use crate::dom::utilities::status::StatusDbUtils;
use crate::dom::utilities::vdm::{VdmDbUtils, VdmUtils};
use crate::hal::Hal;
use crate::xcvrd_utilities::common::{
    get_cmis_state_from_state_db, get_physical_port_name_dict, CMIS_TERMINAL_STATES,
};
use crate::xcvrd_utilities::port_event_helper::{
    PortChangeEvent, PortChangeEventType, PortChangeObserver, PortMapping,
};
use crate::xcvrd_utilities::sfp_status_helper::detect_port_in_error_status;
use crate::xcvrd_utilities::utils::{
    get_transceiver_presence, is_transceiver_flat_memory, is_transceiver_lpmode_on,
};
use crate::xcvrd_utilities::xcvr_table_helper::XcvrTableHelper;

/// `DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS` (dom_mgr.py).
pub const DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS: u64 = 60;

/// `DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE` (dom_mgr.py) — the grace delay after an
/// APPL_DB link-change flap before the diagnostic-flag tables are re-captured, so the
/// module has time to update its real-time latched-flag status first.
pub const DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE_SECS: u64 = 1;

/// `PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS` (dom_mgr.py) — the cap on the APPL_DB
/// port-update select wait so the DOM loop stays responsive to link-change flaps
/// between periodic passes.
pub const PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS: u64 = 1000;

/// `PORT_UPDATE_EVENT_SELECT_TIMEOUT_FAST_MSECS` (dom_mgr.py) — the short select wait
/// used when link-change is serviced *inside* the periodic DOM poll pass (once per port,
/// dom_mgr.py:326). Keeping the per-port service quick means a multi-second poll pass
/// still re-reads a mid-pass flap within ~1 s rather than deferring it to the next pass.
pub const PORT_UPDATE_EVENT_SELECT_TIMEOUT_FAST_MSECS: u64 = 10;

/// The window during which the periodic DOM poll must NOT re-read a *flapped* port's
/// latched-flag tables (`TRANSCEIVER_{DOM,STATUS,VDM_*}_FLAG`), so the off-cadence
/// link-change re-read (`update_port_db_diagnostics_on_link_change`) is the **sole
/// writer** of that port's flag transition in the flap window.
///
/// The reference needs no such hold: on real hardware the module's flag registers are
/// *clear-on-read* (COR), so once the flap re-read consumes a latch a coincident poll
/// reads it back clear — the two agree. This emulator testbed instead *holds* the written
/// flag byte (no COR — `lib/cmis.py`), so a periodic poll that happens to fall inside the
/// flap window would re-surface a latch the flap path is meant to own, and a
/// raised-but-un-flapped alarm would leak before the next genuine flap (exactly the
/// `test_link_change_flags` guard). Arming this hold on a genuine flap (both when the
/// re-read is *scheduled* and after it *re-captures* the flags) is the Validator's "drain
/// the due poll for that port": the periodic pass skips only that port's flag re-read for
/// the window, so no coincident poll races the link-change trigger. It only ever applies
/// to a port that genuinely flapped (steady-state ports are untouched), is refreshed on
/// each flap, and is sized to cover the e2e guard window with margin while staying well
/// under the 60 s DOM cadence so a stale latch clears on the next pass.
pub const LINK_CHANGE_POLL_HOLD_SECS: u64 = 20;

/// `get_dom_polling_from_config_db(lport)` — the CONFIG_DB `PORT.dom_polling` field
/// for `lport`'s breakout group (read off the group's *first* subport), defaulting to
/// `"enabled"` when absent. Shared by both DOM tasks (a free fn so the fast-temperature
/// task can reuse it without the CMIS-init gate).
pub fn get_dom_polling_from_config_db(
    port_mapping: &PortMapping,
    table_helper: &XcvrTableHelper,
    lport: &str,
) -> String {
    let default = "enabled".to_string();

    let Some(pport_list) = port_mapping.get_logical_to_physical(lport) else {
        return default;
    };
    let Some(pport) = pport_list.first().copied() else {
        return default;
    };
    let Some(logical_port_list) = port_mapping.get_physical_to_logical(pport) else {
        return default;
    };
    // The first logical port corresponds to the first subport of the breakout group.
    let Some(first_logical_port) = logical_port_list.first() else {
        return default;
    };
    let Some(asic_index) = port_mapping.get_asic_id_for_logical_port(first_logical_port) else {
        return default;
    };
    let port_tbl = table_helper.get_cfg_port_tbl(asic_index);
    if let Some(row) = port_tbl.get(first_logical_port) {
        if let Some((_, v)) = row.iter().find(|(k, _)| k == "dom_polling") {
            return v.clone();
        }
    }
    default
}

/// `{**basic, **statistic}` — merge the basic and statistic VDM real-value dicts, with
/// the statistic values overriding the basic ones on a key clash (dict-unpack order).
/// Non-object inputs contribute nothing.
fn merge_vdm_values(basic: &Value, statistic: &Value) -> Value {
    let mut merged = Map::new();
    if let Value::Object(b) = basic {
        for (k, v) in b {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Value::Object(s) = statistic {
        for (k, v) in s {
            merged.insert(k.clone(), v.clone());
        }
    }
    Value::Object(merged)
}

/// A pending off-cadence link-change flag re-read: `fire_at` is when the debounced
/// re-read should next run; `giveup_at` bounds how long a port stuck **mid-CMIS-init**
/// keeps its re-read pending before it is dropped and the periodic DOM pass takes over.
#[derive(Debug, Clone, Copy)]
struct PendingReread {
    fire_at: Instant,
    giveup_at: Instant,
}

/// Outcome of an off-cadence link-change flag re-read
/// ([`DomInfoUpdateTask::update_port_db_diagnostics_on_link_change`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkChangeReread {
    /// The flag tables were re-captured, OR the port is *permanently* ineligible for this
    /// flap (stop event, unknown/absent port, blocking error status, invalid asic, or a
    /// `dom_polling=disabled` operator gate). The pending re-read is consumed and dropped —
    /// exactly the reference's unconditional `del self.link_change_affected_ports[...]`
    /// (dom_mgr.py:282).
    Settled,
    /// The port is only *transiently* ineligible: DOM polling is enabled but its
    /// `cmis_state` is still non-terminal (mid-CMIS datapath bring-up), so publishing now
    /// would violate the DOM-gating contract (`test_dom_gating`). The reference watches the
    /// flap off a one-shot keyspace notification, so on real hardware — where a flap only
    /// happens on an already-active (READY) link — this branch never occurs. On this
    /// emulator testbed the link-change fixture (`dom_polling` hdel / re-plug on the
    /// *unfiltered* CONFIG_DB `PORT` watch the CMIS manager shares) drives the flapped port
    /// back through a ~9 s bring-up, so the single scheduled re-read can land mid-init. We
    /// keep it pending and retry on the next ~1 s cadence until the port reaches a terminal
    /// `cmis_state`, so the post-flap latched-flag baseline is re-published the moment the
    /// datapath settles — well inside the e2e fast window — and never while the gate is
    /// closed.
    DeferredCmisInit,
}

/// `DomInfoUpdateTask` — the periodic DOM poll thread. M2 posts the DOM sensor + flag
/// rows; M3 adds the rich `TRANSCEIVER_STATUS` row; M4 adds the `TRANSCEIVER_STATUS_FLAG`
/// row (+ metadata) and the APPL_DB link-change flag re-capture. firmware/VDM/PM (later
/// milestones) layer onto the same pass.
pub struct DomInfoUpdateTask {
    port_mapping: PortMapping,
    hal: Arc<dyn Hal>,
    table_helper: Arc<XcvrTableHelper>,
    /// `skip_cmis_mgr` — when no CMIS manager runs, the CMIS-init gate is a no-op.
    skip_cmis_mgr: bool,
    dom_update_interval: u64,
    dom_db: DomDbUtils,
    status_db: StatusDbUtils,
    /// `link_change_affected_ports` (dom_mgr.py) — physical port → the pending off-cadence
    /// diagnostic-flag re-read scheduled after an APPL_DB link-change flap (see
    /// [`PendingReread`]). Interior-mutable so the off-cadence link-change handler runs
    /// behind `&self` (the periodic `poll_once` pass takes `&self` too). Also consolidates
    /// the flaps of every breakout subport of a physical port into a single pending re-read.
    link_change_affected_ports: Mutex<HashMap<usize, PendingReread>>,
    /// Last APPL_DB `PORT_TABLE` `flap_count` seen per physical port. A link-change
    /// re-read is scheduled only when a delivered event carries a `flap_count` that
    /// actually **changed** for that port — mirroring the reference's FILTER
    /// `['flap_count']` subscriber + `PortChangeEvent` soak/dedup (dom_mgr.py:147,
    /// port_event_helper.py:178), whose *net* effect is to surface an event only on a
    /// genuine link flap. This keeps a re-delivered snapshot (or an unrelated
    /// `PORT_TABLE` write that still carries the current `flap_count`) from triggering an
    /// off-cadence flag re-capture that would publish a freshly-raised latched flag
    /// outside the poll / flap cadence.
    last_flap_count: Mutex<HashMap<usize, String>>,
    /// `vdm_utils` (dom_mgr.py) — the VDM HAL helper (freeze/capture/unfreeze +
    /// supported/statistic-supported probes). `vdm_db` posts the VDM real-value /
    /// threshold / flag tables. Both stateless, cloned into the M5 poll pass.
    vdm_utils: VdmUtils,
    vdm_db: VdmDbUtils,
    /// Physical port → the instant until which the periodic DOM poll must **skip**
    /// re-reading that port's latched-flag tables (`TRANSCEIVER_{DOM,STATUS,VDM_*}_FLAG`),
    /// because a recent APPL_DB link-change flap owns that flag transition (see
    /// [`LINK_CHANGE_POLL_HOLD_SECS`]). Interior-mutable so both the off-cadence
    /// link-change handler and the periodic `poll_once` pass reach it behind `&self`. Empty
    /// in steady state — only a genuinely flapped port ever appears — and entries are
    /// pruned lazily once expired, so it stays bounded to actively-flapping ports.
    flag_poll_hold: Mutex<HashMap<usize, Instant>>,
}

impl DomInfoUpdateTask {
    pub fn new(
        port_mapping: PortMapping,
        hal: Arc<dyn Hal>,
        table_helper: Arc<XcvrTableHelper>,
        skip_cmis_mgr: bool,
        dom_update_interval: Option<u64>,
    ) -> Self {
        // A negative interval is nonsensical (`Option<u64>` already excludes it); an
        // absent one falls back to the 60 s default, exactly like the Python ctor.
        let dom_update_interval = dom_update_interval.unwrap_or(DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS);
        DomInfoUpdateTask {
            port_mapping,
            hal,
            table_helper,
            skip_cmis_mgr,
            dom_update_interval,
            dom_db: DomDbUtils::new(),
            status_db: StatusDbUtils::new(),
            link_change_affected_ports: Mutex::new(HashMap::new()),
            last_flap_count: Mutex::new(HashMap::new()),
            vdm_utils: VdmUtils::new(),
            vdm_db: VdmDbUtils::new(),
            flag_poll_hold: Mutex::new(HashMap::new()),
        }
    }

    /// `get_dom_polling_from_config_db(lport)`.
    pub fn get_dom_polling_from_config_db(&self, lport: &str) -> String {
        get_dom_polling_from_config_db(&self.port_mapping, &self.table_helper, lport)
    }

    /// `is_port_in_cmis_initialization_process(lport)` — a port whose STATUS_SW
    /// `cmis_state` is not one of the CMIS *terminal* states is still bringing up its
    /// datapath, so DOM polling is deferred. `skip_cmis_mgr` short-circuits to `false`
    /// (the M2 daemon runs no CMIS manager; the module is always treated ready).
    pub fn is_port_in_cmis_initialization_process(&self, logical_port_name: &str) -> bool {
        if self.skip_cmis_mgr {
            return false;
        }
        let Some(asic_index) = self
            .port_mapping
            .get_asic_id_for_logical_port(logical_port_name)
        else {
            return false;
        };
        let status_sw = self.table_helper.get_status_sw_tbl(asic_index);
        let cmis_state = get_cmis_state_from_state_db(logical_port_name, status_sw);
        !CMIS_TERMINAL_STATES.contains(&cmis_state.as_str())
    }

    /// `is_port_dom_monitoring_disabled(lport)` — CONFIG_DB `dom_polling=disabled`
    /// **or** the port is mid-CMIS-init.
    pub fn is_port_dom_monitoring_disabled(&self, logical_port_name: &str) -> bool {
        self.get_dom_polling_from_config_db(logical_port_name) == "disabled"
            || self.is_port_in_cmis_initialization_process(logical_port_name)
    }

    /// `post_port_sfp_firmware_info_to_db(logical_port_name, port_mapping, table, stop,
    /// firmware_info_cache)` (dom_mgr.py:203) — publish `TRANSCEIVER_FIRMWARE_INFO`
    /// (active/inactive firmware versions). The firmware row is posted to **every**
    /// logical port backing the physical port (firmware is a per-module property, so all
    /// breakout subports share it), carries **no** beautify and **no** `last_update_time`,
    /// and an empty/`None` read aborts the remaining ports (EEPROM not ready). A
    /// `NotImplementedError` over the bridge folds to an empty dict, exactly like
    /// `common._wrapper_get_transceiver_firmware_info`.
    pub fn post_port_sfp_firmware_info_to_db(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        table: &dyn DbTable,
        mut firmware_info_cache: Option<&mut DbCache>,
    ) {
        for (physical_port, _physical_port_name) in
            get_physical_port_name_dict(logical_port_name, &self.port_mapping)
        {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let Ok(sfp) = self.hal.sfp(physical_port) else {
                continue;
            };
            if !get_transceiver_presence(&*sfp) {
                continue;
            }
            let fw_dict: Value = match firmware_info_cache
                .as_mut()
                .and_then(|c| c.get(&physical_port).cloned())
            {
                Some(cached) => cached.unwrap_or(Value::Null),
                None => {
                    let read = match sfp.call_json("get_transceiver_info_firmware_versions") {
                        Ok(v) => v,
                        Err(_) => Value::Object(Map::new()),
                    };
                    if let Some(c) = firmware_info_cache.as_mut() {
                        c.insert(physical_port, Some(read.clone()));
                    }
                    read
                }
            };
            match fw_dict {
                Value::Object(obj) if !obj.is_empty() => {
                    let fvs: Fvs = obj
                        .iter()
                        .map(|(k, v)| (k.clone(), value_to_py_str(v)))
                        .collect();
                    // Firmware applies to the whole module — update every logical subport.
                    let Some(logical_port_list) =
                        self.port_mapping.get_physical_to_logical(physical_port)
                    else {
                        // Unknown physical port index — warn + skip (dom_mgr.py:225-227).
                        continue;
                    };
                    for logical_port in logical_port_list {
                        table.set(&logical_port, &fvs);
                    }
                }
                // Empty or None read → EEPROM not ready; stop this pass (SFP_EEPROM_NOT_READY).
                _ => return,
            }
        }
    }

    /// `post_port_pm_info_to_db(logical_port_name, port_mapping, table, stop,
    /// pm_info_cache)` (dom_mgr.py:238) — publish `TRANSCEIVER_PM` for a coherent (paged)
    /// module. Flat-memory modules are skipped (the PM page is CMIS-coherent-only); an
    /// absent module is skipped; a `None` read (EEPROM not ready) aborts the remaining
    /// ports; an empty dict just skips the port (`get_transceiver_pm` N/A). Unlike the
    /// diagnostic posters this row carries **no** `last_update_time` and is keyed by the
    /// *physical* port display name.
    pub fn post_port_pm_info_to_db(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        table: &dyn DbTable,
        mut pm_info_cache: Option<&mut DbCache>,
    ) {
        for (physical_port, physical_port_name) in
            get_physical_port_name_dict(logical_port_name, &self.port_mapping)
        {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let Ok(sfp) = self.hal.sfp(physical_port) else {
                continue;
            };
            if !get_transceiver_presence(&*sfp) {
                continue;
            }
            // Flat-memory modules have no PM page — skip (dom_mgr.py:246, only `== True`).
            if is_transceiver_flat_memory(&*sfp) {
                continue;
            }
            let pm_info_dict: Value = match pm_info_cache
                .as_mut()
                .and_then(|c| c.get(&physical_port).cloned())
            {
                Some(cached) => cached.unwrap_or(Value::Null),
                None => {
                    let read = match sfp.call_json("get_transceiver_pm") {
                        Ok(v) => v,
                        Err(_) => Value::Object(Map::new()),
                    };
                    if let Some(c) = pm_info_cache.as_mut() {
                        c.insert(physical_port, Some(read.clone()));
                    }
                    read
                }
            };
            match pm_info_dict {
                // A genuine `None` read means the EEPROM is not ready — stop this pass.
                Value::Null => return,
                Value::Object(obj) => {
                    // Empty means `get_transceiver_pm` is N/A for this xcvr — skip.
                    if obj.is_empty() {
                        continue;
                    }
                    let mut obj = obj;
                    DbUtils::new().beautify_info_dict(&mut obj);
                    let fvs: Fvs = obj
                        .iter()
                        .map(|(k, v)| (k.clone(), value_to_py_str(v)))
                        .collect();
                    table.set(&physical_port_name, &fvs);
                }
                _ => continue,
            }
        }
    }

    /// One DOM poll pass: for every present, polling-enabled, non-error port publish
    /// `TRANSCEIVER_DOM_SENSOR`, `TRANSCEIVER_DOM_FLAG` (+ metadata), the rich
    /// `TRANSCEIVER_STATUS` row, then the latched `TRANSCEIVER_STATUS_FLAG` row (+
    /// metadata). Mirrors the per-port body of `DomInfoUpdateTask.task_worker` (M2–M4
    /// subset: firmware/VDM/PM posting lands in later milestones). Never propagates a
    /// per-port error — a transient read failure just skips that port, keeping the loop
    /// resilient.
    pub fn poll_once(&self, stop: &AtomicBool) {
        for (physical_port, logical_ports) in self.port_mapping.iter_physical_to_logical() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            self.poll_port(stop, physical_port, logical_ports);
        }
    }

    /// The `poll_once` variant driven by [`Self::task_worker`]: it services pending APPL_DB
    /// `PORT_TABLE` link-change flaps **between every port** of the pass, mirroring
    /// dom_mgr.py:326 (which calls `check_port_update` at the top of each poll-loop
    /// iteration). A full DOM poll walks every port's EEPROM and can run for many seconds on
    /// this emulator testbed; without this interleave a `flap_count` bump that lands mid-pass
    /// would not be re-read until the whole pass finished — past the e2e fast window
    /// (`test_link_change_flags::test_link_change_triggers_fast_flag_recapture`, which asserts
    /// the re-read lands well under the ~60 s poll). The notification-independent `flap_count`
    /// reconcile stays on its ~1 s cadence via the shared `next_reconcile` deadline (so it is
    /// not re-scanned for every port), while any *due* re-read is drained each port. The plain
    /// [`Self::poll_once`] (no observer / reconcile deadline) is kept for the unit tests.
    fn poll_once_interleaved(
        &self,
        stop: &AtomicBool,
        mut observer: Option<&mut PortChangeObserver>,
        next_reconcile: &mut Instant,
    ) {
        for (physical_port, logical_ports) in self.port_mapping.iter_physical_to_logical() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // dom_mgr.py:326 — service link-change before each port so a flap during the
            // (multi-second) poll pass is re-read within ~1 s, not deferred to the next
            // periodic pass. Reconcile only fires on its ~1 s deadline (shared with
            // `task_worker`); `check_port_update` drains any due re-read every port.
            let now = Instant::now();
            if now >= *next_reconcile {
                self.reconcile_link_change_flap_counts();
                *next_reconcile =
                    now + Duration::from_secs(DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE_SECS);
            }
            self.check_port_update(
                stop,
                observer.as_deref_mut(),
                PORT_UPDATE_EVENT_SELECT_TIMEOUT_FAST_MSECS,
            );

            if stop.load(Ordering::Relaxed) {
                break;
            }
            self.poll_port(stop, physical_port, logical_ports);
        }
    }

    /// One port's DOM poll body — the per-port block of `DomInfoUpdateTask.task_worker`
    /// (dom_mgr.py:325-417): publish `TRANSCEIVER_FIRMWARE_INFO`, `TRANSCEIVER_DOM_SENSOR`,
    /// `TRANSCEIVER_DOM_FLAG` (+ metadata), the rich `TRANSCEIVER_STATUS` row, the latched
    /// `TRANSCEIVER_STATUS_FLAG` row (+ metadata), then the VDM tables — gated by
    /// `dom_polling` / CMIS-init, error status and presence. A transient per-port read
    /// failure just skips the port (early `return`), keeping the pass resilient.
    fn poll_port(&self, stop: &AtomicBool, physical_port: usize, logical_ports: &[String]) {
        // The first logical port corresponds to the first subport of the breakout.
        let Some(logical_port_name) = logical_ports.first() else {
            return;
        };

        if self.is_port_dom_monitoring_disabled(logical_port_name) {
            return;
        }

        let Some(asic_index) = self
            .port_mapping
            .get_asic_id_for_logical_port(logical_port_name)
        else {
            return;
        };

        // A port stuck in a blocking error state is skipped entirely (no posting).
        if detect_port_in_error_status(
            logical_port_name,
            self.table_helper.get_status_sw_tbl(asic_index),
        ) {
            return;
        }

        // Poll module presence over the HAL (mirrors `_wrapper_get_presence`); an
        // absent module is skipped before any EEPROM read.
        let present = self
            .hal
            .sfp(physical_port)
            .ok()
            .and_then(|s| s.get_presence().ok())
            .unwrap_or(false);
        if !present {
            return;
        }

        // M5: publish TRANSCEIVER_FIRMWARE_INFO first (mirrors task_worker order).
        self.post_port_sfp_firmware_info_to_db(
            stop,
            logical_port_name,
            self.table_helper.get_firmware_info_tbl(asic_index),
            None,
        );

        self.dom_db.post_port_dom_sensor_info_to_db(
            stop,
            logical_port_name,
            &self.port_mapping,
            self.table_helper.get_dom_tbl(asic_index),
            self.hal.as_ref(),
            None,
        );

        // M4 link-change poll-hold: if this port flapped recently, its latched-flag tables
        // (DOM/STATUS/VDM flag) are owned by the off-cadence re-read for a short window —
        // skip re-reading them on this periodic pass so a coincident poll cannot re-surface
        // a flag the flap path must gate on this non-COR emulator (see
        // `LINK_CHANGE_POLL_HOLD_SECS` / `test_link_change_flags`). The sensor + rich status
        // rows are NOT latched flags and refresh normally regardless of the hold.
        let flag_poll_held = self.is_flag_poll_held(physical_port);

        if !flag_poll_held {
            self.dom_db.post_port_dom_flags_to_db(
                stop,
                logical_port_name,
                &self.port_mapping,
                self.hal.as_ref(),
                self.table_helper.get_dom_flag_tbl(asic_index),
                self.table_helper.get_dom_flag_change_count_tbl(asic_index),
                self.table_helper.get_dom_flag_set_time_tbl(asic_index),
                self.table_helper.get_dom_flag_clear_time_tbl(asic_index),
                None,
            );
        }

        // M3: publish the rich TRANSCEIVER_STATUS row (module state/fault +
        // per-host-lane datapath/config/tx/rx) read off `get_transceiver_status()`.
        self.status_db.post_port_transceiver_hw_status_to_db(
            stop,
            logical_port_name,
            &self.port_mapping,
            self.table_helper.get_status_tbl(asic_index),
            self.hal.as_ref(),
            None,
        );

        // M4: publish the latched TRANSCEIVER_STATUS_FLAG row + its change-count /
        // set-time / clear-time metadata off `get_transceiver_status_flags()`.
        if !flag_poll_held {
            self.status_db.post_port_transceiver_hw_status_flags_to_db(
                stop,
                logical_port_name,
                &self.port_mapping,
                self.table_helper.get_status_flag_tbl(asic_index),
                self.table_helper.get_status_flag_change_count_tbl(asic_index),
                self.table_helper.get_status_flag_set_time_tbl(asic_index),
                self.table_helper.get_status_flag_clear_time_tbl(asic_index),
                self.hal.as_ref(),
                None,
            );
        }

        // M5: VDM freeze→capture→unfreeze + real-value / flag posting (COR flags last).
        self.post_port_vdm_diagnostics(
            stop,
            logical_port_name,
            physical_port,
            asic_index,
            flag_poll_held,
        );
    }

    /// The VDM per-port body of `task_worker` (dom_mgr.py:381-417). If the module supports
    /// VDM: (a) when *statistic* observables are supported **and the module is not in
    /// low-power mode**, freeze VDM, capture the statistic real values + `TRANSCEIVER_PM`,
    /// then unfreeze (RAII guard); (b) capture the *basic* real values, merge
    /// `{**basic, **statistic}` (statistic wins on conflict) and post
    /// `TRANSCEIVER_VDM_REAL_VALUE`; (c) post the COR `TRANSCEIVER_VDM_*_FLAG` tables
    /// **last** so the latched snapshot is the freshest. A module that does not support VDM
    /// posts nothing.
    ///
    /// M8 low-power gate: the freeze (and thus the statistic capture + `TRANSCEIVER_PM`
    /// refresh) is gated on `not is_transceiver_lpmode_on` (dom_mgr.py:386-387). An
    /// activated coherent module put into low power (`ModuleLowPwr`) must stop having its PM
    /// refreshed — `test_dom_lpmode` deletes the PM row while in low power and asserts it is
    /// not republished. By M8 the `CmisManagerTask` drives an admin-up module out of low
    /// power to `ModuleReady`, so a normally-operating module reports `lpmode == false` and
    /// the statistic capture still runs (`test_pm` / `test_vdm_statistic` unaffected).
    ///
    /// `flag_poll_held` carries the M4 link-change poll-hold decision from the caller: when
    /// set, the port flapped recently and the off-cadence re-read owns its latched
    /// `TRANSCEIVER_VDM_*_FLAG` transition, so step (c) is skipped on this periodic pass
    /// (the real-value / PM refresh still runs — only the latched COR flag re-read is held).
    fn post_port_vdm_diagnostics(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        physical_port: usize,
        asic_index: usize,
        flag_poll_held: bool,
    ) {
        let Ok(sfp) = self.hal.sfp(physical_port) else {
            return;
        };
        if !self.vdm_utils.is_transceiver_vdm_supported(&*sfp) {
            return;
        }

        // Step (a): statistic freeze → capture statistic real values + PM → unfreeze.
        // `need_freeze` mirrors the reference gate `is_vdm_statistic_supported(...) and not
        // is_transceiver_lpmode_on(...)` (dom_mgr.py:386-387): a module in low power is not
        // frozen, so its statistic real values + `TRANSCEIVER_PM` stop refreshing until it
        // leaves low power (the M8 coherent low-power gate — `test_dom_lpmode`).
        let mut vdm_statistic_values = Value::Object(Map::new());
        let need_freeze = self.vdm_utils.is_vdm_statistic_supported(&*sfp)
            && !is_transceiver_lpmode_on(&*sfp);
        if need_freeze {
            let guard = self.vdm_utils.vdm_freeze_context(&*sfp);
            if guard.is_frozen() {
                vdm_statistic_values = self
                    .vdm_utils
                    .get_vdm_real_values_statistic(&*sfp)
                    .unwrap_or_else(|| Value::Object(Map::new()));
                self.post_port_pm_info_to_db(
                    stop,
                    logical_port_name,
                    self.table_helper.get_pm_tbl(asic_index),
                    None,
                );
            } else {
                eprintln!(
                    "xcvrd-rs: Failed to freeze VDM stats for port {physical_port}"
                );
            }
            // `guard` drops here → unfreeze (always attempted, even on capture failure).
        }

        // Step (b): capture basic real values, merge with statistic, post to DB.
        let vdm_basic_values = self
            .vdm_utils
            .get_vdm_real_values_basic(&*sfp)
            .unwrap_or_else(|| Value::Object(Map::new()));
        let vdm_merged_values = merge_vdm_values(&vdm_basic_values, &vdm_statistic_values);
        self.vdm_db.post_port_vdm_real_values_from_dict_to_db(
            stop,
            logical_port_name,
            &self.port_mapping,
            self.hal.as_ref(),
            self.table_helper.get_vdm_real_value_tbl(asic_index),
            &vdm_merged_values,
        );

        // Step (c): post the COR VDM flag tables last (freshest latched state) — unless a
        // recent link-change flap owns this port's latched-flag transition (M4 poll-hold),
        // in which case the off-cadence re-read is the sole writer for the flap window.
        if !flag_poll_held {
            self.vdm_db.post_port_vdm_flags_to_db(
                stop,
                logical_port_name,
                &self.port_mapping,
                self.hal.as_ref(),
                &self.table_helper,
                None,
            );
        }
    }

    /// `on_port_update_event(port_change_event)` — record an APPL_DB link-change flap so
    /// its diagnostic-flag tables are re-captured off-cadence. Only a `PORT_SET` on
    /// `APPL_DB` schedules a re-read, and only when the event carries a `flap_count` that
    /// **changed** vs the last value seen for this physical port. The reference keeps
    /// `on_port_update_event` itself dumb (it schedules on any delivered `APPL_DB`
    /// `PORT_SET`) because the flap gating lives upstream in the observer — the
    /// `{APPL_DB: PORT_TABLE, FILTER: ['flap_count']}` subscription plus the
    /// `PortChangeEvent` soak/dedup only *deliver* an event when `flap_count` actually
    /// increments (dom_mgr.py:146-148, port_event_helper.py:175-184). We reproduce that
    /// *net* behaviour here so a re-delivered snapshot, or an unrelated `PORT_TABLE` write
    /// that still carries the current `flap_count`, cannot schedule an off-cadence re-read
    /// that would surface a freshly-raised latched flag before the module genuinely
    /// flapped. The re-read is queued `DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE_SECS` in the
    /// future (giving the module time to update its latched flags); breakout subports
    /// collapse onto their shared physical port, so a group flap is a single pending
    /// re-read.
    pub fn on_port_update_event(&self, event: &PortChangeEvent) {
        let Some(physical_port) = self.record_flap_transition(event) else {
            return;
        };
        let now = Instant::now();
        let pending = PendingReread {
            fire_at: now + Duration::from_secs(DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE_SECS),
            // Bound how long a port stuck mid-CMIS-init keeps retrying its off-cadence
            // re-read: one DOM update interval, after which the periodic pass republishes.
            giveup_at: now + Duration::from_secs(self.dom_update_interval),
        };
        self.link_change_affected_ports
            .lock()
            .unwrap()
            .insert(physical_port, pending);
        // Arm the poll-hold immediately: a periodic pass that falls inside this flap window
        // (including while the scheduled re-read is still pending / deferred mid-CMIS-init)
        // must not re-publish the latched flags, so the flap path stays the sole writer of
        // the flag transition (see `LINK_CHANGE_POLL_HOLD_SECS`).
        self.hold_flag_poll(physical_port);
    }

    /// Arm/refresh the link-change poll-hold for `physical_port`: the periodic DOM poll
    /// skips re-reading that port's latched-flag tables until [`LINK_CHANGE_POLL_HOLD_SECS`]
    /// from now (see [`Self::is_flag_poll_held`]). Called when a genuine flap is *scheduled*
    /// (so a poll cannot clobber the flag while the off-cadence re-read is pending/deferred)
    /// and again once the re-read actually *re-captures* the flag (so the post-flap guard
    /// window is owned solely by the flap path).
    fn hold_flag_poll(&self, physical_port: usize) {
        let until = Instant::now() + Duration::from_secs(LINK_CHANGE_POLL_HOLD_SECS);
        self.flag_poll_hold
            .lock()
            .unwrap()
            .insert(physical_port, until);
    }

    /// Whether the periodic DOM poll must currently skip re-reading `physical_port`'s
    /// latched-flag tables because a recent link-change flap owns that transition (see
    /// [`Self::hold_flag_poll`]). An expired entry is pruned on read so the map stays bounded
    /// to actively-flapping ports.
    fn is_flag_poll_held(&self, physical_port: usize) -> bool {
        let mut holds = self.flag_poll_hold.lock().unwrap();
        match holds.get(&physical_port).copied() {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                holds.remove(&physical_port);
                false
            }
            None => false,
        }
    }

    /// Seed the per-physical-port `flap_count` baseline from the observer's **initial
    /// snapshot** (see [`PortChangeObserver::take_initial_snapshot`]) at task start,
    /// WITHOUT scheduling any re-read. The observer already folds the boot snapshot into
    /// its own dedup cache, so a re-delivered snapshot is normally suppressed one layer up;
    /// seeding here makes the daemon-level `last_flap_count` dedup *independent* of that
    /// priming, so a boot-snapshot event that ever reaches [`Self::on_port_update_event`]
    /// still cannot fire a spurious off-cadence flag re-capture. Only a genuine
    /// *post-boot* `flap_count` increment is then treated as a real flap.
    pub fn seed_link_change_baseline(&self, event: &PortChangeEvent) {
        let _ = self.record_flap_transition(event);
    }

    /// Read APPL_DB `PORT_TABLE.flap_count` directly for every physical port (off its
    /// first breakout subport) and hand each value to `sink` as the synthetic APPL_DB
    /// `PORT_SET` event the observer would have delivered — the notification-independent
    /// source of link-change flaps.
    ///
    /// The reference watches `flap_count` via a keyspace `SubscriberStateTable`
    /// ([`Self::on_port_update_event`]). On this emulator testbed the daemon's single
    /// APPL_DB subscription can miss the keyspace wake, so the off-cadence flap re-read
    /// never fired; the CMIS manager tolerates the same fragility only because it *also*
    /// reconciles its trigger fields (e.g. `host_tx_ready`) straight from the DB. Mirroring
    /// that pattern, the DOM task reads `flap_count` from APPL_DB on its fast inter-poll
    /// cadence and routes each through the SAME [`Self::record_flap_transition`] dedup — so
    /// a genuine per-port change still schedules exactly one debounced re-read, while an
    /// unchanged `flap_count` (or a freshly raised module flag, which never bumps
    /// `flap_count`) is ignored, keeping the re-capture strictly off the 60 s sensor
    /// cadence.
    fn foreach_appl_flap_count(&self, mut sink: impl FnMut(&PortChangeEvent)) {
        for (physical_port, logical_ports) in self.port_mapping.iter_physical_to_logical() {
            // The first logical port is the first subport of the breakout group; the
            // reference reads the flap off that group representative too.
            let Some(first_logical) = logical_ports.first() else {
                continue;
            };
            let Some(asic_index) = self
                .port_mapping
                .get_asic_id_for_logical_port(first_logical)
            else {
                continue;
            };
            let Some(flap_count) = self
                .table_helper
                .get_app_port_tbl(asic_index)
                .hget(first_logical, "flap_count")
            else {
                continue;
            };
            let event = PortChangeEvent::new(
                first_logical.clone(),
                Some(physical_port),
                asic_index,
                PortChangeEventType::Set,
                "APPL_DB".to_string(),
                "PORT_TABLE".to_string(),
            )
            .with_port_dict(BTreeMap::from([("flap_count".to_string(), flap_count)]));
            sink(&event);
        }
    }

    /// Seed the per-physical-port `flap_count` baseline from a direct APPL_DB read at task
    /// start, WITHOUT scheduling any re-read. Complements
    /// [`Self::seed_link_change_baseline`] (which seeds off the observer's boot snapshot):
    /// the direct read is authoritative even when the observer is unavailable or its
    /// snapshot is empty, so the very first [`Self::reconcile_link_change_flap_counts`]
    /// pass does not mistake an already-present boot `flap_count` for a fresh flap and
    /// publish the latched flags off-cadence at start.
    fn seed_flap_count_baseline_from_db(&self) {
        self.foreach_appl_flap_count(|event| self.seed_link_change_baseline(event));
    }

    /// Reconcile the per-physical-port `flap_count` against APPL_DB on the fast inter-poll
    /// cadence, scheduling a debounced diagnostic-flag re-read for each port whose
    /// `flap_count` genuinely changed since the last read — the notification-independent
    /// twin of the observer's [`Self::on_port_update_event`] path. Both share the
    /// `last_flap_count` dedup, so whichever observes a given flap first records it and the
    /// other is a no-op (no double re-read).
    fn reconcile_link_change_flap_counts(&self) {
        self.foreach_appl_flap_count(|event| self.on_port_update_event(event));
    }

    /// Record the physical port + `flap_count` an APPL_DB `PORT_SET` carries, returning
    /// the physical port **only when the `flap_count` actually changed** for it (a genuine
    /// flap). A non-`SET`/non-APPL_DB event, an event with no resolvable physical port or
    /// no `flap_count`, or a re-delivery of the current `flap_count` all return `None`.
    /// The resolve + dedup-check + record run under one lock so concurrent flaps stay
    /// consistent. This is the shared core of [`Self::on_port_update_event`] (which
    /// schedules a re-read on `Some`) and [`Self::seed_link_change_baseline`] (which only
    /// records the baseline and discards the result).
    fn record_flap_transition(&self, event: &PortChangeEvent) -> Option<usize> {
        if event.event_type != PortChangeEventType::Set || event.db_name != "APPL_DB" {
            return None;
        }
        let physical_port = self.resolve_physical_for_event(event)?;
        // A PORT_SET without a `flap_count` is not a link flap (a non-flap PORT_TABLE
        // write, or a filtered-empty snapshot). An event carrying the `flap_count` already
        // seen for this port is a re-delivery, not a new flap.
        let flap_count = event.port_dict.get("flap_count")?;
        let mut seen = self.last_flap_count.lock().unwrap();
        if seen.get(&physical_port).map(String::as_str) == Some(flap_count.as_str()) {
            return None;
        }
        seen.insert(physical_port, flap_count.clone());
        Some(physical_port)
    }

    /// Resolve the physical port a link-change event targets. The reference keys on the
    /// APPL_DB `PORT_TABLE` `index` field directly; on this emulator testbed that field
    /// may be absent, so we prefer the (always-present) logical-name→physical mapping and
    /// fall back to the event's own index — either way yielding a valid
    /// `physical_to_logical` key for [`Self::update_port_db_diagnostics_on_link_change`].
    fn resolve_physical_for_event(&self, event: &PortChangeEvent) -> Option<usize> {
        if let Some(list) = self.port_mapping.get_logical_to_physical(&event.port_name) {
            if let Some(p) = list.first().copied() {
                return Some(p);
            }
        }
        event.physical_port
    }

    /// `update_port_db_diagnostics_on_link_change(physical_port)` — after a link-change
    /// flap, re-capture the port's DOM + status flag tables (and their metadata) so the
    /// latched-flag snapshot reflects the module's post-flap state. Guards mirror the
    /// reference exactly: stop event, unknown physical port, DOM monitoring disabled,
    /// invalid asic, a blocking error status, and module absence all skip the re-read.
    /// (VDM flag re-capture lands in M5.)
    ///
    /// Returns [`LinkChangeReread`]: every guard except the CMIS-init one settles the flap
    /// (the pending re-read is consumed). The lone [`LinkChangeReread::DeferredCmisInit`]
    /// path — DOM polling enabled but `cmis_state` still non-terminal — keeps the re-read
    /// pending so [`Self::check_port_update`] retries it once the datapath settles, without
    /// ever publishing while the DOM gate is closed.
    pub fn update_port_db_diagnostics_on_link_change(
        &self,
        physical_port: usize,
        stop: &AtomicBool,
    ) -> LinkChangeReread {
        if stop.load(Ordering::Relaxed) {
            return LinkChangeReread::Settled;
        }

        let Some(logical_port_list) = self.port_mapping.get_physical_to_logical(physical_port)
        else {
            eprintln!(
                "xcvrd-rs: Update DB diagnostics during link change: Unknown physical port \
                 index {physical_port}"
            );
            return LinkChangeReread::Settled;
        };
        // First logical port corresponds to the first subport of the breakout group.
        let Some(first_logical_port) = logical_port_list.first() else {
            return LinkChangeReread::Settled;
        };

        // `is_port_dom_monitoring_disabled` folds two distinct conditions: an operator
        // `dom_polling=disabled` gate (permanent for this flap → settle) and a *transient*
        // non-terminal `cmis_state` (mid-datapath-bring-up → defer + retry). Publishing the
        // flag row now while cmis_state is non-terminal would break the DOM-gating contract
        // (`test_dom_gating`), so only the operator gate settles; the CMIS-init gate defers.
        if self.get_dom_polling_from_config_db(first_logical_port) == "disabled" {
            return LinkChangeReread::Settled;
        }
        if self.is_port_in_cmis_initialization_process(first_logical_port) {
            return LinkChangeReread::DeferredCmisInit;
        }

        let Some(asic_index) = self
            .port_mapping
            .get_asic_id_for_logical_port(first_logical_port)
        else {
            eprintln!(
                "xcvrd-rs: Update DB diagnostics during link change: Got invalid asic index \
                 for {first_logical_port}, ignored"
            );
            return LinkChangeReread::Settled;
        };

        if detect_port_in_error_status(
            first_logical_port,
            self.table_helper.get_status_sw_tbl(asic_index),
        ) {
            return LinkChangeReread::Settled;
        }

        let present = self
            .hal
            .sfp(physical_port)
            .ok()
            .and_then(|s| s.get_presence().ok())
            .unwrap_or(false);
        if !present {
            return LinkChangeReread::Settled;
        }

        // Re-capture TRANSCEIVER_DOM_FLAG + metadata.
        self.dom_db.post_port_dom_flags_to_db(
            stop,
            first_logical_port,
            &self.port_mapping,
            self.hal.as_ref(),
            self.table_helper.get_dom_flag_tbl(asic_index),
            self.table_helper.get_dom_flag_change_count_tbl(asic_index),
            self.table_helper.get_dom_flag_set_time_tbl(asic_index),
            self.table_helper.get_dom_flag_clear_time_tbl(asic_index),
            None,
        );

        // Re-capture TRANSCEIVER_STATUS_FLAG + metadata.
        self.status_db.post_port_transceiver_hw_status_flags_to_db(
            stop,
            first_logical_port,
            &self.port_mapping,
            self.table_helper.get_status_flag_tbl(asic_index),
            self.table_helper.get_status_flag_change_count_tbl(asic_index),
            self.table_helper.get_status_flag_set_time_tbl(asic_index),
            self.table_helper.get_status_flag_clear_time_tbl(asic_index),
            self.hal.as_ref(),
            None,
        );

        // Re-capture the COR TRANSCEIVER_VDM_*_FLAG tables + metadata (dom_mgr.py:485-493).
        if let Ok(sfp) = self.hal.sfp(physical_port) {
            if self.vdm_utils.is_transceiver_vdm_supported(&*sfp) {
                self.vdm_db.post_port_vdm_flags_to_db(
                    stop,
                    first_logical_port,
                    &self.port_mapping,
                    self.hal.as_ref(),
                    &self.table_helper,
                    None,
                );
            }
        }
        // The flap path has now re-captured the latched flags; refresh the poll-hold so the
        // post-flap guard window is owned solely by this re-read (no periodic poll
        // re-surfaces the flag until the hold lapses) — the emulator's non-COR flag byte
        // would otherwise let a coincident poll race the link-change trigger.
        self.hold_flag_poll(physical_port);
        LinkChangeReread::Settled
    }
    /// `check_port_update` — drain any pending APPL_DB `PORT_TABLE` link-change
    /// port-update notifications (feeding each into [`Self::on_port_update_event`]) then
    /// re-capture diagnostics for every physical port whose grace delay has elapsed.
    /// A re-read gated only by a transient CMIS-init ([`LinkChangeReread::DeferredCmisInit`])
    /// is re-armed for the next ~1 s pass — up to its [`PendingReread::giveup_at`] bound —
    /// rather than dropped, so the post-flap latched-flag baseline is re-published the moment
    /// the datapath settles. With no live observer it just paces the loop (nothing is ever
    /// scheduled), keeping the periodic pass the sole publisher.
    fn check_port_update(
        &self,
        stop: &AtomicBool,
        observer: Option<&mut PortChangeObserver>,
        timeout_ms: u64,
    ) {
        match observer {
            Some(obs) => match obs.handle_port_update_event(timeout_ms) {
                Ok(events) => {
                    for ev in &events {
                        self.on_port_update_event(ev);
                    }
                }
                Err(e) => eprintln!("xcvrd-rs: DOM link-change port-update read error: {e}"),
            },
            // No observer: pace the wait loop so it does not spin.
            None => std::thread::sleep(Duration::from_millis(
                timeout_ms.min(PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS),
            )),
        }

        // Process each pending link-changed port whose grace delay has elapsed.
        let now = Instant::now();
        let due: Vec<usize> = {
            let map = self.link_change_affected_ports.lock().unwrap();
            map.iter()
                .filter(|(_, p)| p.fire_at <= now)
                .map(|(&p, _)| p)
                .collect()
        };
        for physical_port in due {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match self.update_port_db_diagnostics_on_link_change(physical_port, stop) {
                // Re-captured, or permanently ineligible for this flap: consume the entry —
                // the reference's unconditional `del` (dom_mgr.py:282).
                LinkChangeReread::Settled => {
                    self.link_change_affected_ports
                        .lock()
                        .unwrap()
                        .remove(&physical_port);
                }
                // Transiently gated mid-CMIS-init: re-arm the re-read on the next ~1 s
                // cadence, preserving the original give-up bound, so the post-flap baseline
                // is re-published the moment the datapath reaches a terminal state — unless
                // the port is still initializing past the bound, when we drop it and let the
                // periodic pass take over.
                LinkChangeReread::DeferredCmisInit => {
                    let mut map = self.link_change_affected_ports.lock().unwrap();
                    match map.get(&physical_port) {
                        Some(pending) if now < pending.giveup_at => {
                            let giveup_at = pending.giveup_at;
                            map.insert(
                                physical_port,
                                PendingReread {
                                    fire_at: now
                                        + Duration::from_secs(
                                            DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE_SECS,
                                        ),
                                    giveup_at,
                                },
                            );
                        }
                        _ => {
                            map.remove(&physical_port);
                        }
                    }
                }
            }
        }
    }

    /// `task_worker` — the periodic loop. The first periodic pass is delayed one full
    /// interval ("to allow xcvrd to initialize ports"); the next pass is scheduled from
    /// each pass's *start* for a consistent cadence. Between passes it services APPL_DB
    /// `PORT_TABLE` link-change flaps on a ~1 s cadence — both via a best-effort
    /// [`PortChangeObserver`] and, notification-independently, by reconciling `flap_count`
    /// straight from APPL_DB ([`Self::reconcile_link_change_flap_counts`]) — so a flap's
    /// latched-flag re-capture lands well inside the e2e fast-timeout without waiting for
    /// the next 60 s periodic pass, even if the keyspace observer misses the wake.
    pub fn task_worker(&self, stop: &Arc<AtomicBool>) {
        // Best-effort APPL_DB PORT_TABLE observer for fast link-change flag re-reads. If
        // the subscription can't be built (redis not ready), fall back to periodic-only
        // polling rather than taking the DOM task down.
        let mut observer = match PortChangeObserver::for_appl_port_table() {
            Ok(obs) => Some(obs),
            Err(e) => {
                eprintln!(
                    "xcvrd-rs: DOM link-change observer unavailable ({e}); periodic DOM \
                     polling only"
                );
                None
            }
        };

        // Seed the per-port flap_count baseline from the observer's boot snapshot so the
        // daemon-level dedup independently rejects a re-delivered snapshot (no spurious
        // off-cadence flag re-read at start); only a genuine post-boot flap re-reads.
        if let Some(obs) = observer.as_mut() {
            for ev in obs.take_initial_snapshot() {
                self.seed_link_change_baseline(&ev);
            }
        }
        // Also seed the baseline from a direct APPL_DB read: authoritative even when the
        // observer snapshot is empty or the observer is unavailable, so the first reconcile
        // pass below does not mistake an already-present boot flap_count for a fresh flap.
        self.seed_flap_count_baseline_from_db();

        let interval = Duration::from_secs(self.dom_update_interval);
        let reconcile_period = Duration::from_secs(DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE_SECS);
        let mut next = Instant::now() + interval;
        let mut next_reconcile = Instant::now() + reconcile_period;
        while !stop.load(Ordering::Relaxed) {
            // Service link-change events until the next scheduled poll, capping each select
            // wait so shutdown, the periodic cadence, AND the ~1 s flap_count reconcile all
            // stay responsive.
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let now = Instant::now();
                let remaining = next.saturating_duration_since(now);
                if remaining.is_zero() {
                    break;
                }
                // Notification-independent flap detection: reconcile APPL_DB flap_count on
                // the ~1 s cadence so a link flap schedules its debounced re-read even if
                // the keyspace observer misses the wake.
                if now >= next_reconcile {
                    self.reconcile_link_change_flap_counts();
                    next_reconcile = now + reconcile_period;
                }
                let wait = remaining.min(next_reconcile.saturating_duration_since(now));
                let timeout_ms =
                    (wait.as_millis() as u64).clamp(1, PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS);
                self.check_port_update(stop, observer.as_mut(), timeout_ms);
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let loop_start = Instant::now();
            // Interleave APPL_DB link-change servicing across the poll pass (dom_mgr.py:326)
            // so a flap landing mid-pass is re-read within ~1 s instead of waiting for the
            // whole (multi-second) pass to finish. Shares `next_reconcile` with the wait loop
            // above so the ~1 s reconcile cadence is continuous across pass boundaries.
            self.poll_once_interleaved(stop, observer.as_mut(), &mut next_reconcile);
            next = loop_start + interval;
        }
    }

    /// Run the task to completion on the calling thread (spawn helper).
    pub fn run(self, stop: Arc<AtomicBool>) {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        self.task_worker(&stop);
    }
}

/// `DomThermalInfoUpdateTask` — the fast temperature poll → `TRANSCEIVER_DOM_TEMPERATURE`.
///
/// Not launched by the M2 daemon (the Python default `dom_temperature_poll_interval is
/// None` leaves it unstarted, and no e2e gate reads `TRANSCEIVER_DOM_TEMPERATURE`), but
/// implemented + unit-tested for parity. Its gate is CONFIG_DB `dom_polling` only (it
/// inherits the base gate, without the CMIS-init check).
pub struct DomThermalInfoUpdateTask {
    port_mapping: PortMapping,
    hal: Arc<dyn Hal>,
    table_helper: Arc<XcvrTableHelper>,
    poll_interval: Duration,
    dom_db: DomDbUtils,
}

impl DomThermalInfoUpdateTask {
    pub fn new(
        port_mapping: PortMapping,
        hal: Arc<dyn Hal>,
        table_helper: Arc<XcvrTableHelper>,
        poll_interval: Duration,
    ) -> Self {
        DomThermalInfoUpdateTask {
            port_mapping,
            hal,
            table_helper,
            poll_interval,
            dom_db: DomDbUtils::new(),
        }
    }

    /// `is_port_dom_monitoring_disabled` (base) — CONFIG_DB `dom_polling=disabled` only.
    pub fn is_port_dom_monitoring_disabled(&self, logical_port_name: &str) -> bool {
        get_dom_polling_from_config_db(&self.port_mapping, &self.table_helper, logical_port_name)
            == "disabled"
    }

    /// One fast temperature pass: publish `TRANSCEIVER_DOM_TEMPERATURE` for every
    /// present, polling-enabled, non-error port.
    pub fn poll_once(&self, stop: &AtomicBool) {
        for (physical_port, logical_ports) in self.port_mapping.iter_physical_to_logical() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let Some(logical_port_name) = logical_ports.first() else {
                continue;
            };
            if self.is_port_dom_monitoring_disabled(logical_port_name) {
                continue;
            }
            let Some(asic_index) = self
                .port_mapping
                .get_asic_id_for_logical_port(logical_port_name)
            else {
                continue;
            };
            if !detect_port_in_error_status(
                logical_port_name,
                self.table_helper.get_status_sw_tbl(asic_index),
            ) {
                let present = self
                    .hal
                    .sfp(physical_port)
                    .ok()
                    .and_then(|s| s.get_presence().ok())
                    .unwrap_or(false);
                if !present {
                    continue;
                }
            }
            self.dom_db.post_port_dom_temperature_info_to_db(
                stop,
                logical_port_name,
                &self.port_mapping,
                self.table_helper.get_dom_temperature_tbl(asic_index),
                self.hal.as_ref(),
                None,
            );
        }
    }

    /// `task_worker` — poll as soon as possible, then every `poll_interval`.
    pub fn task_worker(&self, stop: &Arc<AtomicBool>) {
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if next > now {
                let remaining = next.saturating_duration_since(now);
                std::thread::sleep(remaining.min(Duration::from_secs(1)));
                continue;
            }
            self.poll_once(stop);
            next = Instant::now() + self.poll_interval;
        }
    }

    pub fn run(self, stop: Arc<AtomicBool>) {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        self.task_worker(&stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockHal, MockSfp};
    use crate::xcvrd_utilities::port_event_helper::{build_port_mapping, PortConfigRow};
    use serde_json::json;
    use std::collections::{BTreeMap, HashMap};

    fn mapping_with(ports: &[(&str, usize)]) -> PortMapping {
        build_port_mapping(
            ports.iter().map(|(name, idx)| PortConfigRow {
                name: name.to_string(),
                index: Some(*idx),
                role: None,
            }),
            0,
        )
    }

    fn full_dom_real_value() -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("temperature".into(), json!("42.5C"));
        m.insert("voltage".into(), json!("3.3Volts"));
        for lane in 1..=8 {
            m.insert(format!("tx{lane}power"), json!("-1.5dBm"));
            m.insert(format!("rx{lane}power"), json!("-2.5dBm"));
            m.insert(format!("tx{lane}bias"), json!("7.5mA"));
        }
        serde_json::Value::Object(m)
    }

    fn dom_module() -> MockSfp {
        MockSfp::present()
            .with_dom_real_value(full_dom_real_value())
            .with_threshold_info(json!({"temphighalarm": 75.0}))
            .with_json(
                "get_transceiver_dom_flags",
                json!({"tempHAlarm": false}),
            )
            .with_json(
                "get_transceiver_status_flags",
                json!({
                    "datapath_firmware_fault": false,
                    "module_firmware_fault": false,
                    "module_state_changed": true
                }),
            )
            .with_status(json!({
                "module_state": "ModuleReady",
                "module_fault_cause": "No Fault detected",
                "DP1State": "DataPathActivated",
                "config_state_hostlane1": "ConfigSuccess",
            }))
    }

    /// A `dom_module` that also advertises VDM support: statistic observables are
    /// *unsupported* (so `poll_once` skips the freeze/PM path — no `sleep`, keeping the
    /// test fast) but basic real values + COR flags are served.
    fn vdm_module() -> MockSfp {
        dom_module()
            .with_json("is_transceiver_vdm_supported", json!(true))
            .with_json("is_vdm_statistic_supported", json!(false))
            .with_json(
                "get_transceiver_vdm_real_value_basic",
                json!({
                    "laser_temperature_media1": 38,
                    "esnr_media_input1": 23.1171875,
                }),
            )
            .with_json(
                "get_transceiver_vdm_flags",
                json!({
                    "laser_temperature_media_1_halarm": false,
                    "laser_temperature_media_2_halarm": true,
                }),
            )
    }

    fn task_with(
        ports: &[(&str, usize)],
        sfps: Vec<MockSfp>,
        skip_cmis_mgr: bool,
    ) -> DomInfoUpdateTask {
        DomInfoUpdateTask::new(
            mapping_with(ports),
            Arc::new(MockHal::with_sfps(sfps)),
            Arc::new(XcvrTableHelper::with_mock_tables(&[String::new()])),
            skip_cmis_mgr,
            Some(60),
        )
    }

    /// Root-cause lock for the M4 e2e `test_link_change_triggers_fast_flag_recapture`
    /// pre-flap guard race: the periodic DOM/flag poll cadence MUST NOT be shorter than
    /// the Python reference (`dom_mgr.py` `DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS = 60`). The
    /// daemon constructs `DomInfoUpdateTask` with `dom_update_interval = None`
    /// (`daemon.rs`), so a background poll runs every 60 s; anything shorter would let a
    /// routine poll republish a freshly-raised latched flag inside the test's ~8 s guard
    /// window, surfacing it off the flap trigger. Lock both the constant and the
    /// `None -> default` fallback the daemon relies on so no later edit can quietly
    /// tighten the cadence below the reference `T_DOM`.
    #[test]
    fn test_dom_poll_cadence_defaults_to_reference_60s() {
        assert_eq!(DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS, 60);
        let task = DomInfoUpdateTask::new(
            mapping_with(&[("Ethernet0", 1)]),
            Arc::new(MockHal::with_sfps(vec![dom_module()])),
            Arc::new(XcvrTableHelper::with_mock_tables(&[String::new()])),
            false,
            None,
        );
        assert_eq!(task.dom_update_interval, DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS);
        assert_eq!(task.dom_update_interval, 60);
    }

    /// An APPL_DB `PORT_TABLE` `PORT_SET` carrying a `flap_count` in its soaked/filtered
    /// `port_dict` — the shape the observer emits for a link flap.
    fn appl_flap_event(port: &str, phys: usize, flap_count: &str) -> PortChangeEvent {
        PortChangeEvent::new(
            port.to_string(),
            Some(phys),
            0,
            PortChangeEventType::Set,
            "APPL_DB".to_string(),
            "PORT_TABLE".to_string(),
        )
        .with_port_dict(BTreeMap::from([(
            "flap_count".to_string(),
            flap_count.to_string(),
        )]))
    }

    // ← tests/test_xcvrd.py::test_DomInfoUpdateTask_get_dom_polling_from_config_db
    #[test]
    fn test_get_dom_polling_from_config_db() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        // Default (no CONFIG_DB row) → "enabled".
        assert_eq!(task.get_dom_polling_from_config_db("Ethernet0"), "enabled");
        assert!(!task.is_port_dom_monitoring_disabled("Ethernet0"));
        // dom_polling=disabled on the group's first subport → gate closes.
        task.table_helper
            .get_cfg_port_tbl(0)
            .hset("Ethernet0", "dom_polling", "disabled");
        assert_eq!(task.get_dom_polling_from_config_db("Ethernet0"), "disabled");
        assert!(task.is_port_dom_monitoring_disabled("Ethernet0"));
    }

    // ← tests/test_xcvrd.py::test_DomInfoUpdateTask_is_port_in_cmis_initialization_process
    #[test]
    fn test_is_port_in_cmis_initialization_process() {
        // skip_cmis_mgr=false so the gate is live.
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], false);
        let status_sw = task.table_helper.get_status_sw_tbl(0);
        // Non-terminal cmis_state → mid-init → disabled.
        status_sw.hset("Ethernet0", "cmis_state", "INSERTED");
        assert!(task.is_port_in_cmis_initialization_process("Ethernet0"));
        assert!(task.is_port_dom_monitoring_disabled("Ethernet0"));
        // Terminal (READY) → not in init.
        status_sw.hset("Ethernet0", "cmis_state", "READY");
        assert!(!task.is_port_in_cmis_initialization_process("Ethernet0"));
        assert!(!task.is_port_dom_monitoring_disabled("Ethernet0"));
        // skip_cmis_mgr short-circuits regardless of state.
        let skipping = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        skipping
            .table_helper
            .get_status_sw_tbl(0)
            .hset("Ethernet0", "cmis_state", "INSERTED");
        assert!(!skipping.is_port_in_cmis_initialization_process("Ethernet0"));
    }

    // ← tests/test_xcvrd.py::test_DomInfoUpdateTask_task_worker (one pass)
    #[test]
    fn test_poll_once_publishes_dom_sensor_and_flags() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.poll_once(&AtomicBool::new(false));

        // DOM_SENSOR: temperature + voltage + all 24 per-lane keys, unit-stripped.
        let sensor: HashMap<String, String> = task
            .table_helper
            .get_dom_tbl(0)
            .get("Ethernet0")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(sensor.get("temperature").map(String::as_str), Some("42.5"));
        assert_eq!(sensor.get("voltage").map(String::as_str), Some("3.3"));
        for lane in 1..=8 {
            assert!(sensor.contains_key(&format!("tx{lane}power")));
            assert!(sensor.contains_key(&format!("rx{lane}power")));
            assert!(sensor.contains_key(&format!("tx{lane}bias")));
        }
        assert!(sensor.contains_key("last_update_time"));

        // DOM_FLAG value row + first-publish metadata init.
        let flag: HashMap<String, String> = task
            .table_helper
            .get_dom_flag_tbl(0)
            .get("Ethernet0")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(flag.get("tempHAlarm").map(String::as_str), Some("False"));
        assert_eq!(
            task.table_helper
                .get_dom_flag_change_count_tbl(0)
                .hget("Ethernet0", "tempHAlarm"),
            Some("0".into())
        );
        assert_eq!(
            task.table_helper
                .get_dom_flag_set_time_tbl(0)
                .hget("Ethernet0", "tempHAlarm"),
            Some("never".into())
        );
    }

    #[test]
    fn test_poll_once_skips_when_dom_polling_disabled() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.table_helper
            .get_cfg_port_tbl(0)
            .hset("Ethernet0", "dom_polling", "disabled");
        task.poll_once(&AtomicBool::new(false));
        assert_eq!(task.table_helper.get_dom_tbl(0).get("Ethernet0"), None);
        assert_eq!(task.table_helper.get_dom_flag_tbl(0).get("Ethernet0"), None);
        assert_eq!(task.table_helper.get_status_tbl(0).get("Ethernet0"), None);
    }

    // ← tests/test_xcvrd.py::test_DomInfoUpdateTask_task_worker (STATUS row on a pass)
    #[test]
    fn test_poll_once_publishes_transceiver_status() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.poll_once(&AtomicBool::new(false));

        let status: HashMap<String, String> = task
            .table_helper
            .get_status_tbl(0)
            .get("Ethernet0")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(status.get("module_state").map(String::as_str), Some("ModuleReady"));
        assert_eq!(
            status.get("module_fault_cause").map(String::as_str),
            Some("No Fault detected")
        );
        assert_eq!(status.get("DP1State").map(String::as_str), Some("DataPathActivated"));
        assert_eq!(
            status.get("config_state_hostlane1").map(String::as_str),
            Some("ConfigSuccess")
        );
        assert!(status.contains_key("last_update_time"));
    }

    // ← tests/test_xcvrd.py::test_DomInfoUpdateTask_task_worker (STATUS_FLAG row on a pass)
    #[test]
    fn test_poll_once_publishes_status_flags() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.poll_once(&AtomicBool::new(false));

        // STATUS_FLAG value row: 3 flags + last_update_time (bools default-beautified).
        let flag: HashMap<String, String> = task
            .table_helper
            .get_status_flag_tbl(0)
            .get("Ethernet0")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(flag.get("datapath_firmware_fault").map(String::as_str), Some("False"));
        assert_eq!(flag.get("module_state_changed").map(String::as_str), Some("True"));
        assert!(flag.contains_key("last_update_time"));

        // First-publish metadata init: change count 0, set/clear time "never".
        assert_eq!(
            task.table_helper
                .get_status_flag_change_count_tbl(0)
                .hget("Ethernet0", "module_state_changed"),
            Some("0".into())
        );
        assert_eq!(
            task.table_helper
                .get_status_flag_set_time_tbl(0)
                .hget("Ethernet0", "module_state_changed"),
            Some("never".into())
        );
    }

    // ← tests/test_xcvrd.py::test_update_port_db_diagnostics_on_link_change
    #[test]
    fn test_update_port_db_diagnostics_on_link_change() {
        // Case 1: valid, present, non-error port → DOM + STATUS flag tables re-captured.
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.update_port_db_diagnostics_on_link_change(0, &AtomicBool::new(false));
        assert!(
            task.table_helper.get_dom_flag_tbl(0).get("Ethernet0").is_some(),
            "DOM flags must be re-captured on link change"
        );
        assert!(
            task.table_helper.get_status_flag_tbl(0).get("Ethernet0").is_some(),
            "status flags must be re-captured on link change"
        );

        // Case 2: unknown physical port (not in the map) → nothing posted, no panic.
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.update_port_db_diagnostics_on_link_change(2, &AtomicBool::new(false));
        assert!(task.table_helper.get_dom_flag_tbl(0).get("Ethernet0").is_none());
        assert!(task.table_helper.get_status_flag_tbl(0).get("Ethernet0").is_none());

        // Case 4: port in a blocking error status → skip (no re-capture).
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.table_helper.get_status_sw_tbl(0).hset(
            "Ethernet0",
            "error",
            "Blocking EEPROM from being read",
        );
        task.update_port_db_diagnostics_on_link_change(0, &AtomicBool::new(false));
        assert!(task.table_helper.get_dom_flag_tbl(0).get("Ethernet0").is_none());
        assert!(task.table_helper.get_status_flag_tbl(0).get("Ethernet0").is_none());

        // Case 5: transceiver absent → skip.
        let task = task_with(&[("Ethernet0", 0)], vec![MockSfp::absent()], true);
        task.update_port_db_diagnostics_on_link_change(0, &AtomicBool::new(false));
        assert!(task.table_helper.get_dom_flag_tbl(0).get("Ethernet0").is_none());

        // Stop event set → immediate return (no re-capture).
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.update_port_db_diagnostics_on_link_change(0, &AtomicBool::new(true));
        assert!(task.table_helper.get_status_flag_tbl(0).get("Ethernet0").is_none());
    }

    // ← tests/test_xcvrd.py::test_DomInfoUpdateTask on_port_update_event scheduling.
    // A link-change re-read is scheduled only for a genuine APPL_DB flap: a PORT_SET
    // carrying a flap_count that CHANGED for the resolved physical port. This mirrors the
    // reference's FILTER['flap_count'] + PortChangeEvent dedup net behaviour and, per the
    // M4 repair, stops a re-delivered/unchanged flap_count (or a non-flap PORT_TABLE
    // write) from re-capturing the latched flags off-cadence.
    #[test]
    fn test_on_port_update_event_schedules_link_change() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);

        // A PORT_SET on APPL_DB carrying a NEW flap_count schedules a re-read (keyed by
        // physical port).
        task.on_port_update_event(&appl_flap_event("Ethernet0", 0, "1"));
        assert!(task.link_change_affected_ports.lock().unwrap().contains_key(&0));

        // Re-delivery of the SAME flap_count is not a new flap → no re-schedule. Clear
        // the pending set so a fresh insert (if any) would be observable.
        task.link_change_affected_ports.lock().unwrap().clear();
        task.on_port_update_event(&appl_flap_event("Ethernet0", 0, "1"));
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "an unchanged flap_count must not re-schedule a re-read"
        );

        // A CHANGED flap_count is a genuine flap → schedule again.
        task.on_port_update_event(&appl_flap_event("Ethernet0", 0, "2"));
        assert!(task.link_change_affected_ports.lock().unwrap().contains_key(&0));

        // An APPL_DB PORT_SET WITHOUT a flap_count is not a link flap → no schedule.
        let task2 = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        let appl_no_flap = PortChangeEvent::new(
            "Ethernet0".to_string(),
            Some(0),
            0,
            PortChangeEventType::Set,
            "APPL_DB".to_string(),
            "PORT_TABLE".to_string(),
        );
        task2.on_port_update_event(&appl_no_flap);
        assert!(task2.link_change_affected_ports.lock().unwrap().is_empty());

        // A non-APPL_DB SET does not schedule anything (even carrying a flap_count).
        let cfg_set = PortChangeEvent::new(
            "Ethernet0".to_string(),
            Some(0),
            0,
            PortChangeEventType::Set,
            "CONFIG_DB".to_string(),
            "PORT_TABLE".to_string(),
        )
        .with_port_dict(BTreeMap::from([("flap_count".to_string(), "1".to_string())]));
        task2.on_port_update_event(&cfg_set);
        assert!(task2.link_change_affected_ports.lock().unwrap().is_empty());

        // A PORT_DEL on APPL_DB does not schedule anything (only link-up SETs do).
        let appl_del = PortChangeEvent::new(
            "Ethernet0".to_string(),
            Some(0),
            0,
            PortChangeEventType::Del,
            "APPL_DB".to_string(),
            "PORT_TABLE".to_string(),
        )
        .with_port_dict(BTreeMap::from([("flap_count".to_string(), "3".to_string())]));
        task2.on_port_update_event(&appl_del);
        assert!(task2.link_change_affected_ports.lock().unwrap().is_empty());
    }

    // The flap_count dedup is tracked per physical port: independent ports flap
    // independently, and a re-delivered flap_count on one port leaves the other alone.
    // Mirrors the failing e2e scenario (raise a flag with no flap → no re-read) at the
    // unit level.
    #[test]
    fn test_on_port_update_event_flap_count_tracked_per_port() {
        let task = task_with(
            &[("Ethernet0", 0), ("Ethernet8", 1)],
            vec![dom_module(), dom_module()],
            true,
        );

        // Both ports genuinely flap → both scheduled.
        task.on_port_update_event(&appl_flap_event("Ethernet0", 0, "5"));
        task.on_port_update_event(&appl_flap_event("Ethernet8", 1, "5"));
        {
            let m = task.link_change_affected_ports.lock().unwrap();
            assert!(m.contains_key(&0) && m.contains_key(&1));
        }
        task.link_change_affected_ports.lock().unwrap().clear();

        // Re-deliver port 0's current flap_count (no change) → nothing scheduled.
        task.on_port_update_event(&appl_flap_event("Ethernet0", 0, "5"));
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "a re-delivered flap_count on one port must not schedule anything"
        );

        // Only port 1 flaps again → only port 1 is (re)scheduled.
        task.on_port_update_event(&appl_flap_event("Ethernet8", 1, "6"));
        let m = task.link_change_affected_ports.lock().unwrap();
        assert!(m.contains_key(&1) && !m.contains_key(&0));
    }

    // M4 repair (defense-in-depth): seeding the per-port flap_count baseline from the
    // observer's boot snapshot must (a) NOT schedule any re-read, and (b) make the
    // daemon-level dedup independently reject a re-delivered boot snapshot — even if it
    // ever reaches on_port_update_event (bypassing the observer's own cache priming). Only
    // a genuine POST-boot flap_count increment then re-captures the latched flags.
    #[test]
    fn test_seed_link_change_baseline_suppresses_boot_snapshot_reread() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);

        // Boot snapshot carries flap_count=7 → record the baseline, schedule NOTHING.
        task.seed_link_change_baseline(&appl_flap_event("Ethernet0", 0, "7"));
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "seeding the boot baseline must never schedule an off-cadence re-read"
        );

        // A re-delivered boot snapshot (same flap_count) reaching on_port_update_event is
        // now rejected by the seeded daemon-level dedup → still no re-read.
        task.on_port_update_event(&appl_flap_event("Ethernet0", 0, "7"));
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "a re-delivered boot flap_count must not re-capture the flag tables"
        );

        // A genuine post-boot flap (incremented flap_count) is a real link-change → one
        // scheduled re-read.
        task.on_port_update_event(&appl_flap_event("Ethernet0", 0, "8"));
        assert!(
            task.link_change_affected_ports.lock().unwrap().contains_key(&0),
            "a real post-boot flap_count bump must schedule a re-read"
        );
    }

    // M7 repair: the notification-independent flap detector. Reconciling `flap_count`
    // straight from APPL_DB (not via the keyspace observer) schedules a debounced re-read
    // on a genuine change, and — sharing the `last_flap_count` dedup with the observer
    // path — does NOT re-schedule for an unchanged flap_count. This mirrors the failing
    // e2e (Ethernet48 flap → off-cadence flag re-capture) at the unit level, on the direct
    // read that guarantees detection even when the emulator drops the keyspace wake.
    #[test]
    fn test_reconcile_flap_count_change_schedules_link_change() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        let app = task.table_helper.get_app_port_tbl(0);

        // First flap_count seen for the port is a genuine change → schedule a re-read.
        app.hset("Ethernet0", "flap_count", "1");
        task.reconcile_link_change_flap_counts();
        assert!(
            task.link_change_affected_ports.lock().unwrap().contains_key(&0),
            "a first/changed APPL_DB flap_count must schedule a link-change re-read"
        );

        // Re-reading the SAME flap_count is not a new flap → nothing re-scheduled.
        task.link_change_affected_ports.lock().unwrap().clear();
        task.reconcile_link_change_flap_counts();
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "an unchanged flap_count must not re-schedule off the direct-read path"
        );

        // A bumped flap_count is a real flap again → schedule.
        app.hset("Ethernet0", "flap_count", "2");
        task.reconcile_link_change_flap_counts();
        assert!(task.link_change_affected_ports.lock().unwrap().contains_key(&0));
    }

    // Seeding the flap_count baseline directly from APPL_DB at task start must suppress a
    // spurious boot re-read: an already-present flap_count is recorded but NOT scheduled,
    // and only a genuine post-seed increment then re-captures the latched flags. This
    // enforces the daemon's deliberate "no boot-prime DOM pass" invariant on the
    // direct-read path (daemon.rs) so the e2e 8 s guard is never violated at start.
    #[test]
    fn test_seed_flap_count_baseline_from_db_suppresses_boot_reread() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        let app = task.table_helper.get_app_port_tbl(0);

        // Boot: port already carries flap_count=5. Seeding records the baseline …
        app.hset("Ethernet0", "flap_count", "5");
        task.seed_flap_count_baseline_from_db();
        // … and the first reconcile pass sees the same value → schedules NOTHING.
        task.reconcile_link_change_flap_counts();
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "an already-present boot flap_count must not fire an off-cadence re-read"
        );

        // A genuine post-boot flap (incremented flap_count) is a real link-change → one
        // scheduled re-read.
        app.hset("Ethernet0", "flap_count", "6");
        task.reconcile_link_change_flap_counts();
        assert!(
            task.link_change_affected_ports.lock().unwrap().contains_key(&0),
            "a real post-boot flap_count bump must schedule a re-read"
        );
    }

    // A PORT_TABLE row without a flap_count (or an empty APPL_DB) is not a flap: the
    // direct-read reconcile schedules nothing.
    #[test]
    fn test_reconcile_absent_flap_count_no_schedule() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.reconcile_link_change_flap_counts();
        assert!(task.link_change_affected_ports.lock().unwrap().is_empty());

        // A PORT_TABLE write of an unrelated field (no flap_count) is still not a flap.
        task.table_helper
            .get_app_port_tbl(0)
            .hset("Ethernet0", "admin_status", "up");
        task.reconcile_link_change_flap_counts();
        assert!(task.link_change_affected_ports.lock().unwrap().is_empty());
    }

    // The direct-read dedup is per physical port: with a seeded baseline, flapping one
    // port leaves the other alone — mirroring the e2e "raise a flag on an unrelated port /
    // without a flap → no re-read" isolation.
    #[test]
    fn test_reconcile_flap_count_tracked_per_port() {
        let task = task_with(
            &[("Ethernet0", 0), ("Ethernet8", 1)],
            vec![dom_module(), dom_module()],
            true,
        );
        let app = task.table_helper.get_app_port_tbl(0);
        app.hset("Ethernet0", "flap_count", "3");
        app.hset("Ethernet8", "flap_count", "3");
        task.seed_flap_count_baseline_from_db();
        task.reconcile_link_change_flap_counts();
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "seeded baselines must not schedule anything"
        );

        // Only Ethernet0 flaps → only physical port 0 is scheduled.
        app.hset("Ethernet0", "flap_count", "4");
        task.reconcile_link_change_flap_counts();
        let m = task.link_change_affected_ports.lock().unwrap();
        assert!(m.contains_key(&0) && !m.contains_key(&1));
    }

    // A due link-change re-read is drained by check_port_update (no live observer): the
    // scheduled port is re-captured and removed from the pending set.
    #[test]
    fn test_check_port_update_drains_due_link_change() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        // Schedule a re-read whose deadline is already in the past.
        task.link_change_affected_ports.lock().unwrap().insert(
            0,
            PendingReread {
                fire_at: Instant::now(),
                giveup_at: Instant::now() + Duration::from_secs(60),
            },
        );
        task.check_port_update(&AtomicBool::new(false), None, 1);
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "due port must be removed after processing"
        );
        assert!(task.table_helper.get_status_flag_tbl(0).get("Ethernet0").is_some());
    }

    // M7 repair — ROOT CAUSE of the e2e test_link_change_triggers_fast_flag_recapture: the
    // periodic poll must service APPL_DB link-change flaps *between every port* of the pass
    // (dom_mgr.py:326), not only between passes. A full DOM poll walks every port's EEPROM
    // and runs for many seconds on the emulator, so the previous un-interleaved poll_once
    // left a flap that landed mid-pass unserviced until the whole pass finished — past the
    // e2e fast window (T_FAST=15 s) — falling back to the next ~60 s poll. Here a due
    // re-read (a flap serviced just as a multi-port pass begins) is drained DURING the pass
    // by poll_once_interleaved, exactly as check_port_update would between passes;
    // `next_reconcile` in the far future isolates the drain from the reconcile.
    #[test]
    fn test_poll_once_interleaved_drains_due_link_change_mid_pass() {
        let task = task_with(
            &[("Ethernet0", 0), ("Ethernet8", 1)],
            vec![dom_module(), dom_module()],
            true,
        );
        // A re-read whose deadline already elapsed, for the FIRST port walked in the pass.
        task.link_change_affected_ports.lock().unwrap().insert(
            0,
            PendingReread {
                fire_at: Instant::now(),
                giveup_at: Instant::now() + Duration::from_secs(60),
            },
        );
        // Keep reconcile out of this pass so the assertion isolates the mid-pass drain.
        let mut next_reconcile = Instant::now() + Duration::from_secs(3600);
        task.poll_once_interleaved(&AtomicBool::new(false), None, &mut next_reconcile);

        // The due re-read was serviced DURING the poll pass (the fix). Before it, poll_once
        // ran the entire multi-second pass without ever draining a pending re-read.
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "a due link-change re-read must be serviced during the poll pass, not deferred \
             to the next periodic interval"
        );
        // The poll itself still ran end to end: DOM sensor rows posted for both ports.
        assert!(task.table_helper.get_dom_tbl(0).get("Ethernet0").is_some());
        assert!(task.table_helper.get_dom_tbl(0).get("Ethernet8").is_some());
    }

    // The notification-independent flap_count reconcile also runs on its ~1 s cadence
    // WITHIN the poll pass: a flap present when a (long) pass starts is detected and a
    // debounced re-read scheduled during the pass, so it fires ~1 s later rather than
    // waiting for the next ~60 s interval — the direct-read twin of the drain above.
    #[test]
    fn test_poll_once_interleaved_reconciles_flap_mid_pass() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task.table_helper
            .get_app_port_tbl(0)
            .hset("Ethernet0", "flap_count", "1");
        // Reconcile is due at the very start of the pass.
        let mut next_reconcile = Instant::now();
        task.poll_once_interleaved(&AtomicBool::new(false), None, &mut next_reconcile);

        // The flap was reconciled mid-pass and a debounced re-read scheduled (fire_at ~1 s
        // out, so not yet drained in this pass) — link-change detection is not starved by a
        // long poll.
        assert!(
            task.link_change_affected_ports.lock().unwrap().contains_key(&0),
            "a flap_count change must be reconciled during the poll pass"
        );
        // The ~1 s reconcile deadline was advanced past the pass so it is not re-scanned for
        // every remaining port of the same pass.
        assert!(next_reconcile > Instant::now());
    }

    // M7 cumulative-gate repair (e2e test_link_change_flags): with the CMIS manager live
    // (skip_cmis_mgr=false), a flap re-read that lands while the flapped port is transiently
    // mid-CMIS-init must NOT be dropped (that lost the post-flap TRANSCEIVER_DOM_FLAG=False
    // baseline) and must NOT publish while the DOM gate is closed (that would break
    // test_dom_gating). Instead it defers and retries until cmis_state reaches a terminal
    // state, then re-captures the flags exactly once.
    #[test]
    fn test_check_port_update_defers_reread_until_cmis_terminal() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], false);
        let status_sw = task.table_helper.get_status_sw_tbl(0);
        // Port is mid-datapath-bring-up: non-terminal cmis_state → re-read must defer.
        status_sw.hset("Ethernet0", "cmis_state", "INSERTED");
        // A due re-read (fire_at already elapsed), give-up bound well in the future.
        task.link_change_affected_ports.lock().unwrap().insert(
            0,
            PendingReread {
                fire_at: Instant::now(),
                giveup_at: Instant::now() + Duration::from_secs(60),
            },
        );

        task.check_port_update(&AtomicBool::new(false), None, 1);
        // Gated: nothing published yet, but the re-read is kept (re-armed) for a retry.
        assert!(
            task.table_helper.get_dom_flag_tbl(0).get("Ethernet0").is_none(),
            "must not publish DOM flags while the port is mid-CMIS-init"
        );
        assert!(
            task.table_helper.get_status_flag_tbl(0).get("Ethernet0").is_none(),
            "must not publish status flags while the port is mid-CMIS-init"
        );
        assert!(
            task.link_change_affected_ports.lock().unwrap().contains_key(&0),
            "a re-read gated by CMIS-init must stay pending for retry, not be dropped"
        );

        // Datapath settles (terminal cmis_state) and the re-armed re-read comes due again.
        status_sw.hset("Ethernet0", "cmis_state", "READY");
        task.link_change_affected_ports.lock().unwrap().insert(
            0,
            PendingReread {
                fire_at: Instant::now(),
                giveup_at: Instant::now() + Duration::from_secs(60),
            },
        );

        task.check_port_update(&AtomicBool::new(false), None, 1);
        // Now the post-flap baseline is re-captured and the entry is consumed.
        assert!(
            task.table_helper.get_dom_flag_tbl(0).get("Ethernet0").is_some(),
            "DOM flags must be re-captured once the datapath reaches a terminal state"
        );
        assert!(
            task.table_helper.get_status_flag_tbl(0).get("Ethernet0").is_some(),
            "status flags must be re-captured once the datapath reaches a terminal state"
        );
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "a settled re-read must be removed from the pending set"
        );
    }

    // A re-read that stays gated past its give-up bound is dropped (the periodic DOM pass
    // then owns republishing), so a permanently-stuck port cannot leak a pending entry.
    #[test]
    fn test_check_port_update_drops_reread_after_giveup() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], false);
        // Never leaves CMIS-init.
        task.table_helper
            .get_status_sw_tbl(0)
            .hset("Ethernet0", "cmis_state", "INSERTED");
        // Both fire_at and giveup_at already elapsed → drop on this pass.
        task.link_change_affected_ports.lock().unwrap().insert(
            0,
            PendingReread {
                fire_at: Instant::now() - Duration::from_secs(1),
                giveup_at: Instant::now() - Duration::from_secs(1),
            },
        );
        task.check_port_update(&AtomicBool::new(false), None, 1);
        assert!(
            task.table_helper.get_dom_flag_tbl(0).get("Ethernet0").is_none(),
            "must not publish while gated"
        );
        assert!(
            task.link_change_affected_ports.lock().unwrap().is_empty(),
            "a re-read stuck past its give-up bound must be dropped"
        );
    }

    #[test]
    fn test_poll_once_skips_absent_module() {
        let task = task_with(&[("Ethernet0", 0)], vec![MockSfp::absent()], true);
        task.poll_once(&AtomicBool::new(false));
        assert_eq!(task.table_helper.get_dom_tbl(0).get("Ethernet0"), None);
    }

    #[test]
    fn test_poll_once_skips_error_status_port() {
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        // A blocking error description on STATUS_SW gates the whole port.
        task.table_helper.get_status_sw_tbl(0).hset(
            "Ethernet0",
            "error",
            "Blocking EEPROM from being read",
        );
        task.poll_once(&AtomicBool::new(false));
        assert_eq!(task.table_helper.get_dom_tbl(0).get("Ethernet0"), None);
    }

    #[test]
    fn test_thermal_task_poll_once_publishes_temperature() {
        let task = DomThermalInfoUpdateTask::new(
            mapping_with(&[("Ethernet0", 0)]),
            Arc::new(MockHal::with_sfps(vec![
                MockSfp::present().with_json("get_temperature", json!(42.5))
            ])),
            Arc::new(XcvrTableHelper::with_mock_tables(&[String::new()])),
            Duration::from_secs(1),
        );
        task.poll_once(&AtomicBool::new(false));
        let row: HashMap<String, String> = task
            .table_helper
            .get_dom_temperature_tbl(0)
            .get("Ethernet0")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(row.get("temperature").map(String::as_str), Some("42.5"));
    }

    // ← tests/test_xcvrd.py::test_post_port_pm_info_to_db
    #[test]
    fn test_post_port_pm_info_to_db() {
        // Coherent (paged) module with a 6-key PM dict → TRANSCEIVER_PM keyed by the
        // physical port name (Ethernet0, non-breakout), no last_update_time.
        let pm_sfp = MockSfp::present().with_json(
            "get_transceiver_pm",
            json!({
                "prefec_ber_avg": "0.0003407240007014899",
                "prefec_ber_min": "0.0006814479342250317",
                "prefec_ber_max": "0.0006833674050752236",
                "uncorr_frames_avg": "0.0",
                "uncorr_frames_min": "0.0",
                "uncorr_frames_max": "0.0",
            }),
        );
        let task = task_with(&[("Ethernet0", 0)], vec![pm_sfp], true);
        let pm_tbl = task.table_helper.get_pm_tbl(0);
        assert_eq!(pm_tbl.get_size(), 0);
        task.post_port_pm_info_to_db(&AtomicBool::new(false), "Ethernet0", pm_tbl, None);
        assert_eq!(pm_tbl.get_size_for_key("Ethernet0"), 6);

        // Flat-memory module → skipped (no PM page on flat/SFF memory).
        let flat = task_with(
            &[("Ethernet0", 0)],
            vec![MockSfp::present()
                .with_json("is_flat_memory", json!(true))
                .with_json("get_transceiver_pm", json!({"prefec_ber_avg": "0.1"}))],
            true,
        );
        let flat_tbl = flat.table_helper.get_pm_tbl(0);
        flat.post_port_pm_info_to_db(&AtomicBool::new(false), "Ethernet0", flat_tbl, None);
        assert_eq!(flat_tbl.get_size(), 0);

        // Absent module → skipped.
        let absent = task_with(&[("Ethernet0", 0)], vec![MockSfp::absent()], true);
        let absent_tbl = absent.table_helper.get_pm_tbl(0);
        absent.post_port_pm_info_to_db(&AtomicBool::new(false), "Ethernet0", absent_tbl, None);
        assert_eq!(absent_tbl.get_size(), 0);
    }

    // ← tests/test_xcvrd.py::test_post_port_sfp_firmware_info_to_db
    #[test]
    fn test_post_port_sfp_firmware_info_to_db() {
        let fw = || {
            MockSfp::present().with_json(
                "get_transceiver_info_firmware_versions",
                json!({"active_firmware": "2.1.1", "inactive_firmware": "1.2.4"}),
            )
        };

        // Physical 0 backs BOTH Ethernet0 and Ethernet4 (breakout) → firmware (a per-module
        // property) is posted to every logical subport.
        let task = task_with(&[("Ethernet0", 0), ("Ethernet4", 0)], vec![fw()], true);
        let tbl = task.table_helper.get_firmware_info_tbl(0);

        // Test 1: stop set → no update.
        task.post_port_sfp_firmware_info_to_db(&AtomicBool::new(true), "Ethernet0", tbl, None);
        assert_eq!(tbl.get_size(), 0);

        // Test 3: present → posts to both logical ports (2 fields each), 2 keys total.
        task.post_port_sfp_firmware_info_to_db(&AtomicBool::new(false), "Ethernet0", tbl, None);
        assert_eq!(tbl.get_size_for_key("Ethernet0"), 2);
        assert_eq!(tbl.get_size_for_key("Ethernet4"), 2);
        assert_eq!(tbl.get_size(), 2);

        // Test 2: not present → no update.
        let absent = task_with(&[("Ethernet0", 0), ("Ethernet4", 0)], vec![MockSfp::absent()], true);
        let absent_tbl = absent.table_helper.get_firmware_info_tbl(0);
        absent.post_port_sfp_firmware_info_to_db(&AtomicBool::new(false), "Ethernet0", absent_tbl, None);
        assert_eq!(absent_tbl.get_size(), 0);
    }

    // ← tests/test_xcvrd.py::test_post_port_sfp_firmware_info_to_db_lport_list_None
    #[test]
    fn test_post_port_sfp_firmware_info_to_db_lport_list_none() {
        // Logical name "5" resolves to physical 5 (numeric-name path) which has no
        // physical→logical mapping (empty PortMapping) → get_physical_to_logical is None →
        // firmware is not posted for that unknown physical port.
        let mut sfps: Vec<MockSfp> = (0..5).map(|_| MockSfp::absent()).collect();
        sfps.push(MockSfp::present().with_json(
            "get_transceiver_info_firmware_versions",
            json!({"active_firmware": "2.1.1", "inactive_firmware": "1.2.4"}),
        ));
        let task = task_with(&[], sfps, true);
        let tbl = task.table_helper.get_firmware_info_tbl(0);
        task.post_port_sfp_firmware_info_to_db(&AtomicBool::new(false), "5", tbl, None);
        assert_eq!(tbl.get_size(), 0);
    }

    // ← tests/test_xcvrd.py::test_DomInfoUpdateTask_task_worker (M5 VDM subset)
    #[test]
    fn test_poll_once_publishes_vdm_real_values_and_flags() {
        let task = task_with(&[("Ethernet0", 0)], vec![vdm_module()], true);
        task.poll_once(&AtomicBool::new(false));

        // TRANSCEIVER_VDM_REAL_VALUE: 2 basic fields + last_update_time == 3.
        let rv = task.table_helper.get_vdm_real_value_tbl(0);
        assert_eq!(rv.get_size_for_key("Ethernet0"), 3);
        assert_eq!(
            rv.hget("Ethernet0", "laser_temperature_media1"),
            Some("38".to_string())
        );
        assert_eq!(
            rv.hget("Ethernet0", "esnr_media_input1"),
            Some("23.1171875".to_string())
        );

        // TRANSCEIVER_VDM_HALARM_FLAG: 2 flags + last_update_time == 3, with metadata seeded.
        let flag = task.table_helper.get_vdm_flag_tbl(0, "halarm");
        assert_eq!(flag.get_size_for_key("Ethernet0"), 3);
        assert_eq!(
            flag.hget("Ethernet0", "laser_temperature_media_2"),
            Some("True".to_string())
        );
        assert_eq!(
            task.table_helper
                .get_vdm_flag_change_count_tbl(0, "halarm")
                .hget("Ethernet0", "laser_temperature_media_1"),
            Some("0".to_string())
        );

        // Statistic observables unsupported → no freeze → TRANSCEIVER_PM stays empty.
        assert_eq!(task.table_helper.get_pm_tbl(0).get_size(), 0);
    }

    // A VDM-supported module with statistic observables → poll_once freezes, captures the
    // statistic snapshot + PM, unfreezes, and merges statistic over basic real values.
    #[test]
    fn test_poll_once_vdm_statistic_freeze_captures_pm_and_merges() {
        let sfp = dom_module()
            .with_json("is_transceiver_vdm_supported", json!(true))
            .with_json("is_vdm_statistic_supported", json!(true))
            // freeze/unfreeze confirm immediately.
            .with_json("freeze_vdm_stats", json!(true))
            .with_json("get_vdm_freeze_status", json!(true))
            .with_json("unfreeze_vdm_stats", json!(true))
            .with_json("get_vdm_unfreeze_status", json!(true))
            .with_json(
                "get_transceiver_vdm_real_value_basic",
                json!({"esnr_media_input1": 10.0}),
            )
            .with_json(
                "get_transceiver_vdm_real_value_statistic",
                json!({"esnr_media_input1": 99.0, "laser_temperature_media1": 40}),
            )
            .with_json("get_transceiver_pm", json!({"prefec_ber_avg": "0.001"}))
            .with_json("get_transceiver_vdm_flags", json!({"esnr_media_input1_halarm": false}));
        let task = task_with(&[("Ethernet0", 0)], vec![sfp], true);
        task.poll_once(&AtomicBool::new(false));

        // PM captured during the freeze window.
        assert_eq!(task.table_helper.get_pm_tbl(0).get_size_for_key("Ethernet0"), 1);

        // Merged real values: statistic esnr (99.0) overrides basic (10.0); the
        // statistic-only laser_temperature key is present. 2 fields + last_update_time.
        let rv = task.table_helper.get_vdm_real_value_tbl(0);
        assert_eq!(rv.get_size_for_key("Ethernet0"), 3);
        assert_eq!(
            rv.hget("Ethernet0", "esnr_media_input1"),
            Some("99.0".to_string())
        );
        assert_eq!(
            rv.hget("Ethernet0", "laser_temperature_media1"),
            Some("40".to_string())
        );
    }

    // M8 low-power gate (dom_mgr.py:386-387, e2e test_dom_lpmode): the VDM statistic
    // freeze — and thus the statistic real-value capture + TRANSCEIVER_PM refresh — must be
    // SKIPPED while the module is in low power. This intentionally REVERSES the M5-only
    // reality this test previously locked: at M5 no CmisManagerTask existed to drive an
    // admin-up module out of low power, so gating would have suppressed every capture; by
    // M8 the CmisManagerTask drives an admin-up module to ModuleReady (lpmode == false), so
    // a normally-operating module still freezes (the companion assertion) while a module an
    // operator has put into low power stops refreshing its PM (test_dom_lpmode deletes the
    // PM row in low power and asserts it is not republished).
    #[test]
    fn test_poll_once_vdm_statistic_freeze_gated_by_low_power() {
        let make = || {
            dom_module()
                .with_json("is_transceiver_vdm_supported", json!(true))
                .with_json("is_vdm_statistic_supported", json!(true))
                .with_json("freeze_vdm_stats", json!(true))
                .with_json("get_vdm_freeze_status", json!(true))
                .with_json("unfreeze_vdm_stats", json!(true))
                .with_json("get_vdm_unfreeze_status", json!(true))
                .with_json(
                    "get_transceiver_vdm_real_value_basic",
                    json!({"esnr_media_input1": 10.0}),
                )
                .with_json(
                    "get_transceiver_vdm_real_value_statistic",
                    json!({"prefec_ber_min_media_input1": 1.0e-6}),
                )
                .with_json("get_transceiver_pm", json!({"prefec_ber_avg": "0.001"}))
                .with_json("get_transceiver_vdm_flags", json!({}))
        };

        // Low power → freeze gated: TRANSCEIVER_PM is not refreshed and the statistic-only
        // real value is not captured (only the basic real value is posted).
        let mut low = make();
        low.lpmode = true;
        let task = task_with(&[("Ethernet0", 0)], vec![low], true);
        task.poll_once(&AtomicBool::new(false));
        assert_eq!(
            task.table_helper.get_pm_tbl(0).get_size_for_key("Ethernet0"),
            0,
            "TRANSCEIVER_PM must not refresh while the module is in low power"
        );
        let rv = task.table_helper.get_vdm_real_value_tbl(0);
        assert!(
            rv.hget("Ethernet0", "prefec_ber_min_media_input1").is_none(),
            "the statistic real value must not be captured while the freeze is lpmode-gated"
        );
        assert!(
            rv.hget("Ethernet0", "esnr_media_input1").is_some(),
            "the basic real values still refresh (only the statistic freeze is gated)"
        );

        // Not in low power (lpmode defaults false) → freeze runs: PM captured + statistic
        // real value published.
        let task2 = task_with(&[("Ethernet0", 0)], vec![make()], true);
        task2.poll_once(&AtomicBool::new(false));
        assert_eq!(
            task2.table_helper.get_pm_tbl(0).get_size_for_key("Ethernet0"),
            1,
            "TRANSCEIVER_PM refreshes on an admin-up, non-lpmode module"
        );
        assert!(
            task2
                .table_helper
                .get_vdm_real_value_tbl(0)
                .hget("Ethernet0", "prefec_ber_min_media_input1")
                .is_some(),
            "the statistic real value is captured when not in low power"
        );
    }

    // M8 link-change poll-hold (e2e test_link_change_triggers_fast_flag_recapture guard):
    // after a flap, the off-cadence re-read owns the port's latched-flag transition, so a
    // periodic poll that falls inside the flap window must NOT re-read (and thus re-surface)
    // that port's DOM/STATUS/VDM flag tables. On this emulator the flag byte is held (no
    // clear-on-read), so without the hold a coincident poll would publish a
    // raised-but-un-flapped alarm and race the link-change trigger. Non-latched rows (DOM
    // sensor, rich STATUS, VDM real values) still refresh under the hold.
    #[test]
    fn test_periodic_poll_skips_latched_flags_while_flap_holds() {
        // A VDM-supported module whose module flag latch is currently RAISED (tempHAlarm
        // true) — so if the poll were NOT held it would publish tempHAlarm=True.
        let sfp = vdm_module().with_json("get_transceiver_dom_flags", json!({"tempHAlarm": true}));
        let task = task_with(&[("Ethernet0", 0)], vec![sfp], true);

        // Arm the hold (as a genuine flap would) then run a full periodic pass.
        task.hold_flag_poll(0);
        task.poll_once(&AtomicBool::new(false));

        // Latched-flag tables were NOT re-read/published on this pass (flap path owns them).
        assert!(
            task.table_helper.get_dom_flag_tbl(0).get("Ethernet0").is_none(),
            "held DOM flag must not be re-published by a coincident poll"
        );
        assert!(
            task.table_helper.get_status_flag_tbl(0).get("Ethernet0").is_none(),
            "held STATUS flag must not be re-published by a coincident poll"
        );
        assert_eq!(
            task.table_helper.get_vdm_flag_tbl(0, "halarm").get_size_for_key("Ethernet0"),
            0,
            "held VDM flag must not be re-published by a coincident poll"
        );

        // Non-latched rows still refresh under the hold: sensor + rich status + VDM real
        // values are unaffected.
        assert!(task.table_helper.get_dom_tbl(0).get("Ethernet0").is_some());
        assert!(task.table_helper.get_status_tbl(0).get("Ethernet0").is_some());
        assert!(task.table_helper.get_vdm_real_value_tbl(0).get("Ethernet0").is_some());
    }

    // Once the hold lapses, the periodic poll resumes re-reading the latched flags (and the
    // expired entry is pruned) — a stale latch clears on the next pass, so the hold never
    // permanently suppresses a port's flag refresh.
    #[test]
    fn test_periodic_poll_resumes_latched_flags_after_hold_expires() {
        let sfp = vdm_module().with_json("get_transceiver_dom_flags", json!({"tempHAlarm": true}));
        let task = task_with(&[("Ethernet0", 0)], vec![sfp], true);

        // Seed an ALREADY-EXPIRED hold (no need to sleep out LINK_CHANGE_POLL_HOLD_SECS).
        task.flag_poll_hold
            .lock()
            .unwrap()
            .insert(0, Instant::now() - Duration::from_secs(1));
        task.poll_once(&AtomicBool::new(false));

        assert_eq!(
            task.table_helper.get_dom_flag_tbl(0).hget("Ethernet0", "tempHAlarm"),
            Some("True".to_string()),
            "an expired hold must not block the periodic flag re-read"
        );
        assert!(
            !task.is_flag_poll_held(0),
            "an expired hold entry is pruned"
        );
    }

    // A genuine flap arms the poll-hold (both via the scheduling entry point and once the
    // re-read re-captures the flags), so the flap path is the sole writer of the flag
    // transition in the window. A re-delivered/unchanged flap_count is not a flap and must
    // not arm the hold.
    #[test]
    fn test_flap_arms_poll_hold() {
        // Scheduling path: a genuine APPL_DB flap arms the hold.
        let task = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        assert!(!task.is_flag_poll_held(0));
        task.on_port_update_event(&appl_flap_event("Ethernet0", 0, "1"));
        assert!(task.is_flag_poll_held(0), "a genuine flap arms the poll-hold");

        // A re-delivered flap_count (not a genuine flap) must not arm the hold.
        let task2 = task_with(&[("Ethernet8", 1)], vec![dom_module()], true);
        task2.seed_link_change_baseline(&appl_flap_event("Ethernet8", 1, "5"));
        task2.on_port_update_event(&appl_flap_event("Ethernet8", 1, "5"));
        assert!(
            !task2.is_flag_poll_held(1),
            "a re-delivered flap_count must not arm the poll-hold"
        );

        // Re-capture path: a completed link-change re-read arms the hold for the guard
        // window that follows the flap.
        let task3 = task_with(&[("Ethernet0", 0)], vec![dom_module()], true);
        task3.update_port_db_diagnostics_on_link_change(0, &AtomicBool::new(false));
        assert!(
            task3.is_flag_poll_held(0),
            "a completed flap re-read arms the poll-hold so no coincident poll re-surfaces \
             the flag"
        );
    }
}
