#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `xcvrd.py`: DaemonXcvrd, SfpStateUpdateTask, post_port_sfp_info_to_db, _wrapper_* helpers, state-machine constants.
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::db::{StateDb, Table};
use crate::hal::{Chassis, Sfp};
use crate::xcvrd_utilities::common;
use crate::xcvrd_utilities::port_event_helper::{
    PortChangeEvent, PortEventSource, PortEventType, PortMapping, SelectState, SELECT_TIMEOUT_MSECS,
};
use crate::xcvrd_utilities::sfp_status_helper::{self, SFP_STATUS_INSERTED, SFP_STATUS_REMOVED};

pub const SYSLOG_IDENTIFIER: &str = "xcvrd";
pub const EVENT_ON_ALL_SFP: &str = "-1";
pub const SYSTEM_NOT_READY: &str = "system_not_ready";
pub const SYSTEM_BECOME_READY: &str = "system_become_ready";
pub const SYSTEM_FAIL: &str = "system_fail";
pub const NORMAL_EVENT: &str = "normal";
pub const STATE_INIT: i32 = 0;
pub const STATE_NORMAL: i32 = 1;
pub const STATE_EXIT: i32 = 2;
pub const PHYSICAL_PORT_NOT_EXIST: i32 = -1;
pub const SFP_EEPROM_NOT_READY: i32 = -2;
pub const RETRY_TIMES_FOR_SYSTEM_READY: u32 = 24;
pub const RETRY_PERIOD_FOR_SYSTEM_READY_MSECS: u64 = 5000;
pub const RETRY_TIMES_FOR_SYSTEM_FAIL: u32 = 24;
pub const RETRY_PERIOD_FOR_SYSTEM_FAIL_MSECS: u64 = 5000;
pub const SFP_INSERT_EVENT_POLL_PERIOD_MSECS: u64 = 1000;
pub const STATE_MACHINE_UPDATE_PERIOD_MSECS: u64 = 60000;
pub const MGMT_INIT_TIME_DELAY_SECS: u64 = 2;
pub const TIME_FOR_SFP_READY_SECS: u64 = 1;
pub const RETRY_EEPROM_READING_INTERVAL_SECS: u64 = 60;
pub const NPU_SI_SETTINGS_SYNC_STATUS_KEY: &str = "NPU_SI_SETTINGS_SYNC_STATUS";
pub const NPU_SI_SETTINGS_DEFAULT_VALUE: &str = "NPU_SI_SETTINGS_DEFAULT";

/// The external collaborators of [`SfpStateUpdateTask`] — the "module boundary"
/// the Python `test_xcvrd.py` patches (`post_port_sfp_info_to_db`,
/// `update_port_transceiver_status_table_sw`, `del_port_sfp_dom_info_from_db`,
/// the DOM/VDM threshold posters, `notify_media_setting`, `os.kill`,
/// `_wrapper_get_transceiver_change_event`, `_mapping_event_from_change_event`,
/// `_wrapper_soak_sfp_insert_event`, `time.sleep/time.time`, …). Bundling every
/// side effect behind one trait lets the state machine stay pure/`Send` state and
/// lets unit tests inject a counting/scriptable mock, mirroring the Python patches.
pub trait SfpEnv {
    /// `_wrapper_get_transceiver_change_event(timeout)` → `(status, sfp, sfp_error)`.
    fn get_change_event(&self, timeout_ms: u64) -> (bool, BTreeMap<String, String>, BTreeMap<String, String>);
    /// `_wrapper_soak_sfp_insert_event(sfp_insert_events, port_dict)`.
    fn soak_sfp_insert_event(&self, sfp_insert_events: &mut BTreeMap<String, Instant>, port_dict: &mut BTreeMap<String, String>);
    /// `_mapping_event_from_change_event(status, port_dict)`.
    fn map_event(&self, status: bool, port_dict: &mut BTreeMap<String, String>) -> String;
    /// `post_port_sfp_info_to_db(...)` for one logical port → rc.
    fn post_port_sfp_info_to_db(&self, lport: &str, port_mapping: &PortMapping, asic: i32, transceiver_dict: &mut BTreeMap<i32, Option<Value>>) -> i32;
    /// `common.update_port_transceiver_status_table_sw(...)`.
    fn update_status_sw(&self, lport: &str, asic: i32, status: &str, error_descriptions: &str);
    /// `common.del_port_sfp_dom_info_from_db(...)` (the per-port table purge).
    fn del_port_sfp_dom_info(&self, lport: &str, port_mapping: &PortMapping, asic: i32);
    /// `DOMDBUtils.post_port_dom_thresholds_to_db(lport)`.
    fn post_dom_thresholds(&self, lport: &str);
    /// `VDMDBUtils.post_port_vdm_thresholds_to_db(lport)`.
    fn post_vdm_thresholds(&self, lport: &str);
    /// `media_settings_parser.notify_media_setting(lport, ...)`.
    fn notify_media_setting(&self, lport: &str);
    /// Seed `PORT_TABLE|lport` NPU_SI_SETTINGS_SYNC_STATUS to its default.
    fn set_state_port_npu_si(&self, lport: &str, asic: i32);
    /// `sfp.remove_xcvr_api()` for a removed module.
    fn remove_xcvr_api(&self, physical_port: i32);
    /// `_wrapper_get_sfp_error_description(physical_port)`.
    fn get_sfp_error_description(&self, physical_port: i32) -> Option<String>;
    /// `common._wrapper_get_presence(physical_port)`.
    fn is_present(&self, physical_port: i32) -> bool;
    /// `os.kill(os.getppid(), SIGTERM)` — request daemon shutdown.
    fn kill_parent(&self);
    /// `time.sleep(dur)` (a no-op seam in tests).
    fn sleep(&self, dur: Duration);
    /// `time.time()`.
    fn now(&self) -> Instant;
}

/// Rust port of the Python `SfpStateUpdateTask` (xcvrd.py:259). Holds the pure,
/// `Send` task state (port map + retry/error/insert bookkeeping); all side effects
/// go through the [`SfpEnv`] seam. The thread lifecycle (`start`/`join`/
/// `raise_exception`/`is_alive`) is modelled with `std::thread` + a cooperative
/// stop flag (the analogue of the Python `ctypes` async-exception used to unwind a
/// blocking poll).
pub struct SfpStateUpdateTask {
    pub name: String,
    pub port_mapping: PortMapping,
    /// Logical ports whose EEPROM identity read failed (retry set).
    pub retry_eeprom_set: BTreeSet<String>,
    /// Throttle for the retry cadence (`last_retry_eeprom_time`).
    pub last_retry_eeprom_time: Option<Instant>,
    /// Cached SFP error events by physical port index → `(value, vendor_error_map)`.
    pub sfp_error_dict: BTreeMap<i32, (String, BTreeMap<String, String>)>,
    /// Pending SFP-insert soak timestamps by physical port key.
    pub sfp_insert_events: BTreeMap<String, Instant>,
    /// Per-namespace warm/fast-reboot verdict (Python `warm_fast_reboot_status`), computed
    /// once at start-up from `is_syncd_warm_restore_complete(ns) or is_fast_reboot_enabled(ns)`.
    pub warm_fast_reboot_status: HashMap<String, bool>,
    /// STATE_DB namespaces this task serves (`self.namespaces`); `[""]` on single-ASIC.
    pub namespaces: Vec<String>,
    /// Whether this is a multi-ASIC platform (drives namespace → `asic{N}` resolution).
    pub multi_asic: bool,
    // ---- thread lifecycle ----
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<(), String>>>,
}

