//! `SfpStateUpdateTask` — port of the core loop in `xcvrd.py` (`:259-837`).
//!
//! Presence/hot-plug event loop: seeds `TRANSCEIVER_INFO` + `TRANSCEIVER_STATUS_SW`
//! on boot, then processes `get_change_event` batches:
//! insert -> publish identity (+ DOM/VDM thresholds in M2); remove -> delete rows +
//! `status='0'`; error bitmap -> decode -> `STATUS_SW.error` (M3, blocking drops DOM).
//! Generic over the HAL + STATE_DB seams so it is unit-testable with mocks
//! (Part B, analysis §3.6).

#![allow(dead_code, unused_variables)]

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::dom::utilities::dom_sensor::db_utils::DomDbUtils;
use crate::hal::{Hal, SfpApi};
use crate::statedb::{DbError, StateDb};
use crate::xcvrd_utilities::common::{
    del_port_sfp_dom_info_from_db, update_port_transceiver_status_table_sw, wrapper_get_presence,
    NO_ERROR,
};
use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType, PortMapping};
use crate::xcvrd_utilities::sfp_status_helper::{
    fetch_generic_error_description, has_vendor_specific_error, is_error_block_eeprom_reading,
    SFP_STATUS_INSERTED, SFP_STATUS_REMOVED,
};
use crate::xcvrd_utilities::xcvr_table_helper::{
    TRANSCEIVER_DOM_SENSOR_TABLE, TRANSCEIVER_DOM_THRESHOLD_TABLE, TRANSCEIVER_FIRMWARE_INFO_TABLE,
    TRANSCEIVER_INFO_TABLE, TRANSCEIVER_PM_TABLE, TRANSCEIVER_STATUS_SW_TABLE,
    TRANSCEIVER_STATUS_TABLE,
};

use super::{post_port_sfp_info_to_db, PostSfpInfoResult};

/// `RETRY_EEPROM_READING_INTERVAL` (`xcvrd.py:260`) — seconds between EEPROM retries.
const RETRY_EEPROM_READING_INTERVAL: u64 = 60;

/// `MGMT_INIT_TIME_DELAY_SECS` (`xcvrd.py:58`): an SFP insert event is soaked this
/// long (letting module management init settle) before it is processed.
const MGMT_INIT_TIME_DELAY_SECS: u64 = 2;

/// `SFP_INSERT_EVENT_POLL_PERIOD_MSECS` (`xcvrd.py:61`): change-event poll period
/// while insert events are pending in the soak buffer.
pub const SFP_INSERT_EVENT_POLL_PERIOD_MSECS: u64 = 1000;

// Event/state constants (`xcvrd.py:66-75`).
pub const EVENT_ON_ALL_SFP: &str = "-1";
pub const SYSTEM_NOT_READY: &str = "system_not_ready";
pub const SYSTEM_BECOME_READY: &str = "system_become_ready";
pub const SYSTEM_FAIL: &str = "system_fail";
pub const NORMAL_EVENT: &str = "normal";

/// Outer state-machine phase (`STATE_MACHINE_INIT/NORMAL/EXIT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateMachine {
    Init,
    Normal,
    Exit,
}

/// `_mapping_event_from_change_event` output (`xcvrd.py:284`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingEvent {
    /// A per-port plug/unplug/error batch (`NORMAL_EVENT`).
    Normal,
    /// Timeout with the system healthy (`SYSTEM_BECOME_READY`).
    SystemBecomeReady,
    /// The HAL is not ready yet (`SYSTEM_NOT_READY`).
    SystemNotReady,
    /// The HAL reported a failure (`SYSTEM_FAIL`).
    SystemFail,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn system_event_from_str(s: &str) -> MappingEvent {
    match s {
        SYSTEM_BECOME_READY => MappingEvent::SystemBecomeReady,
        SYSTEM_NOT_READY => MappingEvent::SystemNotReady,
        _ => MappingEvent::SystemFail,
    }
}

/// `SfpStateUpdateTask` (`xcvrd.py:259`).
pub struct SfpStateUpdateTask<H: Hal, D: StateDb> {
    hal: H,
    db: D,
    port_mapping: PortMapping,
    stop_event: Arc<AtomicBool>,
    sfp_error_event: Arc<AtomicBool>,
    /// Logical ports whose EEPROM read failed and must be retried.
    retry_eeprom_set: HashSet<String>,
    /// Unix time (secs) of the last EEPROM retry sweep.
    last_retry_eeprom_time: u64,
    /// Cached SFP error events keyed by physical port index (`xcvrd.py` `sfp_error_dict`):
    /// `(raw code, vendor-specific error dict)`. When a logical port is (re)created
    /// there is no platform API to detect the error, so the last error is replayed by
    /// `on_add_logical_port`.
    sfp_error_dict: BTreeMap<usize, (String, BTreeMap<String, String>)>,
    /// SFP insert events being soaked until management init settles (`xcvrd.py`
    /// `sfp_insert_events`): physical-port change-event key -> the time the insert
    /// was first seen.
    sfp_insert_events: BTreeMap<String, Instant>,
}

impl<H: Hal, D: StateDb> SfpStateUpdateTask<H, D> {
    pub fn new(
        hal: H,
        db: D,
        port_mapping: PortMapping,
        stop_event: Arc<AtomicBool>,
        sfp_error_event: Arc<AtomicBool>,
    ) -> Self {
        Self {
            hal,
            db,
            port_mapping,
            stop_event,
            sfp_error_event,
            retry_eeprom_set: HashSet::new(),
            last_retry_eeprom_time: 0,
            sfp_error_dict: BTreeMap::new(),
            sfp_insert_events: BTreeMap::new(),
        }
    }

    /// `init`: post all present ports' info once and seed `TRANSCEIVER_STATUS_SW`. [M1]
    pub fn init(&mut self) -> Result<(), DbError> {
        self.post_port_sfp_info_and_dom_thr_to_db_once()?;
        self.init_port_sfp_status_sw_tbl()?;
        Ok(())
    }

    /// Thread body: seed once, then run `task_worker` until `stop_event`. [M1]
    pub fn run(&mut self) {
        if let Err(e) = self.init() {
            eprintln!("SfpStateUpdateTask: init error: {e}");
        }
        if let Err(e) = self.task_worker() {
            eprintln!("SfpStateUpdateTask: task_worker error: {e}");
        }
    }

