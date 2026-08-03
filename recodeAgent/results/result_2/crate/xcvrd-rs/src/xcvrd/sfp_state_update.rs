//! Port of `xcvrd.py::SfpStateUpdateTask` (`xcvrd.py:259`) - the presence/identity
//! state machine. Reacts to `get_change_event` insert/remove, posts identity at
//! boot, seeds `TRANSCEIVER_STATUS_SW`, and drives the periodic EEPROM-retry flow.
//!
//! M1 realizes: `_mapping_event_from_change_event`, the boot-time identity publish
//! (`_post_port_sfp_info_and_dom_thr_to_db_once`), `_init_port_sfp_status_sw_tbl`,
//! the insert/remove change handling, and `retry_eeprom_reading`. M5 adds the per-type
//! `TRANSCEIVER_VDM_{TYPE}_THRESHOLD` posts at insert (alongside the DOM thresholds)
//! and folds the VDM/PM/firmware tables into the plug-out / blocking-error teardown.
//! Media settings and the full CONFIG_DB logical-port add/remove teardown are later
//! milestones (kept as stubs).
//!
//! The task's HAL/DB dependencies are injected as `&dyn Hal` / `&dyn DbTable` so the
//! Part-B tests run against the mock seams; production wires `BridgeHal` +
//! `RealDbTable` in [`crate::daemon`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::db::DbTable;
use crate::dom::utilities::db::DbCache;
use crate::dom::utilities::dom_sensor::DomDbUtils;
use crate::dom::utilities::vdm::{VdmDbUtils, VdmThresholdTables};
use crate::error::{Result, XcvrdError};
use crate::hal::{ChangeEvent, Hal};
use crate::xcvrd::post_port_sfp_info_to_db;
use crate::xcvrd_utilities::common::{self, CMIS_STATE_INSERTED, CMIS_STATE_READY};
use crate::xcvrd_utilities::port_event_helper::{
    PortChangeEvent, PortChangeEventType, PortMapping,
};
use crate::xcvrd_utilities::sfp_status_helper::{
    fetch_generic_error_description, has_vendor_specific_error, is_error_block_eeprom_reading,
    SFP_STATUS_INSERTED, SFP_STATUS_REMOVED,
};
use crate::xcvrd_utilities::xcvr_table_helper::{
    NPU_SI_SETTINGS_DEFAULT_VALUE, NPU_SI_SETTINGS_SYNC_STATUS_KEY,
};

/// `EVENT_ON_ALL_SFP = '-1'` (`xcvrd.py:66`).
pub const EVENT_ON_ALL_SFP: &str = "-1";
/// `SYSTEM_NOT_READY = 'system_not_ready'`.
pub const SYSTEM_NOT_READY: &str = "system_not_ready";
/// `SYSTEM_BECOME_READY = 'system_become_ready'`.
pub const SYSTEM_BECOME_READY: &str = "system_become_ready";
/// `SYSTEM_FAIL = 'system_fail'`.
pub const SYSTEM_FAIL: &str = "system_fail";
/// `NORMAL_EVENT = 'normal'`.
pub const NORMAL_EVENT: &str = "normal";

/// The event classes `task_worker` derives from a `get_change_event` result
/// (`_mapping_event_from_change_event`, `xcvrd.py:284`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemEvent {
    /// `SYSTEM_NOT_READY`.
    NotReady,
    /// `SYSTEM_BECOME_READY`.
    BecomeReady,
    /// `NORMAL_EVENT` - a real insert/remove/error `port_dict`.
    Normal,
    /// `SYSTEM_FAIL`.
    Fail,
}

impl SystemEvent {
    /// The Python event string this maps to (also what gets stored under
    /// `EVENT_ON_ALL_SFP`).
    pub fn as_str(&self) -> &'static str {
        match self {
            SystemEvent::NotReady => SYSTEM_NOT_READY,
            SystemEvent::BecomeReady => SYSTEM_BECOME_READY,
            SystemEvent::Normal => NORMAL_EVENT,
            SystemEvent::Fail => SYSTEM_FAIL,
        }
    }

    /// Map a stored `EVENT_ON_ALL_SFP` string back to an event (unknown -> Fail, the
    /// Python "just for protection" default).
    fn from_event_str(s: &str) -> SystemEvent {
        if s == SYSTEM_NOT_READY {
            SystemEvent::NotReady
        } else if s == SYSTEM_BECOME_READY {
            SystemEvent::BecomeReady
        } else if s == NORMAL_EVENT {
            SystemEvent::Normal
        } else {
            SystemEvent::Fail
        }
    }
}

/// The HAL + STATE_DB table seams a logical-port lifecycle transition
/// (`on_port_config_change` → `on_add_logical_port` / `on_remove_logical_port`)
/// operates on, bundled so the daemon can wire them once and the Part-B tests can
/// inject mocks. The reference resolves these per `port_change_event.asic_id` from
/// `self.xcvr_table_helper`; on the single-ASIC emulator they are the asic-0 tables.
/// The DOM/status/VDM/PM/firmware tables purged on a logical-port teardown are the
/// same `removal_tables` set the physical plug-out uses (wired on the task), and the
/// STATE_DB `PORT_TABLE` reseed target is the task's `state_port_tbl`.
pub struct LogicalPortCtx<'a> {
    pub hal: &'a dyn Hal,
    pub int_tbl: &'a dyn DbTable,
    pub status_sw_tbl: &'a dyn DbTable,
    pub dom_threshold_tbl: &'a dyn DbTable,
}

/// `SfpStateUpdateTask` (`xcvrd.py:259`).
pub struct SfpStateUpdateTask {
    pub namespaces: Vec<String>,
    pub port_mapping: PortMapping,
    pub skip_cmis_mgr: bool,
    /// Logical ports whose EEPROM identity read failed and must be retried.
    pub retry_eeprom_set: BTreeSet<String>,
    /// Timestamp of the last retry pass (`None` => never, gate open).
    pub last_retry_eeprom_time: Option<Instant>,
    /// Timestamp of the last baseline-recovery scan (`recover_missing_port_baselines`;
    /// `None` => never, gate open). Bounds the STATE_DB read cost of the scan.
    pub last_recover_baseline_time: Option<Instant>,
    /// Grace wait before the one immediate re-read on insert (`TIME_FOR_SFP_READY_SECS`).
    pub sfp_ready_wait: Duration,
    /// The per-port DOM/status STATE_DB tables to purge on plug-out (beyond
    /// `TRANSCEIVER_INFO`): DOM_SENSOR / DOM_TEMPERATURE / DOM_FLAG (+ metadata) /
    /// DOM_THRESHOLD / STATUS / STATUS_FLAG (+ metadata). Wired by the daemon; empty
    /// in unit tests that only assert the INFO teardown. Mirrors the table list
    /// `xcvrd.py` hands to `del_port_sfp_dom_info_from_db` on an SFP-removed event.
    removal_tables: Vec<Arc<dyn DbTable>>,
    /// Per-type VDM threshold value tables published alongside DOM thresholds at
    /// insert (`xcvrd.py:351/831/858`). `None` in unit tests / on a non-VDM build;
    /// wired by the daemon. The DOM loop owns the VDM real-value / flag / PM /
    /// firmware tables — thresholds are the only VDM table posted from this task.
    vdm_threshold_tables: Option<VdmThresholdTables>,
    /// STATE_DB `PORT_TABLE` handle used to (re)seed
    /// `NPU_SI_SETTINGS_SYNC_STATUS = NPU_SI_SETTINGS_DEFAULT` on a physical SFP
    /// plug-out (`xcvrd.py:582-583`) and on a CONFIG_DB logical-port (re)add
    /// (`on_add_logical_port`, `xcvrd.py:788-796`). `None` in unit tests that don't
    /// exercise the NPU-SI reseed; wired by the daemon.
    state_port_tbl: Option<Arc<dyn DbTable>>,
    /// Logical ports currently torn down by a CONFIG_DB logical-port DEL, shared with
    /// the DOM + CMIS worker threads (`Arc<Mutex<..>>`). Those threads iterate their
    /// OWN (boot-time) port-mapping clones, so — unlike the reference, where each task
    /// runs its own `PortChangeObserver` + `on_remove_logical_port` — they would keep
    /// re-publishing `TRANSCEIVER_DOM_SENSOR`/`_STATUS`/`_FIRMWARE_INFO` (DOM) and
    /// re-writing `TRANSCEIVER_STATUS_SW.cmis_state` (CMIS) for a just-removed port,
    /// resurrecting the very rows `on_remove_logical_port` deleted. This set is the
    /// cross-thread signal that a logical port is deconfigured: `on_remove_logical_port`
    /// adds the port (before tearing the tables down), `on_add_logical_port` clears it
    /// (before repopulating), and the DOM/CMIS loops skip (and defensively purge) any
    /// port in it. `None` in unit tests that don't exercise the cross-thread coordination.
    deconfigured_ports: Option<Arc<Mutex<BTreeSet<String>>>>,
    /// `sfp_error_dict` (`xcvrd.py:275`) — SFP error events cached by physical-port
    /// key. A change-event value that is neither INSERTED (`1`) nor REMOVED (`0`) is
    /// an error bitmap; it is stashed here (with the event's vendor `error_dict`) so
    /// a later logical-port (re)creation can re-apply the error — the platform API
    /// can't re-detect it at that point (`on_add_logical_port`, a later milestone).
    pub sfp_error_dict: BTreeMap<String, (String, BTreeMap<String, String>)>,
    /// Per-logical-port count of consecutive baseline-recovery attempts that did NOT
    /// leave the port fully baselined (INFO + DOM_THRESHOLD both present). Bounds the
    /// recovery scan so an un-baselinable present module — e.g. a default/blank emulator
    /// slot that reads `present()` but yields no identity/thresholds — cannot re-drive
    /// the heavy `handle_insert` / threshold re-post every `RECOVER_BASELINE_INTERVAL`
    /// forever. Left unbounded that reposting floods the PyO3 bridge + emulator gRPC and
    /// starves the concurrent ~60s DOM-flag poll, whose supplemental byte-9
    /// `read_eeprom` is the SOLE source of `TRANSCEIVER_DOM_FLAG.vccHAlarm` (the platform
    /// decode omits the supply-voltage flags); a starved read returns `None` so
    /// `vccHAlarm` never publishes and the both-`False` DOM-flag baseline never converges
    /// (`test_dom_flag_groups_temp_and_vcc`). A port that genuinely needs recovery is
    /// baselined within the first attempt(s); the counter is dropped the moment the port
    /// becomes fully baselined or a real change event / logical-port (re)add reprocesses
    /// it, so a later genuine loss is still recovered.
    recover_attempts: BTreeMap<String, u32>,
}

impl SfpStateUpdateTask {
    /// `RETRY_EEPROM_READING_INTERVAL = 60` (`xcvrd.py:260`).
    pub const RETRY_EEPROM_READING_INTERVAL: Duration = Duration::from_secs(60);
    /// `TIME_FOR_SFP_READY_SECS = 1` (`xcvrd.py:64`).
    pub const TIME_FOR_SFP_READY_SECS: Duration = Duration::from_secs(1);
    /// Cadence of the baseline-recovery scan (`recover_missing_port_baselines`) that
    /// re-publishes a present, mapped port whose STATE_DB baseline the emulator's
    /// silent change-event seeding left unpublished. Short enough that a module the
    /// boot one-shot missed is surfaced well within the e2e's observation window,
    /// cheap enough (one STATE_DB read per healthy port) to run every few seconds.
    pub const RECOVER_BASELINE_INTERVAL: Duration = Duration::from_secs(5);
    /// Cap on consecutive un-baselining recovery attempts per logical port (see
    /// [`SfpStateUpdateTask::recover_attempts`]). A present port that genuinely needs its
    /// baseline republished takes it within the first attempt(s); beyond this many the
    /// port is un-baselinable (no identity/thresholds available) and further attempts only
    /// flood the bridge, so recovery backs off until a real change event / (re)add resets
    /// the counter. Small enough to keep bridge load at M6 levels, large enough to absorb
    /// a couple of transient EEPROM-not-ready reads before a healthy module settles.
    pub const MAX_RECOVER_ATTEMPTS: u32 = 3;
    /// Legacy alias retained for name traceability.
    pub const RETRY_PERIOD_SEC: u64 = 60;

    /// `__init__(namespaces, port_mapping, sfp_obj_dict, main_thread_stop_event,
    /// sfp_error_event)`.
    pub fn new(namespaces: Vec<String>, port_mapping: PortMapping, skip_cmis_mgr: bool) -> Self {
        SfpStateUpdateTask {
            namespaces,
            port_mapping,
            skip_cmis_mgr,
            retry_eeprom_set: BTreeSet::new(),
            last_retry_eeprom_time: None,
            last_recover_baseline_time: None,
            sfp_ready_wait: Self::TIME_FOR_SFP_READY_SECS,
            removal_tables: Vec::new(),
            vdm_threshold_tables: None,
            state_port_tbl: None,
            deconfigured_ports: None,
            sfp_error_dict: BTreeMap::new(),
            recover_attempts: BTreeMap::new(),
        }
    }