impl SfpStateUpdateTask {
    /// `__init__` (xcvrd.py:261). Deep-copies the port map like the Python task.
    pub fn new(port_mapping: PortMapping) -> Self {
        SfpStateUpdateTask {
            name: "SfpStateUpdateTask".to_string(),
            port_mapping,
            retry_eeprom_set: BTreeSet::new(),
            last_retry_eeprom_time: None,
            sfp_error_dict: BTreeMap::new(),
            sfp_insert_events: BTreeMap::new(),
            warm_fast_reboot_status: HashMap::new(),
            namespaces: vec![String::new()],
            multi_asic: false,
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Configure the STATE_DB namespaces + multi-ASIC flag this task serves
    /// (Python `self.namespaces = namespaces`). Default `[""]`/single-ASIC matches the KVM
    /// target; tests set a specific namespace to exercise per-namespace resolution.
    pub fn set_namespaces(&mut self, namespaces: Vec<String>, multi_asic: bool) {
        self.namespaces = namespaces;
        self.multi_asic = multi_asic;
    }

    /// `initialize_warm_fast_reboot_status` (xcvrd.py:287): per namespace, cache whether a
    /// warm-restore is complete OR fast reboot is enabled — read **once** at start-up from
    /// the injected STATE_DB seam so a running daemon never re-reads it mid-flight.
    pub fn initialize_warm_fast_reboot_status(&mut self, db: &dyn common::StateDbHget) {
        let verdict = common::is_syncd_warm_restore_complete(db) || common::is_fast_reboot_enabled(db);
        self.warm_fast_reboot_status = self.namespaces.iter().map(|ns| (ns.clone(), verdict)).collect();
    }

    /// `_mapping_event_from_change_event` (xcvrd.py:299): fold a change-event poll
    /// into a state-machine event (mutating `port_dict` for the synthetic cases).
    pub fn mapping_event_from_change_event(&self, status: bool, port_dict: &mut BTreeMap<String, String>) -> String {
        mapping_event_from_change_event(status, port_dict)
    }

    /// `is_warm_fast_reboot_for_lport` (xcvrd.py:294): map the port to its ASIC's namespace
    /// and return that namespace's cached warm/fast-reboot verdict. An unknown logical port
    /// (no `asic_id`) is `False`.
    pub fn is_warm_fast_reboot_for_lport(&self, logical_port: &str) -> bool {
        match self.port_mapping.get_asic_id_for_logical_port(logical_port) {
            None => false,
            Some(asic) => {
                let namespace = common::get_namespace_from_asic_id(asic, self.multi_asic);
                *self.warm_fast_reboot_status.get(&namespace).unwrap_or(&false)
            }
        }
    }

    /// `on_port_config_change` (xcvrd.py:734): dispatch add/remove, ordering the
    /// port-map mutation and the DB effect exactly as the Python does.
    pub fn on_port_config_change(&mut self, env: &dyn SfpEnv, ev: &PortChangeEvent) {
        match ev.event_type {
            PortEventType::PortRemove => {
                self.on_remove_logical_port(env, ev);
                self.port_mapping.handle_port_change_event(ev);
            }
            PortEventType::PortAdd => {
                self.port_mapping.handle_port_change_event(ev);
                self.on_add_logical_port(env, ev);
            }
            _ => {}
        }
    }

    /// `on_remove_logical_port` (xcvrd.py:742): purge the per-port tables and drop
    /// the port from the retry set.
    pub fn on_remove_logical_port(&mut self, env: &dyn SfpEnv, ev: &PortChangeEvent) {
        env.del_port_sfp_dom_info(&ev.port_name, &self.port_mapping, ev.asic_id);
        self.retry_eeprom_set.remove(&ev.port_name);
    }

    /// `on_add_logical_port` (xcvrd.py:781): (re)publish identity + SW status for a
    /// newly-created logical port, honouring any cached SFP error.
    pub fn on_add_logical_port(&mut self, env: &dyn SfpEnv, ev: &PortChangeEvent) {
        env.set_state_port_npu_si(&ev.port_name, ev.asic_id);

        let mut error_description = "N/A".to_string();
        let mut status: Option<String> = None;
        let mut read_eeprom = true;

        if let Some((value, error_dict)) = self.sfp_error_dict.get(&ev.port_index).cloned() {
            status = Some(value.clone());
            let error_bits: u32 = value.parse().unwrap_or(0);
            let mut error_descriptions = sfp_status_helper::fetch_generic_error_description(error_bits);
            if sfp_status_helper::has_vendor_specific_error(error_bits) {
                let vendor = if !error_dict.is_empty() {
                    error_dict.get(&ev.port_index.to_string()).cloned()
                } else {
                    env.get_sfp_error_description(ev.port_index)
                };
                if let Some(v) = vendor {
                    error_descriptions.push(v);
                }
            }
            error_description = error_descriptions.join("|");
            if sfp_status_helper::is_error_block_eeprom_reading(error_bits) {
                read_eeprom = false;
            }
        }

        if env.is_present(ev.port_index) && read_eeprom {
            let mut transceiver_dict: BTreeMap<i32, Option<Value>> = BTreeMap::new();
            if status.is_none() {
                status = Some(SFP_STATUS_INSERTED.to_string());
            }
            let rc = env.post_port_sfp_info_to_db(&ev.port_name, &self.port_mapping, ev.asic_id, &mut transceiver_dict);
            if rc == SFP_EEPROM_NOT_READY {
                self.retry_eeprom_set.insert(ev.port_name.clone());
            } else {
                env.post_dom_thresholds(&ev.port_name);
                env.post_vdm_thresholds(&ev.port_name);
                if !self.is_warm_fast_reboot_for_lport(&ev.port_name) {
                    env.notify_media_setting(&ev.port_name);
                }
            }
        } else if status.is_none() {
            status = Some(SFP_STATUS_REMOVED.to_string());
        }

        env.update_status_sw(&ev.port_name, ev.asic_id, status.as_deref().unwrap_or(SFP_STATUS_REMOVED), &error_description);
    }

    /// `retry_eeprom_reading` (xcvrd.py:849): retry failed identity reads on a
    /// ~60s cadence, publishing + dropping ports that now read successfully.
    pub fn retry_eeprom_reading(&mut self, env: &dyn SfpEnv) {
        if self.retry_eeprom_set.is_empty() {
            return;
        }
        let now = env.now();
        if let Some(last) = self.last_retry_eeprom_time {
            if now.duration_since(last) < Duration::from_secs(RETRY_EEPROM_READING_INTERVAL_SECS) {
                return;
            }
        }
        self.last_retry_eeprom_time = Some(now);

        let mut transceiver_dict: BTreeMap<i32, Option<Value>> = BTreeMap::new();
        let mut retry_success = Vec::new();
        for logical_port in self.retry_eeprom_set.iter().cloned().collect::<Vec<_>>() {
            let asic = self.port_mapping.get_asic_id_for_logical_port(&logical_port).unwrap_or(0);
            let rc = env.post_port_sfp_info_to_db(&logical_port, &self.port_mapping, asic, &mut transceiver_dict);
            if rc != SFP_EEPROM_NOT_READY {
                env.post_dom_thresholds(&logical_port);
                env.post_vdm_thresholds(&logical_port);
                if !self.is_warm_fast_reboot_for_lport(&logical_port) {
                    env.notify_media_setting(&logical_port);
                }
                transceiver_dict.clear();
                retry_success.push(logical_port);
            }
        }
        for logical_port in retry_success {
            self.retry_eeprom_set.remove(&logical_port);
        }
    }

    /// `_init_port_sfp_status_sw_tbl` (xcvrd.py:366): seed TRANSCEIVER_STATUS_SW
    /// `status` for every logical port from live presence.
    pub fn init_port_sfp_status_sw_tbl(&mut self, env: &dyn SfpEnv) {
        for logical_port_name in self.port_mapping.logical_port_list.clone() {
            let asic = match self.port_mapping.get_asic_id_for_logical_port(&logical_port_name) {
                Some(a) => a,
                None => continue,
            };
            let physical_port_list = match self.port_mapping.logical_port_name_to_physical_port_list(&logical_port_name) {
                Some(list) => list,
                None => {
                    env.update_status_sw(&logical_port_name, asic, SFP_STATUS_REMOVED, "N/A");
                    continue;
                }
            };
            for physical_port in physical_port_list {
                let status = if env.is_present(physical_port) { SFP_STATUS_INSERTED } else { SFP_STATUS_REMOVED };
                env.update_status_sw(&logical_port_name, asic, status, "N/A");
            }
        }
    }

    /// `_post_port_sfp_info_and_dom_thr_to_db_once` (xcvrd.py:324): publish all
    /// current identity + DOM-threshold info during boot-up, returning the retry set.
    pub fn post_port_sfp_info_and_dom_thr_to_db_once(&mut self, env: &dyn SfpEnv) -> BTreeSet<String> {
        let mut transceiver_dict: BTreeMap<i32, Option<Value>> = BTreeMap::new();
        let mut retry_eeprom_set = BTreeSet::new();
        let logical_port_list = self.port_mapping.logical_port_list.clone();
        for logical_port_name in &logical_port_list {
            let asic = match self.port_mapping.get_asic_id_for_logical_port(logical_port_name) {
                Some(a) => a,
                None => continue,
            };
            let rc = env.post_port_sfp_info_to_db(logical_port_name, &self.port_mapping, asic, &mut transceiver_dict);
            if rc != SFP_EEPROM_NOT_READY {
                if !self.is_warm_fast_reboot_for_lport(logical_port_name) {
                    env.notify_media_setting(logical_port_name);
                }
            } else {
                retry_eeprom_set.insert(logical_port_name.clone());
            }
        }
        for logical_port_name in &logical_port_list {
            if !retry_eeprom_set.contains(logical_port_name) {
                env.post_dom_thresholds(logical_port_name);
                env.post_vdm_thresholds(logical_port_name);
            }
        }
        retry_eeprom_set
    }

    /// `task_worker` (xcvrd.py:405): the SFP-monitoring state machine. Runs until
    /// `stopping()` or an EXIT transition (which asks the parent to terminate).
    pub fn task_worker(&mut self, env: &dyn SfpEnv, stopping: &mut dyn FnMut() -> bool, sfp_error_event: &Event) {
        let mut retry: u32 = 0;
        let mut timeout = RETRY_PERIOD_FOR_SYSTEM_READY_MSECS;
        let mut state = STATE_INIT;
        let mut transceiver_dict: BTreeMap<i32, Option<Value>> = BTreeMap::new();

        while !stopping() {
            // handle_port_config_change is a no-op seam here.
            self.retry_eeprom_reading(env);
            let mut next_state = state;

            if !self.sfp_insert_events.is_empty() {
                timeout = SFP_INSERT_EVENT_POLL_PERIOD_MSECS;
            }
            let (status, mut port_dict, error_dict) = env.get_change_event(timeout);
            if status {
                env.soak_sfp_insert_event(&mut self.sfp_insert_events, &mut port_dict);
            }
            if port_dict.is_empty() {
                continue;
            }
            let event = env.map_event(status, &mut port_dict);

            if event == SYSTEM_NOT_READY {
                if state == STATE_INIT {
                    if retry >= RETRY_TIMES_FOR_SYSTEM_READY {
                        next_state = STATE_EXIT;
                        sfp_error_event.set();
                    } else {
                        retry += 1;
                        env.sleep(Duration::from_millis(RETRY_PERIOD_FOR_SYSTEM_READY_MSECS));
                    }
                } else {
                    next_state = STATE_EXIT;
                }
            } else if event == SYSTEM_BECOME_READY {
                if state == STATE_INIT {
                    next_state = STATE_NORMAL;
                } else if state != STATE_NORMAL {
                    next_state = STATE_EXIT;
                }
            } else if event == NORMAL_EVENT {
                if state == STATE_NORMAL || state == STATE_INIT {
                    if state == STATE_INIT {
                        next_state = STATE_NORMAL;
                    }
                    self.handle_normal_event(env, &port_dict, &error_dict, &mut transceiver_dict);
                } else {
                    next_state = STATE_EXIT;
                }
            } else if event == SYSTEM_FAIL {
                if state == STATE_INIT {
                    if retry >= RETRY_TIMES_FOR_SYSTEM_FAIL {
                        next_state = STATE_EXIT;
                        sfp_error_event.set();
                    } else {
                        retry += 1;
                        env.sleep(Duration::from_millis(RETRY_PERIOD_FOR_SYSTEM_FAIL_MSECS));
                    }
                } else if state == STATE_NORMAL {
                    next_state = STATE_INIT;
                    timeout = RETRY_PERIOD_FOR_SYSTEM_FAIL_MSECS;
                    retry = 0;
                } else {
                    next_state = STATE_EXIT;
                }
            }

            if next_state != state {
                state = next_state;
            }
            if next_state == STATE_EXIT {
                env.kill_parent();
                break;
            } else if next_state == STATE_NORMAL {
                timeout = STATE_MACHINE_UPDATE_PERIOD_MSECS;
            }
        }
    }

    /// The NORMAL_EVENT body of `task_worker`: per-port insert/remove/error handling.
    fn handle_normal_event(
        &mut self,
        env: &dyn SfpEnv,
        port_dict: &BTreeMap<String, String>,
        error_dict: &BTreeMap<String, String>,
        transceiver_dict: &mut BTreeMap<i32, Option<Value>>,
    ) {
        for (key, value) in port_dict.iter() {
            let key_int = match key.parse::<i32>() {
                Ok(k) => k,
                Err(_) => continue,
            };
            if value.as_str() != SFP_STATUS_INSERTED && value.as_str() != SFP_STATUS_REMOVED {
                self.sfp_error_dict.insert(key_int, (value.clone(), error_dict.clone()));
            } else {
                self.sfp_error_dict.remove(&key_int);
            }

            let logical_port_list = match self.port_mapping.get_physical_to_logical(key_int) {
                Some(list) => list,
                None => continue,
            };
            for logical_port in logical_port_list {
                let asic = match self.port_mapping.get_asic_id_for_logical_port(&logical_port) {
                    Some(a) => a,
                    None => continue,
                };
                if value.as_str() == SFP_STATUS_INSERTED {
                    env.update_status_sw(&logical_port, asic, SFP_STATUS_INSERTED, "N/A");
                    let mut rc = env.post_port_sfp_info_to_db(&logical_port, &self.port_mapping, asic, transceiver_dict);
                    if rc == SFP_EEPROM_NOT_READY {
                        env.sleep(Duration::from_secs(TIME_FOR_SFP_READY_SECS));
                        rc = env.post_port_sfp_info_to_db(&logical_port, &self.port_mapping, asic, transceiver_dict);
                        if rc == SFP_EEPROM_NOT_READY {
                            self.retry_eeprom_set.insert(logical_port.clone());
                        }
                    }
                    if rc != SFP_EEPROM_NOT_READY {
                        env.post_dom_thresholds(&logical_port);
                        env.post_vdm_thresholds(&logical_port);
                        if !self.is_warm_fast_reboot_for_lport(&logical_port) {
                            env.notify_media_setting(&logical_port);
                        }
                        transceiver_dict.clear();
                    }
                } else if value.as_str() == SFP_STATUS_REMOVED {
                    env.remove_xcvr_api(key_int);
                    env.set_state_port_npu_si(&logical_port, asic);
                    env.update_status_sw(&logical_port, asic, SFP_STATUS_REMOVED, "N/A");
                    env.del_port_sfp_dom_info(&logical_port, &self.port_mapping, asic);
                } else {
                    let error_bits: u32 = match value.parse() {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    let mut error_descriptions = sfp_status_helper::fetch_generic_error_description(error_bits);
                    if sfp_status_helper::has_vendor_specific_error(error_bits) {
                        let vendor = if !error_dict.is_empty() {
                            error_dict.get(key).cloned()
                        } else {
                            env.get_sfp_error_description(key_int)
                        };
                        if let Some(v) = vendor {
                            error_descriptions.push(v);
                        }
                    }
                    env.update_status_sw(&logical_port, asic, value, &error_descriptions.join("|"));
                    if sfp_status_helper::is_error_block_eeprom_reading(error_bits) {
                        env.del_port_sfp_dom_info(&logical_port, &self.port_mapping, asic);
                    }
                }
            }
        }
    }

    /// `run`/thread start: spawn `body` on a worker thread, tracking a cooperative
    /// stop flag. `body` is injectable so tests can supply a poll-forever or
    /// immediately-failing worker (the Python tests patch `init` for the same effect).
    pub fn start<F>(&mut self, body: F)
    where
        F: FnOnce(Arc<AtomicBool>) -> Result<(), String> + Send + 'static,
    {
        self.stop_flag.store(false, Ordering::SeqCst);
        let stop = self.stop_flag.clone();
        self.handle = Some(std::thread::spawn(move || body(stop)));
    }

    /// `is_alive`: true while the worker thread has not finished.
    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().map(|h| !h.is_finished()).unwrap_or(false)
    }

    /// `raise_exception`: cooperatively signal the worker to unwind (the Rust
    /// analogue of the Python `ctypes` async `SystemExit`).
    pub fn raise_exception(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    /// `join`: wait for the worker and propagate its `Result` (a worker error is
    /// the analogue of the Python task re-raising its stored exception).
    pub fn join(&mut self) -> Result<(), String> {
        match self.handle.take() {
            Some(h) => h.join().unwrap_or_else(|_| Err("worker panicked".to_string())),
            None => Ok(()),
        }
    }
}

/// `_mapping_event_from_change_event` (xcvrd.py:299) as a pure function so both the
/// task and the mock env can share the real folding logic.
pub fn mapping_event_from_change_event(status: bool, port_dict: &mut BTreeMap<String, String>) -> String {
    let event;
    if status {
        if !port_dict.is_empty() {
            event = NORMAL_EVENT.to_string();
        } else {
            event = SYSTEM_BECOME_READY.to_string();
            port_dict.insert(EVENT_ON_ALL_SFP.to_string(), SYSTEM_BECOME_READY.to_string());
        }
    } else if port_dict.contains_key(EVENT_ON_ALL_SFP) {
        event = port_dict.get(EVENT_ON_ALL_SFP).cloned().unwrap();
    } else {
        event = SYSTEM_FAIL.to_string();
        port_dict.insert(EVENT_ON_ALL_SFP.to_string(), SYSTEM_FAIL.to_string());
    }
    event
}

// ---------------------------------------------------------------------------
// Daemon lifecycle scaffolding.
//
// `DaemonXcvrd::run()` mirrors the Python orchestration (xcvrd.py:1154): init ->
// start the enabled worker threads -> wait on the stop event -> join them
// (SIGKILL on a child-thread exception) -> deinit. The pieces that touch the
// platform HAL / STATE_DB (init, deinit, worker construction, process kill) are
// funnelled through the [`DaemonHooks`] seam so unit tests inject observable
// mocks, exactly like `test_xcvrd.py` patches `DaemonXcvrd.init`, the task
// `start`/`join`, and `os.kill`.
// ---------------------------------------------------------------------------

/// A latch with blocking wait, the subset of Python's `threading.Event` xcvrd
/// uses (`set`/`clear`/`is_set`/`wait`). `stop_event` and `sfp_error_event` are
/// both instances.
#[derive(Default)]
pub struct Event {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Event {
    /// A fresh, shareable (cross-thread) unset event.
    pub fn new() -> Arc<Event> {
        Arc::new(Event::default())
    }
    pub fn set(&self) {
        let mut g = self.flag.lock().unwrap();
        *g = true;
        self.cv.notify_all();
    }
    pub fn clear(&self) {
        *self.flag.lock().unwrap() = false;
    }
    pub fn is_set(&self) -> bool {
        *self.flag.lock().unwrap()
    }
    /// Block until set (returns immediately if already set).
    pub fn wait(&self) {
        let mut g = self.flag.lock().unwrap();
        while !*g {
            g = self.cv.wait(g).unwrap();
        }
    }
}

/// The POSIX signals `signal_handler` distinguishes (xcvrd.py:915).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    Hup,
    Int,
    Term,
    Other(i32),
}

/// A logger whose runtime level can be refreshed. `helper_logger`, the daemon's
/// own `logger_instance`, and each worker thread expose this (xcvrd.py:903).
pub trait LogSink {
    fn update_log_level(&self);
}

/// Production logger: this structural seam's runtime log-level reload is a no-op;
/// the deployed daemon path lives in `daemon.rs`.
#[derive(Default)]
pub struct NoopLogSink;

impl LogSink for NoopLogSink {
    fn update_log_level(&self) {}
}

/// Which worker thread a [`Task`] is — the five started by `DaemonXcvrd::run`.
/// Used to reproduce the type-specific shutdown handling (only
/// `SfpStateUpdateTask` is asked to `raise_exception()` before its join).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum TaskKind {
    Sff,
    Cmis,
    DomInfo,
    DomThermal,
    SfpState,
}

impl TaskKind {
    pub fn name(&self) -> &'static str {
        match self {
            TaskKind::Sff => "SffManagerTask",
            TaskKind::Cmis => "CmisManagerTask",
            TaskKind::DomInfo => "DomInfoUpdateTask",
            TaskKind::DomThermal => "DomThermalInfoUpdateTask",
            TaskKind::SfpState => "SfpStateUpdateTask",
        }
    }
}

/// A worker thread the daemon owns, exposing the lifecycle surface
/// `run()` drives (`start`/`join`/`is_alive`/`raise_exception`).
pub trait Task {
    fn kind(&self) -> TaskKind;
    fn name(&self) -> String {
        self.kind().name().to_string()
    }
    fn start(&self);
    fn join(&self) -> Result<(), String>;
    fn is_alive(&self) -> bool {
        false
    }
    fn raise_exception(&self) {}
    fn update_log_level(&self) {}
}

/// The platform/STATE_DB-touching collaborators of `DaemonXcvrd::run()`, behind a
/// seam so unit tests inject observable mocks — the Rust analogue of
/// `@patch('xcvrd.xcvrd.DaemonXcvrd.init')`, the task `start`/`join` patches, and
/// `@patch('os.kill')`.
pub trait DaemonHooks {
    /// `DaemonXcvrd.init` (xcvrd.py:1034): load the chassis, wait for port config,
    /// build the port map, seed/clean STATE_DB.
    fn init(&self) -> Result<(), String>;
    /// `DaemonXcvrd.deinit` (xcvrd.py:1095): purge the per-port STATE_DB tables.
    fn deinit(&self) -> Result<(), String>;
    /// Construct one worker thread of the given kind.
    fn make_task(&self, kind: TaskKind) -> Box<dyn Task>;
    /// `os.kill(os.getpid(), SIGKILL)` — abort when a child thread died with an
    /// exception (xcvrd.py:1215).
    fn kill_self(&self);
}

/// A worker with no behaviour — a placeholder task the real tasks replace.
struct StubTask {
    kind: TaskKind,
}

impl Task for StubTask {
    fn kind(&self) -> TaskKind {
        self.kind
    }
    fn start(&self) {}
    fn join(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Production hooks. `init`/`deinit` and worker construction are stubbed here
/// (the deployed daemon runs `daemon.rs`).
#[derive(Default)]
pub struct RealDaemonHooks;

impl DaemonHooks for RealDaemonHooks {
    fn init(&self) -> Result<(), String> {
        Ok(())
    }
    fn deinit(&self) -> Result<(), String> {
        Ok(())
    }
    fn make_task(&self, kind: TaskKind) -> Box<dyn Task> {
        Box::new(StubTask { kind })
    }
    fn kill_self(&self) {
        // The deployed daemon aborts via the pmon supervisor; no-op here.
    }
}

/// The per-port STATE_DB teardown seam for [`DaemonXcvrd::deinit_tables`] — the Rust
/// analogue of the Python `common.del_port_sfp_dom_info_from_db` the deinit tests patch.
/// It splits the two table-sets the daemon deletes on shutdown so the warm/fast-reboot
/// gate can be observed: the DOM/VDM/PM/firmware "hardware" tables (always deleted) versus
/// the `TRANSCEIVER_STATUS` + `TRANSCEIVER_STATUS_SW` status pair (deleted only on a cold
/// exit, so an active datapath is not disrupted across a warm/fast reboot).
pub trait DeinitTeardown {
    /// Delete every DOM/VDM/PM/firmware/flag row for this logical port. `TRANSCEIVER_INFO`
    /// is intentionally NOT deleted (the Python passes `intf_tbl = None` @1100) to avoid an
    /// optical-app Tx-disable being triggered by the info-table deletion during shutdown.
    fn del_hw_tables(&self, lport: &str, asic: i32);
    /// Delete the `TRANSCEIVER_STATUS` + `TRANSCEIVER_STATUS_SW` rows for this logical port.
    fn del_status_tables(&self, lport: &str, asic: i32);
}

/// Rust port of the Python `DaemonXcvrd` (xcvrd.py:890) — the top-level daemon:
/// holds the CLI flags + shared events and drives the init/run/deinit lifecycle.
/// Seam for `xcvr_table_helper.get_state_port_tbl(asic_index)` — resolves the per-ASIC
/// STATE_DB PORT table. The daemon wraps the real `XcvrTableHelper`; unit tests inject a
/// resolver returning a `MockTable` (or `None` to model a missing per-ASIC table).
pub trait StatePortTblResolver {
    fn get_state_port_tbl(&self, asic_index: i32) -> Option<Rc<dyn Table>>;
}

pub struct DaemonXcvrd {
    pub skip_cmis_mgr: bool,
    pub enable_sff_mgr: bool,
    pub dom_temperature_poll_interval: Option<i64>,
    pub dom_update_interval: Option<i64>,
    pub namespaces: Vec<String>,
    /// Multi-ASIC platform flag (drives namespace → `asic{N}` resolution in deinit).
    pub multi_asic: bool,
    pub threads: Vec<Box<dyn Task>>,
    pub stop_event: Arc<Event>,
    pub sfp_error_event: Arc<Event>,
    pub helper_logger: Box<dyn LogSink>,
    pub logger_instance: Box<dyn LogSink>,
    pub hooks: Box<dyn DaemonHooks>,
}

impl DaemonXcvrd {
    /// `DaemonXcvrd.__init__` (xcvrd.py:891).
    pub fn new(
        _log_identifier: &str,
        skip_cmis_mgr: bool,
        enable_sff_mgr: bool,
        dom_temperature_poll_interval: Option<i64>,
        dom_update_interval: Option<i64>,
    ) -> Self {
        DaemonXcvrd {
            skip_cmis_mgr,
            enable_sff_mgr,
            dom_temperature_poll_interval,
            dom_update_interval,
            namespaces: vec![String::new()],
            multi_asic: false,
            threads: Vec::new(),
            stop_event: Event::new(),
            sfp_error_event: Event::new(),
            helper_logger: Box::new(NoopLogSink),
            logger_instance: Box::new(NoopLogSink),
            hooks: Box::new(RealDaemonHooks),
        }
    }

    /// `update_loggers_log_level` (xcvrd.py:903): refresh the module logger, the
    /// daemon's own logger, and every worker thread that supports it.
    pub fn update_loggers_log_level(&self) {
        self.helper_logger.update_log_level();
        self.logger_instance.update_log_level();
        for thread in &self.threads {
            thread.update_log_level();
        }
    }

    /// `signal_handler` (xcvrd.py:915): SIGHUP reloads log levels; SIGINT/SIGTERM
    /// request shutdown via the stop event; anything else is logged.
    pub fn signal_handler(&self, sig: Signal) {
        match sig {
            Signal::Hup => {
                eprintln!("xcvrd: Caught SIGHUP...");
                self.update_loggers_log_level();
            }
            Signal::Int => {
                eprintln!("xcvrd: Caught SIGINT - exiting...");
                self.stop_event.set();
            }
            Signal::Term => {
                eprintln!("xcvrd: Caught SIGTERM - exiting...");
                self.stop_event.set();
            }
            Signal::Other(s) => {
                eprintln!("xcvrd: Caught unhandled signal '{s}'");
            }
        }
    }

    /// `run` (xcvrd.py:1154): init, spawn the enabled worker threads, wait for the
    /// stop event, join them (SIGKILL if a child died with an exception), deinit.
    pub fn run(&mut self) -> Result<(), String> {
        eprintln!("xcvrd: Starting up...");

        // Start daemon initialization sequence.
        self.init()?;

        // Spawn the worker threads gated by the CLI flags (xcvrd.py:1160-1192).
        self.threads.clear();
        if self.enable_sff_mgr {
            let t = self.hooks.make_task(TaskKind::Sff);
            t.start();
            self.threads.push(t);
        } else {
            eprintln!("xcvrd: Skipping SFF Task Manager");
        }
        if !self.skip_cmis_mgr {
            let t = self.hooks.make_task(TaskKind::Cmis);
            t.start();
            self.threads.push(t);
        }
        let dom_info = self.hooks.make_task(TaskKind::DomInfo);
        dom_info.start();
        self.threads.push(dom_info);
        if self.dom_temperature_poll_interval.is_some() {
            let t = self.hooks.make_task(TaskKind::DomThermal);
            t.start();
            self.threads.push(t);
        }
        let sfp_state = self.hooks.make_task(TaskKind::SfpState);
        sfp_state.start();
        self.threads.push(sfp_state);

        eprintln!(
            "xcvrd: Start daemon main loop with thread count {}",
            self.threads.len()
        );
        for thread in &self.threads {
            eprintln!("xcvrd: Started thread {}", thread.name());
        }

        // Start main loop.
        self.stop_event.wait();
        eprintln!("xcvrd: Stop daemon main loop");

        // First pass: join threads that already died; a join error means the
        // child raised -> SIGKILL ourselves (xcvrd.py:1204-1215).
        let mut generate_sigkill = false;
        for thread in &self.threads {
            if !thread.is_alive() && thread.join().is_err() {
                generate_sigkill = true;
            }
        }
        if generate_sigkill {
            eprintln!("xcvrd: Exiting main loop as child thread raised exception!");
            self.hooks.kill_self();
        }

        // Second pass: join threads still alive; SfpStateUpdateTask is asked to
        // raise first so its blocking loop unwinds (xcvrd.py:1217-1239).
        for thread in &self.threads {
            if thread.is_alive() {
                if thread.kind() == TaskKind::SfpState {
                    thread.raise_exception();
                }
                let _ = thread.join();
            }
        }

        // Start daemon deinitialization sequence.
        self.deinit()?;

        eprintln!("xcvrd: Shutting down...");

        if self.sfp_error_event.is_set() {
            return Err("SFP_SYSTEM_ERROR".to_string());
        }
        Ok(())
    }

    /// `init` (xcvrd.py:1034) — delegates the platform/DB work to the seam.
    pub fn init(&self) -> Result<(), String> {
        self.hooks.init()
    }

    /// `deinit` (xcvrd.py:1095) — delegates the STATE_DB teardown to the seam.
    pub fn deinit(&self) -> Result<(), String> {
        self.hooks.deinit()
    }

    /// `deinit` (xcvrd.py:1076) — the testable STATE_DB teardown. Pre-fetches the warm/fast
    /// reboot verdict per namespace (read **fresh** from `db`, never cached, so a warm/fast
    /// reboot that started after xcvrd came up is honoured), then for every logical port:
    /// always deletes the DOM/VDM/PM/firmware "hardware" tables, and deletes the
    /// `TRANSCEIVER_STATUS`/`TRANSCEIVER_STATUS_SW` status pair ONLY on a cold exit. Skipping
    /// the status pair on a warm/fast reboot preserves the live datapath state across xcvrd
    /// re-init (test_warm_reboot / test_DaemonXcvrd_init_deinit_fastboot_enabled).
    pub fn deinit_tables(
        &self,
        port_mapping: &PortMapping,
        db: &dyn common::StateDbHget,
        teardown: &dyn DeinitTeardown,
    ) {
        // Pre-fetch warm/fast reboot status for all namespaces (Python @1080-1082).
        let verdict =
            common::is_syncd_warm_restore_complete(db) || common::is_fast_reboot_enabled(db);
        let warm_fast_reboot_status: HashMap<String, bool> =
            self.namespaces.iter().map(|ns| (ns.clone(), verdict)).collect();

        for lport in &port_mapping.logical_port_list {
            let asic = match port_mapping.get_asic_id_for_logical_port(lport) {
                Some(a) => a,
                None => continue,
            };
            let namespace = common::get_namespace_from_asic_id(asic, self.multi_asic);
            let is_warm_fast_reboot = *warm_fast_reboot_status.get(&namespace).unwrap_or(&false);

            teardown.del_hw_tables(lport, asic);
            if !is_warm_fast_reboot {
                teardown.del_status_tables(lport, asic);
            }
        }
    }

    // ---- port init sequence ------------------------------

    /// `wait_for_port_config_done(namespace)` (xcvrd.py:933): subscribe to APPL_DB PORT
    /// and block until a `PortConfigDone`/`PortInitDone` notification arrives (or the stop
    /// event is set). The swss `Select`/`SubscriberStateTable` plumbing is the injected
    /// [`PortEventSource`] seam so the daemon drives real swss and unit tests script it.
    pub fn wait_for_port_config_done(&self, source: &dyn PortEventSource) {
        while !self.stop_event.is_set() {
            match source.select(SELECT_TIMEOUT_MSECS) {
                SelectState::Timeout => continue,
                SelectState::Error => {
                    eprintln!("xcvrd: sel.select() did not return swsscommon.Select.OBJECT");
                    continue;
                }
                SelectState::Object => {}
            }
            if let Some(pop) = source.pop(0) {
                if pop.key == "PortConfigDone" || pop.key == "PortInitDone" {
                    break;
                }
            }
        }
    }

    /// `initialize_port_init_control_fields_in_port_table(port_mapping_data)`
    /// (xcvrd.py:958): for each logical port, seed STATE_DB `PORT_TABLE|<lport>` with
    /// `NPU_SI_SETTINGS_SYNC_STATUS = DEFAULT` when the field is absent. Ports whose ASIC
    /// has no state PORT table are skipped. The per-ASIC table lookup is the injected
    /// [`StatePortTblResolver`] seam.
    pub fn initialize_port_init_control_fields_in_port_table(
        &self,
        port_mapping: &PortMapping,
        resolver: &dyn StatePortTblResolver,
    ) {
        for lport in &port_mapping.logical_port_list {
            let asic_index = match port_mapping.get_asic_id_for_logical_port(lport) {
                Some(a) => a,
                None => continue,
            };
            let state_port_tbl = match resolver.get_state_port_tbl(asic_index) {
                Some(t) => t,
                None => continue,
            };
            let fvs = state_port_tbl.get(lport).ok().flatten().unwrap_or_default();
            let dict: HashMap<String, String> = fvs.into_iter().collect();
            if !dict.contains_key(NPU_SI_SETTINGS_SYNC_STATUS_KEY) {
                let _ = state_port_tbl.set(
                    lport,
                    &[(
                        NPU_SI_SETTINGS_SYNC_STATUS_KEY.to_string(),
                        NPU_SI_SETTINGS_DEFAULT_VALUE.to_string(),
                    )],
                );
            }
        }
    }

    /// `self.sfp_obj_dict = common.get_pluggable_obj_dict(port_mapping_data)`
    /// (xcvrd.py:1064): the set of physical ports backed by a pluggable SFP. In Rust the
    /// `sfp_obj_dict` is the [`Chassis`] seam, so this returns the physical-port set the
    /// daemon iterates (a port whose `chassis.sfp` fails — the Python `get_sfp` raising —
    /// is excluded).
    pub fn initialize_sfp_obj_dict(
        &self,
        port_mapping: &PortMapping,
        chassis: &dyn Chassis,
    ) -> BTreeSet<i32> {
        common::get_pluggable_obj_dict(Some(port_mapping), chassis)
    }

    /// `remove_stale_transceiver_info` (xcvrd.py:999) — delegates to the free
    /// function; the deployed daemon supplies its intf table + chassis.
    pub fn remove_stale_transceiver_info(
        &self,
        port_mapping: &PortMapping,
        intf_tbl: &dyn Table,
        chassis: &dyn Chassis,
    ) {
        remove_stale_transceiver_info(port_mapping, intf_tbl, chassis)
    }
}

/// `_wrapper_is_replaceable` (xcvrd.py:105): FRU-replaceable flag; false on any
/// HAL error (the analogue of the Python `NotImplementedError`/no-platform paths).
pub fn wrapper_is_replaceable(chassis: &dyn Chassis, physical_port: usize) -> bool {
    match chassis.sfp(physical_port) {
        Ok(sfp) => sfp.is_replaceable().unwrap_or(false),
        Err(_) => false,
    }
}

/// `_wrapper_get_transceiver_info` (xcvrd.py:114): identity dict, or `None` when
/// the EEPROM is not yet readable (bridge `Null`) or on any HAL error.
pub fn wrapper_get_transceiver_info(chassis: &dyn Chassis, physical_port: usize) -> Option<Value> {
    match chassis.sfp(physical_port) {
        Ok(sfp) => match sfp.get_transceiver_info() {
            Ok(Value::Null) => None,
            Ok(v) => {
                // An empty object is the emulator's "EEPROM not ready" shape too.
                if v.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                    None
                } else {
                    Some(v)
                }
            }
            Err(_) => None,
        },
        Err(_) => None,
    }
}

/// `_wrapper_get_transceiver_change_event` (xcvrd.py:141): poll + decompose into
/// `(status, sfp, sfp_error)`; on error, an empty (no-change) triple.
pub fn wrapper_get_transceiver_change_event(
    chassis: &dyn Chassis,
    timeout_ms: u64,
) -> (bool, BTreeMap<String, String>, BTreeMap<String, String>) {
    match chassis.get_change_event(timeout_ms) {
        Ok(ev) => (ev.status, ev.sfp, ev.sfp_error),
        Err(_) => (false, BTreeMap::new(), BTreeMap::new()),
    }
}

/// `_wrapper_get_sfp_type` (xcvrd.py:154): module form-factor string, or `None`.
pub fn wrapper_get_sfp_type(chassis: &dyn Chassis, physical_port: usize) -> Option<String> {
    match chassis.sfp(physical_port) {
        Ok(sfp) => sfp.sfp_type().ok().filter(|s| !s.is_empty()),
        Err(_) => None,
    }
}

/// `_wrapper_get_sfp_error_description` (xcvrd.py:167): vendor error text, or `None`.
pub fn wrapper_get_sfp_error_description(chassis: &dyn Chassis, physical_port: usize) -> Option<String> {
    match chassis.sfp(physical_port) {
        Ok(sfp) => sfp.get_error_description().ok().flatten(),
        Err(_) => None,
    }
}

/// `_wrapper_soak_sfp_insert_event` (xcvrd.py:127): defer insert events by
/// `MGMT_INIT_TIME_DELAY_SECS` so management init completes before we act on them.
pub fn wrapper_soak_sfp_insert_event(
    sfp_insert_events: &mut BTreeMap<String, Instant>,
    port_dict: &mut BTreeMap<String, String>,
    now: Instant,
) {
    let entries: Vec<(String, String)> = port_dict.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (key, value) in entries {
        if value == SFP_STATUS_INSERTED {
            sfp_insert_events.insert(key.clone(), now);
            port_dict.remove(&key);
        } else if value == SFP_STATUS_REMOVED {
            sfp_insert_events.remove(&key);
        }
    }
    let ready: Vec<String> = sfp_insert_events
        .iter()
        .filter(|(_, itime)| now.duration_since(**itime) >= Duration::from_secs(MGMT_INIT_TIME_DELAY_SECS))
        .map(|(k, _)| k.clone())
        .collect();
    for key in ready {
        port_dict.insert(key.clone(), SFP_STATUS_INSERTED.to_string());
        sfp_insert_events.remove(&key);
    }
}

/// `str(value)` for STATE_DB writes: strings are **NUL-trimmed only** (CMIS identity
/// fields are fixed-width, NUL-padded) with any trailing ASCII spaces PRESERVED —
/// Python's `str(value)` in `post_port_sfp_info_to_db` strips nothing, so a
/// space-terminated field like `vendor_date="2024-12-14 "` must keep its trailing
/// space to match the reference TRANSCEIVER_INFO projection (an extra `.trim_end()`
/// here diverged for every space-padded field). Bools render `True`/`False`,
/// `None`→`"None"`.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::String(s) => s.trim_end_matches('\0').to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// One identity field for the SFF (non-CMIS) row, matching the Python fixed field
/// list. Missing optional fields default to `"N/A"` (as Python does explicitly for
/// `application_advertisement`/`dom_capability`); other misses stay empty rather
/// than panicking, keeping the daemon resilient on a malformed module.
fn field_str(obj: &serde_json::Map<String, Value>, key: &str) -> String {
    obj.get(key).map(py_str).unwrap_or_else(|| "N/A".to_string())
}

/// `post_port_sfp_info_to_db` (xcvrd.py:178): publish TRANSCEIVER_INFO for a
/// logical port. Returns `PHYSICAL_PORT_NOT_EXIST` if the port has no physical
/// mapping, `SFP_EEPROM_NOT_READY` if identity can't be read yet, else `0`.
pub fn post_port_sfp_info_to_db(
    chassis: &dyn Chassis,
    logical_port_name: &str,
    port_mapping: &PortMapping,
    table: &dyn Table,
    transceiver_dict: &mut BTreeMap<i32, Option<Value>>,
    stop: &dyn Fn() -> bool,
) -> i32 {
    let physical_port_list = match port_mapping.logical_port_name_to_physical_port_list(logical_port_name) {
        Some(list) => list,
        None => return PHYSICAL_PORT_NOT_EXIST,
    };
    let ganged_port = physical_port_list.len() > 1;
    let mut ganged_member_num = 1;

    for physical_port in physical_port_list {
        if stop() {
            break;
        }
        if !common::wrapper_get_presence(chassis, physical_port as usize) {
            continue;
        }
        let port_name = common::get_physical_port_name(logical_port_name, ganged_member_num, ganged_port);
        ganged_member_num += 1;

        let port_info = if let Some(cached) = transceiver_dict.get(&physical_port) {
            cached.clone()
        } else {
            let info = wrapper_get_transceiver_info(chassis, physical_port as usize);
            transceiver_dict.insert(physical_port, info.clone());
            info
        };

        match port_info {
            Some(info) => {
                let is_replaceable = wrapper_is_replaceable(chassis, physical_port as usize);
                let mut fvs: Vec<(String, String)> = Vec::new();
                let obj = info.as_object().cloned().unwrap_or_default();
                if obj.contains_key("cmis_rev") {
                    for (field, value) in &obj {
                        fvs.push((field.clone(), py_str(value)));
                    }
                    fvs.push(("is_replaceable".to_string(), py_str(&Value::Bool(is_replaceable))));
                } else {
                    fvs.push(("type".to_string(), field_str(&obj, "type")));
                    fvs.push(("vendor_rev".to_string(), field_str(&obj, "vendor_rev")));
                    fvs.push(("serial".to_string(), field_str(&obj, "serial")));
                    fvs.push(("manufacturer".to_string(), field_str(&obj, "manufacturer")));
                    fvs.push(("model".to_string(), field_str(&obj, "model")));
                    fvs.push(("vendor_oui".to_string(), field_str(&obj, "vendor_oui")));
                    fvs.push(("vendor_date".to_string(), field_str(&obj, "vendor_date")));
                    fvs.push(("connector".to_string(), field_str(&obj, "connector")));
                    fvs.push(("encoding".to_string(), field_str(&obj, "encoding")));
                    fvs.push(("ext_identifier".to_string(), field_str(&obj, "ext_identifier")));
                    fvs.push(("ext_rateselect_compliance".to_string(), field_str(&obj, "ext_rateselect_compliance")));
                    fvs.push(("cable_type".to_string(), field_str(&obj, "cable_type")));
                    fvs.push(("cable_length".to_string(), field_str(&obj, "cable_length")));
                    fvs.push(("specification_compliance".to_string(), field_str(&obj, "specification_compliance")));
                    fvs.push(("nominal_bit_rate".to_string(), field_str(&obj, "nominal_bit_rate")));
                    fvs.push(("application_advertisement".to_string(), field_str(&obj, "application_advertisement")));
                    fvs.push(("is_replaceable".to_string(), py_str(&Value::Bool(is_replaceable))));
                    fvs.push(("dom_capability".to_string(), field_str(&obj, "dom_capability")));
                }
                if table.set(&port_name, &fvs).is_err() {
                    return SFP_EEPROM_NOT_READY;
                }
            }
            None => return SFP_EEPROM_NOT_READY,
        }
    }
    0
}

/// `waiting_time_compensation_with_sleep` (xcvrd.py:248): sleep only for the
/// remainder of `time_to_wait` since `time_start`.
pub fn waiting_time_compensation_with_sleep(time_start: Instant, time_to_wait: Duration, now: Instant) -> Option<Duration> {
    let time_diff = now.duration_since(time_start);
    if time_diff < time_to_wait {
        Some(time_to_wait - time_diff)
    } else {
        None
    }
}

/// `remove_stale_transceiver_info` (xcvrd.py:999): at init, purge TRANSCEIVER_INFO
/// rows for logical ports whose module is absent (a stale row from a prior plug).
pub fn remove_stale_transceiver_info(
    port_mapping: &PortMapping,
    intf_tbl: &dyn Table,
    chassis: &dyn Chassis,
) {
    for lport in &port_mapping.logical_port_list {
        let found = matches!(intf_tbl.get(lport), Ok(Some(_)));
        if !found {
            continue;
        }
        let pport_list = match port_mapping.get_logical_to_physical(lport) {
            Some(list) if !list.is_empty() => list,
            _ => continue,
        };
        let pport = pport_list[0];
        if !common::wrapper_get_presence(chassis, pport as usize) {
            let _ = common::del_port_sfp_dom_info_from_db(lport, port_mapping, &[Some(intf_tbl)]);
        }
    }
}


#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::hal::ChangeEvent;
    use crate::mock::{MockChassis, MockSfp, MockTable};

