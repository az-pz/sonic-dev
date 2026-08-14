#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `dom/dom_mgr.py`: `DomInfoUpdateTask` — the periodic DOM poll loop.
//!
//! The task state (port map, `link_change_affected_ports`, the update interval)
//! and the poll/scheduling logic are ported faithfully; every side effect (the
//! DOM/status/VDM/PM posters, the port-event observer, presence, gating, the
//! clock and logging) flows through the [`DomEnv`] seam so unit tests can
//! inject a scriptable/counting mock and assert the same collaborator cadence the
//! Python `test_xcvrd.py` asserts.
//!
//! Time is modelled as `f64` seconds via [`DomEnv::now_secs`] (the analogue of
//! `datetime.datetime.now()`); scheduling is computed off the **loop start**
//! timestamp exactly like the Python, which unit tests exercise directly.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::{json, Value};

use crate::db::Table;
use crate::dom::utilities::db::utils::{beautify_info_dict, beautify_info_row, py_str};
use crate::hal::Chassis;
use crate::xcvrd::{Event, SFP_EEPROM_NOT_READY};
use crate::xcvrd_utilities::common::{
    get_cmis_state_from_state_db, get_physical_port_name_dict,
    wrapper_get_transceiver_firmware_info, wrapper_get_transceiver_pm, wrapper_is_flat_memory,
    CMIS_TERMINAL_STATES,
};
use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType, PortMapping};

pub const SYSLOG_IDENTIFIER_DOMINFOUPDATETASK: &str = "DomInfoUpdateTask";
pub const PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS: i64 = 1000;
pub const PORT_UPDATE_EVENT_SELECT_TIMEOUT_FAST_MSECS: i64 = 10;

/// The external collaborators of [`DomInfoUpdateTask::task_worker`] — the module
/// boundary `test_xcvrd.py` patches (`subscribe/handle_port_config_change`, the
/// `PortChangeObserver`, `common._wrapper_get_presence`,
/// `sfp_status_helper.detect_port_in_error_status`, `get_dom_polling_from_config_db`,
/// the DOM/status/VDM/PM posters, `del_port_sfp_dom_info_from_db`, `datetime.now`,
/// …). Bundling them behind one trait keeps the loop pure and lets a unit test
/// inject a counting/scriptable mock.
pub trait DomEnv {
    /// `port_event_helper.subscribe_port_config_change(namespaces)` — setup; may
    /// fail (the analogue of the Python `NotImplementedError` the run-exception
    /// test surfaces).
    fn subscribe_port_config_change(&self) -> Result<(), String> {
        Ok(())
    }
    /// `port_event_helper.handle_port_config_change(...)` (per outer iteration).
    fn handle_port_config_change(&self) {}
    /// `PortChangeObserver.handle_port_update_event(timeout)`.
    fn handle_port_update_event(&self, timeout_msecs: i64);
    /// `datetime.datetime.now()` as seconds (monotonic within a run).
    fn now_secs(&self) -> f64;
    /// `sfp_status_helper.detect_port_in_error_status(lport, status_sw_tbl)`.
    fn detect_port_in_error_status(&self, logical_port_name: &str, asic_index: i32) -> bool;
    /// `get_dom_polling_from_config_db(lport)` → `"enabled"` / `"disabled"`.
    fn get_dom_polling(&self, logical_port_name: &str) -> String;
    /// `is_port_in_cmis_initialization_process(lport)`.
    fn is_port_in_cmis_init(&self, logical_port_name: &str) -> bool {
        false
    }
    /// `common._wrapper_get_presence(physical_port)`.
    fn is_present(&self, physical_port: i32) -> bool;
    /// `post_port_sfp_firmware_info_to_db(lport, ...)` → rc.
    fn post_port_sfp_firmware_info_to_db(&self, logical_port_name: &str) -> i32 {
        0
    }
    /// `dom_db_utils.post_port_dom_sensor_info_to_db(lport)`.
    fn post_port_dom_sensor_info_to_db(&self, logical_port_name: &str);
    /// `dom_db_utils.post_port_dom_flags_to_db(lport)`.
    fn post_port_dom_flags_to_db(&self, logical_port_name: &str);
    /// `status_db_utils.post_port_transceiver_hw_status_to_db(lport)`.
    fn post_port_transceiver_hw_status_to_db(&self, logical_port_name: &str);
    /// `status_db_utils.post_port_transceiver_hw_status_flags_to_db(lport)`.
    fn post_port_transceiver_hw_status_flags_to_db(&self, logical_port_name: &str);
    /// `vdm_utils.is_transceiver_vdm_supported(physical_port)`.
    fn is_transceiver_vdm_supported(&self, physical_port: i32) -> bool {
        false
    }
    /// `vdm_utils.is_vdm_statistic_supported(physical_port)`.
    fn is_vdm_statistic_supported(&self, physical_port: i32) -> bool {
        false
    }
    /// `xcvrd_utils.is_transceiver_lpmode_on(physical_port)`.
    fn is_transceiver_lpmode_on(&self, physical_port: i32) -> bool {
        false
    }
    /// Enter `vdm_utils.vdm_freeze_context` → freeze + confirm (True == frozen).
    fn vdm_freeze(&self, physical_port: i32) -> bool {
        true
    }
    /// Exit the freeze context → unfreeze + confirm.
    fn vdm_unfreeze(&self, physical_port: i32) -> bool {
        true
    }
    /// `vdm_utils.get_vdm_real_values_statistic(physical_port)`.
    fn get_vdm_real_values_statistic(&self, physical_port: i32) -> Value {
        json!({})
    }
    /// `vdm_utils.get_vdm_real_values_basic(physical_port)`.
    fn get_vdm_real_values_basic(&self, physical_port: i32) -> Value {
        json!({})
    }
    /// `post_port_pm_info_to_db(lport, ...)`.
    fn post_port_pm_info_to_db(&self, logical_port_name: &str) {}
    /// `vdm_db_utils.post_port_vdm_real_values_from_dict_to_db(lport, values)`.
    fn post_port_vdm_real_values_from_dict_to_db(&self, logical_port_name: &str, values: Value) {}
    /// `vdm_db_utils.post_port_vdm_flags_to_db(lport)`.
    fn post_port_vdm_flags_to_db(&self, logical_port_name: &str) {}
    /// `common.del_port_sfp_dom_info_from_db(lport, port_mapping, [...])`.
    fn del_port_sfp_dom_info(&self, logical_port_name: &str, asic_id: i32);
    /// `dom_db_utils.post_port_dom_temperature_info_to_db(lport)` — publish only the
    /// module temperature to `TRANSCEIVER_DOM_SENSOR` (the lighter poll owned by
    /// [`DomThermalInfoUpdateTask`]).
    fn post_port_dom_temperature_info_to_db(&self, logical_port_name: &str) {}
    /// `time.sleep(secs)` — the thermal loop's pacing sleep (no-op in tests).
    fn sleep_secs(&self, secs: f64) {}
    /// `self.log_notice(msg)`.
    fn log_notice(&self, msg: &str) {}
    /// `self.log_warning(msg)`.
    fn log_warning(&self, msg: &str) {}
}

/// Rust port of the Python `DomInfoUpdateTask` (dom_mgr.py:141).
pub struct DomInfoUpdateTask {
    pub name: String,
    pub port_mapping: PortMapping,
    pub skip_cmis_mgr: bool,
    pub dom_update_interval: i64,
    /// Physical port → wall-clock deadline (seconds) to refresh diagnostics after
    /// a link change (`link_change_affected_ports`).
    pub link_change_affected_ports: BTreeMap<i32, f64>,
    // ---- thread lifecycle (`threading.Thread` analogue) ----
    stop: Arc<Event>,
    main_thread_stop: Arc<Event>,
    exc: Arc<Mutex<Option<String>>>,
    handle: Option<JoinHandle<Result<(), String>>>,
}

impl DomInfoUpdateTask {
    pub const DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS: i64 = 60;
    pub const DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE: i64 = 1;

    /// `__init__` (dom_mgr.py:150). `dom_update_interval`: `None` → default `60`,
    /// negative → warn + default, else the provided value.
    pub fn new(
        port_mapping: PortMapping,
        main_thread_stop: Arc<Event>,
        skip_cmis_mgr: bool,
        dom_update_interval: Option<i64>,
    ) -> Self {
        let mut interval = Self::DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS;
        if let Some(v) = dom_update_interval {
            if v < 0 {
                eprintln!(
                    "Invalid dom_update_interval {v} provided; using default {} seconds instead",
                    Self::DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS
                );
            } else {
                interval = v;
            }
        }
        DomInfoUpdateTask {
            name: "DomInfoUpdateTask".to_string(),
            port_mapping,
            skip_cmis_mgr,
            dom_update_interval: interval,
            link_change_affected_ports: BTreeMap::new(),
            stop: Event::new(),
            main_thread_stop,
            exc: Arc::new(Mutex::new(None)),
            handle: None,
        }
    }

    /// The task's stop event (`task_stopping_event`), shared so a test's env can
    /// end the loop the way the real `join()` would.
    pub fn stop_event(&self) -> Arc<Event> {
        self.stop.clone()
    }

    pub fn main_thread_stop_event(&self) -> Arc<Event> {
        self.main_thread_stop.clone()
    }

    /// `on_port_config_change` (dom_mgr.py:68): a REMOVE first purges the port's
    /// DOM rows, then the map is updated for every event kind.
    pub fn on_port_config_change(&mut self, env: &dyn DomEnv, port_change_event: &PortChangeEvent) {
        if port_change_event.event_type == PortEventType::PortRemove {
            self.on_remove_logical_port(env, port_change_event);
        }
        self.port_mapping.handle_port_change_event(port_change_event);
    }

    /// `on_remove_logical_port` (dom_mgr.py:495): purge the removed port's DOM /
    /// status / VDM rows.
    pub fn on_remove_logical_port(&self, env: &dyn DomEnv, port_change_event: &PortChangeEvent) {
        env.del_port_sfp_dom_info(&port_change_event.port_name, port_change_event.asic_id);
    }