    /// `task_worker` (`xcvrd.py:395`): the change-event loop. For M1 it handles
    /// the `NORMAL_EVENT` (insert/remove) path; the system-ready retry state
    /// machine and error decode arrive in later milestones. [M1/M3/M5]
    pub fn task_worker(&mut self) -> Result<(), DbError> {
        while !self.stop_event.load(Ordering::Relaxed) {
            self.retry_eeprom_reading()?;

            // Poll faster while insert events are soaking so they re-inject on time.
            let timeout = if self.sfp_insert_events.is_empty() {
                1000
            } else {
                SFP_INSERT_EVENT_POLL_PERIOD_MSECS
            };
            let ev = match self.hal.get_change_event(timeout) {
                Ok(e) => e,
                // Transient HAL failure: keep the supervisor happy and retry.
                Err(_) => continue,
            };
            let mut port_dict: BTreeMap<String, String> = ev.sfp.clone();
            let error_dict = ev.sfp_error.clone();
            // Soak SFP insert events across ports (holds inserts MGMT_INIT_TIME_DELAY_SECS
            // then re-injects them), exactly like the Python `_wrapper_soak_sfp_insert_event`.
            if ev.status {
                self.soak_sfp_insert_event(&mut port_dict);
            }
            if port_dict.is_empty() {
                continue;
            }
            let event = self.mapping_event_from_change_event(ev.status, &mut port_dict);
            if event == MappingEvent::Normal {
                self.handle_normal_event(&port_dict, &error_dict)?;
            }
            // SYSTEM_* events drive the retry/exit state machine (M-later).
        }
        Ok(())
    }

    /// `_wrapper_soak_sfp_insert_event` (`xcvrd.py:127`): buffer SFP insert events
    /// for `MGMT_INIT_TIME_DELAY_SECS` (letting module management init complete)
    /// before letting them be processed. Inserts are moved out of `port_dict` into
    /// the soak buffer; a matching removal cancels a pending insert; buffered
    /// inserts older than the delay are re-injected into `port_dict`. [M5]
    pub fn soak_sfp_insert_event(&mut self, port_dict: &mut BTreeMap<String, String>) {
        self.soak_sfp_insert_event_at(port_dict, Instant::now());
    }

    /// `soak_sfp_insert_event` with an injectable clock (deterministic in tests).
    fn soak_sfp_insert_event_at(&mut self, port_dict: &mut BTreeMap<String, String>, now: Instant) {
        let keys: Vec<String> = port_dict.keys().cloned().collect();
        for key in keys {
            match port_dict.get(&key).map(String::as_str) {
                Some(SFP_STATUS_INSERTED) => {
                    self.sfp_insert_events.insert(key.clone(), now);
                    port_dict.remove(&key);
                }
                Some(SFP_STATUS_REMOVED) => {
                    self.sfp_insert_events.remove(&key);
                }
                _ => {}
            }
        }

        let delay = Duration::from_secs(MGMT_INIT_TIME_DELAY_SECS);
        let ready: Vec<String> = self
            .sfp_insert_events
            .iter()
            .filter(|(_, itime)| now.duration_since(**itime) >= delay)
            .map(|(k, _)| k.clone())
            .collect();
        for key in ready {
            port_dict.insert(key.clone(), SFP_STATUS_INSERTED.to_string());
            self.sfp_insert_events.remove(&key);
        }
    }

    /// `_mapping_event_from_change_event` (`xcvrd.py:284`): change-event -> event.
    /// Mutates `port_dict` (adds `EVENT_ON_ALL_SFP`) exactly like the Python.
    pub fn mapping_event_from_change_event(
        &self,
        status: bool,
        port_dict: &mut BTreeMap<String, String>,
    ) -> MappingEvent {
        if status {
            if !port_dict.is_empty() {
                MappingEvent::Normal
            } else {
                port_dict.insert(EVENT_ON_ALL_SFP.to_string(), SYSTEM_BECOME_READY.to_string());
                MappingEvent::SystemBecomeReady
            }
        } else if let Some(v) = port_dict.get(EVENT_ON_ALL_SFP) {
            system_event_from_str(v)
        } else {
            port_dict.insert(EVENT_ON_ALL_SFP.to_string(), SYSTEM_FAIL.to_string());
            MappingEvent::SystemFail
        }
    }