    /// A map-backed [`common::StateDbHget`] double so the warm/fast-reboot detectors can be
    /// driven with canned WARM_RESTART / FAST_RESTART rows (the Rust analogue of the Python
    /// `@patch('...common.is_syncd_warm_restore_complete', ...)`).
    #[derive(Default)]
    struct MockRebootDb {
        fields: std::collections::HashMap<(String, String), String>,
    }
    impl MockRebootDb {
        /// A DB in which `is_syncd_warm_restore_complete` returns `warm` (via the
        /// WARM_RESTART_ENABLE_TABLE|system.enable row).
        fn warm(warm: bool) -> Self {
            let mut db = MockRebootDb::default();
            if warm {
                db.fields.insert(
                    ("WARM_RESTART_ENABLE_TABLE|system".to_string(), "enable".to_string()),
                    "true".to_string(),
                );
            }
            db
        }
        /// A DB in which `is_fast_reboot_enabled` returns `fast` (via the
        /// FAST_RESTART_ENABLE_TABLE|system.enable row).
        fn fast_reboot(fast: bool) -> Self {
            let mut db = MockRebootDb::default();
            if fast {
                db.fields.insert(
                    ("FAST_RESTART_ENABLE_TABLE|system".to_string(), "enable".to_string()),
                    "true".to_string(),
                );
            }
            db
        }
    }
    impl common::StateDbHget for MockRebootDb {
        fn get_field(&self, key: &str, field: &str) -> Option<String> {
            self.fields.get(&(key.to_string(), field.to_string())).cloned()
        }
    }