    /// Wire the extra per-port DOM/status tables to purge on plug-out (see
    /// [`SfpStateUpdateTask::removal_tables`]). The daemon supplies DOM_SENSOR /
    /// DOM_TEMPERATURE / DOM_FLAG (+metadata) / DOM_THRESHOLD / STATUS / STATUS_FLAG
    /// (+metadata) so a re-plugged module re-initializes its flag metadata cleanly.
    pub fn set_removal_tables(&mut self, tables: Vec<Arc<dyn DbTable>>) {
        self.removal_tables = tables;
    }

    /// Wire the per-type VDM threshold tables published at insert (see
    /// [`SfpStateUpdateTask::vdm_threshold_tables`]). No-op path when left unset.
    pub fn set_vdm_threshold_tables(&mut self, tables: VdmThresholdTables) {
        self.vdm_threshold_tables = Some(tables);
    }

    /// Wire the STATE_DB `PORT_TABLE` handle used to (re)seed
    /// `NPU_SI_SETTINGS_SYNC_STATUS` on plug-out / logical-port (re)add (see
    /// [`SfpStateUpdateTask::state_port_tbl`]).
    pub fn set_state_port_table(&mut self, tbl: Arc<dyn DbTable>) {
        self.state_port_tbl = Some(tbl);
    }

    /// Wire the cross-thread deconfigured-logical-port set the DOM + CMIS worker
    /// threads honor (see [`SfpStateUpdateTask::deconfigured_ports`]). No-op path when
    /// left unset (Part-B unit tests).
    pub fn set_deconfigured_ports(&mut self, set: Arc<Mutex<BTreeSet<String>>>) {
        self.deconfigured_ports = Some(set);
    }

    /// Mark a logical port deconfigured (CONFIG_DB DEL) so the DOM/CMIS threads stop
    /// servicing it before its tables are torn down. No-op if the set isn't wired.
    fn mark_deconfigured(&self, logical_port: &str) {
        if let Some(set) = &self.deconfigured_ports {
            set.lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(logical_port.to_string());
        }
    }

    /// Clear a logical port's deconfigured mark (CONFIG_DB re-ADD) so the DOM/CMIS
    /// threads resume servicing it. No-op if the set isn't wired.
    fn clear_deconfigured(&self, logical_port: &str) {
        if let Some(set) = &self.deconfigured_ports {
            set.lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(logical_port);
        }
    }

    /// `_mapping_event_from_change_event` (`xcvrd.py:284`) - classify a
    /// `(status, port_dict)` change-event, mutating `port_dict` exactly as Python
    /// does (seeding `EVENT_ON_ALL_SFP` on the empty/fail paths).
    pub fn mapping_event_from_change_event(
        &self,
        status: bool,
        port_dict: &mut BTreeMap<String, String>,
    ) -> SystemEvent {
        if status {
            if !port_dict.is_empty() {
                SystemEvent::Normal
            } else {
                port_dict.insert(EVENT_ON_ALL_SFP.to_string(), SYSTEM_BECOME_READY.to_string());
                SystemEvent::BecomeReady
            }
        } else if let Some(v) = port_dict.get(EVENT_ON_ALL_SFP) {
            SystemEvent::from_event_str(v)
        } else {
            port_dict.insert(EVENT_ON_ALL_SFP.to_string(), SYSTEM_FAIL.to_string());
            SystemEvent::Fail
        }
    }

    /// `_post_port_sfp_info_and_dom_thr_to_db_once` (`xcvrd.py:309`) - one-shot
    /// identity publish for every present port at boot; ports whose EEPROM read is
    /// not ready are collected into `retry_eeprom_set`. Then, in a second pass
    /// (mirroring Python), every port *not* deferred to retry gets its
    /// `TRANSCEIVER_DOM_THRESHOLD` published, sharing a single per-boot read cache so
    /// a given module's threshold page is fetched at most once. (VDM thresholds +
    /// media settings are later milestones.)
    pub fn post_port_sfp_info_and_dom_thr_to_db_once(
        &mut self,
        hal: &dyn Hal,
        int_tbl: &dyn DbTable,
        dom_threshold_tbl: &dyn DbTable,
    ) -> Result<()> {
        let mut retry = BTreeSet::new();
        for logical_port_name in self.port_mapping.logical_port_list.clone() {
            if self
                .port_mapping
                .get_asic_id_for_logical_port(&logical_port_name)
                .is_none()
            {
                continue;
            }
            match post_port_sfp_info_to_db(&logical_port_name, &self.port_mapping, int_tbl, hal) {
                Ok(()) => {}
                Err(XcvrdError::EepromNotReady) => {
                    retry.insert(logical_port_name);
                }
                Err(e) => {
                    eprintln!("xcvrd-rs: post identity for {logical_port_name} failed: {e}");
                }
            }
        }
        self.retry_eeprom_set = retry;

        // Second pass: publish DOM thresholds for every port that read successfully,
        // sharing one read cache across the whole boot pass (`dom_thresholds_cache`).
        let mut dom_thresholds_cache: DbCache = DbCache::new();
        for logical_port_name in self.port_mapping.logical_port_list.clone() {
            if self.retry_eeprom_set.contains(&logical_port_name) {
                continue;
            }
            self.post_dom_thresholds(
                hal,
                dom_threshold_tbl,
                &logical_port_name,
                Some(&mut dom_thresholds_cache),
            );
            self.post_vdm_thresholds(hal, &logical_port_name);
        }
        Ok(())
    }

    /// `_init_port_sfp_status_sw_tbl` (`xcvrd.py:356`) - seed `TRANSCEIVER_STATUS_SW`
    /// (status/error) for every logical port at boot: `1` if present, `0` otherwise
    /// (and `0` for a logical port with no physical mapping).
    pub fn init_port_sfp_status_sw_tbl(
        &self,
        hal: &dyn Hal,
        status_sw_tbl: &dyn DbTable,
    ) -> Result<()> {
        for logical_port_name in &self.port_mapping.logical_port_list {
            if self
                .port_mapping
                .get_asic_id_for_logical_port(logical_port_name)
                .is_none()
            {
                continue;
            }
            let physical_port_list = match self
                .port_mapping
                .logical_port_name_to_physical_port_list(logical_port_name)
            {
                Some(list) => list,
                None => {
                    common::update_port_transceiver_status_table_sw(
                        logical_port_name,
                        status_sw_tbl,
                        SFP_STATUS_REMOVED,
                        "N/A",
                    );
                    continue;
                }
            };
            for physical_port in physical_port_list {
                let present = match hal.sfp(physical_port) {
                    Ok(sfp) => sfp.get_presence().unwrap_or(false),
                    Err(_) => false,
                };
                let status = if present { SFP_STATUS_INSERTED } else { SFP_STATUS_REMOVED };
                common::update_port_transceiver_status_table_sw(
                    logical_port_name,
                    status_sw_tbl,
                    status,
                    "N/A",
                );
            }
        }
        Ok(())
    }

    /// M1 projection: drive every present, published logical port to
    /// `cmis_state = READY`. The real datapath state comes from the CMIS manager
    /// (M8); here it is projected so STATE_DB satisfies the presence/identity
    /// contract. Ports still awaiting a retry are skipped.
    pub fn project_cmis_state_for_present_ports(
        &self,
        hal: &dyn Hal,
        status_sw_tbl: &dyn DbTable,
    ) {
        for logical_port_name in &self.port_mapping.logical_port_list {
            if self.retry_eeprom_set.contains(logical_port_name) {
                continue;
            }
            let present = self
                .port_mapping
                .logical_port_name_to_physical_port_list(logical_port_name)
                .and_then(|list| list.first().copied())
                .map(|pport| match hal.sfp(pport) {
                    Ok(sfp) => sfp.get_presence().unwrap_or(false),
                    Err(_) => false,
                })
                .unwrap_or(false);
            if present {
                self.project_cmis_ready(status_sw_tbl, logical_port_name);
            }
        }
    }

    fn project_cmis_ready(&self, status_sw_tbl: &dyn DbTable, logical_port_name: &str) {
        status_sw_tbl.hset(logical_port_name, "cmis_state", CMIS_STATE_READY);
    }

    /// Handle one `get_change_event` result (`task_worker` NORMAL_EVENT body,
    /// `xcvrd.py:523`): for each `(physical_port, code)` resolve the logical port(s)
    /// and apply insert (`1`) / remove (`0`) / an SFP-error bitmap (anything else).
    /// `error_dict` is the event's `sfp_error` map (physical-port key → vendor error
    /// description); empty when the platform reports no vendor detail.
    pub fn handle_change_event(
        &mut self,
        hal: &dyn Hal,
        int_tbl: &dyn DbTable,
        status_sw_tbl: &dyn DbTable,
        dom_threshold_tbl: &dyn DbTable,
        status: bool,
        port_dict: &mut BTreeMap<String, String>,
        error_dict: &BTreeMap<String, String>,
    ) {
        let event = self.mapping_event_from_change_event(status, port_dict);
        if event != SystemEvent::Normal {
            return;
        }
        for (key, value) in port_dict.clone() {
            // Cache SFP error events (a value that is neither INSERTED nor REMOVED):
            // when a logical port is later (re)created there is no way to re-detect
            // the SFP error via the platform API, so `on_add_logical_port` re-applies
            // it from here. Plug/unplug events clear any cached error for the port.
            if value != SFP_STATUS_INSERTED && value != SFP_STATUS_REMOVED {
                self.sfp_error_dict
                    .insert(key.clone(), (value.clone(), error_dict.clone()));
            } else {
                self.sfp_error_dict.remove(&key);
            }
            let phys: i64 = match key.parse() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if phys < 0 {
                continue;
            }
            let logical_port_list = match self.port_mapping.get_physical_to_logical(phys as usize) {
                Some(list) => list,
                None => {
                    eprintln!("xcvrd-rs: got unknown FP port index {key}, ignored");
                    continue;
                }
            };
            for logical_port in logical_port_list {
                if self
                    .port_mapping
                    .get_asic_id_for_logical_port(&logical_port)
                    .is_none()
                {
                    continue;
                }
                if value == SFP_STATUS_INSERTED {
                    self.handle_insert(hal, int_tbl, status_sw_tbl, dom_threshold_tbl, &logical_port);
                } else if value == SFP_STATUS_REMOVED {
                    self.handle_remove(hal, phys as usize, int_tbl, status_sw_tbl, &logical_port);
                } else {
                    self.handle_error(hal, status_sw_tbl, &key, &logical_port, &value, error_dict);
                }
                // A genuine plug/unplug/error transition just (re)wrote this port's real
                // state, so any stale recovery-attempt cap is obsolete: drop it and give a
                // freshly (re)inserted module its full recovery budget again.
                self.recover_attempts.remove(&logical_port);
            }
        }
    }

    /// Process one `get_change_event` poll result — the body of the daemon's
    /// change-event loop, extracted so the resilient wiring is covered by the Part-B
    /// tests. An `Ok` with SFP entries is dispatched to [`Self::handle_change_event`];
    /// an `Err` is a transient bridge read failure that is logged and skipped WITHOUT
    /// tearing the daemon down. Faithfulness note (`daemon::serve`): the daemon must
    /// keep the SAME chassis across a failed poll so the emulator's change-event
    /// baseline (`Chassis._event_cache`) is preserved — otherwise an already-active
    /// SFP-error injection would be absorbed as the fresh chassis's baseline and never
    /// surface as a transition, so `TRANSCEIVER_STATUS_SW.error` would never publish.
    pub fn process_change_event_poll(
        &mut self,
        hal: &dyn Hal,
        int_tbl: &dyn DbTable,
        status_sw_tbl: &dyn DbTable,
        dom_threshold_tbl: &dyn DbTable,
        poll: Result<ChangeEvent>,
    ) {
        match poll {
            Ok(ev) => {
                let mut port_dict = ev.sfp.clone();
                if !port_dict.is_empty() {
                    // Trace the delivered change-event (status/sfp/sfp_error) so a
                    // failed error-injection round is diagnosable from the daemon log.
                    eprintln!(
                        "xcvrd-rs: change-event status={} sfp={:?} sfp_error={:?}",
                        ev.status, ev.sfp, ev.sfp_error
                    );
                    self.handle_change_event(
                        hal,
                        int_tbl,
                        status_sw_tbl,
                        dom_threshold_tbl,
                        ev.status,
                        &mut port_dict,
                        &ev.sfp_error,
                    );
                }
            }
            Err(e) => {
                eprintln!("xcvrd-rs: get_change_event failed: {e}; keeping chassis and retrying");
            }
        }
    }

    fn handle_insert(
        &mut self,
        hal: &dyn Hal,
        int_tbl: &dyn DbTable,
        status_sw_tbl: &dyn DbTable,
        dom_threshold_tbl: &dyn DbTable,
        logical_port: &str,
    ) {
        // A plug-in event clears the error state.
        common::update_port_transceiver_status_table_sw(
            logical_port,
            status_sw_tbl,
            SFP_STATUS_INSERTED,
            "N/A",
        );
        let mut rc = post_port_sfp_info_to_db(logical_port, &self.port_mapping, int_tbl, hal);
        if matches!(rc, Err(XcvrdError::EepromNotReady)) {
            // EEPROM may need a moment; try once more, else defer to the retry loop.
            if !self.sfp_ready_wait.is_zero() {
                std::thread::sleep(self.sfp_ready_wait);
            }
            rc = post_port_sfp_info_to_db(logical_port, &self.port_mapping, int_tbl, hal);
            if matches!(rc, Err(XcvrdError::EepromNotReady)) {
                self.retry_eeprom_set.insert(logical_port.to_string());
            }
        }
        if !matches!(rc, Err(XcvrdError::EepromNotReady)) {
            // Identity read succeeded: publish DOM thresholds for this freshly
            // inserted module (fresh read, no shared cache). cmis_state is owned by
            // CmisManagerTask, which walks the datapath bring-up to READY/FAILED — so
            // this path no longer projects READY (doing so would jump a re-inserted
            // module straight past the non-terminal states the DOM gate keys off,
            // defeating is_port_in_cmis_initialization_process).
            self.post_dom_thresholds(hal, dom_threshold_tbl, logical_port, None);
            self.post_vdm_thresholds(hal, logical_port);
        }
    }