    /// `_post_port_sfp_info_and_dom_thr_to_db_once` (`xcvrd.py:309`): publish every
    /// configured port's identity to `TRANSCEIVER_INFO`, recording ports whose
    /// EEPROM wasn't ready for later retry, then post `TRANSCEIVER_DOM_THRESHOLD`
    /// for the ports that were read successfully. [M1/M2]
    pub fn post_port_sfp_info_and_dom_thr_to_db_once(&mut self) -> Result<(), DbError> {
        let intf_tbl = self.db.table(TRANSCEIVER_INFO_TABLE)?;
        let threshold_tbl = self.db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE)?;
        let logical_ports = self.port_mapping.logical_port_list.clone();
        for logical_port in &logical_ports {
            let phys = match self
                .port_mapping
                .logical_port_name_to_physical_port_list(logical_port)
                .and_then(|l| l.first().copied())
            {
                Some(p) => p,
                None => continue,
            };
            let sfp = match self.hal.sfp(phys) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let rc = post_port_sfp_info_to_db(logical_port, &self.port_mapping, &intf_tbl, &sfp)?;
            if rc == PostSfpInfoResult::EepromNotReady {
                self.retry_eeprom_set.insert(logical_port.clone());
            } else {
                // Read + publish DOM thresholds only for ports whose EEPROM was ready.
                DomDbUtils::post_port_dom_thresholds_to_db(logical_port, &sfp, &threshold_tbl)?;
            }
        }
        Ok(())
    }

    /// `_init_port_sfp_status_sw_tbl` (`xcvrd.py:356`): seed `TRANSCEIVER_STATUS_SW`
    /// `status` (INSERTED/REMOVED) for every configured port. [M1]
    pub fn init_port_sfp_status_sw_tbl(&mut self) -> Result<(), DbError> {
        let status_sw_tbl = self.db.table(TRANSCEIVER_STATUS_SW_TABLE)?;
        let logical_ports = self.port_mapping.logical_port_list.clone();
        for logical_port in &logical_ports {
            let physical_list = match self.port_mapping.logical_port_name_to_physical_port_list(logical_port) {
                Some(l) if !l.is_empty() => l,
                _ => {
                    // No physical port -> mark REMOVED and move on.
                    update_port_transceiver_status_table_sw(
                        logical_port,
                        &status_sw_tbl,
                        SFP_STATUS_REMOVED,
                        NO_ERROR,
                    )?;
                    continue;
                }
            };
            for phys in physical_list {
                let present = match self.hal.sfp(phys) {
                    Ok(sfp) => wrapper_get_presence(&sfp),
                    Err(_) => false,
                };
                let status = if present { SFP_STATUS_INSERTED } else { SFP_STATUS_REMOVED };
                update_port_transceiver_status_table_sw(logical_port, &status_sw_tbl, status, NO_ERROR)?;
            }
        }
        Ok(())
    }

    /// Handle a `NORMAL_EVENT` batch: per physical port, publish identity + SW
    /// status on insert, and delete `TRANSCEIVER_INFO` + `status='0'` on removal.
    /// (Error-bitmap decode and DOM/VDM table clearing are M3.) [M1]
    fn handle_normal_event(
        &mut self,
        port_dict: &BTreeMap<String, String>,
        error_dict: &BTreeMap<String, String>,
    ) -> Result<(), DbError> {
        let status_sw_tbl = self.db.table(TRANSCEIVER_STATUS_SW_TABLE)?;
        let intf_tbl = self.db.table(TRANSCEIVER_INFO_TABLE)?;
        for (key, value) in port_dict {
            let phys: usize = match key.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            // Cache/clear the SFP error for this physical port: an error event is
            // remembered (there is no platform API to re-detect it when a logical
            // port is later (re)created), while an insert/remove clears it. (xcvrd.py:536)
            if value != SFP_STATUS_INSERTED && value != SFP_STATUS_REMOVED {
                self.sfp_error_dict.insert(phys, (value.clone(), error_dict.clone()));
            } else {
                self.sfp_error_dict.remove(&phys);
            }
            let logical_ports = match self.port_mapping.get_physical_to_logical(phys) {
                Some(l) => l,
                None => continue,
            };
            for logical_port in logical_ports {
                if value == SFP_STATUS_INSERTED {
                    // A plug-in event clears the error state.
                    update_port_transceiver_status_table_sw(
                        &logical_port,
                        &status_sw_tbl,
                        SFP_STATUS_INSERTED,
                        NO_ERROR,
                    )?;
                    let sfp = match self.hal.sfp(phys) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let mut rc =
                        post_port_sfp_info_to_db(&logical_port, &self.port_mapping, &intf_tbl, &sfp)?;
                    if rc == PostSfpInfoResult::EepromNotReady {
                        // Python sleeps TIME_FOR_SFP_READY_SECS then retries once.
                        rc = post_port_sfp_info_to_db(
                            &logical_port,
                            &self.port_mapping,
                            &intf_tbl,
                            &sfp,
                        )?;
                        if rc == PostSfpInfoResult::EepromNotReady {
                            self.retry_eeprom_set.insert(logical_port.clone());
                        }
                    }
                    // Publish DOM thresholds once the identity read succeeded. [M2]
                    if rc != PostSfpInfoResult::EepromNotReady {
                        let threshold_tbl = self.db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE)?;
                        DomDbUtils::post_port_dom_thresholds_to_db(
                            &logical_port,
                            &sfp,
                            &threshold_tbl,
                        )?;
                    }
                } else if value == SFP_STATUS_REMOVED {
                    update_port_transceiver_status_table_sw(
                        &logical_port,
                        &status_sw_tbl,
                        SFP_STATUS_REMOVED,
                        NO_ERROR,
                    )?;
                    // Clear the port's identity + DOM rows (Python
                    // `del_port_sfp_dom_info_from_db` across INFO/DOM/THRESHOLD;
                    // STATUS/VDM/PM/FW join in M3). [M1/M2]
                    let dom_tbl = self.db.table(TRANSCEIVER_DOM_SENSOR_TABLE)?;
                    let threshold_tbl = self.db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE)?;
                    del_port_sfp_dom_info_from_db(
                        &logical_port,
                        &[&intf_tbl, &dom_tbl, &threshold_tbl],
                    )?;
                } else {
                    // SFP error bitmap (value is neither '1' nor '0'): decode ->
                    // STATUS_SW.error; the blocking bit removes the (now stale) DOM
                    // rows while the static TRANSCEIVER_INFO is kept. (xcvrd.py:610)
                    self.handle_sfp_error_event(
                        &logical_port,
                        phys,
                        key,
                        value,
                        error_dict,
                        &status_sw_tbl,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Decode an SFP error bitmap and reflect it in `TRANSCEIVER_STATUS_SW`
    /// (`xcvrd.py:610-646`): `error='|'.join(descriptions)` (plus any vendor-specific
    /// text), `status=<raw code>`. A blocking bit means the EEPROM is unreadable, so
    /// the (now out-of-date) DOM rows are dropped while the static `TRANSCEIVER_INFO`
    /// is retained. A later plug-in event ('1') clears the error and repopulates. [M3]
    fn handle_sfp_error_event(
        &self,
        logical_port: &str,
        phys: usize,
        key: &str,
        value: &str,
        error_dict: &BTreeMap<String, String>,
        status_sw_tbl: &D::Table,
    ) -> Result<(), DbError> {
        let error_bits: u32 = match value.parse() {
            Ok(b) => b,
            // Unparseable event code: nothing to decode. Keep the supervisor
            // RUNNING rather than erroring on a single malformed event.
            Err(_) => return Ok(()),
        };
        let mut error_descriptions = fetch_generic_error_description(error_bits);
        if has_vendor_specific_error(error_bits) {
            // Prefer the vendor text carried alongside the event; otherwise ask the
            // SFP (`_wrapper_get_sfp_error_description`).
            let vendor = match error_dict.get(key) {
                Some(v) => Some(v.clone()),
                None => self
                    .hal
                    .sfp(phys)
                    .ok()
                    .and_then(|s| s.get_error_description().ok().flatten()),
            };
            if let Some(v) = vendor {
                error_descriptions.push(v);
            }
        }
        // Any existing error is replaced by the new one; status = the raw bitmap code.
        update_port_transceiver_status_table_sw(
            logical_port,
            status_sw_tbl,
            value,
            &error_descriptions.join("|"),
        )?;
        if is_error_block_eeprom_reading(error_bits) {
            let dom_tbl = self.db.table(TRANSCEIVER_DOM_SENSOR_TABLE)?;
            let threshold_tbl = self.db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE)?;
            del_port_sfp_dom_info_from_db(logical_port, &[&dom_tbl, &threshold_tbl])?;
        }
        Ok(())
    }

    /// `on_port_config_change` (`xcvrd.py:723`): apply a runtime CONFIG_DB `PORT`
    /// add/remove. On remove the port's DB rows are cleared *before* it leaves the
    /// mapping; on add the mapping is updated *before* the port's identity is
    /// published — matching the Python ordering. [M5]
    pub fn on_port_config_change(&mut self, event: &PortChangeEvent) -> Result<(), DbError> {
        match event.event_type {
            PortChangeEventType::Remove => {
                self.on_remove_logical_port(event)?;
                self.port_mapping.handle_port_change_event(event);
            }
            PortChangeEventType::Add => {
                self.port_mapping.handle_port_change_event(event);
                self.on_add_logical_port(event)?;
            }
            // Raw Set/Del ops are consumed by PortChangeObserver, not here.
            PortChangeEventType::Set | PortChangeEventType::Del => {}
        }
        Ok(())
    }

    /// `on_add_logical_port` (`xcvrd.py:770`): a PORT was added at runtime. Query the
    /// module (replaying any cached SFP error) and seed the port's STATE_DB rows:
    /// present + readable -> publish `TRANSCEIVER_INFO` + DOM thresholds and set
    /// `status='1'`; a blocking error or an absent module just seeds
    /// `TRANSCEIVER_STATUS_SW`. [M5]
    pub fn on_add_logical_port(&mut self, event: &PortChangeEvent) -> Result<(), DbError> {
        let status_sw_tbl = self.db.table(TRANSCEIVER_STATUS_SW_TABLE)?;
        let intf_tbl = self.db.table(TRANSCEIVER_INFO_TABLE)?;
        let port_name = &event.port_name;
        let phys = event.port_index as usize;

        let mut error_description = NO_ERROR.to_string();
        let mut status: Option<String> = None;
        let mut read_eeprom = true;

        // Replay a cached SFP error for this physical port, if any.
        if let Some((value, err_dict)) = self.sfp_error_dict.get(&phys).cloned() {
            status = Some(value.clone());
            if let Ok(error_bits) = value.parse::<u32>() {
                let mut descriptions = fetch_generic_error_description(error_bits);
                if has_vendor_specific_error(error_bits) {
                    let vendor = match err_dict.get(&phys.to_string()) {
                        Some(v) => Some(v.clone()),
                        None => self
                            .hal
                            .sfp(phys)
                            .ok()
                            .and_then(|s| s.get_error_description().ok().flatten()),
                    };
                    if let Some(v) = vendor {
                        descriptions.push(v);
                    }
                }
                error_description = descriptions.join("|");
                if is_error_block_eeprom_reading(error_bits) {
                    read_eeprom = false;
                }
            }
        }

        let present = self.hal.sfp(phys).map(|s| wrapper_get_presence(&s)).unwrap_or(false);
        if present && read_eeprom {
            if status.is_none() {
                status = Some(SFP_STATUS_INSERTED.to_string());
            }
            if let Ok(sfp) = self.hal.sfp(phys) {
                let rc = post_port_sfp_info_to_db(port_name, &self.port_mapping, &intf_tbl, &sfp)?;
                if rc == PostSfpInfoResult::EepromNotReady {
                    // Failed to read EEPROM -> retry later.
                    self.retry_eeprom_set.insert(port_name.clone());
                } else {
                    let threshold_tbl = self.db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE)?;
                    DomDbUtils::post_port_dom_thresholds_to_db(port_name, &sfp, &threshold_tbl)?;
                }
            }
        } else if status.is_none() {
            status = Some(SFP_STATUS_REMOVED.to_string());
        }

        update_port_transceiver_status_table_sw(
            port_name,
            &status_sw_tbl,
            status.as_deref().unwrap_or(SFP_STATUS_REMOVED),
            &error_description,
        )?;
        Ok(())
    }

    /// `on_remove_logical_port` (`xcvrd.py:731`): a PORT was removed. Delete the
    /// port's rows from every `TRANSCEIVER_*` table (avoiding a race with the DOM
    /// task, which also clears DOM) and drop any pending EEPROM retry. [M5]
    pub fn on_remove_logical_port(&mut self, event: &PortChangeEvent) -> Result<(), DbError> {
        let port_name = &event.port_name;
        let intf_tbl = self.db.table(TRANSCEIVER_INFO_TABLE)?;
        let dom_tbl = self.db.table(TRANSCEIVER_DOM_SENSOR_TABLE)?;
        let threshold_tbl = self.db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE)?;
        let status_tbl = self.db.table(TRANSCEIVER_STATUS_TABLE)?;
        let status_sw_tbl = self.db.table(TRANSCEIVER_STATUS_SW_TABLE)?;
        let pm_tbl = self.db.table(TRANSCEIVER_PM_TABLE)?;
        let fw_tbl = self.db.table(TRANSCEIVER_FIRMWARE_INFO_TABLE)?;
        del_port_sfp_dom_info_from_db(
            port_name,
            &[
                &intf_tbl,
                &dom_tbl,
                &threshold_tbl,
                &status_tbl,
                &status_sw_tbl,
                &pm_tbl,
                &fw_tbl,
            ],
        )?;

        // The logical port is gone -> no need to retry its EEPROM.
        self.retry_eeprom_set.remove(port_name);
        Ok(())
    }

    /// `retry_eeprom_reading` (`xcvrd.py:837`): retry ports that returned
    /// `SFP_EEPROM_NOT_READY`, throttled to `RETRY_EEPROM_READING_INTERVAL`. [M1]
    pub fn retry_eeprom_reading(&mut self) -> Result<(), DbError> {
        if self.retry_eeprom_set.is_empty() {
            return Ok(());
        }
        let now = now_secs();
        if now.saturating_sub(self.last_retry_eeprom_time) < RETRY_EEPROM_READING_INTERVAL {
            return Ok(());
        }
        self.last_retry_eeprom_time = now;

        let intf_tbl = self.db.table(TRANSCEIVER_INFO_TABLE)?;
        let mut success = Vec::new();
        for logical_port in self.retry_eeprom_set.iter() {
            let phys = match self
                .port_mapping
                .logical_port_name_to_physical_port_list(logical_port)
                .and_then(|l| l.first().copied())
            {
                Some(p) => p,
                None => continue,
            };
            let sfp = match self.hal.sfp(phys) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let rc = post_port_sfp_info_to_db(logical_port, &self.port_mapping, &intf_tbl, &sfp)?;
            if rc != PostSfpInfoResult::EepromNotReady {
                success.push(logical_port.clone());
            }
        }
        for s in success {
            self.retry_eeprom_set.remove(&s);
        }
        Ok(())
    }
}