    /// A counting + scriptable [`SfpEnv`], the Rust analogue of the bundle of
    /// `@patch(...)`es the Python `SfpStateUpdateTask` tests install (change-event
    /// source, `_mapping_event_from_change_event`, `post_port_sfp_info_to_db`,
    /// `update_port_transceiver_status_table_sw`, `del_port_sfp_dom_info_from_db`,
    /// the DOM/VDM threshold posters, `notify_media_setting`, `os.kill`, …).
    #[derive(Default)]
    struct MockSfpEnv {
        /// `_wrapper_get_transceiver_change_event` return value (repeated).
        change_event: RefCell<(bool, BTreeMap<String, String>, BTreeMap<String, String>)>,
        /// `_mapping_event_from_change_event`: scripted `side_effect` list first,
        /// then the fixed `return_value`.
        map_script: RefCell<VecDeque<String>>,
        map_default: RefCell<String>,
        /// `post_port_sfp_info_to_db` return code + call count.
        post_rc: Cell<i32>,
        post_calls: Cell<u32>,
        update_calls: Cell<u32>,
        update_last: RefCell<Option<(String, i32, String, String)>>,
        del_calls: Cell<u32>,
        dom_calls: Cell<u32>,
        vdm_calls: Cell<u32>,
        media_calls: Cell<u32>,
        kill_calls: Cell<u32>,
        set_state_calls: Cell<u32>,
        remove_api_calls: Cell<u32>,
        present: Cell<bool>,
        sfp_error_desc: RefCell<Option<String>>,
        /// Scripted `post_port_sfp_info_to_db` return codes, one popped per call;
        /// when empty/exhausted the fixed `post_rc` is used. Lets a test model a
        /// just-inserted module that reads `SFP_EEPROM_NOT_READY` first and then
        /// succeeds on the post-`TIME_FOR_SFP_READY` re-read.
        post_rc_seq: RefCell<VecDeque<i32>>,
        /// Count of `env.sleep(...)` calls (the insert grace pause).
        sleep_calls: Cell<u32>,
    }

    impl MockSfpEnv {
        fn set_change_event(&self, status: bool, sfp: &[(&str, &str)], sfp_error: &[(&str, &str)]) {
            let sfp_map = sfp.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let err_map = sfp_error.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            *self.change_event.borrow_mut() = (status, sfp_map, err_map);
        }
        fn set_map_default(&self, event: &str) {
            *self.map_default.borrow_mut() = event.to_string();
            self.map_script.borrow_mut().clear();
        }
        fn set_map_script(&self, events: &[&str]) {
            *self.map_script.borrow_mut() = events.iter().map(|e| e.to_string()).collect();
        }
        fn set_post_rc_seq(&self, rcs: &[i32]) {
            *self.post_rc_seq.borrow_mut() = rcs.iter().copied().collect();
        }
    }

    impl SfpEnv for MockSfpEnv {
        fn get_change_event(&self, _timeout_ms: u64) -> (bool, BTreeMap<String, String>, BTreeMap<String, String>) {
            self.change_event.borrow().clone()
        }
        fn soak_sfp_insert_event(&self, _events: &mut BTreeMap<String, Instant>, _port_dict: &mut BTreeMap<String, String>) {
            // The Python task patches `_wrapper_soak_sfp_insert_event` to a no-op.
        }
        fn map_event(&self, _status: bool, _port_dict: &mut BTreeMap<String, String>) -> String {
            if let Some(e) = self.map_script.borrow_mut().pop_front() {
                e
            } else {
                self.map_default.borrow().clone()
            }
        }
        fn post_port_sfp_info_to_db(&self, _lport: &str, _pm: &PortMapping, _asic: i32, _td: &mut BTreeMap<i32, Option<Value>>) -> i32 {
            self.post_calls.set(self.post_calls.get() + 1);
            if let Some(rc) = self.post_rc_seq.borrow_mut().pop_front() {
                rc
            } else {
                self.post_rc.get()
            }
        }
        fn update_status_sw(&self, lport: &str, asic: i32, status: &str, error: &str) {
            self.update_calls.set(self.update_calls.get() + 1);
            *self.update_last.borrow_mut() = Some((lport.to_string(), asic, status.to_string(), error.to_string()));
        }
        fn del_port_sfp_dom_info(&self, _lport: &str, _pm: &PortMapping, _asic: i32) {
            self.del_calls.set(self.del_calls.get() + 1);
        }
        fn post_dom_thresholds(&self, _lport: &str) {
            self.dom_calls.set(self.dom_calls.get() + 1);
        }
        fn post_vdm_thresholds(&self, _lport: &str) {
            self.vdm_calls.set(self.vdm_calls.get() + 1);
        }
        fn notify_media_setting(&self, _lport: &str) {
            self.media_calls.set(self.media_calls.get() + 1);
        }
        fn set_state_port_npu_si(&self, _lport: &str, _asic: i32) {
            self.set_state_calls.set(self.set_state_calls.get() + 1);
        }
        fn remove_xcvr_api(&self, _physical_port: i32) {
            self.remove_api_calls.set(self.remove_api_calls.get() + 1);
        }
        fn get_sfp_error_description(&self, _physical_port: i32) -> Option<String> {
            self.sfp_error_desc.borrow().clone()
        }
        fn is_present(&self, _physical_port: i32) -> bool {
            self.present.get()
        }
        fn kill_parent(&self) {
            self.kill_calls.set(self.kill_calls.get() + 1);
        }
        fn sleep(&self, _dur: Duration) {
            self.sleep_calls.set(self.sleep_calls.get() + 1);
        }
        fn now(&self) -> Instant {
            Instant::now()
        }
    }

    /// A `stopping()` closure over a scripted list of booleans (the analogue of
    /// `stop_event.is_set = MagicMock(side_effect=[...])`); once exhausted, `true`.
    fn scripted_stopping(script: Vec<bool>) -> impl FnMut() -> bool {
        let mut it = script.into_iter();
        move || it.next().unwrap_or(true)
    }

    // ---- test doubles for the daemon lifecycle seams ----------------

    /// Shared, inspectable call log for the injected hooks + tasks (the Rust
    /// stand-in for the `unittest.mock` call counts the Python tests assert).
    #[derive(Default)]
    struct HooksState {
        init: Cell<u32>,
        deinit: Cell<u32>,
        kill: Cell<u32>,
        starts: RefCell<BTreeMap<TaskKind, u32>>,
        joins: RefCell<BTreeMap<TaskKind, u32>>,
    }

    impl HooksState {
        fn starts(&self, k: TaskKind) -> u32 {
            self.starts.borrow().get(&k).copied().unwrap_or(0)
        }
        fn joins(&self, k: TaskKind) -> u32 {
            self.joins.borrow().get(&k).copied().unwrap_or(0)
        }
    }

    /// A counting, scriptable worker thread (`@patch('...Task.start/join')`).
    struct MockTask {
        kind: TaskKind,
        state: Rc<HooksState>,
        alive: bool,
        join_err: bool,
        log_updates: Option<Rc<Cell<u32>>>,
    }