    fn handle_remove(
        &mut self,
        hal: &dyn Hal,
        physical_port: usize,
        int_tbl: &dyn DbTable,
        status_sw_tbl: &dyn DbTable,
        logical_port: &str,
    ) {
        // Remove the SFP API object for this physical port (`xcvrd.py:576-580`).
        // The platform caches the decoded CMIS/SFF api on the Sfp instance; dropping
        // it forces a fresh EEPROM decode (including the VDM/CDB advertisement bits) on
        // the next insert. Without this a re-plugged module keeps the previous api, so
        // `TRANSCEIVER_INFO.vdm_supported` and the DOM-loop `is_transceiver_vdm_supported`
        // gate stay latched at their pre-removal value. Mirror the Python try/except
        // (NotImplementedError/AttributeError): ignore any failure — remove_xcvr_api is
        // an optional SfpBase method and a bridge/mock miss must not abort the removal.
        if let Ok(sfp) = hal.sfp(physical_port) {
            let _ = sfp.call_json("remove_xcvr_api");
        }
        // Re-seed STATE_DB PORT_TABLE.NPU_SI_SETTINGS_SYNC_STATUS = DEFAULT on plug-out
        // (`xcvrd.py:582-583`) so the next insert re-runs the media-SerDes SI publish
        // (M11 seeds DEFAULT -> NOTIFIED once per port; without this reset a re-plugged
        // module would keep the stale NOTIFIED and never re-notify). No-op if the
        // PORT_TABLE handle isn't wired (unit tests that only assert the table teardown).
        self.reseed_npu_si_settings(logical_port);
        common::update_port_transceiver_status_table_sw(
            logical_port,
            status_sw_tbl,
            SFP_STATUS_REMOVED,
            "N/A",
        );
        // Hold cmis_state NON-terminal across the unplug. CmisManagerTask owns
        // cmis_state, but a plug-out is observed here (this loop consumes the change
        // event); the reference marks CMIS_STATE_REMOVED via its PortChangeObserver and
        // re-arms INSERTED on the next insert event. This polling port sets INSERTED
        // directly so the DOM gate (is_port_in_cmis_initialization_process) stays closed
        // continuously from unplug through the re-plug bring-up: get_presence() becomes
        // true the instant the module is re-plugged (before any daemon event runs), so a
        // terminal (REMOVED) window between re-plug and the CMIS manager's next tick
        // could otherwise let a DOM poll publish TRANSCEIVER_DOM_FLAG that then persists
        // into the non-terminal bring-up. Written AFTER the status/error set above so the
        // mock table's replace-set can't clobber it (real HSET + merge-set both preserve
        // the sibling fields).
        status_sw_tbl.hset(logical_port, "cmis_state", CMIS_STATE_INSERTED);
        // Purge the port's stale per-port rows: TRANSCEIVER_INFO plus every wired
        // DOM/status/VDM/PM/firmware table (DOM_SENSOR / DOM_TEMPERATURE / DOM_FLAG +
        // metadata / DOM_THRESHOLD / STATUS / STATUS_FLAG + metadata / per-type
        // VDM_THRESHOLD / VDM_REAL_VALUE / per-type VDM_FLAG + metadata / PM /
        // FIRMWARE_INFO). Deleting DOM_FLAG and its metadata is what lets a re-plugged
        // module re-seed its flag metadata on the next first publish.
        let mut tbls: Vec<&dyn DbTable> = Vec::with_capacity(1 + self.removal_tables.len());
        tbls.push(int_tbl);
        for t in &self.removal_tables {
            tbls.push(&**t);
        }
        common::del_port_sfp_dom_info_from_db(logical_port, &self.port_mapping, &tbls);
        self.retry_eeprom_set.remove(logical_port);
    }