// --------------------------------------------------------------------------
// Part-B unit tests (mirror tests/test_xcvrd.py, retargeted onto the mock seams).
// --------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockHal, MockSfp, MockStateDb};
    use crate::statedb::{Row, TableApi};
    use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType};
    use serde_json::json;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn cmis_info() -> serde_json::Value {
        json!({"cmis_rev": "5.0", "model": "EMU-40G-LR4", "host_lane_count": 8})
    }

    fn mapping_with(port: &str, phys: usize) -> PortMapping {
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new(port, phys, 0, PortChangeEventType::Add));
        pm
    }

    fn new_task(
        hal: MockHal,
        db: MockStateDb,
        pm: PortMapping,
    ) -> SfpStateUpdateTask<MockHal, MockStateDb> {
        SfpStateUpdateTask::new(
            hal,
            db,
            pm,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// <- test_init_port_sfp_status_sw_tbl: present port seeded status='1'.
    #[test]
    fn status_sw_seed_present_port() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![MockSfp::default(), MockSfp::present(cmis_info())]);
        let pm = mapping_with("Ethernet0", 1); // physical index 1 is present
        let mut task = new_task(hal, db.clone(), pm);

        task.init_port_sfp_status_sw_tbl().unwrap();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("1"));
        assert_eq!(sw.hget("Ethernet0", "error").unwrap().as_deref(), Some("N/A"));
    }

    /// <- test_init_port_sfp_status_sw_tbl_no_physical_port_found: a listed port
    /// with no physical mapping is marked REMOVED (status='0').
    #[test]
    fn status_sw_seed_no_physical_port() {
        let db = MockStateDb::new();
        let hal = MockHal::with_ports(1);
        // Craft an inconsistent mapping: listed but no logical->physical entry.
        let mut pm = PortMapping::new();
        pm.logical_port_list.push("Ethernet0".to_string());
        let mut task = new_task(hal, db.clone(), pm);

        task.init_port_sfp_status_sw_tbl().unwrap();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("0"));
    }

    /// <- test_SfpStateUpdateTask_mapping_event_from_change_event.
    #[test]
    fn mapping_event_from_change_event_cases() {
        let task = new_task(MockHal::with_ports(1), MockStateDb::new(), PortMapping::new());

        let mut pd = BTreeMap::new();
        assert_eq!(task.mapping_event_from_change_event(false, &mut pd), MappingEvent::SystemFail);
        assert_eq!(pd.get(EVENT_ON_ALL_SFP).map(String::as_str), Some(SYSTEM_FAIL));

        let mut pd = BTreeMap::new();
        pd.insert(EVENT_ON_ALL_SFP.to_string(), SYSTEM_FAIL.to_string());
        assert_eq!(task.mapping_event_from_change_event(false, &mut pd), MappingEvent::SystemFail);

        let mut pd = BTreeMap::new();
        assert_eq!(task.mapping_event_from_change_event(true, &mut pd), MappingEvent::SystemBecomeReady);
        assert_eq!(pd.get(EVENT_ON_ALL_SFP).map(String::as_str), Some(SYSTEM_BECOME_READY));

        let mut pd = BTreeMap::new();
        pd.insert("1".to_string(), SFP_STATUS_INSERTED.to_string());
        assert_eq!(task.mapping_event_from_change_event(true, &mut pd), MappingEvent::Normal);
    }

    /// <- split of test_SfpStateUpdateTask_task_worker: insert publishes INFO +
    /// STATUS_SW.
    #[test]
    fn normal_insert_publishes_info_and_status() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![MockSfp::present(cmis_info())]);
        let pm = mapping_with("Ethernet0", 0);
        let mut task = new_task(hal, db.clone(), pm);

        let mut port_dict = BTreeMap::new();
        port_dict.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        task.handle_normal_event(&port_dict, &BTreeMap::new()).unwrap();

        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        assert_eq!(intf.hget("Ethernet0", "cmis_rev").unwrap().as_deref(), Some("5.0"));
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("1"));
    }

    /// <- split of test_SfpStateUpdateTask_task_worker: remove deletes INFO and
    /// sets status='0'.
    #[test]
    fn normal_remove_deletes_info_and_sets_status_zero() {
        let db = MockStateDb::new();
        db.table(TRANSCEIVER_INFO_TABLE)
            .unwrap()
            .set("Ethernet0", &row(&[("model", "x")]))
            .unwrap();
        let hal = MockHal::new(vec![MockSfp::default()]); // absent
        let pm = mapping_with("Ethernet0", 0);
        let mut task = new_task(hal, db.clone(), pm);

        let mut port_dict = BTreeMap::new();
        port_dict.insert("0".to_string(), SFP_STATUS_REMOVED.to_string());
        task.handle_normal_event(&port_dict, &BTreeMap::new()).unwrap();

        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        assert!(intf.get("Ethernet0").unwrap().is_none());
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("0"));
    }

    /// <- M2: an insert publishes TRANSCEIVER_DOM_THRESHOLD (once identity read
    /// succeeds), alongside INFO + STATUS_SW.
    #[test]
    fn normal_insert_posts_dom_threshold() {
        let db = MockStateDb::new();
        let mut sfp = MockSfp::present(cmis_info());
        sfp.threshold_info = Some(json!({"temphighalarm": "75.0", "templowalarm": "-5.0"}));
        let hal = MockHal::new(vec![sfp]);
        let pm = mapping_with("Ethernet0", 0);
        let mut task = new_task(hal, db.clone(), pm);

        let mut port_dict = BTreeMap::new();
        port_dict.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        task.handle_normal_event(&port_dict, &BTreeMap::new()).unwrap();

        let thr = db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE).unwrap();
        let r = thr.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.get("temphighalarm").map(String::as_str), Some("75.0"));
        assert!(r.contains_key("last_update_time"));
    }

    /// <- M2: a removal clears the port's DOM sensor + threshold rows (in addition
    /// to INFO), matching `del_port_sfp_dom_info_from_db`.
    #[test]
    fn normal_remove_clears_dom_tables() {
        let db = MockStateDb::new();
        db.table(TRANSCEIVER_INFO_TABLE).unwrap().set("Ethernet0", &row(&[("model", "x")])).unwrap();
        db.table(TRANSCEIVER_DOM_SENSOR_TABLE)
            .unwrap()
            .set("Ethernet0", &row(&[("temperature", "30")]))
            .unwrap();
        db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE)
            .unwrap()
            .set("Ethernet0", &row(&[("temphighalarm", "75.0")]))
            .unwrap();
        let hal = MockHal::new(vec![MockSfp::default()]); // absent
        let pm = mapping_with("Ethernet0", 0);
        let mut task = new_task(hal, db.clone(), pm);

        let mut port_dict = BTreeMap::new();
        port_dict.insert("0".to_string(), SFP_STATUS_REMOVED.to_string());
        task.handle_normal_event(&port_dict, &BTreeMap::new()).unwrap();

        assert!(db.table(TRANSCEIVER_INFO_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
        assert!(db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
        assert!(db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
    }

    /// <- M3 (blocking error): an I2C-stuck bitmap sets STATUS_SW.error with the
    /// decoded descriptions and status=<raw code>, drops DOM sensor + threshold,
    /// and keeps the static INFO.
    #[test]
    fn error_blocking_sets_error_and_removes_dom_keeps_info() {
        let db = MockStateDb::new();
        db.table(TRANSCEIVER_INFO_TABLE)
            .unwrap()
            .set("Ethernet0", &row(&[("manufacturer", "xcvr-emu")]))
            .unwrap();
        db.table(TRANSCEIVER_DOM_SENSOR_TABLE)
            .unwrap()
            .set("Ethernet0", &row(&[("temperature", "30")]))
            .unwrap();
        db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE)
            .unwrap()
            .set("Ethernet0", &row(&[("temphighalarm", "75.0")]))
            .unwrap();
        let hal = MockHal::new(vec![MockSfp::present(cmis_info())]);
        let pm = mapping_with("Ethernet0", 0);
        let mut task = new_task(hal, db.clone(), pm);

        // I2C_STUCK_EVENT = 0x01|0x02|0x08 = 11 (blocking).
        let mut port_dict = BTreeMap::new();
        port_dict.insert("0".to_string(), "11".to_string());
        task.handle_normal_event(&port_dict, &BTreeMap::new()).unwrap();

        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        let err = sw.hget("Ethernet0", "error").unwrap().unwrap();
        assert!(err.contains("Blocking EEPROM from being read"));
        assert!(err.contains("Bus stuck (I2C data or clock shorted)"));
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("11"));
        // Blocking -> DOM sensor + threshold removed; INFO kept (static).
        assert!(db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
        assert!(db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
        assert!(db.table(TRANSCEIVER_INFO_TABLE).unwrap().get("Ethernet0").unwrap().is_some());
    }

    /// <- M3 (non-blocking error): a high-temperature bitmap sets the error but
    /// keeps the DOM rows.
    #[test]
    fn error_nonblocking_sets_error_keeps_dom() {
        let db = MockStateDb::new();
        db.table(TRANSCEIVER_DOM_SENSOR_TABLE)
            .unwrap()
            .set("Ethernet0", &row(&[("temperature", "30")]))
            .unwrap();
        let hal = MockHal::new(vec![MockSfp::present(cmis_info())]);
        let pm = mapping_with("Ethernet0", 0);
        let mut task = new_task(hal, db.clone(), pm);

        // HIGH_TEMP_EVENT = 0x01|0x40 = 65 (non-blocking).
        let mut port_dict = BTreeMap::new();
        port_dict.insert("0".to_string(), "65".to_string());
        task.handle_normal_event(&port_dict, &BTreeMap::new()).unwrap();

        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "error").unwrap().as_deref(), Some("High temperature"));
        // Non-blocking -> DOM retained.
        assert!(db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap().get("Ethernet0").unwrap().is_some());
    }

    /// <- M3 (recovery): after a blocking error a plug-in ('1') clears the error to
    /// N/A, re-sets status=1, and republishes INFO + DOM threshold.
    #[test]
    fn error_recovery_clears_error_and_republishes() {
        let db = MockStateDb::new();
        let mut sfp = MockSfp::present(cmis_info());
        sfp.threshold_info = Some(json!({"temphighalarm": "75.0"}));
        let hal = MockHal::new(vec![sfp]);
        let pm = mapping_with("Ethernet0", 0);
        let mut task = new_task(hal, db.clone(), pm);

        // First a blocking error.
        let mut err_dict = BTreeMap::new();
        err_dict.insert("0".to_string(), "11".to_string());
        task.handle_normal_event(&err_dict, &BTreeMap::new()).unwrap();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert!(sw.hget("Ethernet0", "error").unwrap().unwrap().contains("Blocking"));

        // Then recovery (plug-in event).
        let mut ins_dict = BTreeMap::new();
        ins_dict.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        task.handle_normal_event(&ins_dict, &BTreeMap::new()).unwrap();

        assert_eq!(sw.hget("Ethernet0", "error").unwrap().as_deref(), Some("N/A"));
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("1"));
        assert!(db.table(TRANSCEIVER_INFO_TABLE).unwrap().get("Ethernet0").unwrap().is_some());
        assert!(db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE).unwrap().get("Ethernet0").unwrap().is_some());
    }

    /// <- M2: the boot once-pass seeds TRANSCEIVER_DOM_THRESHOLD for present ports.
    #[test]
    fn info_and_dom_thr_once_seeds_threshold() {
        let db = MockStateDb::new();
        let mut sfp = MockSfp::present(cmis_info());
        sfp.threshold_info = Some(json!({"temphighalarm": "75.0"}));
        let hal = MockHal::new(vec![sfp]);
        let pm = mapping_with("Ethernet0", 0);
        let mut task = new_task(hal, db.clone(), pm);

        task.post_port_sfp_info_and_dom_thr_to_db_once().unwrap();

        assert!(db.table(TRANSCEIVER_INFO_TABLE).unwrap().get("Ethernet0").unwrap().is_some());
        let thr = db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE).unwrap();
        assert_eq!(thr.hget("Ethernet0", "temphighalarm").unwrap().as_deref(), Some("75.0"));
    }

    /// <- test_SfpStateUpdateTask_retry_eeprom_reading: throttle + success removal.
    #[test]
    fn retry_eeprom_reading_throttle_and_success() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![MockSfp::default()]);
        let pm = mapping_with("Ethernet0", 0);
        let mut task = new_task(hal, db, pm);

        // Empty retry set -> no-op.
        task.retry_eeprom_reading().unwrap();
        assert!(task.retry_eeprom_set.is_empty());

        // Recently retried -> throttled, port stays queued.
        task.retry_eeprom_set.insert("Ethernet0".to_string());
        task.last_retry_eeprom_time = now_secs();
        task.retry_eeprom_reading().unwrap();
        assert!(task.retry_eeprom_set.contains("Ethernet0"));

        // Present but EEPROM still unreadable -> remains queued.
        task.hal.sfps[0].presence = true;
        task.hal.sfps[0].info = None;
        task.last_retry_eeprom_time = 0;
        task.retry_eeprom_reading().unwrap();
        assert!(task.retry_eeprom_set.contains("Ethernet0"));

        // EEPROM now readable -> removed from the retry set.
        task.hal.sfps[0].info = Some(cmis_info());
        task.last_retry_eeprom_time = 0;
        task.retry_eeprom_reading().unwrap();
        assert!(!task.retry_eeprom_set.contains("Ethernet0"));
    }

    /// <- new-test (Arc<AtomicBool> stop-flag shutdown of the run loop): a helper
    /// thread flips the stop flag; task_worker must then return.
    #[test]
    fn task_worker_stops_on_atomic_flag() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut task = SfpStateUpdateTask::new(
            MockHal::with_ports(1),
            MockStateDb::new(),
            PortMapping::new(),
            stop.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let stop2 = stop.clone();
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            stop2.store(true, Ordering::Relaxed);
        });

        task.task_worker().unwrap(); // exits once the flag is set
        flipper.join().unwrap();
        assert!(stop.load(Ordering::Relaxed));
    }

    // ---- M5: runtime port config change + multiport isolation ----------------

    /// <- test_SfpStateUpdateTask_handle_port_change_event: a PORT_ADD updates the
    /// mapping and seeds the port (no row delete); a PORT_REMOVE clears the port's
    /// rows and drops it from the mapping.
    #[test]
    fn on_port_config_change_add_then_remove() {
        let db = MockStateDb::new();
        let mut sfp = MockSfp::present(cmis_info());
        sfp.threshold_info = Some(json!({"temphighalarm": "75.0"}));
        let hal = MockHal::new(vec![sfp]);
        let mut task = new_task(hal, db.clone(), PortMapping::new());

        // PORT_ADD: mapping gains Ethernet0; present module -> INFO + status=1.
        task.on_port_config_change(&PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::Add))
            .unwrap();
        assert!(task.port_mapping.is_logical_port("Ethernet0"));
        assert_eq!(task.port_mapping.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(task.port_mapping.get_physical_to_logical(0), Some(vec!["Ethernet0".to_string()]));
        assert_eq!(task.port_mapping.get_logical_to_physical("Ethernet0"), Some(vec![0]));
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert!(intf.get("Ethernet0").unwrap().is_some());
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("1"));

        // PORT_REMOVE: rows cleared, mapping emptied.
        task.on_port_config_change(&PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::Remove))
            .unwrap();
        assert!(!task.port_mapping.is_logical_port("Ethernet0"));
        assert!(task.port_mapping.logical_port_list.is_empty());
        assert!(task.port_mapping.logical_to_physical.is_empty());
        assert!(task.port_mapping.physical_to_logical.is_empty());
        assert!(task.port_mapping.logical_to_asic.is_empty());
        assert!(intf.get("Ethernet0").unwrap().is_none());
        assert!(sw.get("Ethernet0").unwrap().is_none());
    }

    /// <- on_add_logical_port: an absent module only seeds `TRANSCEIVER_STATUS_SW`
    /// with status='0' (no INFO).
    #[test]
    fn on_add_logical_port_absent_seeds_status_removed() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![MockSfp::default()]); // absent
        let mut task = new_task(hal, db.clone(), PortMapping::new());
        let ev = PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::Add);
        task.port_mapping.handle_port_change_event(&ev);
        task.on_add_logical_port(&ev).unwrap();

        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("0"));
        assert_eq!(sw.hget("Ethernet0", "error").unwrap().as_deref(), Some("N/A"));
        assert!(db.table(TRANSCEIVER_INFO_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
    }

    /// <- on_add_logical_port: a cached blocking SFP error for the physical port is
    /// replayed into STATUS_SW (status=<raw>, decoded error) and blocks the EEPROM
    /// read (INFO stays absent) even though the module is present.
    #[test]
    fn on_add_logical_port_replays_cached_blocking_error() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![MockSfp::present(cmis_info())]);
        let mut task = new_task(hal, db.clone(), PortMapping::new());
        let ev = PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::Add);
        task.port_mapping.handle_port_change_event(&ev);
        // I2C-stuck blocking bitmap (11) cached for physical port 0.
        task.sfp_error_dict.insert(0, ("11".to_string(), BTreeMap::new()));
        task.on_add_logical_port(&ev).unwrap();

        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("11"));
        assert!(sw
            .hget("Ethernet0", "error")
            .unwrap()
            .unwrap()
            .contains("Blocking EEPROM from being read"));
        // Blocking error -> no EEPROM read -> INFO stays absent.
        assert!(db.table(TRANSCEIVER_INFO_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
    }

    /// <- test_sfp_removal_from_dict (multiport aspect) / new concurrency test: one
    /// change-event batch touching several ports is applied per-port with no
    /// cross-talk — inserts publish INFO+status=1, the removal clears INFO+status=0.
    #[test]
    fn multiport_insert_remove_batch_isolation() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![
            MockSfp::present(json!({"cmis_rev": "5.0", "model": "MOD0"})),
            MockSfp::present(json!({"cmis_rev": "5.0", "model": "MOD1"})),
            MockSfp::default(), // phys 2 absent (being removed)
        ]);
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::Add));
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet4", 1, 0, PortChangeEventType::Add));
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet8", 2, 0, PortChangeEventType::Add));
        // Pre-seed Ethernet8's INFO so its removal is observable.
        db.table(TRANSCEIVER_INFO_TABLE).unwrap().set("Ethernet8", &row(&[("model", "MOD2")])).unwrap();
        let mut task = new_task(hal, db.clone(), pm);

        let mut port_dict = BTreeMap::new();
        port_dict.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        port_dict.insert("1".to_string(), SFP_STATUS_INSERTED.to_string());
        port_dict.insert("2".to_string(), SFP_STATUS_REMOVED.to_string());
        task.handle_normal_event(&port_dict, &BTreeMap::new()).unwrap();

        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        // Ports 0,1 -> their own INFO + status 1 (distinct, no cross-talk).
        assert_eq!(intf.hget("Ethernet0", "model").unwrap().as_deref(), Some("MOD0"));
        assert_eq!(intf.hget("Ethernet4", "model").unwrap().as_deref(), Some("MOD1"));
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("1"));
        assert_eq!(sw.hget("Ethernet4", "status").unwrap().as_deref(), Some("1"));
        // Port 2 -> INFO cleared, status 0.
        assert!(intf.get("Ethernet8").unwrap().is_none());
        assert_eq!(sw.hget("Ethernet8", "status").unwrap().as_deref(), Some("0"));
    }

    /// <- new concurrency test (partial-unplug isolation): unplugging one module
    /// clears only that port's rows; every other port's INFO/DOM is untouched.
    #[test]
    fn partial_unplug_keeps_other_ports() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![
            MockSfp::present(json!({"cmis_rev": "5.0", "model": "MOD0"})),
            MockSfp::default(), // phys 1 unplugged
            MockSfp::present(json!({"cmis_rev": "5.0", "model": "MOD2"})),
        ]);
        for (lp, model) in [("Ethernet0", "MOD0"), ("Ethernet4", "MOD1"), ("Ethernet8", "MOD2")] {
            db.table(TRANSCEIVER_INFO_TABLE).unwrap().set(lp, &row(&[("model", model)])).unwrap();
            db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap().set(lp, &row(&[("temperature", "30")])).unwrap();
        }
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::Add));
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet4", 1, 0, PortChangeEventType::Add));
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet8", 2, 0, PortChangeEventType::Add));
        let mut task = new_task(hal, db.clone(), pm);

        let mut port_dict = BTreeMap::new();
        port_dict.insert("1".to_string(), SFP_STATUS_REMOVED.to_string());
        task.handle_normal_event(&port_dict, &BTreeMap::new()).unwrap();

        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        let dom = db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap();
        // Only Ethernet4 (phys 1) cleared.
        assert!(intf.get("Ethernet4").unwrap().is_none());
        assert!(dom.get("Ethernet4").unwrap().is_none());
        // Neighbours untouched.
        assert_eq!(intf.hget("Ethernet0", "model").unwrap().as_deref(), Some("MOD0"));
        assert_eq!(intf.hget("Ethernet8", "model").unwrap().as_deref(), Some("MOD2"));
        assert_eq!(dom.hget("Ethernet0", "temperature").unwrap().as_deref(), Some("30"));
        assert_eq!(dom.hget("Ethernet8", "temperature").unwrap().as_deref(), Some("30"));
    }

    /// <- _wrapper_soak_sfp_insert_event: an insert is held out of `port_dict` until
    /// MGMT_INIT_TIME_DELAY_SECS elapses, then re-injected.
    #[test]
    fn soak_holds_insert_then_reinjects_after_delay() {
        let mut task = new_task(MockHal::with_ports(1), MockStateDb::new(), PortMapping::new());
        let base = std::time::Instant::now();

        // Insert -> moved out of port_dict into the soak buffer.
        let mut pd = BTreeMap::new();
        pd.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        task.soak_sfp_insert_event_at(&mut pd, base);
        assert!(pd.is_empty());
        assert!(task.sfp_insert_events.contains_key("0"));

        // Before the delay elapses -> still held.
        task.soak_sfp_insert_event_at(&mut pd, base + std::time::Duration::from_secs(1));
        assert!(pd.is_empty());
        assert!(task.sfp_insert_events.contains_key("0"));

        // After the delay -> re-injected as an insert, buffer cleared.
        task.soak_sfp_insert_event_at(
            &mut pd,
            base + std::time::Duration::from_secs(MGMT_INIT_TIME_DELAY_SECS + 1),
        );
        assert_eq!(pd.get("0").map(String::as_str), Some(SFP_STATUS_INSERTED));
        assert!(!task.sfp_insert_events.contains_key("0"));
    }

    /// <- _wrapper_soak_sfp_insert_event: a removal cancels a pending insert and is
    /// itself left in `port_dict` for normal processing.
    #[test]
    fn soak_removed_event_cancels_pending_insert() {
        let mut task = new_task(MockHal::with_ports(1), MockStateDb::new(), PortMapping::new());
        task.sfp_insert_events.insert("0".to_string(), std::time::Instant::now());

        let mut pd = BTreeMap::new();
        pd.insert("0".to_string(), SFP_STATUS_REMOVED.to_string());
        task.soak_sfp_insert_event_at(&mut pd, std::time::Instant::now());

        // Removal stays (processed as a normal remove) and the pending insert is dropped.
        assert_eq!(pd.get("0").map(String::as_str), Some(SFP_STATUS_REMOVED));
        assert!(!task.sfp_insert_events.contains_key("0"));
    }

    /// <- _wrapper_soak_sfp_insert_event: error events are neither soaked nor
    /// dropped — they pass straight through for decode.
    #[test]
    fn soak_passes_through_error_events() {
        let mut task = new_task(MockHal::with_ports(1), MockStateDb::new(), PortMapping::new());
        let mut pd = BTreeMap::new();
        pd.insert("0".to_string(), "11".to_string()); // blocking error bitmap
        task.soak_sfp_insert_event_at(&mut pd, std::time::Instant::now());
        assert_eq!(pd.get("0").map(String::as_str), Some("11"));
        assert!(task.sfp_insert_events.is_empty());
    }
}