    /// `post_port_sfp_firmware_info_to_db` (dom_mgr.py:203): for every present physical port
    /// backing `logical_port_name`, read the active/inactive firmware versions and write them
    /// to `TRANSCEIVER_FIRMWARE_INFO` for **all** logical ports sharing that physical port. An
    /// empty/unreadable firmware dict returns [`SFP_EEPROM_NOT_READY`] (the EEPROM-not-ready
    /// retry signal); a `stop_event` set mid-loop breaks out. The chassis / destination table
    /// flow through the HAL/DB seams so unit tests inject mocks (the analogue of the Python
    /// `common._wrapper_*` module globals).
    pub fn post_port_sfp_firmware_info_to_db(
        &self,
        logical_port_name: &str,
        chassis: &dyn Chassis,
        table: &dyn Table,
        stop_event: &Event,
        firmware_info_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) -> i32 {
        for (physical_port, _physical_port_name) in
            get_physical_port_name_dict(logical_port_name, &self.port_mapping)
        {
            if stop_event.is_set() {
                break;
            }
            let sfp = match chassis.sfp(physical_port as usize) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !sfp.get_presence().unwrap_or(false) {
                continue;
            }
            let firmware = match firmware_info_cache {
                Some(cache) => {
                    let cached = cache.borrow().get(&physical_port).cloned();
                    match cached {
                        Some(v) => v,
                        None => {
                            let v = wrapper_get_transceiver_firmware_info(
                                chassis,
                                physical_port as usize,
                            );
                            cache.borrow_mut().insert(physical_port, v.clone());
                            v
                        }
                    }
                }
                None => wrapper_get_transceiver_firmware_info(chassis, physical_port as usize),
            };
            match beautify_info_row(&firmware) {
                Some(row) => {
                    // Firmware info is shared, so update every logical port on this physical port.
                    let logical_port_list = match self.port_mapping.get_physical_to_logical(physical_port) {
                        Some(list) => list,
                        None => {
                            eprintln!(
                                "Got unknown physical port index {physical_port} while updating firmware info"
                            );
                            continue;
                        }
                    };
                    for lport in logical_port_list {
                        let _ = table.set(&lport, &row);
                    }
                }
                None => return SFP_EEPROM_NOT_READY,
            }
        }
        0
    }

    /// `post_port_pm_info_to_db` (dom_mgr.py:238): for every present, non-flat-memory physical
    /// port backing `logical_port_name`, read the coherent performance-monitoring values and
    /// write the beautified row to `TRANSCEIVER_PM` under the **physical** port name. A `None`
    /// PM read returns [`SFP_EEPROM_NOT_READY`]; an empty PM dict (the API is N/A for this xcvr)
    /// is skipped; a `stop_event` set mid-loop breaks out.
    pub fn post_port_pm_info_to_db(
        &self,
        logical_port_name: &str,
        chassis: &dyn Chassis,
        table: &dyn Table,
        stop_event: &Event,
        pm_info_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) -> i32 {
        for (physical_port, physical_port_name) in
            get_physical_port_name_dict(logical_port_name, &self.port_mapping)
        {
            if stop_event.is_set() {
                break;
            }
            let sfp = match chassis.sfp(physical_port as usize) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !sfp.get_presence().unwrap_or(false) {
                continue;
            }
            if wrapper_is_flat_memory(chassis, physical_port as usize) {
                continue;
            }
            let pm = match pm_info_cache {
                Some(cache) => {
                    let cached = cache.borrow().get(&physical_port).cloned();
                    match cached {
                        Some(v) => v,
                        None => {
                            let v = wrapper_get_transceiver_pm(chassis, physical_port as usize);
                            cache.borrow_mut().insert(physical_port, v.clone());
                            v
                        }
                    }
                }
                None => wrapper_get_transceiver_pm(chassis, physical_port as usize),
            };
            // Python: None → not-ready; {} → skip (API N/A); else post.
            if pm.is_null() {
                return SFP_EEPROM_NOT_READY;
            }
            let obj = match pm.as_object() {
                Some(o) if !o.is_empty() => o.clone(),
                _ => continue,
            };
            let mut beautified = obj;
            beautify_info_dict(&mut beautified);
            let fvs: Vec<(String, String)> =
                beautified.iter().map(|(k, v)| (k.clone(), py_str(v))).collect();
            let _ = table.set(&physical_port_name, &fvs);
        }
        0
    }

    /// `get_dom_polling_from_config_db` (dom_mgr.py:76): the per-port `dom_polling`
    /// knob read live from CONFIG_DB `PORT`. For a breakout group the value is taken
    /// from the group's FIRST subport (the natsorted lead logical port backing the
    /// physical port), so every subport shares the lead's setting; a non-breakout
    /// port reads its own row. Returns `"disabled"` only when that lead port's
    /// `dom_polling` field is literally `"disabled"`; an absent field or an unknown
    /// port defaults to `"enabled"`. `cfg_port_tbl` is the CONFIG_DB PORT table seam
    /// already resolved for the lead port's asic (`xcvr_table_helper.get_cfg_port_tbl`),
    /// so unit tests inject a `MockTable`.
    pub fn get_dom_polling_from_config_db(&self, lport: &str, cfg_port_tbl: &dyn Table) -> String {
        let mut dom_polling = "enabled".to_string();

        let pport_list = match self.port_mapping.get_logical_to_physical(lport) {
            Some(list) if !list.is_empty() => list,
            _ => {
                eprintln!(
                    "Get dom disabled: Got unknown physical port list None for lport {lport}"
                );
                return dom_polling;
            }
        };
        let pport = pport_list[0];

        let logical_port_list = match self.port_mapping.get_physical_to_logical(pport) {
            Some(list) if !list.is_empty() => list,
            _ => {
                eprintln!("Get dom disabled: Got unknown FP port index {pport}");
                return dom_polling;
            }
        };
        // First logical port corresponds to the first subport.
        let first_logical_port = &logical_port_list[0];

        if let Ok(Some(port_info)) = cfg_port_tbl.get(first_logical_port) {
            if let Some((_, value)) = port_info.iter().find(|(field, _)| field == "dom_polling") {
                dom_polling = value.clone();
            }
        }
        dom_polling
    }

    /// `is_port_in_cmis_initialization_process` (dom_mgr.py:182): true while a port's
    /// CMIS bring-up has NOT reached a terminal state — its `cmis_state`
    /// (STATE_DB `TRANSCEIVER_STATUS_SW`) is outside
    /// `common.CMIS_TERMINAL_STATES = {READY, FAILED, REMOVED}`. The transitional
    /// `UNKNOWN` (the first state after the datapath machine starts, and the default
    /// when the field is absent) counts as in-init, so the DOM poll defers and lets
    /// bring-up complete before it publishes the DomInfoUpdateTask-owned tables. When
    /// the platform has no CMIS manager (`skip_cmis_mgr`) the gate is never armed. An
    /// unmapped logical port (no asic index) is logged and treated as not-in-init.
    /// `status_sw_tbl` is the STATE_DB status table seam already resolved for the
    /// port's asic (`xcvr_table_helper.get_status_sw_tbl`).
    pub fn is_port_in_cmis_initialization_process(
        &self,
        logical_port_name: &str,
        status_sw_tbl: &dyn Table,
    ) -> bool {
        // If CMIS manager is not available for the platform, return False.
        if self.skip_cmis_mgr {
            return false;
        }

        if self
            .port_mapping
            .get_asic_id_for_logical_port(logical_port_name)
            .is_none()
        {
            eprintln!(
                "Got invalid asic index for {logical_port_name} while checking cmis init status"
            );
            return false;
        }

        let cmis_state = get_cmis_state_from_state_db(logical_port_name, status_sw_tbl);
        !CMIS_TERMINAL_STATES.contains(&cmis_state.as_str())
    }

    /// `is_port_dom_monitoring_disabled` (dom_mgr.py:198): the DOM poll is paused for
    /// a port when the `dom_polling` knob is `disabled` OR the port is still in CMIS
    /// datapath initialization. The two predicates flow through the [`DomEnv`] seam
    /// (backed in production by [`Self::get_dom_polling_from_config_db`] /
    /// [`Self::is_port_in_cmis_initialization_process`]) so the loop stays unit
    /// testable with a scriptable mock. The `dom_polling == "disabled"` short-circuit
    /// mirrors Python's `or`; the `skip_cmis_mgr` early-out reproduces the identical
    /// guard inside `is_port_in_cmis_initialization_process`.
    fn is_port_dom_monitoring_disabled(&self, env: &dyn DomEnv, logical_port_name: &str) -> bool {
        if env.get_dom_polling(logical_port_name) == "disabled" {
            return true;
        }
        if self.skip_cmis_mgr {
            return false;
        }
        env.is_port_in_cmis_init(logical_port_name)
    }

    /// `check_port_update` (dom_mgr.py:267): drain port-update events, then apply
    /// any link-change refreshes whose deadline has elapsed.
    pub fn check_port_update(
        &mut self,
        env: &dyn DomEnv,
        stopping: &mut dyn FnMut() -> bool,
        timeout_msecs: i64,
    ) {
        env.handle_port_update_event(timeout_msecs);

        let ports: Vec<i32> = self.link_change_affected_ports.keys().copied().collect();
        for link_changed_port in ports {
            if stopping() {
                env.log_notice("Stop event generated during DOM link change event processing");
                break;
            }
            let due = self
                .link_change_affected_ports
                .get(&link_changed_port)
                .map(|&deadline| deadline <= env.now_secs())
                .unwrap_or(false);
            if due {
                env.log_notice(&format!(
                    "Updating port db diagnostics post link change for port {link_changed_port}"
                ));
                self.update_port_db_diagnostics_on_link_change(env, link_changed_port);
                self.link_change_affected_ports.remove(&link_changed_port);
            }
        }
    }

    /// `on_port_update_event` (dom_mgr.py:427): on an APPL_DB `PORT_SET` (a link
    /// flap bumps `flap_count`) schedule that physical port's flag tables to be
    /// re-read `DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE` seconds later. Consolidates a
    /// breakout group's subport flaps into a single per-physical-port deadline. Any
    /// other event kind / DB is ignored.
    pub fn on_port_update_event(&mut self, env: &dyn DomEnv, port_change_event: &PortChangeEvent) {
        if port_change_event.event_type == PortEventType::PortSet
            && port_change_event.db_name.as_deref() == Some("APPL_DB")
        {
            let deadline = env.now_secs() + Self::DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE as f64;
            self.link_change_affected_ports
                .insert(port_change_event.port_index, deadline);
        }
    }