    impl Task for MockTask {
        fn kind(&self) -> TaskKind {
            self.kind
        }
        fn start(&self) {
            *self.state.starts.borrow_mut().entry(self.kind).or_insert(0) += 1;
        }
        fn join(&self) -> Result<(), String> {
            *self.state.joins.borrow_mut().entry(self.kind).or_insert(0) += 1;
            if self.join_err {
                Err("NotImplementedError".to_string())
            } else {
                Ok(())
            }
        }
        fn is_alive(&self) -> bool {
            self.alive
        }
        fn update_log_level(&self) {
            if let Some(c) = &self.log_updates {
                c.set(c.get() + 1);
            }
        }
    }

    /// Hooks that count init/deinit/kill and mint [`MockTask`]s per a script
    /// (`@patch('...DaemonXcvrd.init')`, `@patch('os.kill')`).
    struct MockHooks {
        state: Rc<HooksState>,
        alive: bool,
        err_kinds: Vec<TaskKind>,
    }

    impl DaemonHooks for MockHooks {
        fn init(&self) -> Result<(), String> {
            self.state.init.set(self.state.init.get() + 1);
            Ok(())
        }
        fn deinit(&self) -> Result<(), String> {
            self.state.deinit.set(self.state.deinit.get() + 1);
            Ok(())
        }
        fn make_task(&self, kind: TaskKind) -> Box<dyn Task> {
            Box::new(MockTask {
                kind,
                state: self.state.clone(),
                alive: self.alive,
                join_err: self.err_kinds.contains(&kind),
                log_updates: None,
            })
        }
        fn kill_self(&self) {
            self.state.kill.set(self.state.kill.get() + 1);
        }
    }

    /// A counting logger (helper_logger / logger_instance / thread logger).
    struct MockLog {
        calls: Rc<Cell<u32>>,
    }

    impl LogSink for MockLog {
        fn update_log_level(&self) {
            self.calls.set(self.calls.get() + 1);
        }
    }

    /// Build a daemon whose lifecycle seam is observable via the returned state.
    fn daemon_with_hooks(
        skip_cmis_mgr: bool,
        enable_sff_mgr: bool,
        dom_temperature_poll_interval: Option<i64>,
        alive: bool,
        err_kinds: Vec<TaskKind>,
    ) -> (DaemonXcvrd, Rc<HooksState>) {
        let state = Rc::new(HooksState::default());
        let mut d = DaemonXcvrd::new(
            SYSLOG_IDENTIFIER,
            skip_cmis_mgr,
            enable_sff_mgr,
            dom_temperature_poll_interval,
            None,
        );
        d.hooks = Box::new(MockHooks {
            state: state.clone(),
            alive,
            err_kinds,
        });
        (d, state)
    }

    /// A standalone worker thread for the logger tests: `log_updates` = Some
    /// models a thread that supports `update_log_level`, None one that doesn't.
    fn mock_task_with_log(kind: TaskKind, log_updates: Option<Rc<Cell<u32>>>) -> Box<dyn Task> {
        Box::new(MockTask {
            kind,
            state: Rc::new(HooksState::default()),
            alive: false,
            join_err: false,
            log_updates,
        })
    }

    #[test]
    fn skeleton_present() {
        // Sanity: the module compiles and is wired into the crate.
        assert!(true);
    }

    #[test]
    fn test_SfpStateUpdateTask_task_run_with_exception() {
        // The worker body raises (the analogue of the Python task storing a
        // NotImplementedError from subscribe_port_config_change); join() must
        // surface it and the task must no longer be alive. Mirrors
        // tests/test_xcvrd.py::test_SfpStateUpdateTask_task_run_with_exception.
        let mut task = SfpStateUpdateTask::new(PortMapping::new());
        task.start(|_stop| Err("NotImplementedError: not implemented (side effect)".to_string()));
        let result = task.join();
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("NotImplementedError"));
        assert!(msg.contains("effect"));
        assert!(!task.is_alive());
    }

    #[test]
    fn test_DaemonXcvrd_run_with_exception() {
        // enable_sff + dom_temperature_poll_interval -> 5 worker threads; the CMIS
        // join raises (like the Python NotImplementedError side_effect), so the
        // dead-thread join pass must SIGKILL the process. Mirrors
        // tests/test_xcvrd.py::test_DaemonXcvrd_run_with_exception.
        let (mut daemon, state) =
            daemon_with_hooks(false, true, Some(10), false, vec![TaskKind::Cmis]);
        daemon.stop_event.set(); // stand in for mocking stop_event.wait
        daemon.run().unwrap();

        assert_eq!(daemon.threads.len(), 5);
        assert_eq!(state.init.get(), 1);
        assert_eq!(state.joins(TaskKind::Sff), 1);
        assert_eq!(state.joins(TaskKind::SfpState), 1);
        assert_eq!(state.joins(TaskKind::DomInfo), 1);
        assert_eq!(state.joins(TaskKind::DomThermal), 1);
        assert_eq!(state.kill.get(), 1);
    }

    #[test]
    fn test_post_port_sfp_info_to_db() {
        // Empty PortMapping -> the logical port has no physical mapping, so the
        // publisher reports PHYSICAL_PORT_NOT_EXIST (nothing written). Mirrors
        // tests/test_xcvrd.py::test_post_port_sfp_info_to_db.
        let chassis = MockChassis::with_sfps(vec![]);
        let port_mapping = PortMapping::new();
        let tbl = MockTable::new();
        let mut transceiver_dict: BTreeMap<i32, Option<Value>> = BTreeMap::new();
        let rc = post_port_sfp_info_to_db(
            &chassis,
            "Ethernet0",
            &port_mapping,
            &tbl,
            &mut transceiver_dict,
            &|| false,
        );
        assert_eq!(rc, PHYSICAL_PORT_NOT_EXIST);
        assert_eq!(tbl.set_count(), 0);
    }

    #[test]
    fn test_post_port_sfp_info_to_db_with_sfp_not_present() {
        // Physical port maps but the module is absent: the loop skips it and no
        // INFO row is written. Mirrors
        // tests/test_xcvrd.py::test_post_port_sfp_info_to_db_with_sfp_not_present.
        let chassis = MockChassis::with_sfps(vec![MockSfp::absent()]);
        let mut port_mapping = PortMapping::new();
        port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        let tbl = MockTable::new();
        let mut transceiver_dict: BTreeMap<i32, Option<Value>> = BTreeMap::new();
        let rc = post_port_sfp_info_to_db(
            &chassis,
            "Ethernet0",
            &port_mapping,
            &tbl,
            &mut transceiver_dict,
            &|| false,
        );
        assert_eq!(rc, 0);
        assert_eq!(tbl.set_count(), 0);
    }

    #[test]
    fn test_post_port_sfp_info_and_dom_thr_to_db_once() {
        // A present SFF (non-CMIS) module publishes its fixed identity field list
        // for every logical port. Mirrors
        // tests/test_xcvrd.py::test_post_port_sfp_info_and_dom_thr_to_db_once.
        let info = json!({
            "type": "22.75", "vendor_rev": "0.5", "serial": "0.7", "manufacturer": "0.7",
            "model": "0.7", "vendor_oui": "0.7", "vendor_date": "0.7", "connector": "0.7",
            "encoding": "0.7", "ext_identifier": "0.7", "ext_rateselect_compliance": "0.7",
            "cable_type": "0.7", "cable_length": "0.7", "specification_compliance": "0.7",
            "nominal_bit_rate": "0.7", "application_advertisement": "0.7", "dom_capability": "0.7"
        });
        let chassis = MockChassis::with_sfps(vec![MockSfp::present_with_info(info)]);
        let mut port_mapping = PortMapping::new();
        port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        let tbl = MockTable::new();
        let mut transceiver_dict: BTreeMap<i32, Option<Value>> = BTreeMap::new();
        let rc = post_port_sfp_info_to_db(
            &chassis,
            "Ethernet0",
            &port_mapping,
            &tbl,
            &mut transceiver_dict,
            &|| false,
        );
        assert_eq!(rc, 0);
        assert_eq!(tbl.field("Ethernet0", "type").as_deref(), Some("22.75"));
        assert_eq!(tbl.field("Ethernet0", "is_replaceable").as_deref(), Some("True"));
    }