    /// SFP error event (`task_worker` else-branch, `xcvrd.py:610`): a change-event
    /// value that is neither INSERTED nor REMOVED is an error bitmap. Decode the
    /// generic bits into descriptions (append any vendor-specific detail), replace
    /// the port's `TRANSCEIVER_STATUS_SW` `status`/`error` (status = the raw bitmap
    /// value, error = `'|'`-joined descriptions), and — if the blocking bit (0x02) is
    /// set — delete the port's DOM/status rows (EEPROM is unreadable so DOM would be
    /// out-of-date) while KEEPING the static `TRANSCEIVER_INFO`.
    fn handle_error(
        &mut self,
        hal: &dyn Hal,
        status_sw_tbl: &dyn DbTable,
        phys_key: &str,
        logical_port: &str,
        value: &str,
        error_dict: &BTreeMap<String, String>,
    ) {
        // Python `int(value)`; an unparseable value is an unrecognized event -> ignore.
        let error_bits: u32 = match value.parse::<u32>() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("xcvrd-rs: {logical_port}: got unrecognized event {value}, ignored");
                return;
            }
        };
        let mut error_descriptions = fetch_generic_error_description(error_bits);

        if has_vendor_specific_error(error_bits) {
            // Prefer the vendor description carried in the event's sfp_error map; if
            // the event carried none, ask the module directly (get_error_description).
            let vendor_specific_error_description = if !error_dict.is_empty() {
                error_dict.get(phys_key).cloned()
            } else {
                phys_key
                    .parse::<usize>()
                    .ok()
                    .and_then(|p| hal.sfp(p).ok())
                    .and_then(|s| s.get_error_description().ok().flatten())
            };
            if let Some(desc) = vendor_specific_error_description {
                error_descriptions.push(desc);
            }
        }

        // Any existing error is replaced by the new one. status = raw bitmap value.
        common::update_port_transceiver_status_table_sw(
            logical_port,
            status_sw_tbl,
            value,
            &error_descriptions.join("|"),
        );

        // Blocking error: EEPROM is inaccessible, so the (now-stale) DOM info is
        // removed. The interface info remains since it is static. Reuses the same
        // per-port DOM/status table set as the plug-out teardown, minus INFO.
        if is_error_block_eeprom_reading(error_bits) {
            let tbls: Vec<&dyn DbTable> = self.removal_tables.iter().map(|t| &**t).collect();
            common::del_port_sfp_dom_info_from_db(logical_port, &self.port_mapping, &tbls);
        }
    }

    /// `retry_eeprom_reading` (`xcvrd.py:837`) - on a 60 s cadence, re-read identity
    /// for every logical port in `retry_eeprom_set`; on success publish INFO + DOM
    /// thresholds and drop it from the set. No re-plug required. cmis_state is driven
    /// separately by CmisManagerTask (which walks the datapath bring-up independently
    /// of the identity read), so the retry path no longer projects READY.
    pub fn retry_eeprom_reading(
        &mut self,
        hal: &dyn Hal,
        int_tbl: &dyn DbTable,
        _status_sw_tbl: &dyn DbTable,
        dom_threshold_tbl: &dyn DbTable,
    ) {
        if self.retry_eeprom_set.is_empty() {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_retry_eeprom_time {
            if now.duration_since(last) < Self::RETRY_EEPROM_READING_INTERVAL {
                return;
            }
        }
        self.last_retry_eeprom_time = Some(now);

        let mut retry_success = BTreeSet::new();
        for logical_port in self.retry_eeprom_set.clone() {
            let rc = post_port_sfp_info_to_db(&logical_port, &self.port_mapping, int_tbl, hal);
            if !matches!(rc, Err(XcvrdError::EepromNotReady)) {
                self.post_dom_thresholds(hal, dom_threshold_tbl, &logical_port, None);
                self.post_vdm_thresholds(hal, &logical_port);
                retry_success.insert(logical_port);
            }
        }
        for lport in retry_success {
            self.retry_eeprom_set.remove(&lport);
        }
    }

    /// Recover the baseline for any present, mapped logical port whose
    /// `TRANSCEIVER_INFO` row is missing from STATE_DB.
    ///
    /// The reference daemon relies on the platform's `get_change_event` to report
    /// every already-present module as an INSERTED transition on the FIRST poll, so a
    /// module the boot one-shot (`_post_port_sfp_info_and_dom_thr_to_db_once`) could
    /// not publish — e.g. it was not yet present when that pass ran — is still
    /// surfaced by the change-event loop. The emulator's `Chassis.get_change_event`,
    /// however, SILENTLY seeds its all-present `_event_cache` baseline on that first
    /// poll and returns no events, and a no-op re-plug of an already-present module
    /// produces no transition either. Such a port therefore never receives an insert
    /// edge and would stay unpublished forever — the "missed edge" behind the
    /// Ethernet60 logical-port e2e (it is the only port whose baseline comes purely
    /// from the boot pass; every other tested port is re-plugged, taking the insert
    /// path).
    ///
    /// This scan closes that gap. INFO is the identity anchor and `TRANSCEIVER_DOM_THRESHOLD`
    /// is the static threshold row the insert/boot path publishes ALONGSIDE it; the reference
    /// always posts the two together (insert / boot / on_add / retry). So a port is "baselined"
    /// only when BOTH exist, and this scan recovers either half that is missing:
    ///   * MISSING `TRANSCEIVER_INFO` → republish the full baseline via the same
    ///     [`Self::handle_insert`] path an SFP insert takes (INFO + DOM/VDM thresholds +
    ///     STATUS_SW together).
    ///   * INFO present but `TRANSCEIVER_DOM_THRESHOLD` missing → re-post ONLY the static
    ///     DOM/VDM threshold rows (e.g. a boot-pass threshold post that did not land — the
    ///     spare Ethernet60 logical-port precondition tripped on exactly this). This never
    ///     re-derives a DOM sensor/flag row (the DOM loop owns those), only the static
    ///     threshold rows the insert path owns, so it cannot fight the DOM loop or the
    ///     link-change flag re-read; `post_dom_thresholds` no-ops when the module reports
    ///     no thresholds, so a genuinely threshold-less module writes nothing.
    /// A port with BOTH rows present is fully baselined and is left entirely untouched.
    ///
    /// It deliberately skips (a) ports already fully baselined (INFO + DOM_THRESHOLD both
    /// present — the steady-state no-op, two STATE_DB reads each), (b) ports a physical
    /// unplug just tore down (`TRANSCEIVER_STATUS_SW.status == "0"`, the race-free signal
    /// `handle_remove` writes synchronously in this loop — so recovery can never
    /// resurrect a just-removed module even while the platform's `get_presence()` bit
    /// still lags true), (c) ports already deferred to the EEPROM retry set, (d) ports
    /// torn down by a CONFIG_DB logical-port DEL (`deconfigured_ports`), and (e) ports
    /// carrying an active blocking SFP error (a non-`N/A` `TRANSCEIVER_STATUS_SW.error`),
    /// so it never resurrects a removed/deconfigured port, double-processes a retry
    /// port, or clobbers an injected error state.
    pub fn recover_missing_port_baselines(
        &mut self,
        hal: &dyn Hal,
        int_tbl: &dyn DbTable,
        status_sw_tbl: &dyn DbTable,
        dom_threshold_tbl: &dyn DbTable,
    ) {
        let now = Instant::now();
        if let Some(last) = self.last_recover_baseline_time {
            if now.duration_since(last) < Self::RECOVER_BASELINE_INTERVAL {
                return;
            }
        }
        self.last_recover_baseline_time = Some(now);

        for logical_port in self.port_mapping.logical_port_list.clone() {
            if self
                .port_mapping
                .get_asic_id_for_logical_port(&logical_port)
                .is_none()
            {
                continue;
            }
            // Fully baselined: both the INFO identity row AND the static DOM_THRESHOLD row
            // are already published -> nothing to recover. The reference publishes the two
            // together at insert/boot/on_add/retry, so their joint presence means the port
            // took (or is taking) the normal insert path. A port that has INFO but is
            // missing DOM_THRESHOLD is NOT fully baselined -> its thresholds are recovered
            // below (the gap the spare Ethernet60 logical-port precondition tripped on).
            let info_present = int_tbl.get(&logical_port).is_some();
            let threshold_present = dom_threshold_tbl.get(&logical_port).is_some();
            if info_present && threshold_present {
                // Fully baselined -> drop any recovery-attempt tally so a genuine future
                // loss gets a fresh full budget of attempts.
                self.recover_attempts.remove(&logical_port);
                continue;
            }
            // A physical unplug just tore this port down: `handle_remove` deletes INFO and
            // sets TRANSCEIVER_STATUS_SW.status = "0" (SFP_STATUS_REMOVED) synchronously,
            // in THIS same change-event loop thread. The module's absence has not
            // necessarily reached the platform's `get_presence()` bit yet (the emulator
            // propagates the unplug asynchronously), so the presence check below can still
            // read true for a brief window and would resurrect the port. Gate on the
            // status the removal path itself wrote -- the race-free signal that this
            // teardown was intentional -- so recovery never republishes a just-removed
            // module (the Ethernet100 unplug regression). status returns to INSERTED only
            // when a genuine re-plug insert republishes INFO, at which point the joint gate
            // above short-circuits recovery anyway.
            if status_sw_tbl.hget(&logical_port, "status").as_deref() == Some(SFP_STATUS_REMOVED) {
                continue;
            }
            // Already deferred to the EEPROM retry loop -> it owns this port (it re-posts
            // INFO + DOM/VDM thresholds together on success).
            if self.retry_eeprom_set.contains(&logical_port) {
                continue;
            }
            // A CONFIG_DB logical-port DEL intentionally tore this port down.
            if let Some(dc) = &self.deconfigured_ports {
                if dc
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&logical_port)
                {
                    continue;
                }
            }
            // An active blocking SFP error legitimately keeps INFO/thresholds absent
            // (handle_error deletes DOM/status incl. DOM_THRESHOLD but keeps INFO; a
            // module never-present-yet-errored has no INFO). Don't clobber that error
            // state with a synthetic insert or re-derive its thresholds.
            if let Some(err) = status_sw_tbl.hget(&logical_port, "error") {
                if err != "N/A" && !err.is_empty() {
                    continue;
                }
            }
            // Resolve the physical port and require the module to be present now.
            let present = match self.port_mapping.get_logical_to_physical(&logical_port) {
                Some(list) if !list.is_empty() => matches!(
                    hal.sfp(list[0]),
                    Ok(sfp) if sfp.get_presence().unwrap_or(false)
                ),
                _ => false,
            };
            if !present {
                continue;
            }
            // Bound the per-port recovery so an un-baselinable present module (identity /
            // thresholds simply never become readable) cannot re-drive the heavy EEPROM
            // reads below every interval forever — that flood starves the concurrent
            // DOM-flag poll's supplemental byte-9 read and drops TRANSCEIVER_DOM_FLAG
            // fields (notably vccHAlarm). A port that truly needs recovery baselines within
            // the first attempt(s); once capped, recovery backs off until the port becomes
            // fully baselined (reset above) or a genuine change event / logical-port
            // (re)add resets the tally. Count only ATTEMPTS that reach the action here.
            let attempts = self.recover_attempts.entry(logical_port.clone()).or_insert(0);
            if *attempts >= Self::MAX_RECOVER_ATTEMPTS {
                continue;
            }
            *attempts += 1;
            if !info_present {
                // No identity row yet for a present, un-torn-down, error-free port: publish
                // the full baseline exactly like an insert (INFO + DOM/VDM thresholds +
                // STATUS_SW) via the same [`Self::handle_insert`] path an SFP insert takes.
                eprintln!("xcvrd-rs: recovering missing baseline for present port {logical_port}");
                self.handle_insert(hal, int_tbl, status_sw_tbl, dom_threshold_tbl, &logical_port);
            } else {
                // INFO present but the static DOM_THRESHOLD row is missing (e.g. its
                // boot-pass post did not land): re-post ONLY the DOM/VDM threshold rows.
                // These are static and owned by the insert path (never the DOM loop), so
                // re-posting is idempotent and cannot fight a periodic sensor/flag publish
                // or the link-change flag re-read; post_dom_thresholds no-ops when the
                // module reports no thresholds.
                eprintln!(
                    "xcvrd-rs: recovering missing DOM_THRESHOLD for present port {logical_port}"
                );
                self.post_dom_thresholds(hal, dom_threshold_tbl, &logical_port, None);
                self.post_vdm_thresholds(hal, &logical_port);
            }
        }
    }

    /// Publish `TRANSCEIVER_DOM_THRESHOLD` for one logical port via the DOM DB poster
    /// (`self.dom_db_utils.post_port_dom_thresholds_to_db`). `db_cache` is `Some` only
    /// on the boot pass, where the whole run shares one read cache; insert/retry read
    /// fresh. The poster no-ops when the module is absent or reports no thresholds.
    fn post_dom_thresholds(
        &self,
        hal: &dyn Hal,
        dom_threshold_tbl: &dyn DbTable,
        logical_port: &str,
        db_cache: Option<&mut DbCache>,
    ) {
        let stop = AtomicBool::new(false);
        DomDbUtils::new().post_port_dom_thresholds_to_db(
            &stop,
            logical_port,
            &self.port_mapping,
            dom_threshold_tbl,
            hal,
            db_cache,
        );
    }

    /// `self.vdm_db_utils.post_port_vdm_thresholds_to_db` — publish the per-type VDM
    /// threshold tables for a freshly-identified module, alongside the DOM thresholds
    /// (`xcvrd.py:351/831/858`). No-op when the VDM threshold tables aren't wired.
    fn post_vdm_thresholds(&self, hal: &dyn Hal, logical_port: &str) {
        let tables = match &self.vdm_threshold_tables {
            Some(t) => t,
            None => return,
        };
        let stop = AtomicBool::new(false);
        VdmDbUtils::new().post_port_vdm_thresholds_to_db(
            &stop,
            logical_port,
            &self.port_mapping,
            hal,
            tables,
        );
    }

    /// `init` (`xcvrd.py:384`).
    pub fn init(&mut self) -> Result<()> {
        todo!("xcvrd.py:SfpStateUpdateTask.init")
    }

    /// `task_worker(stopping_event, sfp_error_event)` (`xcvrd.py:395`) - the full
    /// INIT/NORMAL/EXIT state machine; the M1 daemon drives the per-event handling
    /// via [`Self::handle_change_event`] directly (see [`crate::daemon`]).
    pub fn task_worker(&mut self, _stop: &Arc<AtomicBool>, _sfp_error_event: &Arc<AtomicBool>) {
        todo!("xcvrd.py:SfpStateUpdateTask.task_worker")
    }

    /// Thread entry (`run`, `xcvrd.py:695`).
    pub fn run(mut self, stop: Arc<AtomicBool>, sfp_error_event: Arc<AtomicBool>) {
        self.task_worker(&stop, &sfp_error_event)
    }

    /// `on_port_config_change` (`xcvrd.py:723`) — dispatch a CONFIG_DB PORT
    /// add/remove. On REMOVE the teardown runs BEFORE the port leaves the mapping
    /// (so `del_port_sfp_dom_info_from_db` can still resolve its physical-port name);
    /// on ADD the mapping is updated FIRST so `on_add_logical_port` resolves the
    /// module. Mirrors the Python ordering exactly.
    pub fn on_port_config_change(&mut self, ctx: &LogicalPortCtx, port_change_event: &PortChangeEvent) {
        match port_change_event.event_type {
            PortChangeEventType::PortRemove => {
                self.on_remove_logical_port(ctx, port_change_event);
                self.port_mapping.handle_port_change_event(port_change_event);
            }
            PortChangeEventType::PortAdd => {
                self.port_mapping.handle_port_change_event(port_change_event);
                self.on_add_logical_port(ctx, port_change_event);
            }
            _ => {}
        }
    }

    /// `on_remove_logical_port` (`xcvrd.py:731`) — a CONFIG_DB logical-port DEL tears
    /// down the ENTIRE per-port table set. Unlike a physical unplug (`handle_remove`,
    /// which PRESERVES `TRANSCEIVER_STATUS_SW` as `status='0'`), a logical-port removal
    /// is a full deconfiguration: it deletes `TRANSCEIVER_INFO` + every wired
    /// DOM/status/VDM/PM/firmware table (including the DOM/VDM THRESHOLD tables) AND
    /// `TRANSCEIVER_STATUS_SW` itself. The table set is `int_tbl` + the shared
    /// `removal_tables` + `status_sw_tbl` — exactly the list `xcvrd.py:740-764` hands
    /// to `del_port_sfp_dom_info_from_db` (the only addition over the plug-out list is
    /// `status_sw_tbl`). Also drops the port from the EEPROM retry set.
    pub fn on_remove_logical_port(&mut self, ctx: &LogicalPortCtx, port_change_event: &PortChangeEvent) {
        // Signal the DOM/CMIS threads to STOP servicing this port BEFORE its tables are
        // deleted, so they can't resurrect a row between the delete and this mark.
        self.mark_deconfigured(&port_change_event.port_name);
        let mut tbls: Vec<&dyn DbTable> = Vec::with_capacity(2 + self.removal_tables.len());
        tbls.push(ctx.int_tbl);
        for t in &self.removal_tables {
            tbls.push(&**t);
        }
        tbls.push(ctx.status_sw_tbl);
        common::del_port_sfp_dom_info_from_db(
            &port_change_event.port_name,
            &self.port_mapping,
            &tbls,
        );

        // The logical port has been removed, no need to retry EEPROM reading.
        self.retry_eeprom_set.remove(&port_change_event.port_name);
    }

    /// `on_add_logical_port` (`xcvrd.py:770`) — a CONFIG_DB logical-port (re)ADD
    /// repopulates the port's tables and re-seeds
    /// `STATE_DB PORT_TABLE.NPU_SI_SETTINGS_SYNC_STATUS = NPU_SI_SETTINGS_DEFAULT`.
    /// Three cases (mirroring the reference): (1) present, no SFP error → publish
    /// identity + DOM/VDM thresholds (or queue for retry on a not-ready EEPROM);
    /// (2) present with a non-blocking error → still read; a blocking error skips the
    /// EEPROM read; (3) absent → only update `TRANSCEIVER_STATUS_SW`. The final
    /// `TRANSCEIVER_STATUS_SW` `status`/`error` reflects the resolved plug/error state.
    pub fn on_add_logical_port(&mut self, ctx: &LogicalPortCtx, port_change_event: &PortChangeEvent) {
        let port_name = &port_change_event.port_name;
        let port_index = port_change_event.port_index;
        // Clear the deconfigured mark FIRST so the DOM/CMIS threads may resume as the
        // tables are repopulated below (they only re-publish DOM/cmis_state, never the
        // INFO/threshold/status rows this method writes, so an early resume is benign).
        self.clear_deconfigured(port_name);
        // A logical-port (re)add repopulates this port's baseline directly below, so any
        // stale recovery-attempt cap from a prior un-baselinable window is obsolete.
        self.recover_attempts.remove(port_name);
        // sfp_error_dict is keyed by the physical-port change-event key (a string),
        // matching how `handle_change_event` caches it (`str(physical_port)`).
        let port_index_key = port_index.to_string();

        // Re-seed NPU_SI_SETTINGS_SYNC_STATUS = DEFAULT for the (re)added port so the
        // media-SerDes SI publish re-runs for the fresh logical port (`xcvrd.py:788-796`).
        self.reseed_npu_si_settings(port_name);

        let mut error_description = "N/A".to_string();
        let mut status: Option<String> = None;
        let mut read_eeprom = true;
        if let Some((value, error_dict)) = self.sfp_error_dict.get(&port_index_key).cloned() {
            status = Some(value.clone());
            if let Ok(error_bits) = value.parse::<u32>() {
                let mut error_descriptions = fetch_generic_error_description(error_bits);
                if has_vendor_specific_error(error_bits) {
                    let vendor_specific_error_description = if !error_dict.is_empty() {
                        error_dict.get(&port_index_key).cloned()
                    } else {
                        ctx.hal
                            .sfp(port_index as usize)
                            .ok()
                            .and_then(|s| s.get_error_description().ok().flatten())
                    };
                    if let Some(desc) = vendor_specific_error_description {
                        error_descriptions.push(desc);
                    }
                }
                error_description = error_descriptions.join("|");
                if is_error_block_eeprom_reading(error_bits) {
                    read_eeprom = false;
                }
            }
        }

        // SFP information not in DB.
        let present = ctx
            .hal
            .sfp(port_index as usize)
            .and_then(|s| s.get_presence())
            .unwrap_or(false);
        if present && read_eeprom {
            if status.is_none() {
                status = Some(SFP_STATUS_INSERTED.to_string());
            }
            let rc = post_port_sfp_info_to_db(port_name, &self.port_mapping, ctx.int_tbl, ctx.hal);
            if matches!(rc, Err(XcvrdError::EepromNotReady)) {
                // Failed to read EEPROM, put it to the retry set.
                self.retry_eeprom_set.insert(port_name.clone());
            } else {
                self.post_dom_thresholds(ctx.hal, ctx.dom_threshold_tbl, port_name, None);
                self.post_vdm_thresholds(ctx.hal, port_name);
                // media_settings_parser::notify_media_setting is M11 — the NPU-SI SI
                // publish that flips SYNC_STATUS DEFAULT -> NOTIFIED. Not wired yet, so
                // the re-seeded DEFAULT stands (the M7 e2e asserts DEFAULT on re-add).
            }
        } else if status.is_none() {
            status = Some(SFP_STATUS_REMOVED.to_string());
        }
        common::update_port_transceiver_status_table_sw(
            port_name,
            ctx.status_sw_tbl,
            status.as_deref().unwrap_or(SFP_STATUS_REMOVED),
            &error_description,
        );
    }

    /// Re-seed `STATE_DB PORT_TABLE.NPU_SI_SETTINGS_SYNC_STATUS = DEFAULT` for a
    /// logical port (`xcvrd.py:583` on plug-out, `xcvrd.py:794` on logical (re)add).
    /// The real STATE_DB `set` merges, so the other `PORT_TABLE` fields
    /// (`host_tx_ready`, …) are preserved. No-op if the handle isn't wired.
    fn reseed_npu_si_settings(&self, logical_port: &str) {
        if let Some(tbl) = &self.state_port_tbl {
            tbl.set(
                logical_port,
                &[(
                    NPU_SI_SETTINGS_SYNC_STATUS_KEY.to_string(),
                    NPU_SI_SETTINGS_DEFAULT_VALUE.to_string(),
                )],
            );
        }
    }

    /// `update_log_level` (`xcvrd.py:866`).
    pub fn update_log_level(&self) {
        todo!("xcvrd.py:SfpStateUpdateTask.update_log_level")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDbTable, MockHal, MockSfp};
    use crate::xcvrd_utilities::port_event_helper::PortChangeEventType;
    use serde_json::json;

    fn cmis_info() -> serde_json::Value {
        json!({"cmis_rev": "5.0", "manufacturer": "xcvr-emu", "model": "EMU-100G"})
    }

    fn thresholds() -> serde_json::Value {
        json!({"temphighalarm": "75.0", "vcchighalarm": "3.5"})
    }

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

    fn task_with(ports: &[(&str, usize)]) -> SfpStateUpdateTask {
        SfpStateUpdateTask::new(vec![String::new()], mapping_with(ports), false)
    }

    // Direct port of tests/test_xcvrd.py:test_SfpStateUpdateTask_mapping_event_from_change_event.
    #[test]
    fn test_mapping_event_from_change_event() {
        let task = task_with(&[]);

        let mut port_dict = BTreeMap::new();
        assert_eq!(
            task.mapping_event_from_change_event(false, &mut port_dict),
            SystemEvent::Fail
        );
        assert_eq!(port_dict.get(EVENT_ON_ALL_SFP).map(String::as_str), Some(SYSTEM_FAIL));

        let mut port_dict = BTreeMap::new();
        port_dict.insert(EVENT_ON_ALL_SFP.to_string(), SYSTEM_FAIL.to_string());
        assert_eq!(
            task.mapping_event_from_change_event(false, &mut port_dict),
            SystemEvent::Fail
        );

        let mut port_dict = BTreeMap::new();
        assert_eq!(
            task.mapping_event_from_change_event(true, &mut port_dict),
            SystemEvent::BecomeReady
        );
        assert_eq!(
            port_dict.get(EVENT_ON_ALL_SFP).map(String::as_str),
            Some(SYSTEM_BECOME_READY)
        );

        let mut port_dict = BTreeMap::new();
        port_dict.insert("1".to_string(), SFP_STATUS_INSERTED.to_string());
        assert_eq!(
            task.mapping_event_from_change_event(true, &mut port_dict),
            SystemEvent::Normal
        );
    }

    // Direct port of tests/test_xcvrd.py:test_init_port_sfp_status_sw_tbl - a present
    // port is seeded INSERTED('1'), an absent one REMOVED('0').
    #[test]
    fn test_init_port_sfp_status_sw_tbl() {
        let task = task_with(&[("Ethernet0", 0), ("Ethernet4", 1)]);
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let hal = MockHal::with_sfps(vec![MockSfp::present(), MockSfp::default()]);

        task.init_port_sfp_status_sw_tbl(&hal, &status_sw).unwrap();
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("1"));
        assert_eq!(status_sw.hget("Ethernet4", "status").as_deref(), Some("0"));
        assert_eq!(status_sw.hget("Ethernet0", "error").as_deref(), Some("N/A"));
    }

    // Port of tests/test_xcvrd.py:test_init_port_sfp_status_sw_tbl_no_physical_port_found -
    // a logical port with no physical mapping is marked REMOVED.
    #[test]
    fn test_init_port_sfp_status_sw_tbl_no_physical_port_found() {
        // Ethernet0 is in the logical list + asic map but NOT in logical_to_physical,
        // so logical_port_name_to_physical_port_list returns None.
        let mut pm = PortMapping::new();
        pm.logical_port_list.push("Ethernet0".to_string());
        pm.logical_to_asic.insert("Ethernet0".to_string(), 0);
        let task = SfpStateUpdateTask::new(vec![String::new()], pm, false);
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let hal = MockHal::with_sfps(vec![]);

        task.init_port_sfp_status_sw_tbl(&hal, &status_sw).unwrap();
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("0"));
    }

    // Boot identity publish: a present CMIS module populates TRANSCEIVER_INFO and is
    // not queued for retry; a present module with an unreadable EEPROM is queued. The
    // readable port also gets its DOM thresholds published; the queued one does not.
    #[test]
    fn test_post_port_sfp_info_and_dom_thr_to_db_once() {
        let mut task = task_with(&[("Ethernet0", 0), ("Ethernet4", 1)]);
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![
            MockSfp {
                threshold_info: thresholds(),
                ..MockSfp::present().with_info(cmis_info())
            },
            MockSfp::present(), // present but info is null -> not ready
        ]);

        task.post_port_sfp_info_and_dom_thr_to_db_once(&hal, &int_tbl, &dom_thr)
            .unwrap();
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
        assert!(int_tbl.get("Ethernet4").is_none());
        assert!(task.retry_eeprom_set.contains("Ethernet4"));
        assert!(!task.retry_eeprom_set.contains("Ethernet0"));
        // DOM thresholds published for the readable port, skipped for the queued one.
        assert_eq!(dom_thr.hget("Ethernet0", "temphighalarm").as_deref(), Some("75.0"));
        assert!(dom_thr.get("Ethernet4").is_none());
    }

    // Direct port of tests/test_xcvrd.py:test_SfpStateUpdateTask_retry_eeprom_reading
    // (gate on empty set / interval, requeue on failure, drop on success).
    #[test]
    fn test_retry_eeprom_reading() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let mut hal = MockHal::with_sfps(vec![MockSfp::present()]); // not ready initially

        // Empty retry set -> nothing happens.
        task.retry_eeprom_reading(&hal, &int_tbl, &status_sw, &dom_thr);
        assert!(int_tbl.get("Ethernet0").is_none());

        // Within the interval (last retry just now) -> skipped.
        task.retry_eeprom_set.insert("Ethernet0".to_string());
        task.last_retry_eeprom_time = Some(Instant::now());
        task.retry_eeprom_reading(&hal, &int_tbl, &status_sw, &dom_thr);
        assert!(int_tbl.get("Ethernet0").is_none());
        assert!(task.retry_eeprom_set.contains("Ethernet0"));

        // Gate open but EEPROM still not ready -> stays queued.
        task.last_retry_eeprom_time = None;
        task.retry_eeprom_reading(&hal, &int_tbl, &status_sw, &dom_thr);
        assert!(task.retry_eeprom_set.contains("Ethernet0"));

        // Gate open and EEPROM now readable -> INFO + DOM thresholds published, dropped.
        task.last_retry_eeprom_time = None;
        hal.sfps[0] = MockSfp {
            threshold_info: thresholds(),
            ..MockSfp::present().with_info(cmis_info())
        };
        task.retry_eeprom_reading(&hal, &int_tbl, &status_sw, &dom_thr);
        assert!(!task.retry_eeprom_set.contains("Ethernet0"));
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
        // cmis_state is CmisManagerTask's responsibility now, not the retry path.
        assert_eq!(status_sw.hget("Ethernet0", "cmis_state"), None);
        assert_eq!(dom_thr.hget("Ethernet0", "temphighalarm").as_deref(), Some("75.0"));
    }

    // Baseline recovery: a present, mapped port whose TRANSCEIVER_INFO was never
    // published (the emulator folds an already-present module into its change-event
    // baseline with no insert edge — the Ethernet60 "missed edge") is republished
    // exactly like an insert, while a healthy port is left untouched.
    #[test]
    fn test_recover_missing_port_baselines_publishes_present_unpublished_port() {
        let mut task = task_with(&[("Ethernet0", 0), ("Ethernet4", 1)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![
            MockSfp {
                threshold_info: thresholds(),
                ..MockSfp::present().with_info(cmis_info())
            },
            MockSfp {
                threshold_info: thresholds(),
                ..MockSfp::present().with_info(cmis_info())
            },
        ]);

        // Ethernet0 is fully baselined (INFO + DOM_THRESHOLD both present) -> left
        // untouched. Ethernet4's baseline is entirely missing -> recovered.
        int_tbl.set(
            "Ethernet0",
            &[("manufacturer".to_string(), "already-here".to_string())],
        );
        dom_thr.set(
            "Ethernet0",
            &[("temphighalarm".to_string(), "sentinel".to_string())],
        );

        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);

        // Ethernet4 (present, INFO absent) is recovered: INFO + DOM thresholds + STATUS_SW.
        assert_eq!(int_tbl.hget("Ethernet4", "manufacturer").as_deref(), Some("xcvr-emu"));
        assert_eq!(dom_thr.hget("Ethernet4", "temphighalarm").as_deref(), Some("75.0"));
        assert_eq!(status_sw.hget("Ethernet4", "status").as_deref(), Some("1"));
        assert_eq!(status_sw.hget("Ethernet4", "error").as_deref(), Some("N/A"));
        // The fully-baselined port is left exactly as it was (not re-read / overwritten):
        // its INFO and its DOM_THRESHOLD sentinel both survive.
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("already-here"));
        assert_eq!(dom_thr.hget("Ethernet0", "temphighalarm").as_deref(), Some("sentinel"));
    }

    // Recovery re-posts a MISSING TRANSCEIVER_DOM_THRESHOLD for a present port whose INFO
    // is already published (e.g. its boot-pass threshold post did not land). This closes
    // the spare-logical-port precondition gap (Ethernet60 INFO present but DOM_THRESHOLD
    // absent): the static threshold rows are re-posted without touching INFO or STATUS_SW.
    #[test]
    fn test_recover_missing_port_baselines_reposts_missing_dom_threshold() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![MockSfp {
            threshold_info: thresholds(),
            ..MockSfp::present().with_info(cmis_info())
        }]);
        // INFO present (took the insert path) but DOM_THRESHOLD never landed. Seed a
        // STATUS_SW the recovery must not disturb (a healthy, inserted port).
        int_tbl.set(
            "Ethernet0",
            &[("manufacturer".to_string(), "xcvr-emu".to_string())],
        );
        status_sw.set(
            "Ethernet0",
            &[
                ("status".to_string(), "1".to_string()),
                ("error".to_string(), "N/A".to_string()),
            ],
        );

        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);

        // The missing threshold rows are re-posted from a fresh module read.
        assert_eq!(dom_thr.hget("Ethernet0", "temphighalarm").as_deref(), Some("75.0"));
        assert_eq!(dom_thr.hget("Ethernet0", "vcchighalarm").as_deref(), Some("3.5"));
        // INFO is left as-is (not re-read) and STATUS_SW is untouched by threshold recovery.
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("1"));
    }

    // Baseline recovery must NOT resurrect a port a physical unplug just tore down.
    // handle_remove deletes INFO and sets TRANSCEIVER_STATUS_SW.status = "0"
    // (SFP_STATUS_REMOVED) synchronously in the change-event loop; the emulator's
    // get_presence() bit can still lag `true` for a brief window afterwards, so the
    // presence gate alone would let recovery re-insert the just-removed module (the
    // Ethernet100 unplug regression). Gating on the removal-written status="0" is the
    // race-free guard: even a still-present module is left torn down.
    #[test]
    fn test_recover_missing_port_baselines_skips_just_removed_port() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        // Module still reads present (get_presence lag) but was just unplugged.
        let hal = MockHal::with_sfps(vec![MockSfp {
            threshold_info: thresholds(),
            ..MockSfp::present().with_info(cmis_info())
        }]);
        // Exactly the STATUS_SW handle_remove leaves behind: status="0", error="N/A".
        status_sw.set(
            "Ethernet0",
            &[
                ("status".to_string(), "0".to_string()),
                ("error".to_string(), "N/A".to_string()),
            ],
        );

        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);

        // INFO stays deleted -- the unplug teardown is not resurrected.
        assert!(int_tbl.get("Ethernet0").is_none());
        assert!(dom_thr.get("Ethernet0").is_none());
        // The removal-written status is preserved (not overwritten to INSERTED).
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("0"));
    }

    // Baseline recovery only acts on PRESENT modules — an absent (unplugged) mapped
    // port is never resurrected.
    #[test]
    fn test_recover_missing_port_baselines_skips_absent_module() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![MockSfp::default()]); // presence == false

        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);

        assert!(int_tbl.get("Ethernet0").is_none());
        assert!(dom_thr.get("Ethernet0").is_none());
    }

    // M7 flood guard: an un-baselinable PRESENT port — its identity publishes but its
    // DOM_THRESHOLD never lands because the module reports no thresholds — must NOT be
    // re-driven through the heavy recovery action every scan forever. Left unbounded that
    // reposting floods the PyO3 bridge + emulator gRPC and starves the concurrent DOM-flag
    // poll's supplemental byte-9 read_eeprom (the sole source of
    // TRANSCEIVER_DOM_FLAG.vccHAlarm), so vccHAlarm never publishes and the both-False DOM
    // flag baseline never converges (test_dom_flag_groups_temp_and_vcc). Recovery is capped
    // at MAX_RECOVER_ATTEMPTS, then backs off; becoming fully baselined resets the tally.
    #[test]
    fn test_recover_missing_port_baselines_caps_unbaselinable_port() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        // Present module whose INFO publishes but which reports NO thresholds: its
        // DOM_THRESHOLD row never lands, so the port is never "fully baselined".
        let hal = MockHal::with_sfps(vec![MockSfp::present().with_info(cmis_info())]);

        // Drive many recovery scans, bypassing the 5s interval gate each time.
        for _ in 0..(SfpStateUpdateTask::MAX_RECOVER_ATTEMPTS + 5) {
            task.last_recover_baseline_time = None;
            task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);
        }

        // INFO landed on the first attempt but thresholds never did -> the port stays
        // un-baselined, yet the attempt tally is CAPPED, not unbounded.
        assert!(int_tbl.get("Ethernet0").is_some());
        assert!(dom_thr.get("Ethernet0").is_none());
        assert_eq!(
            task.recover_attempts.get("Ethernet0"),
            Some(&SfpStateUpdateTask::MAX_RECOVER_ATTEMPTS)
        );

        // Once the threshold finally lands (port fully baselined), the next scan drops the
        // tally so a genuine future loss gets a fresh recovery budget.
        dom_thr.set(
            "Ethernet0",
            &[("temphighalarm".to_string(), "75.0".to_string())],
        );
        task.last_recover_baseline_time = None;
        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);
        assert_eq!(task.recover_attempts.get("Ethernet0"), None);
    }

    // A genuine change event (plug/unplug/error) rewrites the port's real state, so it must
    // reset the recovery-attempt cap: a re-inserted module gets a fresh recovery budget.
    #[test]
    fn test_recover_attempts_reset_on_change_event() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![MockSfp::present().with_info(cmis_info())]);

        // Cap the un-baselinable port.
        for _ in 0..(SfpStateUpdateTask::MAX_RECOVER_ATTEMPTS + 2) {
            task.last_recover_baseline_time = None;
            task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);
        }
        assert_eq!(
            task.recover_attempts.get("Ethernet0"),
            Some(&SfpStateUpdateTask::MAX_RECOVER_ATTEMPTS)
        );

        // A genuine INSERTED change event for the same physical port clears the tally.
        let mut insert = BTreeMap::new();
        insert.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        task.handle_change_event(
            &hal,
            &int_tbl,
            &status_sw,
            &dom_thr,
            true,
            &mut insert,
            &BTreeMap::new(),
        );
        assert_eq!(task.recover_attempts.get("Ethernet0"), None);
    }

    // Baseline recovery skips a port torn down by a CONFIG_DB logical-port DEL
    // (present in `deconfigured_ports`) so it never fights on_remove_logical_port.
    #[test]
    fn test_recover_missing_port_baselines_skips_deconfigured_port() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        task.set_deconfigured_ports(Arc::new(Mutex::new(BTreeSet::from([
            "Ethernet0".to_string()
        ]))));
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![MockSfp {
            threshold_info: thresholds(),
            ..MockSfp::present().with_info(cmis_info())
        }]);

        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);

        assert!(int_tbl.get("Ethernet0").is_none());
        assert!(dom_thr.get("Ethernet0").is_none());
    }

    // Baseline recovery skips a port carrying an active blocking SFP error (non-`N/A`
    // STATUS_SW.error) so it never clobbers an injected error state with a fake insert.
    #[test]
    fn test_recover_missing_port_baselines_skips_error_blocked_port() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![MockSfp {
            threshold_info: thresholds(),
            ..MockSfp::present().with_info(cmis_info())
        }]);
        status_sw.set(
            "Ethernet0",
            &[
                ("status".to_string(), "2".to_string()),
                ("error".to_string(), "Blocking EEPROM from being read".to_string()),
            ],
        );

        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);

        assert!(int_tbl.get("Ethernet0").is_none());
        assert!(dom_thr.get("Ethernet0").is_none());
        // The error state is preserved (not overwritten to INSERTED / N/A).
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("2"));
        assert_eq!(
            status_sw.hget("Ethernet0", "error").as_deref(),
            Some("Blocking EEPROM from being read")
        );
    }

    // Baseline recovery is interval-gated: a second scan within the interval is a no-op
    // (bounds STATE_DB read cost); forcing the gate open re-publishes.
    #[test]
    fn test_recover_missing_port_baselines_interval_gated() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![MockSfp {
            threshold_info: thresholds(),
            ..MockSfp::present().with_info(cmis_info())
        }]);

        // First scan (gate open): present + unpublished -> recovered, timestamp recorded.
        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));

        // Delete the row and scan again immediately: within the interval -> no-op.
        int_tbl.del("Ethernet0");
        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);
        assert!(int_tbl.get("Ethernet0").is_none());

        // Force the gate open -> republished.
        task.last_recover_baseline_time = None;
        task.recover_missing_port_baselines(&hal, &int_tbl, &status_sw, &dom_thr);
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
    }

    // Insert then remove: TRANSCEIVER_INFO + STATUS_SW.status track plug state.
    // handle_insert no longer projects cmis_state (CmisManagerTask owns it); a plug-out
    // parks cmis_state at INSERTED so the DOM gate stays closed across the re-plug.
    #[test]
    fn test_handle_change_event_insert_then_remove() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![MockSfp {
            threshold_info: thresholds(),
            ..MockSfp::present().with_info(cmis_info())
        }]);

        let mut insert = BTreeMap::new();
        insert.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        task.handle_change_event(&hal, &int_tbl, &status_sw, &dom_thr, true, &mut insert, &BTreeMap::new());
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("1"));
        // Insert does NOT drive cmis_state (that is CmisManagerTask's bring-up).
        assert_eq!(status_sw.hget("Ethernet0", "cmis_state"), None);
        // A fresh insert publishes DOM thresholds for the port.
        assert_eq!(dom_thr.hget("Ethernet0", "vcchighalarm").as_deref(), Some("3.5"));

        let hal_absent = MockHal::with_sfps(vec![MockSfp::default()]);
        let mut remove = BTreeMap::new();
        remove.insert("0".to_string(), SFP_STATUS_REMOVED.to_string());
        task.handle_change_event(&hal_absent, &int_tbl, &status_sw, &dom_thr, true, &mut remove, &BTreeMap::new());
        assert!(int_tbl.get("Ethernet0").is_none());
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("0"));
        // Plug-out parks cmis_state at a non-terminal state so DOM stays gated until the
        // CMIS manager re-drives the bring-up on the next insert.
        assert_eq!(
            status_sw.hget("Ethernet0", "cmis_state").as_deref(),
            Some(CMIS_STATE_INSERTED)
        );
    }

    // On plug-out every wired DOM/status table is purged alongside TRANSCEIVER_INFO,
    // so a re-plugged module re-seeds its flag metadata on the next first publish
    // (the invariant test_dom_flag_metadata_initialized_on_first_publish relies on).
    #[test]
    fn test_handle_remove_purges_dom_status_tables() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");

        let dom = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
        let dom_flag = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG"));
        let dom_flag_cc = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_FLAG_CHANGE_COUNT"));
        let status = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS"));
        let status_flag = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_FLAG"));
        task.set_removal_tables(vec![
            dom.clone(),
            dom_flag.clone(),
            dom_flag_cc.clone(),
            status.clone(),
            status_flag.clone(),
        ]);

        // Seed stale per-port rows (keyed by the port's physical-port name).
        int_tbl.hset("Ethernet0", "manufacturer", "xcvr-emu");
        dom.hset("Ethernet0", "temperature", "22.0");
        dom_flag.hset("Ethernet0", "tempHAlarm", "False");
        dom_flag_cc.hset("Ethernet0", "tempHAlarm", "3");
        status.hset("Ethernet0", "status", "1");
        status_flag.hset("Ethernet0", "tx_fault", "False");

        let hal_absent = MockHal::with_sfps(vec![MockSfp::default()]);
        let mut remove = BTreeMap::new();
        remove.insert("0".to_string(), SFP_STATUS_REMOVED.to_string());
        task.handle_change_event(&hal_absent, &int_tbl, &status_sw, &dom_thr, true, &mut remove, &BTreeMap::new());

        assert!(int_tbl.get("Ethernet0").is_none());
        assert!(dom.get("Ethernet0").is_none());
        assert!(dom_flag.get("Ethernet0").is_none());
        assert!(dom_flag_cc.get("Ethernet0").is_none());
        assert!(status.get("Ethernet0").is_none());
        assert!(status_flag.get("Ethernet0").is_none());
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("0"));
    }

    // Plug-out drops the platform's cached CMIS/SFF api (`xcvrd.py:576-580`) so the
    // next insert re-decodes the EEPROM (VDM/CDB advertisement included). Assert the
    // removal path invokes `remove_xcvr_api` on the removed physical port's Sfp. The
    // call is issued on the handle `MockHal::sfp(0)` returns; its `call_log` Arc is
    // shared with `hal.sfps[0]`, so the invocation is observable here.
    #[test]
    fn test_handle_remove_drops_cached_xcvr_api() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");

        let hal = MockHal::with_sfps(vec![MockSfp::present()]);
        let mut remove = BTreeMap::new();
        remove.insert("0".to_string(), SFP_STATUS_REMOVED.to_string());
        task.handle_change_event(&hal, &int_tbl, &status_sw, &dom_thr, true, &mut remove, &BTreeMap::new());

        assert!(
            hal.sfps[0]
                .call_log
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "remove_xcvr_api"),
            "handle_remove must call remove_xcvr_api to drop the cached api"
        );
    }

    // A faulted insert (EEPROM unreadable) does NOT populate INFO and queues the port
    // for the retry loop (the read-retry recovery path). No DOM thresholds are posted.
    #[test]
    fn test_handle_change_event_insert_faulted_queues_retry() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let hal = MockHal::with_sfps(vec![MockSfp::present()]); // present, EEPROM not ready

        let mut insert = BTreeMap::new();
        insert.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        task.handle_change_event(&hal, &int_tbl, &status_sw, &dom_thr, true, &mut insert, &BTreeMap::new());

        assert!(int_tbl.get("Ethernet0").is_none());
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("1"));
        assert!(task.retry_eeprom_set.contains("Ethernet0"));
        assert!(dom_thr.get("Ethernet0").is_none());
    }

    // SFP error event: a change-event value that is neither '1' nor '0' is an error
    // bitmap. A blocking error (0x02) sets TRANSCEIVER_STATUS_SW.status/error, caches
    // the event in sfp_error_dict, and purges the wired DOM tables while KEEPING the
    // static TRANSCEIVER_INFO. A non-blocking error sets the error but keeps DOM. A
    // subsequent plug-in ('1') clears both the error and the sfp_error_dict entry.
    #[test]
    fn test_handle_change_event_sfp_error() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let dom = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
        let status = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS"));
        task.set_removal_tables(vec![dom.clone(), status.clone()]);

        // Static INFO + DOM present before the error.
        int_tbl.hset("Ethernet0", "manufacturer", "xcvr-emu");
        dom.hset("Ethernet0", "temperature", "22.0");
        status.hset("Ethernet0", "status", "1");

        // BLOCKING(0x02) | POWER_BUDGET(0x04) | INSERTED(0x01) = 7 -> blocking.
        let hal = MockHal::with_sfps(vec![MockSfp::present()]);
        let mut ev = BTreeMap::new();
        ev.insert("0".to_string(), "7".to_string());
        task.handle_change_event(&hal, &int_tbl, &status_sw, &dom_thr, true, &mut ev, &BTreeMap::new());

        // status = raw bitmap; error = '|'-joined generic descriptions (bit order).
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("7"));
        assert_eq!(
            status_sw.hget("Ethernet0", "error").as_deref(),
            Some("Blocking EEPROM from being read|Power budget exceeded")
        );
        // Blocking -> DOM/status rows purged, INFO retained.
        assert!(dom.get("Ethernet0").is_none());
        assert!(status.get("Ethernet0").is_none());
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
        // Error is cached for a later logical-port re-creation.
        assert!(task.sfp_error_dict.contains_key("0"));

        // Non-blocking error (HIGH_TEMP 0x40 | INSERTED) keeps DOM.
        dom.hset("Ethernet0", "temperature", "23.0");
        let mut ev2 = BTreeMap::new();
        ev2.insert("0".to_string(), "65".to_string());
        task.handle_change_event(&hal, &int_tbl, &status_sw, &dom_thr, true, &mut ev2, &BTreeMap::new());
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("65"));
        assert_eq!(status_sw.hget("Ethernet0", "error").as_deref(), Some("High temperature"));
        assert!(dom.get("Ethernet0").is_some());

        // Recovery: a plug-in clears the error and the cache entry.
        let hal_ok = MockHal::with_sfps(vec![MockSfp::present().with_info(cmis_info())]);
        let mut insert = BTreeMap::new();
        insert.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        task.handle_change_event(&hal_ok, &int_tbl, &status_sw, &dom_thr, true, &mut insert, &BTreeMap::new());
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("1"));
        assert_eq!(status_sw.hget("Ethernet0", "error").as_deref(), Some("N/A"));
        assert!(!task.sfp_error_dict.contains_key("0"));
    }

    // Mirrors tests/test_status_error.py at the unit level: the exact injected error
    // bitmaps must decode to the exact TRANSCEIVER_STATUS_SW.error strings the e2e
    // asserts, blocking errors (BLOCKING bit 0x02) must remove DOM while KEEPING the
    // static INFO, and the non-blocking high-temperature error must keep DOM.
    #[test]
    fn test_handle_change_event_e2e_bitmaps() {
        let run = |value: &str| -> (Option<String>, bool) {
            let mut task = task_with(&[("Ethernet0", 0)]);
            task.sfp_ready_wait = Duration::ZERO;
            let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
            let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
            let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
            let dom = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
            task.set_removal_tables(vec![dom.clone()]);
            int_tbl.hset("Ethernet0", "manufacturer", "xcvr-emu");
            dom.hset("Ethernet0", "temperature", "22.0");
            let hal = MockHal::with_sfps(vec![MockSfp::present()]);
            let mut ev = BTreeMap::new();
            ev.insert("0".to_string(), value.to_string());
            task.handle_change_event(
                &hal, &int_tbl, &status_sw, &dom_thr, true, &mut ev, &BTreeMap::new(),
            );
            // Static INFO is always retained through an error.
            assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
            (status_sw.hget("Ethernet0", "error"), dom.get("Ethernet0").is_some())
        };

        // I2C_STUCK_EVENT = INSERTED|BLOCKING|I2C_STUCK = 0x0B = 11 (blocking).
        let (err, dom_present) = run("11");
        assert_eq!(
            err.as_deref(),
            Some("Blocking EEPROM from being read|Bus stuck (I2C data or clock shorted)")
        );
        assert!(!dom_present, "blocking error must remove DOM");

        // BAD_EEPROM_EVENT = INSERTED|BLOCKING|BAD_EEPROM = 0x13 = 19 (blocking).
        let (err, dom_present) = run("19");
        assert_eq!(
            err.as_deref(),
            Some("Blocking EEPROM from being read|Bad or unsupported EEPROM")
        );
        assert!(!dom_present, "blocking error must remove DOM");

        // HIGH_TEMP_EVENT = INSERTED|HIGH_TEMP = 0x41 = 65 (non-blocking).
        let (err, dom_present) = run("65");
        assert_eq!(err.as_deref(), Some("High temperature"));
        assert!(dom_present, "non-blocking error must keep DOM");
    }

    // Regression guard for the daemon's resilient change-event loop
    // (`daemon::serve` + `process_change_event_poll`): a transient get_change_event
    // error must NOT drop change-event continuity. After a failed poll (which the
    // daemon logs + skips WITHOUT rebuilding the chassis), the next successful poll's
    // active SFP-error injection is still decoded and published. If the daemon instead
    // tore down and rebuilt the chassis, a fresh emulator baseline would absorb the
    // injection and the error would never surface — the exact M3 e2e failure mode.
    #[test]
    fn test_process_change_event_poll_survives_read_error() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let dom = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
        task.set_removal_tables(vec![dom.clone()]);
        dom.hset("Ethernet0", "temperature", "22.0");

        let hal = MockHal::with_sfps(vec![MockSfp::present()]);
        // First poll fails transiently; the I2C-stuck injection surfaces on the next.
        hal.fail_next_polls(1);
        let mut sfp = BTreeMap::new();
        sfp.insert("0".to_string(), "11".to_string()); // I2C_STUCK_EVENT
        hal.push_change_event(ChangeEvent {
            status: true,
            sfp,
            sfp_error: BTreeMap::new(),
        });

        // Errored poll: skipped, nothing published, DOM untouched.
        let poll1 = hal.get_change_event(1000);
        assert!(poll1.is_err());
        task.process_change_event_poll(&hal, &int_tbl, &status_sw, &dom_thr, poll1);
        assert!(status_sw.hget("Ethernet0", "error").is_none());
        assert!(dom.get("Ethernet0").is_some());

        // Next poll delivers the injection: decoded + published, DOM removed (blocking).
        let poll2 = hal.get_change_event(1000);
        assert!(poll2.is_ok());
        task.process_change_event_poll(&hal, &int_tbl, &status_sw, &dom_thr, poll2);
        assert_eq!(
            status_sw.hget("Ethernet0", "error").as_deref(),
            Some("Blocking EEPROM from being read|Bus stuck (I2C data or clock shorted)")
        );
        assert!(dom.get("Ethernet0").is_none());
    }

    // Guards the daemon boot->steady-state change-event ordering (`daemon::serve`). The
    // emulator's FIRST in-loop `get_change_event` reports NO port changes on a clean
    // all-present baseline (it returns an empty `sfp` map on its first call while it self-seeds
    // `_event_cache` and lazily builds its STATE_DB injection reader). That first poll must be
    // a no-op, and a subsequently injected SFP-error then arrives as a transition and must
    // decode + publish `TRANSCEIVER_STATUS_SW.error`. Reproduces the baseline-poll->inject
    // ordering the tests/test_status_error.py e2e exercises, at the mock seam. (At runtime the
    // daemon loads the embedded `SonicDBConfig` via `env::init_embedded_db_config` BEFORE its
    // first `get_change_event`, so the emulator's `_get_statedb` can resolve STATE_DB and read the
    // injection hash; the boot prime then self-seeds `Chassis._event_cache` all-present before any
    // test can inject, so the injected error surfaces as a transition instead of being absorbed.
    // Without that load the emulator's `_get_statedb` fail-caches `False` for the chassis lifetime
    // — the regression these tests guard.)
    #[test]
    fn test_baseline_poll_then_inject_publishes_error() {
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let dom = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
        task.set_removal_tables(vec![dom.clone()]);
        int_tbl.hset("Ethernet0", "manufacturer", "xcvr-emu");
        dom.hset("Ethernet0", "temperature", "22.0");

        let hal = MockHal::with_sfps(vec![MockSfp::present()]);
        // Boot-time prime poll: the daemon's serve() issues one `get_change_event` prime
        // (CHANGE_EVENT_PRIME_MS) BEFORE the boot DOM poll so the emulator self-seeds its
        // all-present `_event_cache` before any injection. That first call reports no port
        // changes (empty sfp, status true) — modeled here — and MUST be a no-op; the injected
        // error then surfaces only on the NEXT poll (below) as a transition against that seeded
        // baseline. This is the unit-level guard for that ordering: baseline first, error next.
        hal.push_change_event(ChangeEvent {
            status: true,
            sfp: BTreeMap::new(),
            sfp_error: BTreeMap::new(),
        });
        // Then the injected non-blocking high-temperature error arrives as a transition.
        let mut sfp = BTreeMap::new();
        sfp.insert("0".to_string(), "65".to_string()); // HIGH_TEMP_EVENT (non-blocking)
        hal.push_change_event(ChangeEvent {
            status: true,
            sfp,
            sfp_error: BTreeMap::new(),
        });

        let baseline = hal.get_change_event(1000);
        task.process_change_event_poll(&hal, &int_tbl, &status_sw, &dom_thr, baseline);
        assert!(
            status_sw.hget("Ethernet0", "error").is_none(),
            "baseline poll must not publish an error"
        );
        assert!(dom.get("Ethernet0").is_some(), "baseline poll must not touch DOM");

        let poll = hal.get_change_event(1000);
        task.process_change_event_poll(&hal, &int_tbl, &status_sw, &dom_thr, poll);
        assert_eq!(
            status_sw.hget("Ethernet0", "error").as_deref(),
            Some("High temperature")
        );
        assert!(dom.get("Ethernet0").is_some(), "non-blocking error keeps DOM");
    }

    // e2e-faithful guard for the M4 SFP-error RUNTIME path — the exact behaviour the
    // three tests/test_status_error.py cases assert — driven through the daemon's
    // real change-event entry point `process_change_event_poll` (the loop body in
    // `daemon::serve`) with the emulator's EXACT event shape: `Chassis.get_change_event`
    // reports an injected error bitmap as the port's value in the `sfp` map
    // (physical-port -> str(code)) with an ALWAYS-EMPTY `sfp_error` map. Each bitmap must
    // decode to the exact TRANSCEIVER_STATUS_SW.error string and set status=raw-code; a
    // blocking error (BLOCKING bit 0x02) must purge DOM while KEEPING the static
    // TRANSCEIVER_INFO; the non-blocking high-temperature error keeps DOM. What lets an
    // injected code REACH this path at runtime is that the daemon loads the embedded
    // `swsscommon.SonicDBConfig` via `env::init_embedded_db_config` BEFORE its first
    // `get_change_event` (the Rust `swss-common` bindings connect by db-id and never load that
    // Python singleton, so the emulator's `_get_statedb` would otherwise fail-cache `False`), and
    // then makes a boot-time `get_change_event` prime — the emulator `Chassis`'s `_get_statedb`
    // connects on that prime and self-seeds `Chassis._event_cache` all-present before a test can
    // inject (the test injects only after seeing INFO+DOM), so the later injection surfaces as a
    // transition instead of being absorbed as a fresh baseline — and the loop keeps ONE chassis for
    // its whole lifetime (panic-proofed so a transient pass failure never tears `serve` down and
    // lets `run()` recreate the chassis, which would reset `Chassis._event_cache` and absorb an
    // active injection as baseline). This pins the decode/publish/teardown contract that path
    // relies on, including BAD_EEPROM=19 through the live poll (previously only covered via a
    // direct handle_change_event call, not process_change_event_poll).
    #[test]
    fn test_process_change_event_poll_e2e_error_bitmaps() {
        // (injected bitmap, expected error string, DOM retained?)
        let cases = [
            ("11", "Blocking EEPROM from being read|Bus stuck (I2C data or clock shorted)", false),
            ("19", "Blocking EEPROM from being read|Bad or unsupported EEPROM", false),
            ("65", "High temperature", true),
        ];
        for (bitmap, expected_err, dom_retained) in cases {
            let mut task = task_with(&[("Ethernet0", 0)]);
            task.sfp_ready_wait = Duration::ZERO;
            let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
            let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
            let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
            let dom = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
            task.set_removal_tables(vec![dom.clone()]);
            // Steady-state baseline before the fault: static INFO + DOM present.
            int_tbl.hset("Ethernet0", "manufacturer", "xcvr-emu");
            dom.hset("Ethernet0", "temperature", "22.0");
            let hal = MockHal::with_sfps(vec![MockSfp::present().with_info(cmis_info())]);

            // Emulator-exact injection: the code rides in `sfp`, `sfp_error` stays empty.
            let mut sfp = BTreeMap::new();
            sfp.insert("0".to_string(), bitmap.to_string());
            hal.push_change_event(ChangeEvent {
                status: true,
                sfp,
                sfp_error: BTreeMap::new(),
            });
            let poll = hal.get_change_event(1000);
            task.process_change_event_poll(&hal, &int_tbl, &status_sw, &dom_thr, poll);

            assert_eq!(
                status_sw.hget("Ethernet0", "error").as_deref(),
                Some(expected_err),
                "bitmap {bitmap}: wrong TRANSCEIVER_STATUS_SW.error",
            );
            assert_eq!(
                status_sw.hget("Ethernet0", "status").as_deref(),
                Some(bitmap),
                "bitmap {bitmap}: status must be the raw injected code",
            );
            assert_eq!(
                dom.get("Ethernet0").is_some(),
                dom_retained,
                "bitmap {bitmap}: DOM retention wrong",
            );
            // Static identity is always retained through an SFP error.
            assert_eq!(
                int_tbl.hget("Ethernet0", "manufacturer").as_deref(),
                Some("xcvr-emu"),
                "bitmap {bitmap}: TRANSCEIVER_INFO must be kept",
            );
        }

        // Recovery: a subsequent plug-in event clears a blocking error back to N/A
        // (the module re-seated after the I2C-stuck fault), through the same poll path.
        let mut task = task_with(&[("Ethernet0", 0)]);
        task.sfp_ready_wait = Duration::ZERO;
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let dom = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
        task.set_removal_tables(vec![dom.clone()]);
        dom.hset("Ethernet0", "temperature", "22.0");
        let hal = MockHal::with_sfps(vec![MockSfp::present().with_info(cmis_info())]);

        let mut err_ev = BTreeMap::new();
        err_ev.insert("0".to_string(), "11".to_string()); // I2C_STUCK_EVENT (blocking)
        hal.push_change_event(ChangeEvent {
            status: true,
            sfp: err_ev,
            sfp_error: BTreeMap::new(),
        });
        let poll = hal.get_change_event(1000);
        task.process_change_event_poll(&hal, &int_tbl, &status_sw, &dom_thr, poll);
        assert!(
            status_sw
                .hget("Ethernet0", "error")
                .as_deref()
                .unwrap_or("")
                .contains("Bus stuck (I2C data or clock shorted)"),
            "blocking error must be published before recovery",
        );
        assert!(dom.get("Ethernet0").is_none(), "blocking error removes DOM");

        let mut ins_ev = BTreeMap::new();
        ins_ev.insert("0".to_string(), SFP_STATUS_INSERTED.to_string());
        hal.push_change_event(ChangeEvent {
            status: true,
            sfp: ins_ev,
            sfp_error: BTreeMap::new(),
        });
        let poll = hal.get_change_event(1000);
        task.process_change_event_poll(&hal, &int_tbl, &status_sw, &dom_thr, poll);
        assert_eq!(
            status_sw.hget("Ethernet0", "error").as_deref(),
            Some("N/A"),
            "plug-in event must clear the error back to N/A",
        );
        assert_eq!(
            status_sw.hget("Ethernet0", "status").as_deref(),
            Some(SFP_STATUS_INSERTED),
        );
    }

    // cmis projection: present published ports go READY; ports still in the retry set
    // are skipped.
    #[test]
    fn test_project_cmis_state_for_present_ports() {
        let mut task = task_with(&[("Ethernet0", 0), ("Ethernet4", 1)]);
        task.retry_eeprom_set.insert("Ethernet4".to_string());
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let hal = MockHal::with_sfps(vec![MockSfp::present(), MockSfp::present()]);

        task.project_cmis_state_for_present_ports(&hal, &status_sw);
        assert_eq!(status_sw.hget("Ethernet0", "cmis_state").as_deref(), Some("READY"));
        assert!(status_sw.hget("Ethernet4", "cmis_state").is_none());
    }

    // Direct port of tests/test_xcvrd.py:test_SfpStateUpdateTask_on_add_logical_port
    // (5738). Ethernet0 -> physical port 1. Four cases, one reused task (the sfp is
    // rebuilt per case, as the Python test flips mock_get_presence / mock_post_sfp_info):
    //   1. present + EEPROM not ready  -> status INSERTED, queued for retry, no thresholds
    //   2. present + EEPROM readable    -> INFO + DOM thresholds published, not queued
    //   3. absent                       -> status REMOVED
    //   4. absent + a cached SFP error  -> status = raw error code, error = decoded string
    // Every case re-seeds NPU_SI_SETTINGS_SYNC_STATUS = DEFAULT (xcvrd.py:794). The
    // Python `mock_update_media_setting` assertions are OMITTED: media-settings notify
    // (which would flip SYNC_STATUS DEFAULT -> NOTIFIED) is M11, deliberately not wired
    // here, so the re-seeded DEFAULT stands — exactly what the M7 e2e asserts on re-add.
    #[test]
    fn test_on_add_logical_port() {
        let mut task = task_with(&[("Ethernet0", 1)]);
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let state_port: Arc<dyn DbTable> = Arc::new(MockDbTable::new("PORT_TABLE"));
        task.set_state_port_table(state_port.clone());
        let ev = PortChangeEvent::new("Ethernet0", 1, 0, PortChangeEventType::PortAdd);

        // Case 1: present, EEPROM not ready (present sfp with null info).
        {
            let hal = MockHal::with_sfps(vec![MockSfp::default(), MockSfp::present()]);
            let ctx = LogicalPortCtx {
                hal: &hal,
                int_tbl: &int_tbl,
                status_sw_tbl: &status_sw,
                dom_threshold_tbl: &dom_thr,
            };
            task.on_add_logical_port(&ctx, &ev);
        }
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some(SFP_STATUS_INSERTED));
        assert_eq!(status_sw.hget("Ethernet0", "error").as_deref(), Some("N/A"));
        assert!(int_tbl.get("Ethernet0").is_none(), "not-ready EEPROM posts no INFO");
        assert!(dom_thr.get("Ethernet0").is_none(), "not-ready EEPROM posts no thresholds");
        assert!(task.retry_eeprom_set.contains("Ethernet0"));
        assert_eq!(
            state_port.hget("Ethernet0", NPU_SI_SETTINGS_SYNC_STATUS_KEY).as_deref(),
            Some(NPU_SI_SETTINGS_DEFAULT_VALUE),
            "NPU_SI_SETTINGS_SYNC_STATUS must be re-seeded to DEFAULT",
        );
        task.retry_eeprom_set.clear();

        // Case 2: present, EEPROM readable -> INFO + DOM thresholds published.
        {
            let hal = MockHal::with_sfps(vec![
                MockSfp::default(),
                MockSfp {
                    threshold_info: thresholds(),
                    ..MockSfp::present().with_info(cmis_info())
                },
            ]);
            let ctx = LogicalPortCtx {
                hal: &hal,
                int_tbl: &int_tbl,
                status_sw_tbl: &status_sw,
                dom_threshold_tbl: &dom_thr,
            };
            task.on_add_logical_port(&ctx, &ev);
        }
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some(SFP_STATUS_INSERTED));
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
        assert_eq!(dom_thr.hget("Ethernet0", "temphighalarm").as_deref(), Some("75.0"));
        assert!(!task.retry_eeprom_set.contains("Ethernet0"));

        // Case 3: absent -> status REMOVED.
        {
            let hal = MockHal::with_sfps(vec![MockSfp::default(), MockSfp::default()]);
            let ctx = LogicalPortCtx {
                hal: &hal,
                int_tbl: &int_tbl,
                status_sw_tbl: &status_sw,
                dom_threshold_tbl: &dom_thr,
            };
            task.on_add_logical_port(&ctx, &ev);
        }
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some(SFP_STATUS_REMOVED));
        assert_eq!(status_sw.hget("Ethernet0", "error").as_deref(), Some("N/A"));

        // Case 4: absent + cached SFP error (BLOCKING 0x02 | POWER_BUDGET 0x04 = "6").
        // sfp_error_dict is keyed by the physical-port string ("1"); status becomes the
        // raw code and error the '|'-joined generic descriptions (no vendor part).
        task.sfp_error_dict
            .insert("1".to_string(), ("6".to_string(), BTreeMap::new()));
        {
            let hal = MockHal::with_sfps(vec![MockSfp::default(), MockSfp::default()]);
            let ctx = LogicalPortCtx {
                hal: &hal,
                int_tbl: &int_tbl,
                status_sw_tbl: &status_sw,
                dom_threshold_tbl: &dom_thr,
            };
            task.on_add_logical_port(&ctx, &ev);
        }
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("6"));
        assert_eq!(
            status_sw.hget("Ethernet0", "error").as_deref(),
            Some("Blocking EEPROM from being read|Power budget exceeded"),
        );
    }

    // on_remove_logical_port (xcvrd.py:731) tears down the ENTIRE per-port table set
    // INCLUDING TRANSCEIVER_STATUS_SW — the one difference from a physical unplug
    // (handle_remove), which preserves STATUS_SW as status='0'. Also drops the port
    // from the EEPROM retry set and marks it deconfigured (so the DOM/CMIS threads stop
    // servicing it).
    #[test]
    fn test_on_remove_logical_port_tears_down_full_set_incl_status_sw() {
        let mut task = task_with(&[("Ethernet0", 1)]);
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        let dom_thr_unused = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let dom = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
        let dom_threshold = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD"));
        let vdm_halarm = Arc::new(MockDbTable::new("TRANSCEIVER_VDM_HALARM_THRESHOLD"));
        let status = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS"));
        let firmware = Arc::new(MockDbTable::new("TRANSCEIVER_FIRMWARE_INFO"));
        task.set_removal_tables(vec![
            dom.clone(),
            dom_threshold.clone(),
            vdm_halarm.clone(),
            status.clone(),
            firmware.clone(),
        ]);
        let deconfigured: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
        task.set_deconfigured_ports(deconfigured.clone());
        task.retry_eeprom_set.insert("Ethernet0".to_string());

        // Seed a fully-populated port (rows keyed by the physical-port name Ethernet0).
        int_tbl.hset("Ethernet0", "manufacturer", "xcvr-emu");
        dom.hset("Ethernet0", "temperature", "22.0");
        dom_threshold.hset("Ethernet0", "temphighalarm", "75.0");
        vdm_halarm.hset("Ethernet0", "laser_temperature_media1", "80.0");
        status.hset("Ethernet0", "status", "1");
        firmware.hset("Ethernet0", "active_firmware", "1.0");
        status_sw.hset("Ethernet0", "status", "1");
        status_sw.hset("Ethernet0", "cmis_state", "READY");

        let hal = MockHal::with_sfps(vec![MockSfp::default(), MockSfp::default()]);
        let ctx = LogicalPortCtx {
            hal: &hal,
            int_tbl: &int_tbl,
            status_sw_tbl: &status_sw,
            dom_threshold_tbl: &dom_thr_unused,
        };
        let ev = PortChangeEvent::new("Ethernet0", 1, 0, PortChangeEventType::PortRemove);
        task.on_remove_logical_port(&ctx, &ev);

        assert!(int_tbl.get("Ethernet0").is_none(), "INFO deleted");
        assert!(dom.get("Ethernet0").is_none(), "DOM_SENSOR deleted");
        assert!(dom_threshold.get("Ethernet0").is_none(), "DOM_THRESHOLD deleted");
        assert!(vdm_halarm.get("Ethernet0").is_none(), "VDM_HALARM_THRESHOLD deleted");
        assert!(status.get("Ethernet0").is_none(), "STATUS deleted");
        assert!(firmware.get("Ethernet0").is_none(), "FIRMWARE_INFO deleted");
        assert!(
            status_sw.get("Ethernet0").is_none(),
            "STATUS_SW deleted on a logical-port removal (unlike a physical unplug)",
        );
        assert!(!task.retry_eeprom_set.contains("Ethernet0"), "dropped from retry set");
        assert!(
            deconfigured.lock().unwrap().contains("Ethernet0"),
            "port marked deconfigured so DOM/CMIS threads stop servicing it",
        );
    }

    // on_port_config_change (xcvrd.py:723) round-trip: a CONFIG_DB PORT_DEL tears the
    // whole set down + removes the logical port from the mapping + marks it
    // deconfigured; a following PORT_ADD re-adds the mapping, repopulates INFO /
    // DOM_THRESHOLD / STATUS_SW, re-seeds NPU_SI_SETTINGS_SYNC_STATUS = DEFAULT, and
    // clears the deconfigured mark. Mirrors the M7 e2e (test_logical_port.py).
    #[test]
    fn test_on_port_config_change_remove_then_readd() {
        let mut task = task_with(&[("Ethernet0", 1)]);
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        // DOM_THRESHOLD is torn down on remove (in removal_tables) AND repopulated on
        // add (via ctx.dom_threshold_tbl) — so it must be the SAME table instance, as
        // in the daemon wiring.
        let dom_threshold = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD"));
        let dom = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
        let status = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS"));
        task.set_removal_tables(vec![dom.clone(), dom_threshold.clone(), status.clone()]);
        let state_port: Arc<dyn DbTable> = Arc::new(MockDbTable::new("PORT_TABLE"));
        task.set_state_port_table(state_port.clone());
        let deconfigured: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
        task.set_deconfigured_ports(deconfigured.clone());

        // A present, readable CMIS module at physical port 1 (index 1).
        let hal = MockHal::with_sfps(vec![
            MockSfp::default(),
            MockSfp {
                threshold_info: thresholds(),
                ..MockSfp::present().with_info(cmis_info())
            },
        ]);

        // Fully-populated starting state.
        int_tbl.hset("Ethernet0", "manufacturer", "xcvr-emu");
        dom.hset("Ethernet0", "temperature", "22.0");
        dom_threshold.hset("Ethernet0", "temphighalarm", "75.0");
        status.hset("Ethernet0", "status", "1");
        status_sw.hset("Ethernet0", "status", "1");

        // --- PORT_DEL ---
        let remove_ev = PortChangeEvent::new("Ethernet0", 1, 0, PortChangeEventType::PortRemove);
        {
            let ctx = LogicalPortCtx {
                hal: &hal,
                int_tbl: &int_tbl,
                status_sw_tbl: &status_sw,
                dom_threshold_tbl: &*dom_threshold,
            };
            task.on_port_config_change(&ctx, &remove_ev);
        }
        assert!(int_tbl.get("Ethernet0").is_none());
        assert!(dom.get("Ethernet0").is_none());
        assert!(dom_threshold.get("Ethernet0").is_none());
        assert!(status.get("Ethernet0").is_none());
        assert!(status_sw.get("Ethernet0").is_none());
        assert!(
            !task.port_mapping.is_logical_port("Ethernet0"),
            "logical port removed from the mapping on PORT_DEL",
        );
        assert!(deconfigured.lock().unwrap().contains("Ethernet0"));

        // --- PORT_ADD (re-add) ---
        let add_ev = PortChangeEvent::new("Ethernet0", 1, 0, PortChangeEventType::PortAdd);
        {
            let ctx = LogicalPortCtx {
                hal: &hal,
                int_tbl: &int_tbl,
                status_sw_tbl: &status_sw,
                dom_threshold_tbl: &*dom_threshold,
            };
            task.on_port_config_change(&ctx, &add_ev);
        }
        assert!(
            task.port_mapping.is_logical_port("Ethernet0"),
            "logical port back in the mapping on PORT_ADD",
        );
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
        assert_eq!(dom_threshold.hget("Ethernet0", "temphighalarm").as_deref(), Some("75.0"));
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some(SFP_STATUS_INSERTED));
        assert_eq!(
            state_port.hget("Ethernet0", NPU_SI_SETTINGS_SYNC_STATUS_KEY).as_deref(),
            Some(NPU_SI_SETTINGS_DEFAULT_VALUE),
        );
        assert!(
            !deconfigured.lock().unwrap().contains("Ethernet0"),
            "deconfigured mark cleared on re-add so DOM/CMIS resume",
        );
    }
}