    /// `update_port_db_diagnostics_on_link_change` (dom_mgr.py:445): re-read ONLY the
    /// latched flag tables (`TRANSCEIVER_DOM_FLAG`, `TRANSCEIVER_STATUS_FLAG`, and —
    /// when VDM is supported — `TRANSCEIVER_VDM_*_FLAG`) for the physical port after
    /// a link change. A fast, targeted trigger separate from presence and the
    /// periodic poll. Gated the same way as the poll's flag publish: known physical
    /// port, DOM monitoring enabled, valid asic index, not in error status, present.
    pub fn update_port_db_diagnostics_on_link_change(&self, env: &dyn DomEnv, physical_port: i32) {
        if self.stop.is_set() {
            return;
        }
        // `physical_port not in self.port_obj_dict` / unknown physical index: the
        // logical mapping is our port-known analogue.
        let logical_port_list = match self.port_mapping.get_physical_to_logical(physical_port) {
            Some(list) if !list.is_empty() => list,
            _ => {
                env.log_warning(&format!(
                    "Update DB diagnostics during link change: Unknown physical port index {physical_port}"
                ));
                return;
            }
        };
        // First logical port corresponds to the first subport.
        let first_logical_port = logical_port_list[0].clone();

        if self.is_port_dom_monitoring_disabled(env, &first_logical_port) {
            return;
        }
        let asic_index = match self.port_mapping.get_asic_id_for_logical_port(&first_logical_port) {
            Some(a) => a,
            None => {
                env.log_warning(&format!(
                    "Update DB diagnostics during link change: Got invalid asic index for {first_logical_port}, ignored"
                ));
                return;
            }
        };
        if env.detect_port_in_error_status(&first_logical_port, asic_index) {
            return;
        }
        if !env.is_present(physical_port) {
            return;
        }

        // Update TRANSCEIVER_DOM_FLAG + metadata, then TRANSCEIVER_STATUS_FLAG +
        // metadata, then (if the module advertises VDM) the VDM flag tables.
        env.post_port_dom_flags_to_db(&first_logical_port);
        env.post_port_transceiver_hw_status_flags_to_db(&first_logical_port);
        if env.is_transceiver_vdm_supported(physical_port) {
            env.post_port_vdm_flags_to_db(&first_logical_port);
        }
    }

    /// `task_worker` (dom_mgr.py:284): the DOM monitoring loop. Runs until
    /// `stopping()`; each pass polls every physical port on the loop-start cadence.
    pub fn task_worker(
        &mut self,
        env: &dyn DomEnv,
        stopping: &mut dyn FnMut() -> bool,
    ) -> Result<(), String> {
        env.log_notice("Start DOM monitoring loop");
        env.subscribe_port_config_change()?;

        let interval = self.dom_update_interval as f64;
        let mut next_periodic_db_update_time = env.now_secs() + interval;

        while !stopping() {
            env.handle_port_config_change();

            loop {
                let remaining_secs = next_periodic_db_update_time - env.now_secs();
                if remaining_secs <= 0.0 {
                    break;
                }
                let select_timeout_msecs = PORT_UPDATE_EVENT_SELECT_TIMEOUT_MSECS
                    .min((remaining_secs * 1000.0) as i64)
                    .max(1);
                self.check_port_update(env, stopping, select_timeout_msecs);
                if stopping() {
                    env.log_notice(
                        "Stop event generated during DOM monitoring loop while checking port update",
                    );
                    break;
                }
            }

            if stopping() {
                break;
            }

            let dom_loop_start_time = env.now_secs();
            let ports: Vec<(i32, Vec<String>)> = self
                .port_mapping
                .physical_to_logical
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            for (physical_port, logical_ports) in ports {
                self.check_port_update(env, stopping, PORT_UPDATE_EVENT_SELECT_TIMEOUT_FAST_MSECS);

                if stopping() {
                    env.log_notice("Stop event generated during DOM monitoring loop");
                    break;
                }

                let logical_port_name = &logical_ports[0];

                if self.is_port_dom_monitoring_disabled(env, logical_port_name) {
                    continue;
                }

                let asic_index = match self.port_mapping.get_asic_id_for_logical_port(logical_port_name) {
                    Some(a) => a,
                    None => {
                        env.log_warning(&format!("Got invalid asic index for {logical_port_name}, ignored"));
                        continue;
                    }
                };

                if !env.detect_port_in_error_status(logical_port_name, asic_index) {
                    if !env.is_present(physical_port) {
                        continue;
                    }

                    env.post_port_sfp_firmware_info_to_db(logical_port_name);
                    env.post_port_dom_sensor_info_to_db(logical_port_name);
                    env.post_port_dom_flags_to_db(logical_port_name);
                    env.post_port_transceiver_hw_status_to_db(logical_port_name);
                    env.post_port_transceiver_hw_status_flags_to_db(logical_port_name);

                    if env.is_transceiver_vdm_supported(physical_port) {
                        // (a) capture statistic observables + PM under a VDM freeze.
                        let mut vdm_statistic_values = json!({});
                        let need_freeze = env.is_vdm_statistic_supported(physical_port)
                            && !env.is_transceiver_lpmode_on(physical_port);
                        if need_freeze {
                            let vdm_frozen = env.vdm_freeze(physical_port);
                            if !vdm_frozen {
                                env.log_warning(&format!(
                                    "Failed to freeze VDM stats for port {physical_port}"
                                ));
                            } else {
                                vdm_statistic_values = env.get_vdm_real_values_statistic(physical_port);
                                env.post_port_pm_info_to_db(logical_port_name);
                            }
                            env.vdm_unfreeze(physical_port);
                        }

                        // (b) merge with basic observables and post.
                        let vdm_basic_values = env.get_vdm_real_values_basic(physical_port);
                        let merged = merge_objects(&vdm_basic_values, &vdm_statistic_values);
                        env.post_port_vdm_real_values_from_dict_to_db(logical_port_name, merged);

                        // (c) COR flags last.
                        env.post_port_vdm_flags_to_db(logical_port_name);
                    }
                }
            }

            next_periodic_db_update_time = dom_loop_start_time + interval;
        }

        env.log_notice("Stop DOM monitoring loop");
        Ok(())
    }

    // ---- thread lifecycle -------------------------------------------------
    /// Spawn the worker thread (`threading.Thread.start`). The body mirrors the
    /// Python `run()`: a set stop event returns immediately; any error is stored,
    /// signals the main-thread stop event, and is re-raised by `join()`.
    pub fn start_worker<F>(&mut self, worker: F)
    where
        F: FnOnce() -> Result<(), String> + Send + 'static,
    {
        // The "was I told to stop before I even began?" gate (`run()`'s opening
        // `if self.task_stopping_event.is_set(): return`) is decided at start()
        // time, mirroring Python where the guard reflects the state at thread
        // entry. Capturing it here (rather than re-reading `stop` inside the
        // thread) avoids racing with `join()`, which sets `stop` to signal the
        // *running* worker loop to exit — not to cancel a not-yet-started worker.
        let start_gated = self.stop.is_set();
        let main_stop = self.main_thread_stop.clone();
        let exc = self.exc.clone();
        self.handle = Some(std::thread::spawn(move || {
            if start_gated {
                return Ok(());
            }
            match worker() {
                Ok(()) => Ok(()),
                Err(e) => {
                    *exc.lock().unwrap() = Some(e.clone());
                    main_stop.set();
                    Err(e)
                }
            }
        }));
    }

    /// `join()` (dom_mgr.py:129): set the stop event, join, re-raise the stored
    /// exception.
    pub fn join(&mut self) -> Result<(), String> {
        self.stop.set();
        match self.handle.take() {
            Some(h) => h.join().unwrap_or_else(|_| Err("DomInfoUpdateTask thread panicked".to_string())),
            None => Ok(()),
        }
    }

    /// `threading.Thread.is_alive()`.
    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().map(|h| !h.is_finished()).unwrap_or(false)
    }

    pub fn take_exception(&self) -> Option<String> {
        self.exc.lock().unwrap().clone()
    }
}

/// Rust port of the Python `DomThermalInfoUpdateTask` (dom_mgr.py:533): a lightweight
/// periodic loop that publishes only the module temperature for every present, monitored
/// physical port. Runs on its own `poll_interval` cadence and shares the `DomEnv` seam so
/// the loop stays pure and unit-testable.
pub struct DomThermalInfoUpdateTask {
    pub name: String,
    pub port_mapping: PortMapping,
    /// The physical ports backed by a device object (`port_obj_dict`); a physical port
    /// absent from this set is skipped (the Python `if physical_port not in
    /// self.port_obj_dict`).
    pub port_obj_dict: BTreeSet<i32>,
    pub skip_cmis_mgr: bool,
    pub poll_interval: i64,
    stop: Arc<Event>,
    main_thread_stop: Arc<Event>,
    exc: Arc<Mutex<Option<String>>>,
    handle: Option<JoinHandle<Result<(), String>>>,
}

impl DomThermalInfoUpdateTask {
    /// `__init__` (dom_mgr.py:536).
    pub fn new(
        port_mapping: PortMapping,
        port_obj_dict: BTreeSet<i32>,
        main_thread_stop: Arc<Event>,
        skip_cmis_mgr: bool,
        poll_interval: i64,
    ) -> Self {
        DomThermalInfoUpdateTask {
            name: "DomThermalInfoUpdateTask".to_string(),
            port_mapping,
            port_obj_dict,
            skip_cmis_mgr,
            poll_interval,
            stop: Event::new(),
            main_thread_stop,
            exc: Arc::new(Mutex::new(None)),
            handle: None,
        }
    }

    pub fn stop_event(&self) -> Arc<Event> {
        self.stop.clone()
    }

    pub fn main_thread_stop_event(&self) -> Arc<Event> {
        self.main_thread_stop.clone()
    }

    /// `is_port_dom_monitoring_disabled` (dom_mgr.py:198) — same predicate the DOM poll
    /// uses: paused when `dom_polling == "disabled"` or (with CMIS active) the port is
    /// still in datapath initialization.
    fn is_port_dom_monitoring_disabled(&self, env: &dyn DomEnv, logical_port_name: &str) -> bool {
        if env.get_dom_polling(logical_port_name) == "disabled" {
            return true;
        }
        if self.skip_cmis_mgr {
            return false;
        }
        env.is_port_in_cmis_init(logical_port_name)
    }

    /// `task_worker` (dom_mgr.py:542): poll temperature ASAP, then every
    /// `poll_interval` seconds. Each pass, for every physical port in `port_obj_dict`,
    /// publish the temperature for the first logical subport — skipping ports whose DOM
    /// monitoring is disabled, whose ASIC index is invalid, or which are neither in error
    /// status nor present. Per-module isolation: one port's skip/failure never blocks the
    /// others.
    pub fn task_worker(
        &mut self,
        env: &dyn DomEnv,
        stopping: &mut dyn FnMut() -> bool,
    ) -> Result<(), String> {
        env.log_notice("Start DOM thermal monitoring loop");

        let interval = self.poll_interval as f64;
        // Poll transceiver temperature as soon as possible.
        let mut next_periodic_db_update_time = env.now_secs();

        while !stopping() {
            let now = env.now_secs();
            if next_periodic_db_update_time > now {
                env.sleep_secs((1.0_f64).min(next_periodic_db_update_time - now).max(0.0));
                continue;
            }

            let ports: Vec<(i32, Vec<String>)> = self
                .port_mapping
                .physical_to_logical
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            for (physical_port, logical_ports) in ports {
                if !self.port_obj_dict.contains(&physical_port) {
                    continue;
                }

                // First logical port corresponds to the first subport of the group.
                let logical_port_name = &logical_ports[0];

                if self.is_port_dom_monitoring_disabled(env, logical_port_name) {
                    continue;
                }

                let asic_index = match self.port_mapping.get_asic_id_for_logical_port(logical_port_name) {
                    Some(a) => a,
                    None => {
                        env.log_warning(&format!("Got invalid asic index for {logical_port_name}, ignored"));
                        continue;
                    }
                };

                if !env.detect_port_in_error_status(logical_port_name, asic_index) {
                    if !env.is_present(physical_port) {
                        continue;
                    }
                }

                env.post_port_dom_temperature_info_to_db(logical_port_name);
            }

            next_periodic_db_update_time = now + interval;
        }

        env.log_notice("Stop DOM thermal monitoring loop");
        Ok(())
    }