    #[test]
    fn test_init_port_sfp_status_sw_tbl() {
        // A present module seeds TRANSCEIVER_STATUS_SW status = INSERTED for its
        // logical port. Mirrors tests/test_xcvrd.py::test_init_port_sfp_status_sw_tbl.
        let mut port_mapping = PortMapping::new();
        port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd));
        let mut task = SfpStateUpdateTask::new(port_mapping);
        let env = MockSfpEnv::default();
        env.present.set(true);
        task.init_port_sfp_status_sw_tbl(&env);
        assert_eq!(env.update_calls.get(), 1);
        let last = env.update_last.borrow().clone().unwrap();
        assert_eq!(last.0, "Ethernet0");
        assert_eq!(last.2, SFP_STATUS_INSERTED);
    }

    #[test]
    fn test_init_port_sfp_status_sw_tbl_no_physical_port_found() {
        // A logical port with no physical mapping is still marked REMOVED and the
        // loop continues. Mirrors
        // tests/test_xcvrd.py::test_init_port_sfp_status_sw_tbl_no_physical_port_found.
        let mut port_mapping = PortMapping::new();
        port_mapping.logical_port_list.push("Ethernet0".to_string());
        port_mapping.logical_to_asic.insert("Ethernet0".to_string(), 0);
        // Deliberately omit logical_to_physical so the physical list resolves None.
        let mut task = SfpStateUpdateTask::new(port_mapping);
        let env = MockSfpEnv::default();
        task.init_port_sfp_status_sw_tbl(&env);
        assert_eq!(env.update_calls.get(), 1);
        let last = env.update_last.borrow().clone().unwrap();
        assert_eq!(last.0, "Ethernet0");
        assert_eq!(last.2, SFP_STATUS_REMOVED);
    }

    // ---- port-init sequence tests ----

    use crate::xcvrd_utilities::port_event_helper::{PortEventSource, PortPop, SelectState};

    /// Scripted [`PortEventSource`] with a `select()` call counter, mirroring the Python
    /// `mock_select.select` / `mock_selectable.pop` doubles.
    struct MockWaitSource {
        state: Cell<SelectState>,
        pops: RefCell<VecDeque<PortPop>>,
        select_calls: Cell<usize>,
    }
    impl MockWaitSource {
        fn new(pops: Vec<PortPop>) -> Self {
            MockWaitSource {
                state: Cell::new(SelectState::Object),
                pops: RefCell::new(pops.into_iter().collect()),
                select_calls: Cell::new(0),
            }
        }
    }
    impl PortEventSource for MockWaitSource {
        fn select(&self, _timeout_msecs: i64) -> SelectState {
            self.select_calls.set(self.select_calls.get() + 1);
            self.state.get()
        }
        fn pop(&self, _table_index: usize) -> Option<PortPop> {
            self.pops.borrow_mut().pop_front()
        }
    }

    /// A [`StatePortTblResolver`] returning a fixed table (or `None`).
    struct MockStatePortResolver {
        tbl: Option<Rc<dyn Table>>,
    }
    impl StatePortTblResolver for MockStatePortResolver {
        fn get_state_port_tbl(&self, _asic_index: i32) -> Option<Rc<dyn Table>> {
            self.tbl.clone()
        }
    }

    // Port of test_DaemonXcvrd_wait_for_port_config_done: pop Ethernet0 (ignored), then
    // PortConfigDone breaks the loop — select() is called exactly twice.
    #[test]
    fn test_DaemonXcvrd_wait_for_port_config_done() {
        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);
        let source = MockWaitSource::new(vec![
            PortPop::new("Ethernet0", "SET", vec![("index".to_string(), "1".to_string())]),
            PortPop::new("PortConfigDone", "", vec![]),
        ]);
        daemon.wait_for_port_config_done(&source);
        assert_eq!(source.select_calls.get(), 2);
    }

    // Port of test_DaemonXcvrd_initialize_port_init_control_fields_in_port_table: with no
    // per-ASIC state table nothing is written; with an empty row the NPU_SI sync-status
    // default is seeded once.
    #[test]
    fn test_DaemonXcvrd_initialize_port_init_control_fields_in_port_table() {
        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);
        let mut port_mapping = PortMapping::new();
        port_mapping.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet0",
            1,
            0,
            PortEventType::PortAdd,
        ));

        // No state PORT table for the ASIC → no write.
        let none_resolver = MockStatePortResolver { tbl: None };
        daemon.initialize_port_init_control_fields_in_port_table(&port_mapping, &none_resolver);

        // Empty row → NPU_SI sync-status default is seeded.
        let tbl = Rc::new(MockTable::new());
        let resolver = MockStatePortResolver { tbl: Some(tbl.clone() as Rc<dyn Table>) };
        daemon.initialize_port_init_control_fields_in_port_table(&port_mapping, &resolver);
        assert_eq!(
            tbl.field("Ethernet0", NPU_SI_SETTINGS_SYNC_STATUS_KEY).as_deref(),
            Some(NPU_SI_SETTINGS_DEFAULT_VALUE)
        );
        assert_eq!(tbl.set_count(), 1);

        // Idempotent: a second pass sees the field present and does not rewrite it.
        daemon.initialize_port_init_control_fields_in_port_table(&port_mapping, &resolver);
        assert_eq!(tbl.set_count(), 1);
    }

    // Port of test_get_pluggable_obj_dict: a None mapping yields an empty set (and never
    // touches the chassis); with a mapping, ports whose SFP object is unavailable are
    // excluded.
    #[test]
    fn test_initialize_sfp_obj_dict() {
        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);

        // physical_to_logical = {1, 2, 3}; the chassis has SFPs at 0,1,2 so sfp(3) fails.
        let chassis =
            MockChassis::with_sfps(vec![MockSfp::present(), MockSfp::present(), MockSfp::present()]);
        let mut port_mapping = PortMapping::new();
        for (name, idx) in [("Ethernet0", 1), ("Ethernet4", 2), ("Ethernet8", 3)] {
            port_mapping.handle_port_change_event(&PortChangeEvent::new(
                name,
                idx,
                0,
                PortEventType::PortAdd,
            ));
        }

        // None mapping → empty.
        assert!(common::get_pluggable_obj_dict(None, &chassis).is_empty());

        let dict = daemon.initialize_sfp_obj_dict(&port_mapping, &chassis);
        assert_eq!(dict.len(), 2);
        assert!(dict.contains(&1));
        assert!(dict.contains(&2));
        assert!(!dict.contains(&3));
    }

    #[test]
    fn test_remove_stale_transceiver_info() {
        // Parametrized like tests/test_xcvrd.py::test_remove_stale_transceiver_info:
        // logical ports whose module is absent get their TRANSCEIVER_INFO row purged.
        let cases: Vec<(Vec<&str>, Vec<bool>, Vec<&str>)> = vec![
            (vec!["Ethernet0", "Ethernet1"], vec![false, false], vec!["Ethernet0", "Ethernet1"]),
            (vec!["Ethernet0", "Ethernet1"], vec![true, false], vec!["Ethernet1"]),
            (vec!["Ethernet0", "Ethernet1"], vec![true, true], vec![]),
            (vec![], vec![], vec![]),
        ];
        for (logical_ports, presence, expected_removed) in cases {
            let mut port_mapping = PortMapping::new();
            let sfps: Vec<MockSfp> = presence
                .iter()
                .map(|&p| MockSfp { present: p, ..Default::default() })
                .collect();
            let chassis = MockChassis::with_sfps(sfps);
            let intf_tbl = MockTable::new();
            for (i, lport) in logical_ports.iter().enumerate() {
                port_mapping.handle_port_change_event(&PortChangeEvent::new(*lport, i as i32, 0, PortEventType::PortAdd));
                intf_tbl.set(lport, &[("model".into(), "EMU".into())]).unwrap();
            }
            remove_stale_transceiver_info(&port_mapping, &intf_tbl, &chassis);
            for lport in &logical_ports {
                if expected_removed.contains(lport) {
                    assert!(!intf_tbl.contains(lport), "{lport} should be purged");
                } else {
                    assert!(intf_tbl.contains(lport), "{lport} should be kept");
                }
            }
        }
    }

    #[test]
    fn test_DaemonXcvrd_run() {
        // Defaults: CMIS on, no SFF, no DOM-thermal -> 3 threads (cmis, dom, sfp).
        // Verifies the lifecycle sequence: init once, every thread start+join once,
        // deinit once, no SIGKILL. Mirrors tests/test_xcvrd.py::test_DaemonXcvrd_run.
        let (mut daemon, state) = daemon_with_hooks(false, false, None, false, vec![]);
        daemon.stop_event.set(); // stand in for mocking stop_event.wait
        daemon.run().unwrap();

        assert_eq!(state.init.get(), 1);
        assert_eq!(state.deinit.get(), 1);
        assert_eq!(state.starts(TaskKind::SfpState), 1);
        assert_eq!(state.starts(TaskKind::DomInfo), 1);
        assert_eq!(state.joins(TaskKind::SfpState), 1);
        assert_eq!(state.joins(TaskKind::DomInfo), 1);
        assert_eq!(state.kill.get(), 0);
        assert_eq!(daemon.threads.len(), 3);
    }

    /// `test_update_port_db_diagnostics_on_link_change` (tests/test_xcvrd.py): the
    /// behaviour lives on `DomInfoUpdateTask` (the DOM manager owns the link-change
    /// flag re-read), so the faithful port + its full gate matrix is exercised by
    /// `dom::dom_mgr::tests::test_update_port_db_diagnostics_on_link_change`. This
    /// case pins the happy-path contract from the daemon module's vantage point: an
    /// APPL_DB `PORT_SET` schedules the port, and once its deadline elapses the DOM
    /// + STATUS flag tables are re-read for that port.
    #[test]
    fn test_update_port_db_diagnostics_on_link_change() {
        use crate::dom::dom_mgr::{DomEnv, DomInfoUpdateTask};
        use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType};
        use std::cell::RefCell;

        // Minimal DomEnv double recording the flag-poster calls. The clock advances
        // on each read so a deadline scheduled at now+1 elapses by the next check.
        #[derive(Default)]
        struct RecEnv {
            clock: RefCell<f64>,
            dom_flags: RefCell<Vec<String>>,
            status_flags: RefCell<Vec<String>>,
        }
        impl DomEnv for RecEnv {
            fn handle_port_update_event(&self, _timeout_msecs: i64) {}
            fn now_secs(&self) -> f64 {
                let mut c = self.clock.borrow_mut();
                let v = *c + 100.0;
                *c = v;
                v
            }
            fn detect_port_in_error_status(&self, _lport: &str, _asic: i32) -> bool {
                false
            }
            fn get_dom_polling(&self, _lport: &str) -> String {
                "enabled".to_string()
            }
            fn is_present(&self, _physical_port: i32) -> bool {
                true
            }
            fn post_port_dom_sensor_info_to_db(&self, _lport: &str) {}
            fn post_port_dom_flags_to_db(&self, lport: &str) {
                self.dom_flags.borrow_mut().push(lport.to_string());
            }
            fn post_port_transceiver_hw_status_to_db(&self, _lport: &str) {}
            fn post_port_transceiver_hw_status_flags_to_db(&self, lport: &str) {
                self.status_flags.borrow_mut().push(lport.to_string());
            }
            fn del_port_sfp_dom_info(&self, _lport: &str, _asic: i32) {}
        }

        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet4", 2, 0, PortEventType::PortAdd));
        let mut task = DomInfoUpdateTask::new(pm, Event::new(), true, Some(0));
        let env = RecEnv::default();

        // A flap (APPL_DB PORT_SET) schedules the port at now + 1s.
        let mut flap = PortChangeEvent::new("Ethernet4", 2, 0, PortEventType::PortSet);
        flap.db_name = Some("APPL_DB".to_string());
        task.on_port_update_event(&env, &flap);
        assert!(task.link_change_affected_ports.contains_key(&2));

        // Once the deadline elapses the DOM + STATUS flag tables are re-read.
        let mut never = || false;
        task.check_port_update(&env, &mut never, 1000);
        assert_eq!(env.dom_flags.borrow().as_slice(), &["Ethernet4".to_string()]);
        assert_eq!(env.status_flags.borrow().as_slice(), &["Ethernet4".to_string()]);
        assert!(!task.link_change_affected_ports.contains_key(&2));
    }

    #[test]
    fn test_SfpStateUpdateTask_handle_port_change_event() {
        // PORT_ADD builds the port map (no DOM/INFO purge); PORT_REMOVE tears it
        // down and purges once. Mirrors
        // tests/test_xcvrd.py::test_SfpStateUpdateTask_handle_port_change_event.
        let mut task = SfpStateUpdateTask::new(PortMapping::new());
        let env = MockSfpEnv::default();
        env.present.set(false);

        let add = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd);
        task.on_port_config_change(&env, &add);
        assert_eq!(task.port_mapping.logical_port_list.iter().filter(|p| *p == "Ethernet0").count(), 1);
        assert_eq!(task.port_mapping.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(task.port_mapping.get_physical_to_logical(1), Some(vec!["Ethernet0".to_string()]));
        assert_eq!(task.port_mapping.get_logical_to_physical("Ethernet0"), Some(vec![1]));
        assert_eq!(env.del_calls.get(), 0);

        let remove = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortRemove);
        task.on_port_config_change(&env, &remove);
        assert!(task.port_mapping.logical_port_list.is_empty());
        assert!(task.port_mapping.logical_to_physical.is_empty());
        assert!(task.port_mapping.physical_to_logical.is_empty());
        assert!(task.port_mapping.logical_to_asic.is_empty());
        assert_eq!(env.del_calls.get(), 1);
    }

    #[test]
    fn test_SfpStateUpdateTask_task_run_stop() {
        // start() -> alive; raise_exception() unwinds the cooperative worker loop;
        // join() -> not alive. Mirrors
        // tests/test_xcvrd.py::test_SfpStateUpdateTask_task_run_stop.
        let mut task = SfpStateUpdateTask::new(PortMapping::new());
        task.start(|stop| {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        });
        // Give the worker a moment to spin up.
        for _ in 0..100 {
            if task.is_alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(task.is_alive());
        task.raise_exception();
        task.join().unwrap();
        assert!(!task.is_alive());
    }

    #[test]
    fn test_SfpStateUpdateTask_retry_eeprom_reading() {
        // Empty set -> no publish; recent retry -> throttled; elapsed + NOT_READY
        // keeps the port; elapsed + success drops it. Mirrors
        // tests/test_xcvrd.py::test_SfpStateUpdateTask_retry_eeprom_reading.
        let mut task = SfpStateUpdateTask::new(PortMapping::new());
        let env = MockSfpEnv::default();

        task.retry_eeprom_reading(&env);
        assert_eq!(env.post_calls.get(), 0);

        task.retry_eeprom_set.insert("Ethernet0".to_string());
        task.last_retry_eeprom_time = Some(Instant::now());
        task.retry_eeprom_reading(&env);
        assert_eq!(env.post_calls.get(), 0);

        // last_retry = None forces the interval check to pass (like Python's 0).
        task.last_retry_eeprom_time = None;
        env.post_rc.set(SFP_EEPROM_NOT_READY);
        task.retry_eeprom_reading(&env);
        assert!(task.retry_eeprom_set.contains("Ethernet0"));

        task.last_retry_eeprom_time = None;
        env.post_rc.set(0);
        task.retry_eeprom_reading(&env);
        assert!(!task.retry_eeprom_set.contains("Ethernet0"));
    }

    #[test]
    fn test_SfpStateUpdateTask_mapping_event_from_change_event() {
        // The pure change-event folding: fail/become-ready synthesize an
        // EVENT_ON_ALL_SFP entry; a non-empty port_dict with status is NORMAL.
        // Mirrors tests/test_xcvrd.py::test_SfpStateUpdateTask_mapping_event_from_change_event.
        let task = SfpStateUpdateTask::new(PortMapping::new());

        let mut port_dict: BTreeMap<String, String> = BTreeMap::new();
        assert_eq!(task.mapping_event_from_change_event(false, &mut port_dict), SYSTEM_FAIL);
        assert_eq!(port_dict.get(EVENT_ON_ALL_SFP).map(String::as_str), Some(SYSTEM_FAIL));

        let mut port_dict: BTreeMap<String, String> = BTreeMap::new();
        port_dict.insert(EVENT_ON_ALL_SFP.to_string(), SYSTEM_FAIL.to_string());
        assert_eq!(task.mapping_event_from_change_event(false, &mut port_dict), SYSTEM_FAIL);

        let mut port_dict: BTreeMap<String, String> = BTreeMap::new();
        assert_eq!(task.mapping_event_from_change_event(true, &mut port_dict), SYSTEM_BECOME_READY);
        assert_eq!(port_dict.get(EVENT_ON_ALL_SFP).map(String::as_str), Some(SYSTEM_BECOME_READY));

        let mut port_dict: BTreeMap<String, String> = BTreeMap::new();
        port_dict.insert("1".to_string(), SFP_STATUS_INSERTED.to_string());
        assert_eq!(task.mapping_event_from_change_event(true, &mut port_dict), NORMAL_EVENT);
    }

    #[test]
    fn test_SfpStateUpdateTask_is_warm_fast_reboot_for_lport_invalid_asic() {
        // No ASIC for the logical port => not a warm/fast reboot. Mirrors
        // tests/test_xcvrd.py::test_SfpStateUpdateTask_is_warm_fast_reboot_for_lport_invalid_asic.
        let mut task = SfpStateUpdateTask::new(PortMapping::new());
        // Even with a warm reboot cached for the default namespace, an unmapped lport is False.
        task.initialize_warm_fast_reboot_status(&MockRebootDb::warm(true));
        assert!(!task.is_warm_fast_reboot_for_lport("Ethernet0"));
    }

    #[test]
    fn test_SfpStateUpdateTask_is_warm_fast_reboot_for_lport() {
        // Ethernet0 → asic 2 (multi-ASIC), a warm-restore is complete for that namespace →
        // the port is mid warm/fast reboot. Mirrors
        // tests/test_xcvrd.py::test_SfpStateUpdateTask_is_warm_fast_reboot_for_lport.
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 2, PortEventType::PortAdd));
        let mut task = SfpStateUpdateTask::new(pm);
        task.set_namespaces(vec!["asic2".to_string()], true);
        task.initialize_warm_fast_reboot_status(&MockRebootDb::warm(true));

        assert!(task.is_warm_fast_reboot_for_lport("Ethernet0"));
        // A different port (unmapped) still resolves to False.
        assert!(!task.is_warm_fast_reboot_for_lport("Ethernet4"));
    }


    #[test]
    fn test_SfpStateUpdateTask_task_worker() {
        // The SFP-monitor state machine, driven through the same 7 scenarios as
        // tests/test_xcvrd.py::test_SfpStateUpdateTask_task_worker.
        let mut task = SfpStateUpdateTask::new(PortMapping::new());
        let env = MockSfpEnv::default();
        let sfp_error_event = Event::new();

        // Scenario 1: STATE_INIT + SYSTEM_NOT_READY (repeated) -> STATE_EXIT.
        env.set_change_event(true, &[("0", "0")], &[]);
        env.set_map_default(SYSTEM_NOT_READY);
        task.task_worker(&env, &mut || false, &sfp_error_event);
        assert_eq!(env.kill_calls.get(), 1);
        assert!(sfp_error_event.is_set());

        // Scenario 2: STATE_INIT + SYSTEM_FAIL (repeated) -> STATE_EXIT.
        env.kill_calls.set(0);
        sfp_error_event.clear();
        env.set_map_default(SYSTEM_FAIL);
        task.task_worker(&env, &mut || false, &sfp_error_event);
        assert_eq!(env.kill_calls.get(), 1);
        assert!(sfp_error_event.is_set());

        // Scenario 3: BECOME_READY -> NORMAL, then NOT_READY -> EXIT (no error).
        env.kill_calls.set(0);
        sfp_error_event.clear();
        env.set_map_script(&[SYSTEM_BECOME_READY, SYSTEM_NOT_READY]);
        task.task_worker(&env, &mut || false, &sfp_error_event);
        assert_eq!(env.kill_calls.get(), 1);
        assert!(!sfp_error_event.is_set());

        // Scenario 4: BECOME_READY -> NORMAL, FAIL -> INIT, then FAIL*retry -> EXIT.
        env.kill_calls.set(0);
        sfp_error_event.clear();
        let mut script: Vec<&str> = vec![SYSTEM_BECOME_READY, SYSTEM_FAIL];
        for _ in 0..(RETRY_TIMES_FOR_SYSTEM_READY + 1) {
            script.push(SYSTEM_FAIL);
        }
        env.set_map_script(&script);
        task.task_worker(&env, &mut || false, &sfp_error_event);
        assert_eq!(env.kill_calls.get(), 1);
        assert!(sfp_error_event.is_set());

        // From here, drive a single loop iteration per call (stopping = [false, true]).
        task.port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd));

        // Scenario 5: SFP insert, EEPROM read failure -> retry set + no DOM/VDM/media.
        env.set_change_event(true, &[("1", SFP_STATUS_INSERTED)], &[]);
        env.set_map_default(NORMAL_EVENT);
        env.post_rc.set(SFP_EEPROM_NOT_READY);
        env.update_calls.set(0);
        env.post_calls.set(0);
        env.dom_calls.set(0);
        env.vdm_calls.set(0);
        env.media_calls.set(0);
        let mut stop5 = scripted_stopping(vec![false, true]);
        task.task_worker(&env, &mut stop5, &sfp_error_event);
        assert_eq!(env.update_calls.get(), 1);
        assert_eq!(env.post_calls.get(), 2); // first + retry
        assert_eq!(env.dom_calls.get(), 0);
        assert_eq!(env.vdm_calls.get(), 0);
        assert_eq!(env.media_calls.get(), 0);
        assert!(task.retry_eeprom_set.contains("Ethernet0"));
        task.retry_eeprom_set.clear();

        // Scenario 6: SFP insert, EEPROM read success -> DOM/VDM/media published.
        env.post_rc.set(0);
        env.update_calls.set(0);
        env.post_calls.set(0);
        env.dom_calls.set(0);
        env.vdm_calls.set(0);
        env.media_calls.set(0);
        let mut stop6 = scripted_stopping(vec![false, true]);
        task.task_worker(&env, &mut stop6, &sfp_error_event);
        assert_eq!(env.update_calls.get(), 1);
        assert_eq!(env.post_calls.get(), 1);
        assert_eq!(env.dom_calls.get(), 1);
        assert_eq!(env.vdm_calls.get(), 1);
        assert_eq!(env.media_calls.get(), 1);

        // Scenario 7: SFP remove -> xcvr-API invalidation + status update + per-port
        // table purge. Asserting `remove_api_calls == 1` mirrors
        // tests/test_xcvrd.py::test_sfp_removal_from_dict's
        // `mock_sfp.remove_xcvr_api.assert_called_once()`: the removed event must drop
        // the cached xcvr API so a re-plug rebuilds it from live EEPROM (otherwise
        // memoized CMIS/VDM capability probes stay stale and TRANSCEIVER_PM never
        // republishes).
        env.set_change_event(true, &[("1", SFP_STATUS_REMOVED)], &[]);
        env.update_calls.set(0);
        env.del_calls.set(0);
        env.remove_api_calls.set(0);
        let mut stop7 = scripted_stopping(vec![false, true]);
        task.task_worker(&env, &mut stop7, &sfp_error_event);
        assert_eq!(env.update_calls.get(), 1);
        assert_eq!(env.del_calls.get(), 1);
        assert_eq!(env.remove_api_calls.get(), 1);

        // Scenario 8: SFP error event (blocking|power-budget) -> update + DOM purge.
        let error = 1u32 | sfp_status_helper::SFP_ERROR_BIT_BLOCKING | sfp_status_helper::SFP_ERROR_BIT_POWER_BUDGET_EXCEEDED;
        env.set_change_event(true, &[("1", &error.to_string())], &[]);
        env.update_calls.set(0);
        env.del_calls.set(0);
        let mut stop8 = scripted_stopping(vec![false, true]);
        task.task_worker(&env, &mut stop8, &sfp_error_event);
        assert_eq!(env.update_calls.get(), 1);
        assert_eq!(env.del_calls.get(), 1);
    }

    /// A *just-inserted* module whose identity EEPROM is not
    /// ready on the first read must be re-read once after the `TIME_FOR_SFP_READY`
    /// grace pause, and when that re-read succeeds be fully published
    /// (`TRANSCEIVER_INFO` via `post_port_sfp_info_to_db`, plus DOM/VDM thresholds and
    /// media settings) *without* being parked in the slow `retry_eeprom_set`.
    ///
    /// This pins the exact hot-plug path the replug tests exercise
    /// (test_presence.py::test_replug_restores_info et al.): the runtime daemon was
    /// dropping a transient first-read miss straight into the ~60s retry cadence, so
    /// `TRANSCEIVER_INFO` only repopulated long after the 15s (T_FAST) window. The
    /// reference `SfpStateUpdateTask.task_worker` instead sleeps and re-reads once —
    /// which the daemon now mirrors. Scenario 5/6 of `test_..._task_worker` only cover
    /// "both reads fail" and "first read succeeds"; this covers the missing
    /// first-fails-then-succeeds transition that is the actual replug behaviour.
    #[test]
    fn test_insert_not_ready_then_ready_after_sleep_publishes_without_retry_set() {
        let mut port_mapping = PortMapping::new();
        port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd));
        let mut task = SfpStateUpdateTask::new(port_mapping);
        let env = MockSfpEnv::default();

        // Plug-in whose first identity read is NOT_READY and whose post-sleep re-read
        // succeeds (0).
        env.set_change_event(true, &[("1", SFP_STATUS_INSERTED)], &[]);
        env.set_map_default(NORMAL_EVENT);
        env.set_post_rc_seq(&[SFP_EEPROM_NOT_READY, 0]);

        let sfp_error_event = Event::new();
        let mut stop = scripted_stopping(vec![false, true]);
        task.task_worker(&env, &mut stop, &sfp_error_event);

        assert_eq!(env.sleep_calls.get(), 1, "insert grace pause taken exactly once");
        assert_eq!(env.post_calls.get(), 2, "first read + one post-sleep re-read");
        assert_eq!(env.dom_calls.get(), 1, "DOM thresholds published after the successful re-read");
        assert_eq!(env.vdm_calls.get(), 1, "VDM thresholds published after the successful re-read");
        assert_eq!(env.media_calls.get(), 1, "media settings notified after the successful re-read");
        assert!(
            !task.retry_eeprom_set.contains("Ethernet0"),
            "a prompt post-sleep re-read must repopulate now, not defer to the slow retry set",
        );
    }

    /// Decode the injected change-event error bitmaps end-to-end through
    /// the mockable HAL/DB seam and assert the exact `TRANSCEIVER_STATUS_SW`
    /// (`status`, `error`) that must reach STATE_DB, plus the DOM teardown gate.
    /// The existing `task_worker` scenario 8 only counts calls; here we assert the
    /// decoded error *content* and the blocking-vs-non-blocking DOM decision, which
    /// is what `test_status_error` verifies.
    #[test]
    fn status_sw_error_decode_and_removal_teardown() {
        let mut port_mapping = PortMapping::new();
        port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd));
        let mut task = SfpStateUpdateTask::new(port_mapping);
        let env = MockSfpEnv::default();
        let empty_err = BTreeMap::new();

        // --- Blocking error (I2C_STUCK_EVENT = INSERTED|BLOCKING|I2C_STUCK) ---
        // status field carries the raw bitmap; error is the '|'-joined decode;
        // the blocking bit purges the DOM tables while INFO is preserved.
        let i2c = 1u32 | sfp_status_helper::SFP_ERROR_BIT_BLOCKING | sfp_status_helper::SFP_ERROR_BIT_I2C_STUCK;
        let mut td: BTreeMap<i32, Option<Value>> = BTreeMap::new();
        env.del_calls.set(0);
        task.handle_normal_event(
            &env,
            &[("1".to_string(), i2c.to_string())].into_iter().collect(),
            &empty_err,
            &mut td,
        );
        let (lport, _asic, status, error) = env.update_last.borrow().clone().unwrap();
        assert_eq!(lport, "Ethernet0");
        assert_eq!(status, i2c.to_string());
        assert_eq!(error, "Blocking EEPROM from being read|Bus stuck (I2C data or clock shorted)");
        assert_eq!(env.del_calls.get(), 1, "blocking error must purge DOM tables");
        assert!(task.sfp_error_dict.contains_key(&1), "error state is cached");

        // --- Non-blocking error (HIGH_TEMP_EVENT = INSERTED|HIGH_TEMP) ---
        // status still records the bitmap and error names the condition, but the
        // DOM tables are kept (no blocking bit -> no del).
        let hot = 1u32 | sfp_status_helper::SFP_ERROR_BIT_HIGH_TEMP;
        env.del_calls.set(0);
        task.handle_normal_event(
            &env,
            &[("1".to_string(), hot.to_string())].into_iter().collect(),
            &empty_err,
            &mut td,
        );
        let (_lport, _asic, status, error) = env.update_last.borrow().clone().unwrap();
        assert_eq!(status, hot.to_string());
        assert_eq!(error, "High temperature");
        assert_eq!(env.del_calls.get(), 0, "non-blocking error keeps DOM tables");

        // --- Plug-in recovery (SFP_STATUS_INSERTED) clears the error state ---
        // status returns to "1"/error "N/A", the cached error is dropped, and the
        // identity read is re-attempted (post rc ok -> DOM/VDM/media republished).
        env.post_rc.set(0);
        env.del_calls.set(0);
        env.post_calls.set(0);
        task.handle_normal_event(
            &env,
            &[("1".to_string(), SFP_STATUS_INSERTED.to_string())].into_iter().collect(),
            &empty_err,
            &mut td,
        );
        let (_lport, _asic, status, error) = env.update_last.borrow().clone().unwrap();
        assert_eq!(status, SFP_STATUS_INSERTED);
        assert_eq!(error, "N/A");
        assert!(!task.sfp_error_dict.contains_key(&1), "plug-in clears cached error");
        assert_eq!(env.post_calls.get(), 1, "plug-in re-reads identity");
        assert_eq!(env.del_calls.get(), 0);

        // --- Physical removal (SFP_STATUS_REMOVED) tears the hardware tables down ---
        // xcvr API invalidated, status set to "0"/"N/A" (STATUS_SW preserved, just
        // updated), and the per-port DOM/hardware rows purged.
        env.del_calls.set(0);
        env.remove_api_calls.set(0);
        task.handle_normal_event(
            &env,
            &[("1".to_string(), SFP_STATUS_REMOVED.to_string())].into_iter().collect(),
            &empty_err,
            &mut td,
        );
        let (lport, _asic, status, error) = env.update_last.borrow().clone().unwrap();
        assert_eq!(lport, "Ethernet0");
        assert_eq!(status, SFP_STATUS_REMOVED);
        assert_eq!(error, "N/A");
        assert_eq!(env.remove_api_calls.get(), 1, "removal invalidates the xcvr API");
        assert_eq!(env.del_calls.get(), 1, "removal purges the per-port tables");
        assert!(!task.sfp_error_dict.contains_key(&1));
    }

    #[test]
    fn test_SfpStateUpdateTask_on_add_logical_port() {
        // Publish identity + SW status for a newly-created logical port, across the
        // present/EEPROM-fail, present/EEPROM-ok, absent, and cached-error cases.
        // Mirrors tests/test_xcvrd.py::test_SfpStateUpdateTask_on_add_logical_port.
        let mut port_mapping = PortMapping::new();
        port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd));
        let mut task = SfpStateUpdateTask::new(port_mapping);
        let env = MockSfpEnv::default();
        let ev = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd);

        // present + EEPROM read failure -> INSERTED status, retry set, no DOM/VDM/media.
        env.present.set(true);
        env.post_rc.set(SFP_EEPROM_NOT_READY);
        task.on_add_logical_port(&env, &ev);
        assert_eq!(env.update_calls.get(), 1);
        let last = env.update_last.borrow().clone().unwrap();
        assert_eq!(last.2, SFP_STATUS_INSERTED);
        assert_eq!(last.3, "N/A");
        assert_eq!(env.post_calls.get(), 1);
        assert_eq!(env.dom_calls.get(), 0);
        assert_eq!(env.vdm_calls.get(), 0);
        assert_eq!(env.media_calls.get(), 0);
        assert!(task.retry_eeprom_set.contains("Ethernet0"));
        task.retry_eeprom_set.clear();

        // present + EEPROM read success -> DOM/VDM/media published.
        env.post_rc.set(0);
        env.update_calls.set(0);
        env.post_calls.set(0);
        task.on_add_logical_port(&env, &ev);
        assert_eq!(env.update_calls.get(), 1);
        assert_eq!(env.post_calls.get(), 1);
        assert_eq!(env.dom_calls.get(), 1);
        assert_eq!(env.vdm_calls.get(), 1);
        assert_eq!(env.media_calls.get(), 1);
        assert!(!task.retry_eeprom_set.contains("Ethernet0"));

        // absent -> REMOVED status.
        env.present.set(false);
        env.update_calls.set(0);
        task.on_add_logical_port(&env, &ev);
        assert_eq!(env.update_calls.get(), 1);
        assert_eq!(env.update_last.borrow().clone().unwrap().2, SFP_STATUS_REMOVED);

        // absent + cached error -> error value + joined generic descriptions.
        let error = sfp_status_helper::SFP_ERROR_BIT_BLOCKING | sfp_status_helper::SFP_ERROR_BIT_POWER_BUDGET_EXCEEDED;
        task.sfp_error_dict.insert(1, (error.to_string(), BTreeMap::new()));
        env.update_calls.set(0);
        task.on_add_logical_port(&env, &ev);
        assert_eq!(env.update_calls.get(), 1);
        let last = env.update_last.borrow().clone().unwrap();
        assert_eq!(last.2, error.to_string());
        assert_eq!(last.3, "Blocking EEPROM from being read|Power budget exceeded");
    }

    #[test]
    fn test_sfp_insert_events() {
        // Insert events are soaked until MGMT_INIT_TIME_DELAY_SECS elapses, then
        // re-emitted. Mirrors tests/test_xcvrd.py::test_sfp_insert_events.
        let mut sfp_insert_events: BTreeMap<String, Instant> = BTreeMap::new();
        let mut port_dict: BTreeMap<String, String> = BTreeMap::new();
        for i in 1..=5 {
            port_dict.insert(i.to_string(), SFP_STATUS_INSERTED.to_string());
        }
        let start = Instant::now();
        // Before the delay elapses: events are held (port_dict emptied).
        wrapper_soak_sfp_insert_event(&mut sfp_insert_events, &mut port_dict, start);
        assert!(port_dict.is_empty());
        assert_eq!(sfp_insert_events.len(), 5);
        // After the delay elapses: events are re-emitted into port_dict.
        let later = start + Duration::from_secs(MGMT_INIT_TIME_DELAY_SECS + 1);
        wrapper_soak_sfp_insert_event(&mut sfp_insert_events, &mut port_dict, later);
        assert_eq!(port_dict.len(), 5);
        for i in 1..=5 {
            assert_eq!(port_dict.get(&i.to_string()).map(String::as_str), Some(SFP_STATUS_INSERTED));
        }
        assert!(sfp_insert_events.is_empty());
    }

    #[test]
    fn test_sfp_remove_events() {
        // A removal cancels a pending soaked insert. Mirrors
        // tests/test_xcvrd.py::test_sfp_remove_events.
        let mut sfp_insert_events: BTreeMap<String, Instant> = BTreeMap::new();
        let now = Instant::now();
        let mut insert: BTreeMap<String, String> = (1..=5).map(|i| (i.to_string(), SFP_STATUS_INSERTED.to_string())).collect();
        wrapper_soak_sfp_insert_event(&mut sfp_insert_events, &mut insert, now);
        assert_eq!(sfp_insert_events.len(), 5);

        let mut removal: BTreeMap<String, String> = (1..=5).map(|i| (i.to_string(), SFP_STATUS_REMOVED.to_string())).collect();
        let expected = removal.clone();
        wrapper_soak_sfp_insert_event(&mut sfp_insert_events, &mut removal, now);
        // Pending inserts are cancelled and the removal dict is unchanged.
        assert!(sfp_insert_events.is_empty());
        assert_eq!(removal, expected);
    }

    #[test]
    fn test_wrapper_is_replaceable() {
        // Mirrors tests/test_xcvrd.py::test_wrapper_is_replaceable.
        let c = MockChassis::with_sfps(vec![MockSfp { replaceable: true, ..Default::default() }]);
        assert!(wrapper_is_replaceable(&c, 0));
        let c = MockChassis::with_sfps(vec![MockSfp { replaceable: false, ..Default::default() }]);
        assert!(!wrapper_is_replaceable(&c, 0));
        // Missing SFP (HAL error) -> false (NotImplementedError fallback).
        assert!(!wrapper_is_replaceable(&c, 9));
    }

    #[test]
    fn test_wrapper_get_transceiver_info() {
        // Present identity -> Some; a not-ready (Null) EEPROM -> None. Adapted from
        // tests/test_xcvrd.py::test_wrapper_get_transceiver_info (typed as Option).
        let c = MockChassis::with_sfps(vec![MockSfp::present_with_info(json!({"model": "EMU-40G-LR4"}))]);
        assert!(wrapper_get_transceiver_info(&c, 0).is_some());
        let c = MockChassis::with_sfps(vec![MockSfp::present_eeprom_not_ready()]);
        assert!(wrapper_get_transceiver_info(&c, 0).is_none());
        // Missing SFP -> None.
        assert!(wrapper_get_transceiver_info(&c, 9).is_none());
    }

    #[test]
    fn test_wrapper_get_transceiver_change_event() {
        // A pushed change event decomposes into (status, sfp, sfp_error). Adapted
        // from tests/test_xcvrd.py::test_wrapper_get_transceiver_change_event.
        let c = MockChassis::with_sfps(vec![]);
        let mut sfp = BTreeMap::new();
        sfp.insert("1".to_string(), SFP_STATUS_INSERTED.to_string());
        let mut sfp_error = BTreeMap::new();
        sfp_error.insert("1".to_string(), "N/A".to_string());
        c.push_change_event(ChangeEvent { status: true, sfp: sfp.clone(), sfp_error: sfp_error.clone() });

        let (status, got_sfp, got_err) = wrapper_get_transceiver_change_event(&c, 0);
        assert!(status);
        assert_eq!(got_sfp, sfp);
        assert_eq!(got_err, sfp_error);
    }

    #[test]
    fn test_wrapper_get_sfp_type() {
        // Mirrors tests/test_xcvrd.py::test_wrapper_get_sfp_type.
        let c = MockChassis::with_sfps(vec![MockSfp { sfp_type: "QSFP".to_string(), ..Default::default() }]);
        assert_eq!(wrapper_get_sfp_type(&c, 0).as_deref(), Some("QSFP"));
        // Missing SFP -> None.
        assert_eq!(wrapper_get_sfp_type(&c, 9), None);
    }

    #[test]
    fn test_wrapper_get_sfp_error_description() {
        // Mirrors tests/test_xcvrd.py::test_wrapper_get_sfp_error_description.
        let c = MockChassis::with_sfps(vec![MockSfp { error_description: Some("N/A".to_string()), ..Default::default() }]);
        assert_eq!(wrapper_get_sfp_error_description(&c, 0).as_deref(), Some("N/A"));
        // Missing SFP -> None.
        assert_eq!(wrapper_get_sfp_error_description(&c, 9), None);
    }

    // Port of test_wrapper_is_flat_memory: api.is_flat_memory()==True -> Some(true); a
    // raising get_sfp -> None (falsy).
    #[test]
    fn test_wrapper_is_flat_memory() {
        let mut flat = MockSfp::present();
        flat.set_json_call("is_flat_memory", json!(true));
        let c = MockChassis::with_sfps(vec![flat]);
        assert_eq!(common::wrapper_is_flat_memory_api(&c, 0), Some(true));
        // No SFP at index 9 (get_sfp raises) -> None.
        assert_eq!(common::wrapper_is_flat_memory_api(&c, 9), None);

        // api.is_flat_memory()==False -> Some(false).
        let mut paged = MockSfp::present();
        paged.set_json_call("is_flat_memory", json!(false));
        let c = MockChassis::with_sfps(vec![paged]);
        assert_eq!(common::wrapper_is_flat_memory_api(&c, 0), Some(false));
    }

    // Port of test_wrapper_is_flat_memory_no_xcvr_api: get_xcvr_api()==None -> True.
    #[test]
    fn test_wrapper_is_flat_memory_no_xcvr_api() {
        // No scripted is_flat_memory -> call_json returns JSON null -> "no api" -> Some(true).
        let c = MockChassis::with_sfps(vec![MockSfp::present()]);
        assert_eq!(common::wrapper_is_flat_memory_api(&c, 0), Some(true));
    }

    // Port of test_wrapper_get_transceiver_pm: get_transceiver_pm truthy/falsy propagates;
    // a raising get_sfp yields an empty object.
    #[test]
    fn test_wrapper_get_transceiver_pm() {
        let mut has_pm = MockSfp::present();
        has_pm.set_json_call("get_transceiver_pm", json!(true));
        let c = MockChassis::with_sfps(vec![has_pm]);
        assert_eq!(common::wrapper_get_transceiver_pm(&c, 0), json!(true));

        let mut no_pm = MockSfp::present();
        no_pm.set_json_call("get_transceiver_pm", json!(false));
        let c = MockChassis::with_sfps(vec![no_pm]);
        assert_eq!(common::wrapper_get_transceiver_pm(&c, 0), json!(false));

        // No SFP at index 9 (get_sfp raises) -> {}.
        assert_eq!(common::wrapper_get_transceiver_pm(&c, 9), json!({}));
    }

    /// A recording [`DeinitTeardown`]: captures which table-sets were torn down per port so
    /// the warm/fast-reboot gate can be asserted (the Rust analogue of inspecting the Python
    /// `mock_del_port_sfp_dom_info_from_db.call_args_list`).
    #[derive(Default)]
    struct RecordingTeardown {
        hw: RefCell<Vec<(String, i32)>>,
        status: RefCell<Vec<(String, i32)>>,
    }
    impl DeinitTeardown for RecordingTeardown {
        fn del_hw_tables(&self, lport: &str, asic: i32) {
            self.hw.borrow_mut().push((lport.to_string(), asic));
        }
        fn del_status_tables(&self, lport: &str, asic: i32) {
            self.status.borrow_mut().push((lport.to_string(), asic));
        }
    }

    #[test]
    fn test_DaemonXcvrd_init_deinit_fastboot_enabled() {
        // Fast reboot enabled → deinit must delete the HW/DOM tables but NOT the
        // TRANSCEIVER_STATUS/TRANSCEIVER_STATUS_SW pair, so the live datapath survives.
        // Mirrors tests/test_xcvrd.py::test_DaemonXcvrd_init_deinit_fastboot_enabled.
        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        let teardown = RecordingTeardown::default();

        daemon.deinit_tables(&pm, &MockRebootDb::fast_reboot(true), &teardown);

        assert_eq!(teardown.hw.borrow().as_slice(), &[("Ethernet0".to_string(), 0)]);
        assert!(teardown.status.borrow().is_empty(), "status pair must NOT be deleted on fast reboot");
    }

    #[test]
    fn test_DaemonXcvrd_init_deinit_cold() {
        // Cold (no warm/fast reboot) → deinit deletes BOTH the HW/DOM tables and the
        // TRANSCEIVER_STATUS/TRANSCEIVER_STATUS_SW status pair. Mirrors
        // tests/test_xcvrd.py::test_DaemonXcvrd_init_deinit_cold.
        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet4", 1, 0, PortEventType::PortAdd));
        let teardown = RecordingTeardown::default();

        daemon.deinit_tables(&pm, &MockRebootDb::default(), &teardown);

        assert_eq!(
            teardown.hw.borrow().as_slice(),
            &[("Ethernet0".to_string(), 0), ("Ethernet4".to_string(), 0)]
        );
        assert_eq!(
            teardown.status.borrow().as_slice(),
            &[("Ethernet0".to_string(), 0), ("Ethernet4".to_string(), 0)],
            "status pair MUST be deleted on a cold exit"
        );
    }

    /// NEW (bridge/mock seams): a completed warm-restore alone (no fast reboot) gates the
    /// status-table teardown — `is_syncd_warm_restore_complete` true must preserve the
    /// TRANSCEIVER_STATUS pair exactly like fast reboot does.
    #[test]
    fn warm_restore_complete_gates_deinit() {
        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        let teardown = RecordingTeardown::default();

        // Only WARM_RESTART is set (fast reboot off) — the status pair must still be kept.
        daemon.deinit_tables(&pm, &MockRebootDb::warm(true), &teardown);

        assert_eq!(teardown.hw.borrow().len(), 1);
        assert!(
            teardown.status.borrow().is_empty(),
            "a completed warm restore must preserve the TRANSCEIVER_STATUS pair"
        );
    }

    #[test]
    fn test_DaemonXcvrd_signal_handler() {
        // SIGHUP reloads log levels (observed via the logger being called), SIGINT
        // and SIGTERM request shutdown via the stop event. Mirrors
        // tests/test_xcvrd.py::test_DaemonXcvrd_signal_handler.
        let hup = Rc::new(Cell::new(0u32));
        let mut daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);
        daemon.helper_logger = Box::new(MockLog { calls: hup.clone() });

        daemon.signal_handler(Signal::Hup);
        assert_eq!(hup.get(), 1);
        assert!(!daemon.stop_event.is_set());

        daemon.signal_handler(Signal::Int);
        assert!(daemon.stop_event.is_set());

        daemon.stop_event.clear();
        daemon.signal_handler(Signal::Term);
        assert!(daemon.stop_event.is_set());
    }

    #[test]
    fn test_DaemonXcvrd_update_loggers_log_level() {
        // helper_logger + logger_instance are refreshed once, and each worker that
        // supports update_log_level is called (the one without it is skipped).
        // Mirrors tests/test_xcvrd.py::test_DaemonXcvrd_update_loggers_log_level.
        let helper = Rc::new(Cell::new(0u32));
        let inst = Rc::new(Cell::new(0u32));
        let mut daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);
        daemon.helper_logger = Box::new(MockLog { calls: helper.clone() });
        daemon.logger_instance = Box::new(MockLog { calls: inst.clone() });

        let with_update = Rc::new(Cell::new(0u32));
        daemon
            .threads
            .push(mock_task_with_log(TaskKind::SfpState, Some(with_update.clone())));
        daemon
            .threads
            .push(mock_task_with_log(TaskKind::DomInfo, None)); // no update_log_level

        daemon.update_loggers_log_level();

        assert_eq!(helper.get(), 1);
        assert_eq!(inst.get(), 1);
        assert_eq!(with_update.get(), 1);
    }

    #[test]
    fn test_DaemonXcvrd_update_loggers_log_level_empty_threads() {
        // With no worker threads, helper_logger + logger_instance are still each
        // refreshed exactly once. Mirrors
        // tests/test_xcvrd.py::test_DaemonXcvrd_update_loggers_log_level_empty_threads.
        let helper = Rc::new(Cell::new(0u32));
        let inst = Rc::new(Cell::new(0u32));
        let mut daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);
        daemon.helper_logger = Box::new(MockLog { calls: helper.clone() });
        daemon.logger_instance = Box::new(MockLog { calls: inst.clone() });

        daemon.update_loggers_log_level();

        assert_eq!(helper.get(), 1);
        assert_eq!(inst.get(), 1);
    }

    #[test]
    fn test_sfp_removal_from_dict() {
        // Port of tests/test_xcvrd.py::test_sfp_removal_from_dict, focused on its
        // distinguishing assertion (Python lines 6669-6676): on an `SFP_STATUS_REMOVED`
        // change event the task must (a) push one SW-status update, (b) purge the
        // per-port DOM info once, and (c) invalidate the cached xcvr API exactly once
        // via `remove_xcvr_api` (`mock_sfp.remove_xcvr_api.assert_called_once()`).
        // The earlier SYSTEM_* / insert state-machine transitions of the Python test
        // are already exercised exhaustively by test_SfpStateUpdateTask_task_worker.
        let mut task = SfpStateUpdateTask::new(PortMapping::new());
        task.port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd));
        let env = MockSfpEnv::default();
        let sfp_error_event = Event::new();

        env.set_change_event(true, &[("1", SFP_STATUS_REMOVED)], &[]);
        env.set_map_default(NORMAL_EVENT);
        let mut stop = scripted_stopping(vec![false, true]);
        task.task_worker(&env, &mut stop, &sfp_error_event);

        assert_eq!(env.update_calls.get(), 1);
        assert_eq!(env.del_calls.get(), 1);
        assert_eq!(env.remove_api_calls.get(), 1);
    }

    #[test]
    fn test_DaemonXcvrd_dom_update_interval_parameter() {
        // DaemonXcvrd stores dom_update_interval verbatim (None / 0 / custom).
        // Mirrors tests/test_xcvrd.py::test_DaemonXcvrd_dom_update_interval_parameter.
        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, None);
        assert_eq!(daemon.dom_update_interval, None);

        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, Some(0));
        assert_eq!(daemon.dom_update_interval, Some(0));

        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, Some(120));
        assert_eq!(daemon.dom_update_interval, Some(120));

        let daemon = DaemonXcvrd::new(SYSLOG_IDENTIFIER, false, false, None, Some(1000));
        assert_eq!(daemon.dom_update_interval, Some(1000));
    }

    // ---- unit tests beyond the ported behaviors --------

    #[test]
    fn test_mock_sfp_identity_to_info_row() {
        // A CMIS module (identity contains `cmis_rev`) dumps every identity field
        // verbatim plus an appended `is_replaceable`, exercising the CMIS branch of
        // post_port_sfp_info_to_db against the mock HAL/DB seams.
        let info = json!({
            "cmis_rev": "5.0",
            "model": "EMU-400G-DR4",
            "manufacturer": "ACME",
            "vendor_oui": "00-01-02",
            "serial": "SN123",
            "type": "QSFP-DD Double Density 8X Pluggable Transceiver",
            "application_advertisement": "{1: {...}}"
        });
        let chassis = MockChassis::with_sfps(vec![MockSfp::present_with_info(info)]);
        let mut port_mapping = PortMapping::new();
        port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        let tbl = MockTable::new();
        let mut transceiver_dict: BTreeMap<i32, Option<Value>> = BTreeMap::new();
        let rc = post_port_sfp_info_to_db(&chassis, "Ethernet0", &port_mapping, &tbl, &mut transceiver_dict, &|| false);
        assert_eq!(rc, 0);
        assert_eq!(tbl.field("Ethernet0", "cmis_rev").as_deref(), Some("5.0"));
        assert_eq!(tbl.field("Ethernet0", "model").as_deref(), Some("EMU-400G-DR4"));
        assert_eq!(tbl.field("Ethernet0", "vendor_oui").as_deref(), Some("00-01-02"));
        // is_replaceable is appended for CMIS modules too (mock reports replaceable).
        assert_eq!(tbl.field("Ethernet0", "is_replaceable").as_deref(), Some("True"));
    }

    #[test]
    fn test_stringify_nul_trim_and_pybool_rendering() {
        // py_str mirrors Python `str(value)`: trailing CMIS NUL padding is trimmed
        // but trailing ASCII SPACES are PRESERVED (Python's str() keeps them),
        // bools render True/False, numbers plainly.
        assert_eq!(py_str(&Value::String("ACME\0\0\0".to_string())), "ACME");
        // Trailing spaces survive (only NULs are trimmed) — the vendor_date golden
        // contract: `str("2024-12-14 ")` keeps its trailing space verbatim.
        assert_eq!(py_str(&Value::String("2024-12-14 ".to_string())), "2024-12-14 ");
        assert_eq!(py_str(&Value::String("EMU-40G-LR4   ".to_string())), "EMU-40G-LR4   ");
        // NUL padding is trimmed; a space sitting before the NUL padding survives.
        assert_eq!(py_str(&Value::String("trailing \0".to_string())), "trailing ");
        assert_eq!(py_str(&Value::Bool(true)), "True");
        assert_eq!(py_str(&Value::Bool(false)), "False");
        assert_eq!(py_str(&json!(100000)), "100000");
        assert_eq!(py_str(&Value::Null), "None");
    }

    #[test]
    fn test_status_sw_init_ready_transitions() {
        // TRANSCEIVER_STATUS_SW.status tracks presence across insert/remove/re-plug,
        // the observable liveness contract of _init_port_sfp_status_sw_tbl + the
        // NORMAL_EVENT insert/remove handlers (via update_status_sw).
        let mut port_mapping = PortMapping::new();
        port_mapping.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        let mut task = SfpStateUpdateTask::new(port_mapping);
        let env = MockSfpEnv::default();

        // Absent at init -> status marked removed (0).
        env.present.set(false);
        task.init_port_sfp_status_sw_tbl(&env);
        assert_eq!(env.update_last.borrow().clone().unwrap().2, SFP_STATUS_REMOVED);

        // Present at init -> status marked inserted (1).
        env.present.set(true);
        task.init_port_sfp_status_sw_tbl(&env);
        assert_eq!(env.update_last.borrow().clone().unwrap().2, SFP_STATUS_INSERTED);

        // Insert event publishes status=1; remove event restores status=0.
        env.post_rc.set(0);
        let mut td: BTreeMap<i32, Option<Value>> = BTreeMap::new();
        let mut inserted: BTreeMap<String, String> = BTreeMap::new();
        inserted.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        task.handle_normal_event(&env, &inserted, &BTreeMap::new(), &mut td);
        assert_eq!(env.update_last.borrow().clone().unwrap().2, SFP_STATUS_INSERTED);

        let mut removed: BTreeMap<String, String> = BTreeMap::new();
        removed.insert("0".to_string(), SFP_STATUS_REMOVED.to_string());
        task.handle_normal_event(&env, &removed, &BTreeMap::new(), &mut td);
        assert_eq!(env.update_last.borrow().clone().unwrap().2, SFP_STATUS_REMOVED);
    }

}