    // ---- thread lifecycle (mirrors DomInfoUpdateTask) ---------------------
    pub fn start_worker<F>(&mut self, worker: F)
    where
        F: FnOnce() -> Result<(), String> + Send + 'static,
    {
        let start_gated = self.stop.is_set();
        let main_stop = self.main_thread_stop.clone();
        let exc = self.exc.clone();
        self.handle = Some(std::thread::spawn(move || {
            if start_gated {
                return Ok(());
            }
            match worker() {
                Ok(()) => Ok(()),
                Err(e) => {
                    *exc.lock().unwrap() = Some(e.clone());
                    main_stop.set();
                    Err(e)
                }
            }
        }));
    }

    pub fn join(&mut self) -> Result<(), String> {
        self.stop.set();
        match self.handle.take() {
            Some(h) => h
                .join()
                .unwrap_or_else(|_| Err("DomThermalInfoUpdateTask thread panicked".to_string())),
            None => Ok(()),
        }
    }

    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().map(|h| !h.is_finished()).unwrap_or(false)
    }

    pub fn take_exception(&self) -> Option<String> {
        self.exc.lock().unwrap().clone()
    }
}

/// `{**basic, **statistic}` for two JSON objects (statistic overrides basic);
/// non-objects contribute nothing.
fn merge_objects(basic: &Value, statistic: &Value) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(o) = basic.as_object() {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
    if let Some(o) = statistic.as_object() {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use crate::mock::{MockChassis, MockSfp, MockTable};

    /// A counting + scriptable [`DomEnv`], the Rust analogue of the bundle of
    /// `@patch(...)`es the Python `DomInfoUpdateTask` tests install. Ends the loop
    /// through the shared stop event on a configurable trigger (post/config count
    /// or a port-update event), mirroring how `join()`/`is_set` would.
    struct MockDomEnv {
        stop: Arc<Event>,
        clock: Cell<f64>,
        now_advance: f64,
        dom_sensor_clock_advance: f64,
        dom_sensor_times: RefCell<Vec<f64>>,

        handle_port_update_calls: Cell<u32>,
        last_update_timeout: Cell<i64>,
        config_change_calls: Cell<u32>,

        firmware_calls: Cell<u32>,
        dom_sensor_calls: Cell<u32>,
        dom_temperature_calls: Cell<u32>,
        dom_temperature_args: RefCell<Vec<String>>,
        dom_flags_calls: Cell<u32>,
        dom_flags_args: RefCell<Vec<String>>,
        status_calls: Cell<u32>,
        status_flags_calls: Cell<u32>,
        status_flags_args: RefCell<Vec<String>>,
        vdm_flags_args: RefCell<Vec<String>>,
        pm_calls: Cell<u32>,
        vdm_real_calls: Cell<u32>,
        vdm_flags_calls: Cell<u32>,
        del_calls: Cell<u32>,
        freeze_calls: Cell<u32>,
        unfreeze_calls: Cell<u32>,

        detect_error: Cell<bool>,
        present: Cell<bool>,
        dom_polling: RefCell<VecDeque<String>>,
        dom_polling_default: RefCell<String>,
        dom_polling_by_port: RefCell<BTreeMap<String, String>>,
        cmis_init: Cell<bool>,
        vdm_supported: Cell<bool>,
        vdm_statistic_supported: Cell<bool>,
        lpmode_on: Cell<bool>,
        freeze_result: Cell<bool>,
        unfreeze_result: Cell<bool>,

        subscribe_result: RefCell<Result<(), String>>,
        log_notices: RefCell<Vec<String>>,

        stop_after_dom_sensor: Cell<Option<u32>>,
        stop_after_dom_temperature: Cell<Option<u32>>,
        stop_after_config: Cell<Option<u32>>,
        stop_on_handle_update: Cell<bool>,
    }

    impl MockDomEnv {
        fn new(stop: Arc<Event>) -> Self {
            MockDomEnv {
                stop,
                clock: Cell::new(0.0),
                now_advance: 1.0,
                dom_sensor_clock_advance: 0.0,
                dom_sensor_times: RefCell::new(Vec::new()),
                handle_port_update_calls: Cell::new(0),
                last_update_timeout: Cell::new(0),
                config_change_calls: Cell::new(0),
                firmware_calls: Cell::new(0),
                dom_sensor_calls: Cell::new(0),
                dom_temperature_calls: Cell::new(0),
                dom_temperature_args: RefCell::new(Vec::new()),
                dom_flags_calls: Cell::new(0),
                dom_flags_args: RefCell::new(Vec::new()),
                status_calls: Cell::new(0),
                status_flags_calls: Cell::new(0),
                status_flags_args: RefCell::new(Vec::new()),
                vdm_flags_args: RefCell::new(Vec::new()),
                pm_calls: Cell::new(0),
                vdm_real_calls: Cell::new(0),
                vdm_flags_calls: Cell::new(0),
                del_calls: Cell::new(0),
                freeze_calls: Cell::new(0),
                unfreeze_calls: Cell::new(0),
                detect_error: Cell::new(false),
                present: Cell::new(true),
                dom_polling: RefCell::new(VecDeque::new()),
                dom_polling_default: RefCell::new("enabled".to_string()),
                dom_polling_by_port: RefCell::new(BTreeMap::new()),
                cmis_init: Cell::new(false),
                vdm_supported: Cell::new(false),
                vdm_statistic_supported: Cell::new(false),
                lpmode_on: Cell::new(false),
                freeze_result: Cell::new(true),
                unfreeze_result: Cell::new(true),
                subscribe_result: RefCell::new(Ok(())),
                log_notices: RefCell::new(Vec::new()),
                stop_after_dom_sensor: Cell::new(None),
                stop_after_dom_temperature: Cell::new(None),
                stop_after_config: Cell::new(None),
                stop_on_handle_update: Cell::new(false),
            }
        }
        fn set_dom_polling_script(&self, values: &[&str]) {
            *self.dom_polling.borrow_mut() = values.iter().map(|s| s.to_string()).collect();
        }
        fn set_dom_polling_for(&self, lport: &str, value: &str) {
            self.dom_polling_by_port
                .borrow_mut()
                .insert(lport.to_string(), value.to_string());
        }
        fn notices(&self) -> Vec<String> {
            self.log_notices.borrow().clone()
        }
    }

    impl DomEnv for MockDomEnv {
        fn subscribe_port_config_change(&self) -> Result<(), String> {
            self.subscribe_result.borrow().clone()
        }
        fn handle_port_config_change(&self) {
            self.config_change_calls.set(self.config_change_calls.get() + 1);
            if let Some(n) = self.stop_after_config.get() {
                if self.config_change_calls.get() >= n {
                    self.stop.set();
                }
            }
        }
        fn handle_port_update_event(&self, timeout_msecs: i64) {
            self.handle_port_update_calls.set(self.handle_port_update_calls.get() + 1);
            self.last_update_timeout.set(timeout_msecs);
            if self.stop_on_handle_update.get() {
                self.stop.set();
            }
        }
        fn now_secs(&self) -> f64 {
            let v = self.clock.get() + self.now_advance;
            self.clock.set(v);
            v
        }
        fn detect_port_in_error_status(&self, _lport: &str, _asic: i32) -> bool {
            self.detect_error.get()
        }
        fn get_dom_polling(&self, _lport: &str) -> String {
            if let Some(v) = self.dom_polling_by_port.borrow().get(_lport) {
                return v.clone();
            }
            self.dom_polling
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| self.dom_polling_default.borrow().clone())
        }
        fn is_port_in_cmis_init(&self, _lport: &str) -> bool {
            self.cmis_init.get()
        }
        fn is_present(&self, _physical_port: i32) -> bool {
            self.present.get()
        }
        fn post_port_sfp_firmware_info_to_db(&self, _lport: &str) -> i32 {
            self.firmware_calls.set(self.firmware_calls.get() + 1);
            0
        }
        fn post_port_dom_sensor_info_to_db(&self, _lport: &str) {
            self.dom_sensor_calls.set(self.dom_sensor_calls.get() + 1);
            self.dom_sensor_times.borrow_mut().push(self.clock.get());
            if self.dom_sensor_clock_advance != 0.0 {
                self.clock.set(self.clock.get() + self.dom_sensor_clock_advance);
            }
            if let Some(n) = self.stop_after_dom_sensor.get() {
                if self.dom_sensor_calls.get() >= n {
                    self.stop.set();
                }
            }
        }
        fn post_port_dom_flags_to_db(&self, _lport: &str) {
            self.dom_flags_calls.set(self.dom_flags_calls.get() + 1);
            self.dom_flags_args.borrow_mut().push(_lport.to_string());
        }
        fn post_port_dom_temperature_info_to_db(&self, _lport: &str) {
            self.dom_temperature_calls.set(self.dom_temperature_calls.get() + 1);
            self.dom_temperature_args.borrow_mut().push(_lport.to_string());
            if let Some(n) = self.stop_after_dom_temperature.get() {
                if self.dom_temperature_calls.get() >= n {
                    self.stop.set();
                }
            }
        }
        fn post_port_transceiver_hw_status_to_db(&self, _lport: &str) {
            self.status_calls.set(self.status_calls.get() + 1);
        }
        fn post_port_transceiver_hw_status_flags_to_db(&self, _lport: &str) {
            self.status_flags_calls.set(self.status_flags_calls.get() + 1);
            self.status_flags_args.borrow_mut().push(_lport.to_string());
        }
        fn is_transceiver_vdm_supported(&self, _physical_port: i32) -> bool {
            self.vdm_supported.get()
        }
        fn is_vdm_statistic_supported(&self, _physical_port: i32) -> bool {
            self.vdm_statistic_supported.get()
        }
        fn is_transceiver_lpmode_on(&self, _physical_port: i32) -> bool {
            self.lpmode_on.get()
        }
        fn vdm_freeze(&self, _physical_port: i32) -> bool {
            self.freeze_calls.set(self.freeze_calls.get() + 1);
            self.freeze_result.get()
        }
        fn vdm_unfreeze(&self, _physical_port: i32) -> bool {
            self.unfreeze_calls.set(self.unfreeze_calls.get() + 1);
            self.unfreeze_result.get()
        }
        fn post_port_pm_info_to_db(&self, _lport: &str) {
            self.pm_calls.set(self.pm_calls.get() + 1);
        }
        fn post_port_vdm_real_values_from_dict_to_db(&self, _lport: &str, _values: Value) {
            self.vdm_real_calls.set(self.vdm_real_calls.get() + 1);
        }
        fn post_port_vdm_flags_to_db(&self, _lport: &str) {
            self.vdm_flags_calls.set(self.vdm_flags_calls.get() + 1);
            self.vdm_flags_args.borrow_mut().push(_lport.to_string());
        }
        fn del_port_sfp_dom_info(&self, _lport: &str, _asic: i32) {
            self.del_calls.set(self.del_calls.get() + 1);
        }
        fn log_notice(&self, msg: &str) {
            self.log_notices.borrow_mut().push(msg.to_string());
        }
    }

    fn task(interval: Option<i64>) -> DomInfoUpdateTask {
        DomInfoUpdateTask::new(PortMapping::new(), Event::new(), true, interval)
    }

    fn single_port_task(interval: Option<i64>) -> DomInfoUpdateTask {
        let mut t = task(interval);
        t.port_mapping.physical_to_logical.insert(1, vec!["Ethernet0".to_string()]);
        t.port_mapping.logical_to_asic.insert("Ethernet0".to_string(), 0);
        t
    }

    /// Build a task whose `PortMapping` resolves each `(physical, logical)` pair
    /// (asic 0), so a link-change refresh scheduled for that physical port can
    /// resolve its first logical subport and post the flag tables.
    fn task_with_phys(interval: Option<i64>, ports: &[(i32, &str)]) -> DomInfoUpdateTask {
        let mut t = task(interval);
        for (phys, name) in ports {
            t.port_mapping.physical_to_logical.insert(*phys, vec![name.to_string()]);
            t.port_mapping.logical_to_asic.insert(name.to_string(), 0);
        }
        t
    }

    fn stopping_from(stop: Arc<Event>) -> impl FnMut() -> bool {
        move || stop.is_set()
    }

    /// Port of `test_DomInfoUpdateTask_dom_update_interval_parameter`.
    #[test]
    fn test_dom_update_interval_parameter() {
        assert_eq!(task(None).dom_update_interval, DomInfoUpdateTask::DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS);
        assert_eq!(task(None).dom_update_interval, 60);
        assert_eq!(task(Some(0)).dom_update_interval, 0);
        assert_eq!(task(Some(120)).dom_update_interval, 120);
        assert_eq!(task(Some(1000)).dom_update_interval, 1000);
        // Negative → default, DEFAULT constant unchanged.
        assert_eq!(task(Some(-5)).dom_update_interval, 60);
        assert_eq!(DomInfoUpdateTask::DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS, 60);
    }

    /// Port of `test_DomInfoUpdateTask_handle_port_change_event`.
    #[test]
    fn test_handle_port_change_event() {
        let mut t = task(None);
        let stop = t.stop_event();
        let env = MockDomEnv::new(stop);

        let add = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd);
        t.on_port_config_change(&env, &add);
        assert!(t.port_mapping.logical_port_list.contains(&"Ethernet0".to_string()));
        assert_eq!(t.port_mapping.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(t.port_mapping.get_physical_to_logical(1), Some(vec!["Ethernet0".to_string()]));
        assert_eq!(t.port_mapping.get_logical_to_physical("Ethernet0"), Some(vec![1]));
        assert_eq!(env.del_calls.get(), 0);

        let remove = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortRemove);
        t.on_port_config_change(&env, &remove);
        assert!(t.port_mapping.logical_port_list.is_empty());
        assert!(t.port_mapping.logical_to_physical.is_empty());
        assert!(t.port_mapping.physical_to_logical.is_empty());
        assert!(t.port_mapping.logical_to_asic.is_empty());
        assert_eq!(env.del_calls.get(), 1);
    }

    /// Port of `test_DomInfoUpdateTask_check_port_update` (all five scenarios). The
    /// link-change refresh now runs `update_port_db_diagnostics_on_link_change`,
    /// which posts the flag tables via the finer seams, so we assert on which
    /// logical ports had their DOM/STATUS flags re-read.
    #[test]
    fn test_check_port_update() {
        // Env with a fixed clock at 1000s (now_advance 0 so now() is stable).
        let make_env = || {
            let mut e = MockDomEnv::new(Event::new());
            e.now_advance = 0.0;
            e.clock.set(1000.0);
            e
        };

        // 1: no affected ports -> handle called once(1000), no flag re-reads.
        {
            let mut t = task(None);
            let env = make_env();
            let mut never = || false;
            t.check_port_update(&env, &mut never, 1000);
            assert_eq!(env.handle_port_update_calls.get(), 1);
            assert_eq!(env.last_update_timeout.get(), 1000);
            assert_eq!(env.dom_flags_calls.get(), 0);
            assert_eq!(env.status_flags_calls.get(), 0);
        }
        // 2: past deadline -> flag re-read fired for that port, port removed.
        {
            let mut t = task_with_phys(None, &[(0, "Ethernet0")]);
            let env = make_env();
            t.link_change_affected_ports.insert(0, 995.0);
            let mut never = || false;
            t.check_port_update(&env, &mut never, 100);
            assert_eq!(env.last_update_timeout.get(), 100);
            assert_eq!(env.dom_flags_args.borrow().as_slice(), &["Ethernet0".to_string()]);
            assert_eq!(env.status_flags_args.borrow().as_slice(), &["Ethernet0".to_string()]);
            assert!(!t.link_change_affected_ports.contains_key(&0));
        }
        // 3: future deadline -> no re-read, port retained.
        {
            let mut t = task_with_phys(None, &[(4, "Ethernet4")]);
            let env = make_env();
            t.link_change_affected_ports.insert(4, 1005.0);
            let mut never = || false;
            t.check_port_update(&env, &mut never, 1000);
            assert_eq!(env.dom_flags_calls.get(), 0);
            assert!(t.link_change_affected_ports.contains_key(&4));
        }
        // 4: mixed -> two past fire (both ports re-read), future retained.
        {
            let mut t = task_with_phys(None, &[(0, "Ethernet0"), (8, "Ethernet8"), (12, "Ethernet12")]);
            let env = make_env();
            t.link_change_affected_ports.insert(0, 998.0);
            t.link_change_affected_ports.insert(8, 999.0);
            t.link_change_affected_ports.insert(12, 1005.0);
            let mut never = || false;
            t.check_port_update(&env, &mut never, 1000);
            assert_eq!(env.dom_flags_calls.get(), 2);
            let args = env.dom_flags_args.borrow();
            assert!(args.contains(&"Ethernet0".to_string()) && args.contains(&"Ethernet8".to_string()));
            assert!(t.link_change_affected_ports.contains_key(&12));
            assert!(!t.link_change_affected_ports.contains_key(&0));
            assert!(!t.link_change_affected_ports.contains_key(&8));
        }
        // 5: stop set -> break before processing, port retained.
        {
            let mut t = task_with_phys(None, &[(16, "Ethernet16")]);
            let env = make_env();
            t.link_change_affected_ports.insert(16, 999.0);
            let mut always = || true;
            t.check_port_update(&env, &mut always, 1000);
            assert_eq!(env.handle_port_update_calls.get(), 1);
            assert_eq!(env.dom_flags_calls.get(), 0);
            assert!(t.link_change_affected_ports.contains_key(&16));
        }
    }

    /// `on_port_update_event` schedules a re-read only
    /// for an APPL_DB `PORT_SET` (a flap), one deadline per physical port; other
    /// event kinds / DBs are ignored.
    #[test]
    fn test_on_port_update_event() {
        let mut t = task(None);
        let mut env = MockDomEnv::new(Event::new());
        env.now_advance = 0.0;
        env.clock.set(500.0);

        // APPL_DB PORT_SET for physical port 3 -> scheduled at now + 1s.
        let mut set_appl = PortChangeEvent::new("Ethernet12", 3, 0, PortEventType::PortSet);
        set_appl.db_name = Some("APPL_DB".to_string());
        t.on_port_update_event(&env, &set_appl);
        assert_eq!(
            t.link_change_affected_ports.get(&3).copied(),
            Some(500.0 + DomInfoUpdateTask::DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE as f64)
        );

        // A CONFIG_DB SET (wrong DB) is ignored.
        let mut set_config = PortChangeEvent::new("Ethernet16", 4, 0, PortEventType::PortSet);
        set_config.db_name = Some("CONFIG_DB".to_string());
        t.on_port_update_event(&env, &set_config);
        assert!(!t.link_change_affected_ports.contains_key(&4));

        // A DEL event (even on APPL_DB) is ignored.
        let mut del_appl = PortChangeEvent::new("Ethernet20", 5, 0, PortEventType::PortDel);
        del_appl.db_name = Some("APPL_DB".to_string());
        t.on_port_update_event(&env, &del_appl);
        assert!(!t.link_change_affected_ports.contains_key(&5));
    }

    /// `update_port_db_diagnostics_on_link_change`
    /// re-reads the DOM + STATUS flag tables (and VDM flags only when advertised),
    /// and honours each gate (stopping, unknown port, dom-disabled, error status,
    /// absence).
    #[test]
    fn test_update_port_db_diagnostics_on_link_change() {
        // Happy path (no VDM): DOM + STATUS flags re-read, VDM skipped.
        {
            let t = task_with_phys(None, &[(2, "Ethernet4")]);
            let env = MockDomEnv::new(t.stop_event());
            t.update_port_db_diagnostics_on_link_change(&env, 2);
            assert_eq!(env.dom_flags_args.borrow().as_slice(), &["Ethernet4".to_string()]);
            assert_eq!(env.status_flags_args.borrow().as_slice(), &["Ethernet4".to_string()]);
            assert_eq!(env.vdm_flags_calls.get(), 0);
        }
        // VDM advertised -> VDM flags also re-read.
        {
            let t = task_with_phys(None, &[(2, "Ethernet4")]);
            let mut env = MockDomEnv::new(t.stop_event());
            env.vdm_supported.set(true);
            t.update_port_db_diagnostics_on_link_change(&env, 2);
            assert_eq!(env.vdm_flags_args.borrow().as_slice(), &["Ethernet4".to_string()]);
        }
        // Stopping -> nothing posted.
        {
            let t = task_with_phys(None, &[(2, "Ethernet4")]);
            t.stop.set();
            let env = MockDomEnv::new(t.stop_event());
            t.update_port_db_diagnostics_on_link_change(&env, 2);
            assert_eq!(env.dom_flags_calls.get(), 0);
        }
        // Unknown physical port -> warning, nothing posted.
        {
            let t = task(None);
            let env = MockDomEnv::new(t.stop_event());
            t.update_port_db_diagnostics_on_link_change(&env, 99);
            assert_eq!(env.dom_flags_calls.get(), 0);
        }
        // DOM monitoring disabled -> nothing posted.
        {
            let t = task_with_phys(None, &[(2, "Ethernet4")]);
            let env = MockDomEnv::new(t.stop_event());
            env.set_dom_polling_script(&["disabled"]);
            t.update_port_db_diagnostics_on_link_change(&env, 2);
            assert_eq!(env.dom_flags_calls.get(), 0);
        }
        // Port in error status -> nothing posted.
        {
            let t = task_with_phys(None, &[(2, "Ethernet4")]);
            let env = MockDomEnv::new(t.stop_event());
            env.detect_error.set(true);
            t.update_port_db_diagnostics_on_link_change(&env, 2);
            assert_eq!(env.dom_flags_calls.get(), 0);
        }
        // Not present -> nothing posted.
        {
            let t = task_with_phys(None, &[(2, "Ethernet4")]);
            let env = MockDomEnv::new(t.stop_event());
            env.present.set(false);
            t.update_port_db_diagnostics_on_link_change(&env, 2);
            assert_eq!(env.dom_flags_calls.get(), 0);
        }
    }

    /// Port of `test_DomInfoUpdateTask_task_worker`: error-status gating (round 1)
    /// then a successful poll pass with VDM (round 2).
    #[test]
    fn test_task_worker() {
        // Round 1: detect_port_in_error_status True -> nothing posted.
        {
            let mut t = single_port_task(Some(0));
            let stop = t.stop_event();
            let mut env = MockDomEnv::new(stop.clone());
            env.detect_error.set(true);
            // End the loop after two outer iterations (each runs the port pass once).
            env.stop_after_config.set(Some(2));
            let mut stopping = stopping_from(stop);
            t.task_worker(&env, &mut stopping).unwrap();

            assert_eq!(env.firmware_calls.get(), 0);
            assert_eq!(env.dom_sensor_calls.get(), 0);
            assert_eq!(env.dom_flags_calls.get(), 0);
            assert_eq!(env.status_calls.get(), 0);
            assert_eq!(env.status_flags_calls.get(), 0);
            assert_eq!(env.vdm_real_calls.get(), 0);
            assert_eq!(env.vdm_flags_calls.get(), 0);
            assert_eq!(env.pm_calls.get(), 0);
        }

        // Round 2: no error; polling 'disabled' then 'enabled'; VDM supported.
        {
            let mut t = single_port_task(Some(0));
            let stop = t.stop_event();
            let mut env = MockDomEnv::new(stop.clone());
            env.detect_error.set(false);
            env.present.set(true);
            env.set_dom_polling_script(&["disabled", "enabled"]);
            env.vdm_supported.set(true);
            env.vdm_statistic_supported.set(true);
            env.lpmode_on.set(false);
            // Stop once the enabled pass has posted the DOM sensor row.
            env.stop_after_dom_sensor.set(Some(1));
            let mut stopping = stopping_from(stop);
            t.task_worker(&env, &mut stopping).unwrap();

            assert_eq!(env.firmware_calls.get(), 1);
            assert_eq!(env.dom_sensor_calls.get(), 1);
            assert_eq!(env.dom_flags_calls.get(), 1);
            assert_eq!(env.status_calls.get(), 1);
            assert_eq!(env.status_flags_calls.get(), 1);
            assert_eq!(env.vdm_real_calls.get(), 1);
            assert_eq!(env.vdm_flags_calls.get(), 1);
            assert_eq!(env.pm_calls.get(), 1);
        }
    }

    /// New Rust unit test: when the VDM freeze itself fails, the poll still
    /// unfreezes (the `finally` of vdm_freeze_context), skips the PM capture, but
    /// STILL posts the basic real values and the flags (dom_mgr.py:386-420). When
    /// the freeze succeeds but the *unfreeze* fails, PM is captured and every VDM
    /// write still runs (the unfreeze failure is logged, not fatal).
    #[test]
    fn test_task_worker_vdm_failure() {
        // Freeze fails -> no PM, unfreeze still attempted, real + flags still posted.
        {
            let mut t = single_port_task(Some(0));
            let stop = t.stop_event();
            let env = MockDomEnv::new(stop.clone());
            env.vdm_supported.set(true);
            env.vdm_statistic_supported.set(true);
            env.lpmode_on.set(false);
            env.freeze_result.set(false); // freeze never confirms
            env.stop_after_dom_sensor.set(Some(1));
            let mut stopping = stopping_from(stop);
            t.task_worker(&env, &mut stopping).unwrap();

            assert_eq!(env.freeze_calls.get(), 1);
            assert_eq!(env.unfreeze_calls.get(), 1); // finally always unfreezes
            assert_eq!(env.pm_calls.get(), 0); // no PM when the freeze fails
            assert_eq!(env.vdm_real_calls.get(), 1); // basic real values still posted
            assert_eq!(env.vdm_flags_calls.get(), 1);
        }
        // Freeze ok but unfreeze fails -> PM captured, every VDM write still runs.
        {
            let mut t = single_port_task(Some(0));
            let stop = t.stop_event();
            let env = MockDomEnv::new(stop.clone());
            env.vdm_supported.set(true);
            env.vdm_statistic_supported.set(true);
            env.lpmode_on.set(false);
            env.freeze_result.set(true);
            env.unfreeze_result.set(false); // unfreeze confirm fails (logged, not fatal)
            env.stop_after_dom_sensor.set(Some(1));
            let mut stopping = stopping_from(stop);
            t.task_worker(&env, &mut stopping).unwrap();

            assert_eq!(env.freeze_calls.get(), 1);
            assert_eq!(env.unfreeze_calls.get(), 1);
            assert_eq!(env.pm_calls.get(), 1);
            assert_eq!(env.vdm_real_calls.get(), 1);
            assert_eq!(env.vdm_flags_calls.get(), 1);
        }
    }

    /// New Rust unit test: the VDM freeze/PM path is entered only when statistic
    /// observables are supported (dom_mgr.py:388-390). A VDM-capable module with no
    /// statistic observables skips the freeze and PM entirely, yet still posts the
    /// basic real values and the flags.
    #[test]
    fn test_task_worker_vdm_freeze_conditions() {
        let mut t = single_port_task(Some(0));
        let stop = t.stop_event();
        let env = MockDomEnv::new(stop.clone());
        env.vdm_supported.set(true);
        env.vdm_statistic_supported.set(false); // no statistic observables -> no freeze
        env.lpmode_on.set(false);
        env.stop_after_dom_sensor.set(Some(1));
        let mut stopping = stopping_from(stop);
        t.task_worker(&env, &mut stopping).unwrap();

        assert_eq!(env.freeze_calls.get(), 0);
        assert_eq!(env.unfreeze_calls.get(), 0);
        assert_eq!(env.pm_calls.get(), 0);
        assert_eq!(env.vdm_real_calls.get(), 1); // basic real values still posted
        assert_eq!(env.vdm_flags_calls.get(), 1);
    }

    /// New Rust unit test (`vdm_freeze_skipped_in_lpmode`): a module in low power is
    /// never frozen (dom_mgr.py:389, `not is_transceiver_lpmode_on(...)`), so its PM
    /// and statistic observables stop refreshing — but the basic real values and the
    /// flags are still published.
    #[test]
    fn vdm_freeze_skipped_in_lpmode() {
        let mut t = single_port_task(Some(0));
        let stop = t.stop_event();
        let env = MockDomEnv::new(stop.clone());
        env.vdm_supported.set(true);
        env.vdm_statistic_supported.set(true);
        env.lpmode_on.set(true); // low power -> skip freeze
        env.stop_after_dom_sensor.set(Some(1));
        let mut stopping = stopping_from(stop);
        t.task_worker(&env, &mut stopping).unwrap();

        assert_eq!(env.freeze_calls.get(), 0);
        assert_eq!(env.pm_calls.get(), 0);
        assert_eq!(env.vdm_real_calls.get(), 1);
        assert_eq!(env.vdm_flags_calls.get(), 1);
    }

    // ---- DomThermalInfoUpdateTask ----

    /// Build a thermal task over three physical ports (1→Ethernet0, 2→Ethernet4,
    /// 3→Ethernet8); only Ethernet0/Ethernet4 have an ASIC binding, so Ethernet8
    /// exercises the invalid-asic skip. All three are in `port_obj_dict`.
    fn thermal_task(poll_interval: i64) -> DomThermalInfoUpdateTask {
        let mut pm = PortMapping::new();
        pm.physical_to_logical.insert(1, vec!["Ethernet0".to_string()]);
        pm.physical_to_logical.insert(2, vec!["Ethernet4".to_string()]);
        pm.physical_to_logical.insert(3, vec!["Ethernet8".to_string()]);
        pm.logical_to_asic.insert("Ethernet0".to_string(), 0);
        pm.logical_to_asic.insert("Ethernet4".to_string(), 0);
        // Ethernet8 intentionally has no asic binding.
        let port_obj_dict: BTreeSet<i32> = [1, 2, 3].into_iter().collect();
        DomThermalInfoUpdateTask::new(pm, port_obj_dict, Event::new(), true, poll_interval)
    }

    /// New Rust unit test (`test_DomThermalInfoUpdateTask_task_worker`): the thermal poll
    /// publishes temperature per module with full per-module isolation — the disabled port
    /// (Ethernet4) and the invalid-asic port (Ethernet8) are skipped while the present,
    /// monitored port (Ethernet0) still posts. (The Python smoke test's bare-MagicMock
    /// `port_obj_dict` skips every port; here `port_obj_dict` includes all three so the
    /// isolation is actually exercised.)
    #[test]
    fn test_DomThermalInfoUpdateTask_task_worker() {
        let mut t = thermal_task(0);
        let stop = t.stop_event();
        let env = MockDomEnv::new(stop.clone());
        env.detect_error.set(false);
        env.present.set(true);
        env.set_dom_polling_for("Ethernet4", "disabled"); // per-module: Ethernet4 paused
        // Stop after the first (and only) temperature post so the loop terminates.
        env.stop_after_dom_temperature.set(Some(1));
        let mut stopping = stopping_from(stop);
        t.task_worker(&env, &mut stopping).unwrap();

        // Only Ethernet0 posted temperature; Ethernet4 (disabled) and Ethernet8
        // (invalid asic) were isolated out.
        assert_eq!(env.dom_temperature_calls.get(), 1);
        assert_eq!(env.dom_temperature_args.borrow().as_slice(), &["Ethernet0".to_string()]);
    }

    /// New Rust unit test: a physical port absent from `port_obj_dict` is skipped even
    /// though it is present and monitored — the Python `if physical_port not in
    /// self.port_obj_dict: continue` gate.
    #[test]
    fn dom_thermal_skips_port_not_in_obj_dict() {
        let mut pm = PortMapping::new();
        pm.physical_to_logical.insert(1, vec!["Ethernet0".to_string()]);
        pm.logical_to_asic.insert("Ethernet0".to_string(), 0);
        // Empty port_obj_dict -> physical port 1 is not present in the device dict.
        let mut t = DomThermalInfoUpdateTask::new(pm, BTreeSet::new(), Event::new(), true, 0);
        let stop = t.stop_event();
        let env = MockDomEnv::new(stop.clone());
        env.present.set(true);
        // The thermal loop never calls handle_port_config_change; drive termination via a
        // one-shot stopping closure that allows exactly one processing pass.
        let mut passes = 0;
        let mut stopping = || {
            passes += 1;
            passes > 1
        };
        t.task_worker(&env, &mut stopping).unwrap();
        assert_eq!(env.dom_temperature_calls.get(), 0);
    }

    /// Port of `test_DomInfoUpdateTask_task_worker_stop_event_during_port_update_wait`.
    #[test]
    fn test_task_worker_stop_event_during_port_update_wait() {
        let mut t = single_port_task(None);
        t.dom_update_interval = 1000; // large -> stay in the inner wait loop
        let stop = t.stop_event();
        let mut env = MockDomEnv::new(stop.clone());
        // The stop event fires inside check_port_update's handle_port_update_event.
        env.stop_on_handle_update.set(true);
        let mut stopping = stopping_from(stop);
        t.task_worker(&env, &mut stopping).unwrap();

        assert!(env.handle_port_update_calls.get() >= 1);
        assert!(env
            .notices()
            .iter()
            .any(|m| m == "Stop event generated during DOM monitoring loop while checking port update"));
    }

    /// Port of `test_DomInfoUpdateTask_scheduling_uses_loop_start_time`: with a 60s
    /// interval and 30s of simulated per-pass processing, successive DOM posts are
    /// ~interval apart (loop-start scheduling), not interval+processing.
    #[test]
    fn test_scheduling_uses_loop_start_time() {
        let mut t = single_port_task(None);
        t.dom_update_interval = 60;
        let stop = t.stop_event();
        let mut env = MockDomEnv::new(stop.clone());
        env.dom_sensor_clock_advance = 30.0; // simulate long processing per pass
        env.stop_after_dom_sensor.set(Some(2));
        let mut stopping = stopping_from(stop);
        t.task_worker(&env, &mut stopping).unwrap();

        let times = env.dom_sensor_times.borrow().clone();
        assert!(times.len() >= 2, "expected >= 2 DOM posts, got {}", times.len());
        let delta = times[1] - times[0];
        assert!(delta < 70.0, "expected ~60s loop-start cadence, got {delta}");
        assert!(env.dom_sensor_calls.get() >= 2);
        assert!(env.dom_flags_calls.get() >= 2);
        assert!(env.status_calls.get() >= 2);
        assert!(env.status_flags_calls.get() >= 2);
    }

    /// the FIRST periodic DOM
    /// poll is delayed by a full `dom_update_interval` (dom_mgr.py:296-298 — "Adding
    /// dom_info_update_periodic_secs to allow xcvrd to initialize ports before
    /// starting the periodic update"). The latched flag tables (TRANSCEIVER_DOM_FLAG
    /// / _STATUS_FLAG) must therefore NOT be published at startup: they first appear
    /// on the delayed poll or a link-change re-read, never before. An immediate
    /// startup poll would pre-publish `<flag>=False` for every present port and
    /// defeat the link-change fast-recapture isolation the GUARD window checks
    /// (test_link_change_flags): a baseline flap's `wait_until(<flag>=="False")` would
    /// return instantly on the stale row instead of blocking on the flap's own
    /// re-read. We drive the mock clock (advanced 1.0s per `now_secs`) and confirm the
    /// first DOM/flag publish only lands after the interval has been drained by the
    /// pre-poll wait loop — which itself stays responsive to link-change events.
    #[test]
    fn test_first_poll_delayed_by_full_interval() {
        let interval: i64 = 5;
        let mut t = single_port_task(Some(interval));
        let stop = t.stop_event();
        let mut env = MockDomEnv::new(stop.clone());
        // Stop as soon as the first poll pass posts its DOM sensor row.
        env.stop_after_dom_sensor.set(Some(1));
        let mut stopping = stopping_from(stop);
        t.task_worker(&env, &mut stopping).unwrap();

        // Exactly one poll pass ran, and its first publish happened only AFTER the
        // full interval elapsed on the mock clock — not immediately at startup.
        let times = env.dom_sensor_times.borrow().clone();
        assert_eq!(times.len(), 1, "expected exactly one first-poll DOM post");
        assert!(
            times[0] >= interval as f64,
            "first DOM poll must be delayed a full interval ({interval}s); got first \
             post at t={} (an immediate startup poll would land at ~1s)",
            times[0]
        );
        // The pre-first-poll wait loop stayed responsive to link changes: it drained
        // APPL_DB port-update events (handle_port_update_event) before the first
        // flag publish, so a flap in that window is serviced off the ~60s poll.
        assert!(
            env.handle_port_update_calls.get() >= 1,
            "the pre-poll wait loop should service link-change events during the \
             startup interval, before the first periodic flag publish"
        );
        // The latched flag tables were first published WITH the delayed poll, not
        // before it (one publish each, coincident with the single sensor post).
        assert_eq!(env.dom_flags_calls.get(), 1);
        assert_eq!(env.status_flags_calls.get(), 1);
    }

    /// Port of `test_DomInfoUpdateTask_task_run_stop`: a set stop event makes the
    /// worker return immediately, so the thread is not alive after join.
    #[test]
    fn test_task_run_stop() {
        let mut t = task(None);
        t.stop.set(); // task_stopping_event set before start
        t.start_worker(|| Ok(()));
        t.join().unwrap();
        assert!(!t.is_alive());
    }

    /// NEW Rust unit test for `test_DomInfoUpdateTask_task_run_with_exception`: the
    /// setup error (the Rust analogue of `subscribe_port_config_change` raising
    /// `NotImplementedError`) propagates through `join()`, signals the main-thread
    /// stop event, and leaves the thread not alive.
    #[test]
    fn test_task_run_with_exception() {
        let mut t = task(None);
        let main_stop = t.main_thread_stop_event();
        t.start_worker(|| {
            Err("NotImplementedError side_effect in \
                 sonic-xcvrd/xcvrd/dom/dom_mgr.py subscribe_port_config_change"
                .to_string())
        });
        let result = t.join();
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("NotImplementedError"));
        assert!(msg.contains("subscribe_port_config_change"));
        assert!(msg.contains("dom/dom_mgr.py"));
        assert!(!t.is_alive());
        assert!(main_stop.is_set());
        assert_eq!(t.take_exception().as_deref(), Some(msg.as_str()));
    }

    #[test]
    fn merge_objects_overrides_basic_with_statistic() {
        let basic = json!({"a": 1, "b": 2});
        let stat = json!({"b": 3, "c": 4});
        assert_eq!(merge_objects(&basic, &stat), json!({"a": 1, "b": 3, "c": 4}));
        assert_eq!(merge_objects(&Value::Null, &stat), json!({"b": 3, "c": 4}));
    }

    // -----------------------------------------------------------------------------
    // firmware/PM posters (translated from test_xcvrd.py + a projection test).
    // -----------------------------------------------------------------------------

    fn phys0_task(physical_to_logical: Option<Vec<&str>>) -> DomInfoUpdateTask {
        let mut pm = PortMapping::new();
        pm.logical_to_physical.insert("Ethernet0".to_string(), 0);
        if let Some(list) = physical_to_logical {
            pm.physical_to_logical
                .insert(0, list.iter().map(|s| s.to_string()).collect());
        }
        DomInfoUpdateTask::new(pm, Event::new(), true, None)
    }

    /// Port of `test_post_port_sfp_firmware_info_to_db`: a `stop_event` or an absent module
    /// writes nothing; a present module posts the active/inactive firmware to **every** logical
    /// port backing the physical port (Ethernet0 + Ethernet4).
    #[test]
    fn test_post_port_sfp_firmware_info_to_db() {
        let fw = json!({"active_firmware": "2.1.1", "inactive_firmware": "1.2.4"});
        let task = phys0_task(Some(vec!["Ethernet0", "Ethernet4"]));
        let table = MockTable::new();

        // Test 1: stop_event set → loop breaks before any write.
        let stop = Event::new();
        stop.set();
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_info_firmware_versions", fw.clone());
        let chassis = MockChassis::with_sfps(vec![sfp]);
        task.post_port_sfp_firmware_info_to_db("Ethernet0", &chassis, &table, &stop, None);
        assert_eq!(table.get_size().unwrap(), 0);

        // Test 2: module not present → skipped.
        let stop = Event::new();
        let mut sfp = MockSfp::absent();
        sfp.set_json_call("get_transceiver_info_firmware_versions", fw.clone());
        let chassis = MockChassis::with_sfps(vec![sfp]);
        task.post_port_sfp_firmware_info_to_db("Ethernet0", &chassis, &table, &stop, None);
        assert_eq!(table.get_size().unwrap(), 0);

        // Test 3: present → both logical ports get the 2-field firmware row.
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_info_firmware_versions", fw.clone());
        let chassis = MockChassis::with_sfps(vec![sfp]);
        task.post_port_sfp_firmware_info_to_db("Ethernet0", &chassis, &table, &stop, None);
        assert_eq!(table.get_size_for_key("Ethernet0"), 2);
        assert_eq!(table.get_size_for_key("Ethernet4"), 2);
        assert_eq!(table.get_size().unwrap(), 2);
    }

    /// Port of `test_post_port_sfp_firmware_info_to_db_lport_list_None`: when the physical port
    /// resolves to no logical ports, the poster logs a warning and writes nothing.
    #[test]
    fn test_post_port_sfp_firmware_info_to_db_lport_list_None() {
        let fw = json!({"active_firmware": "2.1.1", "inactive_firmware": "1.2.4"});
        // physical_to_logical intentionally unset → get_physical_to_logical(0) == None.
        let task = phys0_task(None);
        let table = MockTable::new();
        let stop = Event::new();
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_info_firmware_versions", fw);
        let chassis = MockChassis::with_sfps(vec![sfp]);
        task.post_port_sfp_firmware_info_to_db("Ethernet0", &chassis, &table, &stop, None);
        assert_eq!(table.set_count(), 0);
    }

    /// Port of `test_post_port_pm_info_to_db`: a present, non-flat coherent module's 6 PM values
    /// are posted under the physical port name.
    #[test]
    fn test_post_port_pm_info_to_db() {
        let pm_dict = json!({
            "prefec_ber_avg": "0.0003407240007014899",
            "prefec_ber_min": "0.0006814479342250317",
            "prefec_ber_max": "0.0006833674050752236",
            "uncorr_frames_avg": "0.0",
            "uncorr_frames_min": "0.0",
            "uncorr_frames_max": "0.0"
        });
        let task = phys0_task(None);
        let table = MockTable::new();
        assert_eq!(table.get_size().unwrap(), 0);
        let stop = Event::new();
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_pm", pm_dict);
        let chassis = MockChassis::with_sfps(vec![sfp]);
        let rc = task.post_port_pm_info_to_db("Ethernet0", &chassis, &table, &stop, None);
        assert_eq!(rc, 0);
        assert_eq!(table.get_size_for_key("Ethernet0"), 6);
    }

    /// NEW: pm_info_to_db_projection — the three PM read outcomes the Python poster distinguishes:
    /// a `null` read is EEPROM-not-ready (writes nothing), an empty object is skipped (API N/A),
    /// and numeric values are beautified (stringified) into the row under the physical port name.
    #[test]
    fn pm_info_to_db_projection() {
        let task = phys0_task(None);
        let stop = Event::new();

        // (a) null PM read → SFP_EEPROM_NOT_READY, nothing written.
        let table = MockTable::new();
        let sfp = MockSfp::present(); // no get_transceiver_pm json_call → call_json returns null
        let chassis = MockChassis::with_sfps(vec![sfp]);
        let rc = task.post_port_pm_info_to_db("Ethernet0", &chassis, &table, &stop, None);
        assert_eq!(rc, SFP_EEPROM_NOT_READY);
        assert_eq!(table.set_count(), 0);

        // (b) empty PM object → skipped (API not applicable), rc normal, nothing written.
        let table = MockTable::new();
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_pm", json!({}));
        let chassis = MockChassis::with_sfps(vec![sfp]);
        let rc = task.post_port_pm_info_to_db("Ethernet0", &chassis, &table, &stop, None);
        assert_eq!(rc, 0);
        assert_eq!(table.set_count(), 0);

        // (c) numeric PM values are stringified under the physical port name.
        let table = MockTable::new();
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_pm", json!({"prefec_ber_avg": 5, "cd_avg": 12}));
        let chassis = MockChassis::with_sfps(vec![sfp]);
        let rc = task.post_port_pm_info_to_db("Ethernet0", &chassis, &table, &stop, None);
        assert_eq!(rc, 0);
        assert_eq!(table.get_size_for_key("Ethernet0"), 2);
        assert_eq!(table.field("Ethernet0", "cd_avg").as_deref(), Some("12"));
        assert_eq!(table.field("Ethernet0", "prefec_ber_avg").as_deref(), Some("5"));
    }

    // -----------------------------------------------------------------------------
    // DOM gating / polling knob.
    // -----------------------------------------------------------------------------

    /// Port of `test_DomInfoUpdateTask_get_dom_polling_from_config_db`: the knob is
    /// read from the group's FIRST subport, so every subport of a breakout group
    /// inherits the lead port's setting. Ethernet4/12/8/0 all sit on physical port 1
    /// (natsorted lead = Ethernet0, `dom_polling=disabled`) → all `disabled`;
    /// Ethernet16 owns physical port 2 (`enabled`) → `enabled`; an unmapped port
    /// (Ethernet20) falls back to the `enabled` default.
    #[test]
    #[allow(non_snake_case)]
    fn test_DomInfoUpdateTask_get_dom_polling_from_config_db() {
        let mut t = task(None);
        for (name, phys) in [
            ("Ethernet4", 1),
            ("Ethernet12", 1),
            ("Ethernet8", 1),
            ("Ethernet0", 1),
            ("Ethernet16", 2),
        ] {
            t.port_mapping
                .handle_port_change_event(&PortChangeEvent::new(name, phys, 0, PortEventType::PortAdd));
        }

        // CONFIG_DB PORT table: the group-1 lead Ethernet0 is disabled; the sibling
        // rows are enabled (never consulted — the lead dominates the group);
        // Ethernet16 is enabled; Ethernet20 has no row at all.
        let cfg = MockTable::new();
        cfg.hset("Ethernet0", "dom_polling", "disabled").unwrap();
        cfg.hset("Ethernet4", "dom_polling", "enabled").unwrap();
        cfg.hset("Ethernet8", "dom_polling", "enabled").unwrap();
        cfg.hset("Ethernet12", "dom_polling", "enabled").unwrap();
        cfg.hset("Ethernet16", "dom_polling", "enabled").unwrap();

        for lport in ["Ethernet0", "Ethernet4", "Ethernet8", "Ethernet12"] {
            assert_eq!(
                t.get_dom_polling_from_config_db(lport, &cfg),
                "disabled",
                "{lport} inherits the group lead's disabled setting"
            );
        }
        assert_eq!(t.get_dom_polling_from_config_db("Ethernet16", &cfg), "enabled");
        assert_eq!(t.get_dom_polling_from_config_db("Ethernet20", &cfg), "enabled");
    }

    /// Port of `test_DomInfoUpdateTask_is_port_in_cmis_initialization_process`: the
    /// CMIS-init gate is armed only when a CMIS manager exists (`skip_cmis_mgr` ==
    /// false) and the port's `cmis_state` is non-terminal; the transitional/absent
    /// `UNKNOWN` counts as in-init, a terminal state does not, and an unmapped port
    /// (no asic index) is treated as not-in-init.
    #[test]
    #[allow(non_snake_case)]
    fn test_DomInfoUpdateTask_is_port_in_cmis_initialization_process() {
        use crate::xcvrd_utilities::common::{
            CMIS_STATE_INSERTED, CMIS_STATE_READY, CMIS_STATE_UNKNOWN,
        };

        let make = |skip_cmis_mgr: bool| {
            let mut t = DomInfoUpdateTask::new(PortMapping::new(), Event::new(), skip_cmis_mgr, None);
            t.port_mapping.handle_port_change_event(&PortChangeEvent::new(
                "Ethernet0",
                1,
                0,
                PortEventType::PortAdd,
            ));
            t
        };
        let status_tbl = |state: Option<&str>| {
            let tbl = MockTable::new();
            if let Some(s) = state {
                tbl.hset("Ethernet0", "cmis_state", s).unwrap();
            }
            tbl
        };

        // skip_cmis_mgr → never in init (state ignored).
        assert!(!make(true)
            .is_port_in_cmis_initialization_process("Ethernet0", &status_tbl(Some(CMIS_STATE_INSERTED))));
        // Non-terminal INSERTED → in init.
        assert!(make(false)
            .is_port_in_cmis_initialization_process("Ethernet0", &status_tbl(Some(CMIS_STATE_INSERTED))));
        // The transitional UNKNOWN (explicit and absent-field default) → in init.
        assert!(make(false)
            .is_port_in_cmis_initialization_process("Ethernet0", &status_tbl(Some(CMIS_STATE_UNKNOWN))));
        assert!(make(false).is_port_in_cmis_initialization_process("Ethernet0", &status_tbl(None)));
        // Terminal READY → not in init.
        assert!(!make(false)
            .is_port_in_cmis_initialization_process("Ethernet0", &status_tbl(Some(CMIS_STATE_READY))));
        // Unmapped logical port (no asic index) → not in init (logged).
        assert!(!make(false)
            .is_port_in_cmis_initialization_process("INVALID_PORT", &status_tbl(None)));
    }

    /// `dom_polling=disabled` halts
    /// EVERY DB write for the port. The poll pass still runs (its fast-timeout
    /// port-update check fires), but the disabled knob short-circuits
    /// `is_port_dom_monitoring_disabled`, so no DOM/status/VDM/PM/firmware row is
    /// published — the observable behind test_dom_polling (a cleared DOM_SENSOR is
    /// not repopulated while disabled).
    #[test]
    fn dom_polling_disabled_skips_write() {
        let mut t = single_port_task(Some(0)); // skip_cmis_mgr = true
        let stop = t.stop_event();
        let env = MockDomEnv::new(stop.clone());
        *env.dom_polling_default.borrow_mut() = "disabled".to_string();
        // Let the gated port pass execute once, then end the loop on the 2nd outer iter.
        env.stop_after_config.set(Some(2));
        let mut stopping = stopping_from(stop);
        t.task_worker(&env, &mut stopping).unwrap();

        // The port pass ran (its fast-timeout check_port_update fired) ...
        assert_eq!(
            env.last_update_timeout.get(),
            PORT_UPDATE_EVENT_SELECT_TIMEOUT_FAST_MSECS
        );
        // ... yet dom_polling=disabled skipped every DB write for the port.
        assert_eq!(env.firmware_calls.get(), 0);
        assert_eq!(env.dom_sensor_calls.get(), 0);
        assert_eq!(env.dom_flags_calls.get(), 0);
        assert_eq!(env.status_calls.get(), 0);
        assert_eq!(env.status_flags_calls.get(), 0);
        assert_eq!(env.vdm_real_calls.get(), 0);
        assert_eq!(env.vdm_flags_calls.get(), 0);
        assert_eq!(env.pm_calls.get(), 0);
    }

    /// while a port is in CMIS
    /// datapath initialization the DOM pass is gated (test_dom_gating). With a CMIS
    /// manager present (`skip_cmis_mgr` == false) an asserted in-init signal skips the
    /// whole port pass — no DOM sensor/flags/status/VDM writes. The contrast case
    /// (`skip_cmis_mgr` == true) short-circuits the gate, proving it is specifically
    /// the CMIS-init condition — not presence or error — that pauses the poll.
    #[test]
    fn port_in_cmis_init_gates_dom() {
        // Gated: CMIS manager present + port mid CMIS init → whole pass skipped.
        {
            let mut t = DomInfoUpdateTask::new(PortMapping::new(), Event::new(), false, Some(0));
            t.port_mapping
                .physical_to_logical
                .insert(1, vec!["Ethernet0".to_string()]);
            t.port_mapping
                .logical_to_asic
                .insert("Ethernet0".to_string(), 0);
            let stop = t.stop_event();
            let env = MockDomEnv::new(stop.clone());
            env.cmis_init.set(true);
            env.stop_after_config.set(Some(2));
            let mut stopping = stopping_from(stop);
            t.task_worker(&env, &mut stopping).unwrap();

            assert_eq!(
                env.last_update_timeout.get(),
                PORT_UPDATE_EVENT_SELECT_TIMEOUT_FAST_MSECS
            );
            assert_eq!(env.dom_sensor_calls.get(), 0);
            assert_eq!(env.dom_flags_calls.get(), 0);
            assert_eq!(env.status_calls.get(), 0);
            assert_eq!(env.status_flags_calls.get(), 0);
            assert_eq!(env.vdm_real_calls.get(), 0);
        }
        // Not gated: no CMIS manager short-circuits the gate → DOM still published
        // despite the same in-init signal.
        {
            let mut t = single_port_task(Some(0)); // skip_cmis_mgr = true
            let stop = t.stop_event();
            let env = MockDomEnv::new(stop.clone());
            env.cmis_init.set(true);
            env.stop_after_dom_sensor.set(Some(1));
            let mut stopping = stopping_from(stop);
            t.task_worker(&env, &mut stopping).unwrap();

            assert_eq!(env.dom_sensor_calls.get(), 1);
            assert_eq!(env.dom_flags_calls.get(), 1);
            assert_eq!(env.status_calls.get(), 1);
        }
    }
}
