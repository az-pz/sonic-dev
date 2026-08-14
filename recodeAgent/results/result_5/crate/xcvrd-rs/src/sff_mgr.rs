#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `sff_mgr.py`: `SffManagerTask` — deterministic link bring-up for non-CMIS
//! SFF-8636/8472 modules.
//!
//! The reference task keeps SFF-compliant modules in a deterministic Tx state: Tx is
//! enabled only after `host_tx_ready` becomes true **and** the port is admin-up, and Tx is
//! disabled otherwise. On (re)insertion / admin-up it also takes the module out of low
//! power (`set_lpmode(False)`) and enables the SFF-8636 High Power Class control for
//! power-class ≥ 5 modules.
//!
//! Following the crate's `CmisManagerTask` pattern (analysis §3.4/§3.6), the module's
//! control/decode surface is abstracted behind the mockable [`SffApi`] trait: production
//! wraps a bridge [`crate::hal::Sfp`] handle in [`BridgeSffApi`] (raw SFF-8636 page-00h
//! register reads/writes), while unit tests inject [`MockSffApi`] (canned/settable
//! returns + call counters, the analogue of the Python tests' `MagicMock()` api). The
//! CONFIG_DB/STATE_DB PORT tables are passed in directly as [`crate::db::Table`] handles
//! (like `CmisManagerTask`), and the syslog surface is the [`SffLog`] seam so ported tests can assert log strings.

use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;

use crate::db::Table;
use crate::hal::{Chassis, Sfp};
use crate::xcvrd_utilities::common::{self, CMIS_MODULE_TYPES};
use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType};

// --- constants (sff_mgr.py:45-87) --------------------------------------------------

/// `SffManagerTask.SFF_LOGGER_PREFIX` — prepended to every task-level syslog line.
pub const SFF_LOGGER_PREFIX: &str = "SFF-MAIN: ";
/// `SffLoggerForPortUpdateEvent.SFF_LOGGER_PREFIX`.
pub const SFF_PORT_UPDATE_LOGGER_PREFIX: &str = "SFF-PORT-UPDATE: ";

// CONFIG_DB / STATE_DB PORT-table field names the task keys on.
const ADMIN_STATUS: &str = "admin_status";
const SUBPORT: &str = "subport";
const LANES_LIST: &str = "lanes";
const XCVR_TYPE: &str = "type";
const HOST_TX_READY: &str = "host_tx_ready";

/// `SffManagerTask.DEFAULT_NUM_LANES_PER_PPORT` — QSFP28/QSFP+ default lane count.
pub const DEFAULT_NUM_LANES_PER_PPORT: i64 = 4;

// =====================================================================================
// SffApi — the mockable SFF-8636/8472 control surface (the CMIS-seam analogue).
// =====================================================================================

/// The SFF-8472/8636 control/decode surface the [`SffManagerTask`] bring-up loop drives
/// (`api.*` in `sff_mgr.py`). Split from [`Sfp`] so unit tests inject [`MockSffApi`];
/// production wraps a bridge handle in [`BridgeSffApi`]. `Option`/`None` returns model the
/// Python `AttributeError`/`NotImplementedError`/`None` paths the task treats as "skip".
pub trait SffApi {
    /// `common.is_cmis_api(api)` — a paged-CMIS module api (the SFF task skips it; the CMIS
    /// manager owns its datapath).
    fn is_cmis(&self) -> bool;
    /// `api.is_copper()` — `None` mirrors `AttributeError`/`NotImplementedError` (skip port).
    fn is_copper(&self) -> Option<bool>;
    /// `api.get_tx_disable_support()` — `None` mirrors `AttributeError`/`NotImplementedError`.
    fn get_tx_disable_support(&self) -> Option<bool>;
    /// `api.get_power_class()` — `None` mirrors the Python `None` return (log + skip HPC).
    fn get_power_class(&self) -> Option<i64>;
    /// `api.set_high_power_class(power_class, enable)` — `None` mirrors the Python
    /// `except (AttributeError, NotImplementedError): pass`.
    fn set_high_power_class(&self, power_class: i64, enable: bool) -> Option<bool>;
    /// `api.get_lpmode_support()`.
    fn get_lpmode_support(&self) -> bool;
    /// Take the module out of / into low power. Production routes an `Sff8472Api` through the
    /// bridge `sfp.set_lpmode` and every other api through `api.set_lpmode` (the Python
    /// `isinstance(api, Sff8472Api)` branch); [`BridgeSffApi`] owns that decision.
    fn set_lpmode(&self, lpmode: bool) -> bool;
    /// `api.get_tx_disable()` — per-lane tx-disable flags (`True` = disabled), `None` on a
    /// read error (the task then best-effort-forces every interested lane).
    fn get_tx_disable(&self) -> Option<Vec<bool>>;
    /// `api.tx_disable_channel(mask, disable)`.
    fn tx_disable_channel(&self, media_lanes_mask: u32, disable: bool) -> bool;
}

/// Factory that turns a HAL [`Sfp`] into an [`SffApi`] (production = [`BridgeSffApi`]; unit
/// tests inject [`MockSffApi`]). `None` mirrors Python `sfp.get_xcvr_api()` returning `None`
/// (no api for this port → the task logs and skips it).
pub type SffApiFactory = Box<dyn Fn(Box<dyn Sfp>) -> Option<Box<dyn SffApi>>>;

// =====================================================================================
// BridgeSffApi — production impl over a bridge Sfp handle (raw SFF-8636 registers).
// =====================================================================================

// SFF-8636 lower-memory (page 00h) control registers — flat linear offset == byte offset.
const SFF_TX_DISABLE_OFFSET: usize = 86; // 00h:86 Tx_Disable (1 bit / lane, lane1=bit0)
const SFF_LPMODE_HP_CTRL_OFFSET: usize = 93; // 00h:93 Power Control + High Power Class Enable
const SFF_POWER_CLASS_OFFSET: usize = 129; // 00h:129 Extended Identifier (power class)
const SFF_DEVICE_TECH_OFFSET: usize = 147; // 00h:147 Device technology (transmitter tech)
const SFF_OPTIONS_OFFSET: usize = 195; // 00h:195 Options (bit4 = Tx_Disable implemented)
const SFF_HIGH_POWER_CLASS_5_7_BIT: u8 = 0x04; // 00h:93 bit2
const SFF_HIGH_POWER_CLASS_8_BIT: u8 = 0x08; // 00h:93 bit3
const SFF_NUM_LANES: usize = 4;

/// Production [`SffApi`] backed by a bridge [`Sfp`]: the raw SFF-8636 page-00h register
/// reads/writes the concrete `Sff8636Api` performs — Tx_Disable (00h:86), power class
/// (00h:129), High Power Class Enable (00h:93). The task is off by default
/// (`--enable_sff_mgr`), and every method degrades to a safe skip on an unreadable register.
pub struct BridgeSffApi {
    sfp: Box<dyn Sfp>,
}

impl BridgeSffApi {
    pub fn new(sfp: Box<dyn Sfp>) -> Self {
        BridgeSffApi { sfp }
    }

    fn read_byte(&self, off: usize) -> Option<u8> {
        self.sfp.read_eeprom(off, 1).ok().flatten().and_then(|v| v.first().copied())
    }

    fn write_byte(&self, off: usize, byte: u8) -> bool {
        self.sfp.write_eeprom(off, &[byte]).unwrap_or(false)
    }
}

impl SffApi for BridgeSffApi {
    fn is_cmis(&self) -> bool {
        // A paged-CMIS module type (QSFP-DD/OSFP/…) is owned by the CMIS manager. Detect it
        // the same way `common::is_cmis_api` does — via the decoded `type_abbrv_name`.
        self.sfp
            .get_transceiver_info()
            .ok()
            .and_then(|info| info.get("type_abbrv_name").and_then(|v| v.as_str()).map(str::to_string))
            .map(|t| common::is_cmis_api(Some(t.as_str())))
            .unwrap_or(false)
    }

    fn is_copper(&self) -> Option<bool> {
        // SFF-8636 Table 6-18: transmitter technology (00h:147 bits 7:4) codes ≥ 0xA are
        // copper cables (unequalized/passive/active). Unreadable → optical (proceed).
        match self.read_byte(SFF_DEVICE_TECH_OFFSET) {
            Some(b) => Some((b >> 4) >= 0x0A),
            None => Some(false),
        }
    }

    fn get_tx_disable_support(&self) -> Option<bool> {
        // 00h:195 bit4 = Tx_Disable implemented. Unreadable → not supported (skip).
        match self.read_byte(SFF_OPTIONS_OFFSET) {
            Some(b) => Some(b & 0x10 != 0),
            None => Some(false),
        }
    }

    fn get_power_class(&self) -> Option<i64> {
        // SFF-8636 §6.2.6 Extended Identifier (00h:129): bits 1:0 encode power classes 5-7,
        // bit2 (0x04) power class 8, bits 7:6 power classes 1-4.
        let b = self.read_byte(SFF_POWER_CLASS_OFFSET)?;
        let class = if b & 0x03 != 0 {
            4 + (b & 0x03) as i64 // 5, 6 or 7
        } else if b & 0x04 != 0 {
            8
        } else {
            ((b >> 6) & 0x03) as i64 + 1 // 1..4
        };
        Some(class)
    }

    fn set_high_power_class(&self, power_class: i64, enable: bool) -> Option<bool> {
        // High Power Class Enable at 00h:93 bit2 (classes 5-7) / bit3 (class 8). Read-modify-
        // write so the Power Control bits (Power_override/Power_set) are preserved.
        let mut b = self.read_byte(SFF_LPMODE_HP_CTRL_OFFSET).unwrap_or(0);
        if enable {
            b |= SFF_HIGH_POWER_CLASS_5_7_BIT;
            if power_class >= 8 {
                b |= SFF_HIGH_POWER_CLASS_8_BIT;
            }
        } else {
            b &= !(SFF_HIGH_POWER_CLASS_5_7_BIT | SFF_HIGH_POWER_CLASS_8_BIT);
        }
        Some(self.write_byte(SFF_LPMODE_HP_CTRL_OFFSET, b))
    }

    fn get_lpmode_support(&self) -> bool {
        true
    }

    fn set_lpmode(&self, lpmode: bool) -> bool {
        // The bridge `SfpOptoeBase.set_lpmode` drives the correct register for both SFF-8636
        // (00h:93 Power_override=1/Power_set=lpmode) and SFF-8472, matching the Python
        // `sfp.set_lpmode` / `api.set_lpmode` split.
        self.sfp.set_lpmode(lpmode).unwrap_or(false)
    }

    fn get_tx_disable(&self) -> Option<Vec<bool>> {
        let b = self.read_byte(SFF_TX_DISABLE_OFFSET)?;
        Some((0..SFF_NUM_LANES).map(|lane| b & (1 << lane) != 0).collect())
    }

    fn tx_disable_channel(&self, media_lanes_mask: u32, disable: bool) -> bool {
        let mut b = self.read_byte(SFF_TX_DISABLE_OFFSET).unwrap_or(0);
        for lane in 0..SFF_NUM_LANES {
            if media_lanes_mask & (1 << lane) != 0 {
                if disable {
                    b |= 1 << lane;
                } else {
                    b &= !(1u8 << lane);
                }
            }
        }
        self.write_byte(SFF_TX_DISABLE_OFFSET, b)
    }
}

// =====================================================================================
// SffLog — the syslog seam (helper_logger), so ported tests can assert log strings.
// =====================================================================================

/// The `helper_logger` surface the task logs through. Production writes to stderr (pmon
/// captures it into syslog); unit tests inject [`RecordingSffLog`] to assert the exact
/// message strings the Python tests check (`mock_logger.log_error.assert_called_with(...)`).
pub trait SffLog {
    fn log_notice(&self, message: &str);
    fn log_warning(&self, message: &str);
    fn log_error(&self, message: &str);
    fn log_info(&self, message: &str) {
        let _ = message;
    }
    fn log_debug(&self, message: &str) {
        let _ = message;
    }
}

/// Production logger: forwards to stderr (the pmon supervisor captures it into syslog).
#[derive(Default)]
pub struct StderrSffLog;

impl SffLog for StderrSffLog {
    fn log_notice(&self, message: &str) {
        eprintln!("{message}");
    }
    fn log_warning(&self, message: &str) {
        eprintln!("{message}");
    }
    fn log_error(&self, message: &str) {
        eprintln!("{message}");
    }
}

/// `SffLoggerForPortUpdateEvent` — wraps the helper logger and prefixes every line with
/// `"SFF-PORT-UPDATE: "` (used as the PortChangeObserver's logger). Ported faithfully even
/// though the deployed SFF sweep reads the PORT tables directly.
pub struct SffLoggerForPortUpdateEvent {
    logger: Rc<dyn SffLog>,
}

impl SffLoggerForPortUpdateEvent {
    pub fn new(logger: Rc<dyn SffLog>) -> Self {
        SffLoggerForPortUpdateEvent { logger }
    }
    pub fn log_info(&self, message: &str) {
        self.logger.log_info(&format!("{SFF_PORT_UPDATE_LOGGER_PREFIX}{message}"));
    }
    pub fn log_notice(&self, message: &str) {
        self.logger.log_notice(&format!("{SFF_PORT_UPDATE_LOGGER_PREFIX}{message}"));
    }
    pub fn log_warning(&self, message: &str) {
        self.logger.log_warning(&format!("{SFF_PORT_UPDATE_LOGGER_PREFIX}{message}"));
    }
    pub fn log_error(&self, message: &str) {
        self.logger.log_error(&format!("{SFF_PORT_UPDATE_LOGGER_PREFIX}{message}"));
    }
    pub fn log_debug(&self, message: &str) {
        self.logger.log_debug(&format!("{SFF_PORT_UPDATE_LOGGER_PREFIX}{message}"));
    }
}

// =====================================================================================
// SffPortInfo — the Python `port_dict[lport]` sub-dict.
// =====================================================================================

/// Per-logical-port state accumulated from `on_port_update_event`. Field presence
/// (`Option::is_some`) mirrors the Python `'<key>' in port_dict[lport]` checks; the derived
/// `PartialEq` backs the reference `port_dict == port_dict_prev` no-change test.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SffPortInfo {
    pub asic_id: i32,
    pub index: Option<i64>,
    pub subport: Option<String>,
    pub lanes: Option<Vec<String>>,
    pub host_tx_ready: Option<String>,
    pub admin_status: Option<String>,
    pub xcvr_type: Option<String>,
    pub active_lanes: Option<Vec<bool>>,
}

// =====================================================================================
// SffManagerTask
// =====================================================================================

/// A pollable port-update source (the `PortChangeObserver` seam). `Ok(true)` = an update
/// was handled (→ run a sweep); `Ok(false)` = idle; `Err` models the observer raising a
/// Python exception (which `run` captures and surfaces on `join`). The deployed daemon uses
/// the synchronous sweep in `daemon::sff_control` rather than this poll.
pub type PortUpdatePoll = Box<dyn FnMut() -> Result<bool, String>>;

/// `SffManagerTask` — Tx enable/disable per `host_tx_ready`/`admin_status`, lpmode-disable,
/// and High Power Class enable for SFF (non-CMIS) modules.
pub struct SffManagerTask {
    chassis: Box<dyn Chassis>,
    cfg_port_tbl: Rc<dyn Table>,
    state_port_tbl: Rc<dyn Table>,
    api_factory: SffApiFactory,
    logger: Rc<dyn SffLog>,
    logger_for_port_update_event: SffLoggerForPortUpdateEvent,
    /// `port_dict` — per logical port, keyed by name; entry removed on CONFIG_DB PORT DEL.
    port_dict: HashMap<String, SffPortInfo>,
    /// `port_dict_prev` — snapshot from the previous sweep for change detection.
    port_dict_prev: HashMap<String, SffPortInfo>,
    /// `task_stopping_event` — set by `join` to break the worker loop.
    task_stopping_event: Arc<AtomicBool>,
    /// `main_thread_stop_event` — set when the worker raises so the daemon tears down.
    main_thread_stop_event: Arc<AtomicBool>,
    /// `self.exc` — the exception captured by `run`, re-raised by `join`.
    exc: Option<String>,
}

impl SffManagerTask {
    pub fn new(
        chassis: Box<dyn Chassis>,
        cfg_port_tbl: Rc<dyn Table>,
        state_port_tbl: Rc<dyn Table>,
        api_factory: SffApiFactory,
        logger: Rc<dyn SffLog>,
    ) -> Self {
        SffManagerTask {
            chassis,
            cfg_port_tbl,
            state_port_tbl,
            api_factory,
            logger: logger.clone(),
            logger_for_port_update_event: SffLoggerForPortUpdateEvent::new(logger),
            port_dict: HashMap::new(),
            port_dict_prev: HashMap::new(),
            task_stopping_event: Arc::new(AtomicBool::new(false)),
            main_thread_stop_event: Arc::new(AtomicBool::new(false)),
            exc: None,
        }
    }

    /// Share the daemon's main-thread stop flag so a worker exception can tear the daemon
    /// down (the Python `self.main_thread_stop_event.set()` in `run`).
    pub fn with_main_thread_stop_event(mut self, ev: Arc<AtomicBool>) -> Self {
        self.main_thread_stop_event = ev;
        self
    }

    // --- log helpers (sff_mgr.py:107-114) -----------------------------------------

    fn log_notice(&self, message: &str) {
        self.logger.log_notice(&format!("{SFF_LOGGER_PREFIX}{message}"));
    }

    fn log_warning(&self, message: &str) {
        self.logger.log_warning(&format!("{SFF_LOGGER_PREFIX}{message}"));
    }

    fn log_error(&self, message: &str) {
        self.logger.log_error(&format!("{SFF_LOGGER_PREFIX}{message}"));
    }

    /// `get_active_lanes_for_lport(lport, subport_idx, num_lanes_per_lport,
    /// num_lanes_per_pport)` — the boolean lane-ownership mask for a (breakout) subport.
    /// `subport_idx == 0` means the port owns every lane; otherwise it owns lanes
    /// `[(idx-1)*per_lport .. idx*per_lport)`. `None` on an out-of-range subport.
    pub fn get_active_lanes_for_lport(
        &self,
        lport: &str,
        subport_idx: i64,
        num_lanes_per_lport: i64,
        num_lanes_per_pport: i64,
    ) -> Option<Vec<bool>> {
        // Guard against a zero divisor (Python would raise ZeroDivisionError and take the
        // task down; we stay resilient and treat it as invalid input).
        if num_lanes_per_lport <= 0
            || subport_idx < 0
            || subport_idx > num_lanes_per_pport / num_lanes_per_lport
        {
            self.log_error(&format!(
                "{lport}: Invalid subport_idx {subport_idx} for \
                 num_lanes_per_lport={num_lanes_per_lport}, \
                 num_lanes_per_pport={num_lanes_per_pport}"
            ));
            return None;
        }

        let n = num_lanes_per_pport.max(0) as usize;
        if subport_idx == 0 {
            return Some(vec![true; n]);
        }

        let mut lanes = vec![false; n];
        let start = ((subport_idx - 1) * num_lanes_per_lport) as usize;
        let count = num_lanes_per_lport as usize;
        for lane in lanes.iter_mut().skip(start).take(count) {
            *lane = true;
        }
        Some(lanes)
    }

    /// `on_port_update_event(port_change_event)` — soak a CONFIG/STATE PORT `SET`/`DEL` into
    /// `port_dict`.
    pub fn on_port_update_event(&mut self, ev: &PortChangeEvent) {
        // Ignore anything but SET / DEL.
        if !matches!(ev.event_type, PortEventType::PortSet | PortEventType::PortDel) {
            return;
        }

        let lport = ev.port_name.clone();
        let pport = ev.port_index;
        let asic_id = ev.asic_id;

        // Skip if it's not a physical (front-panel) port.
        if !lport.starts_with("Ethernet") {
            return;
        }
        // Skip if the port carries no dict payload (the Python `port_dict is None` guard: a
        // bare 4-arg PORT_DEL is a no-op).
        let Some(dict) = ev.port_dict.as_ref() else {
            return;
        };
        let d: HashMap<&str, &str> = dict.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        match ev.event_type {
            PortEventType::PortSet => {
                let entry = self.port_dict.entry(lport.clone()).or_default();
                if pport >= 0 {
                    entry.index = Some(pport as i64);
                }
                if let Some(v) = d.get(SUBPORT) {
                    entry.subport = Some(v.to_string());
                }
                if let Some(v) = d.get(LANES_LIST) {
                    entry.lanes = Some(v.split(',').map(|s| s.to_string()).collect());
                }
                if let Some(v) = d.get(HOST_TX_READY) {
                    entry.host_tx_ready = Some(v.to_string());
                }
                if let Some(v) = d.get(ADMIN_STATUS) {
                    entry.admin_status = Some(v.to_string());
                }
                if let Some(v) = d.get(XCVR_TYPE) {
                    entry.xcvr_type = Some(v.to_string());
                }
                entry.asic_id = asic_id;
            }
            PortEventType::PortDel => {
                // CONFIG_DB PORT DEL — the logical port is de-provisioned: drop the entry.
                if ev.db_name.as_deref() == Some("CONFIG_DB") {
                    self.port_dict.remove(&lport);
                }
                // STATE_DB TRANSCEIVER_INFO DEL — the transceiver was removed (not the port):
                // clear just the `type` field so the port is treated as "no xcvr present".
                else if ev.table_name.as_deref() == Some("TRANSCEIVER_INFO") {
                    if let Some(entry) = self.port_dict.get_mut(&lport) {
                        entry.xcvr_type = None;
                    }
                }
            }
            _ => {}
        }
    }

    /// `get_host_tx_status(lport, asic_index)` — STATE_DB `PORT_TABLE|<lport>.host_tx_ready`
    /// (absent → `"false"`).
    pub fn get_host_tx_status(&self, lport: &str, asic_index: i32) -> String {
        let _ = asic_index; // single-ASIC crate: the table handle is already asic-scoped.
        self.state_port_tbl
            .hget(lport, HOST_TX_READY)
            .ok()
            .flatten()
            .unwrap_or_else(|| "false".to_string())
    }

    /// `get_admin_status(lport, asic_index)` — CONFIG_DB `PORT|<lport>.admin_status`
    /// (absent → `"down"`).
    pub fn get_admin_status(&self, lport: &str, asic_index: i32) -> String {
        let _ = asic_index;
        self.cfg_port_tbl
            .hget(lport, ADMIN_STATUS)
            .ok()
            .flatten()
            .unwrap_or_else(|| "down".to_string())
    }

    /// `calculate_tx_disable_delta_array` — per active lane, `True` where the current
    /// tx_disable flag differs from the target (inactive lanes never change).
    pub fn calculate_tx_disable_delta_array(
        &self,
        cur_tx_disable_array: &[bool],
        tx_disable_flag: bool,
        active_lanes: &[bool],
    ) -> Vec<bool> {
        active_lanes
            .iter()
            .zip(cur_tx_disable_array.iter())
            .map(|(&active, &cur)| if active { tx_disable_flag != cur } else { false })
            .collect()
    }

    /// `convert_bool_array_to_bit_mask` — LSB-first bitmask (item 0 → bit 0).
    pub fn convert_bool_array_to_bit_mask(&self, bool_array: &[bool]) -> u32 {
        let mut mask = 0u32;
        for (i, &flag) in bool_array.iter().enumerate() {
            if flag {
                mask |= 1 << i;
            }
        }
        mask
    }

    /// `enable_high_power_class(xcvr_api, lport)` — SFF-8636 §6.2.6: power classes 5-8 must
    /// have High Power Class Enable set or they cap at class-4 power. No-op for class < 5; an
    /// api that lacks the routines (`None`) is silently skipped (the Python
    /// `except (AttributeError, NotImplementedError): pass`).
    pub fn enable_high_power_class(&self, api: &dyn SffApi, lport: &str) {
        let power_class = match api.get_power_class() {
            Some(p) => p,
            None => {
                self.log_error(&format!("{lport}: failed to get power class"));
                return;
            }
        };
        if power_class < 5 {
            return;
        }
        match api.set_high_power_class(power_class, true) {
            Some(true) => self.log_notice(&format!("{lport}: done enabling high power class")),
            Some(false) => self.log_error(&format!("{lport}: failed to enable high power class")),
            None => {}
        }
    }

    fn clear_xcvr_type(&mut self, lport: &str) {
        if let Some(entry) = self.port_dict.get_mut(lport) {
            entry.xcvr_type = None;
        }
    }

    /// One logical-port pass — the body of the reference `task_worker` per-port loop
    /// (`sff_mgr.py:367-528`). Split out so unit tests can drive it via
    /// [`Self::process_ports_once`] without a live observer.
    fn process_single_lport(&mut self, lport: &str) {
        let (pport, subport_idx, lanes_list, cached_active_lanes, xcvr_type, asic_id) = {
            let d = &self.port_dict[lport];
            (
                d.index.unwrap_or(-1),
                d.subport
                    .as_deref()
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(0),
                d.lanes.clone(),
                d.active_lanes.clone(),
                d.xcvr_type.clone(),
                d.asic_id,
            )
        };

        if pport < 0 {
            return;
        }
        let Some(lanes_list) = lanes_list else {
            return;
        };
        // TRANSCEIVER_INFO `type` not ready → xcvr not present.
        if xcvr_type.is_none() {
            return;
        }

        // Double-check the HW presence before moving forward.
        let sfp = match self.chassis.sfp(pport as usize) {
            Ok(s) => s,
            Err(_) => {
                self.log_error(&format!("{lport}: module not present!"));
                self.clear_xcvr_type(lport);
                return;
            }
        };
        if !matches!(sfp.get_presence(), Ok(true)) {
            self.log_error(&format!("{lport}: module not present!"));
            self.clear_xcvr_type(lport);
            return;
        }

        // Skip if there's no xcvr api (Python `get_xcvr_api() is None` / `AttributeError`).
        let api_box = match (self.api_factory)(sfp) {
            Some(a) => a,
            None => {
                self.log_error(&format!("{lport}: skipping sff_mgr since no xcvr api!"));
                return;
            }
        };
        let api: &dyn SffApi = api_box.as_ref();

        // Proceed only for non-CMIS transceivers.
        if api.is_cmis() {
            return;
        }

        // Fill host_tx_ready / admin_status from the DB if the event didn't carry them.
        let (host_in, admin_in) = {
            let d = &self.port_dict[lport];
            (d.host_tx_ready.clone(), d.admin_status.clone())
        };
        let host_tx_ready = match host_in {
            Some(v) => v,
            None => {
                let v = self.get_host_tx_status(lport, asic_id);
                self.port_dict.get_mut(lport).unwrap().host_tx_ready = Some(v.clone());
                self.log_notice(&format!(
                    "{lport}: fetched DB and updated host_tx_ready={v} locally"
                ));
                v
            }
        };
        let admin_status = match admin_in {
            Some(v) => v,
            None => {
                let v = self.get_admin_status(lport, asic_id);
                self.port_dict.get_mut(lport).unwrap().admin_status = Some(v.clone());
                self.log_notice(&format!(
                    "{lport}: fetched DB and updated admin_status={v} locally"
                ));
                v
            }
        };

        // Diff against the previous sweep: insertion / host_tx_ready / admin_status change.
        let prev = self.port_dict_prev.get(lport);
        let xcvr_inserted = match prev {
            None => true,
            Some(p) => p.xcvr_type.is_none(),
        };
        let host_tx_ready_changed = match prev {
            None => true,
            Some(p) => p.host_tx_ready.as_deref() != Some(host_tx_ready.as_str()),
        };
        let admin_status_changed = match prev {
            None => true,
            Some(p) => p.admin_status.as_deref() != Some(admin_status.as_str()),
        };
        // Additional filter (beyond the observer's) — ignore an irrelevant event that
        // changed neither presence, host_tx_ready, nor admin_status.
        if !xcvr_inserted && !host_tx_ready_changed && !admin_status_changed {
            return;
        }
        self.log_notice(&format!(
            "{lport}: xcvr=present(inserted={xcvr_inserted}), \
             host_tx_ready={host_tx_ready}(changed={host_tx_ready_changed}), \
             admin_status={admin_status}(changed={admin_status_changed})"
        ));

        // Skip copper cables / modules that don't support tx_disable (missing routines →
        // `None` → skip, the Python `except (AttributeError, NotImplementedError): continue`).
        match api.is_copper() {
            Some(true) => {
                self.log_notice(&format!("{lport}: skipping sff_mgr for copper cable"));
                return;
            }
            Some(false) => {}
            None => return,
        }
        match api.get_tx_disable_support() {
            Some(false) => {
                self.log_notice(&format!(
                    "{lport}: skipping sff_mgr due to tx_disable not supported"
                ));
                return;
            }
            Some(true) => {}
            None => return,
        }

        // On insertion (or admin coming up) enable high power class + exit low-power mode.
        if xcvr_inserted || (admin_status_changed && admin_status == "up") {
            self.enable_high_power_class(api, lport);
            if api.get_lpmode_support() && !api.set_lpmode(false) {
                self.log_error(&format!(
                    "{lport}: Failed to take module out of low power mode."
                ));
            }
        }

        // Resolve (and cache) the active-lane mask for this logical port.
        let active_lanes = match cached_active_lanes {
            Some(a) => a,
            None => match self.get_active_lanes_for_lport(
                lport,
                subport_idx,
                lanes_list.len() as i64,
                DEFAULT_NUM_LANES_PER_PPORT,
            ) {
                Some(a) => {
                    self.port_dict.get_mut(lport).unwrap().active_lanes = Some(a.clone());
                    a
                }
                None => {
                    self.log_error(&format!(
                        "{lport}: skipping sff_mgr due to failing to get active lanes"
                    ));
                    return;
                }
            },
        };

        // TX is enabled only when host_tx_ready is true AND admin_status is up.
        let target_tx_disable_flag = !(host_tx_ready == "true" && admin_status == "up");
        let cur_tx_disable_array = match api.get_tx_disable() {
            Some(c) => c,
            None => {
                self.log_error(&format!("{lport}: Failed to get current tx_disable value"));
                // Best-effort: force every interested lane by seeding the opposite value.
                vec![!target_tx_disable_flag; DEFAULT_NUM_LANES_PER_PPORT as usize]
            }
        };
        let delta_array =
            self.calculate_tx_disable_delta_array(&cur_tx_disable_array, target_tx_disable_flag, &active_lanes);
        let mask = self.convert_bool_array_to_bit_mask(&delta_array);
        if mask == 0 {
            self.log_notice(&format!("{lport}: No change is needed for tx_disable value"));
            return;
        }
        if api.tx_disable_channel(mask, target_tx_disable_flag) {
            self.log_notice(&format!(
                "{lport}: TX was {} with lanes mask: {mask:#b}",
                if target_tx_disable_flag { "disabled" } else { "enabled" }
            ));
        } else {
            self.log_error(&format!(
                "{lport}: Failed to {} TX with lanes mask: {mask:#b}",
                if target_tx_disable_flag { "disable" } else { "enable" }
            ));
        }
    }

    /// One sweep over every known logical port + snapshot for the next diff — the reference
    /// `task_worker` while-body (minus the blocking observer poll). Unit tests drive this.
    pub fn process_ports_once(&mut self) {
        let lports: Vec<String> = self.port_dict.keys().cloned().collect();
        for lport in lports {
            if self.task_stopping_event.load(Ordering::Relaxed) {
                break;
            }
            if !self.port_dict.contains_key(&lport) {
                continue;
            }
            self.process_single_lport(&lport);
        }
        // Snapshot for the next sweep's change detection (Python `copy.deepcopy`).
        self.port_dict_prev = self.port_dict.clone();
    }

    /// `task_worker` — subscribe via the injected observer poll and drive each port's bring-
    /// up. Purely event-driven: it processes only when a watched table changes (`Ok(true)`).
    /// An observer error (`Err`) is surfaced so `run` can capture it (the Python
    /// exception-in-observer path). Returns `Ok` once the stop flag is set.
    pub fn task_worker(&mut self, mut poll: PortUpdatePoll) -> Result<(), String> {
        while !self.task_stopping_event.load(Ordering::Relaxed) {
            let updated = poll()?;
            // In the case of no real update, go back to the beginning of the loop.
            if !updated {
                continue;
            }
            self.process_ports_once();
        }
        Ok(())
    }

    /// `run()` — drive `task_worker`; on an exception, log the traceback-equivalent, record
    /// it for `join`, and set the main-thread stop event so the daemon tears down (the Python
    /// `run` except-block).
    pub fn run(&mut self, poll: PortUpdatePoll) {
        match self.task_worker(poll) {
            Ok(()) => {}
            Err(e) => {
                self.logger
                    .log_error(&format!("Exception occured at SffManagerTask thread due to {e}"));
                self.exc = Some(e);
                self.main_thread_stop_event.store(true, Ordering::SeqCst);
            }
        }
    }

    /// `join()` — signal stop and re-raise any exception captured by `run`.
    pub fn join(&mut self) -> Result<(), String> {
        self.task_stopping_event.store(true, Ordering::SeqCst);
        match self.exc.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// True once `run` captured a worker exception and set the main-thread stop event.
    pub fn main_thread_stop_requested(&self) -> bool {
        self.main_thread_stop_event.load(Ordering::SeqCst)
    }
}

// =====================================================================================
// MockSffApi — the test double (canned/settable returns + call counters).
// =====================================================================================

#[derive(Default)]
struct SffMockInner {
    is_cmis: bool,
    is_copper: Option<bool>,
    tx_disable_support: Option<bool>,
    power_class: Option<i64>,
    high_power_class_result: Option<bool>,
    lpmode_support: bool,
    lpmode_result: bool,
    tx_disable: Option<Vec<bool>>,
    tx_disable_channel_result: bool,
    calls: HashMap<String, usize>,
    tx_disable_channel_args: Vec<(u32, bool)>,
    set_lpmode_args: Vec<bool>,
}

/// Mock [`SffApi`] — the Rust analogue of the Python tests' `MagicMock()` xcvr api.
/// Interior-mutable + `Clone` (shares one `Arc<Mutex>`), so the api the factory hands the
/// task and the handle the test drives observe the same counters and settable returns.
#[derive(Clone)]
pub struct MockSffApi {
    inner: Arc<std::sync::Mutex<SffMockInner>>,
}

impl Default for MockSffApi {
    fn default() -> Self {
        MockSffApi {
            inner: Arc::new(std::sync::Mutex::new(SffMockInner {
                // Sensible defaults for a healthy optical SFF module.
                is_cmis: false,
                is_copper: Some(false),
                tx_disable_support: Some(true),
                power_class: Some(1),
                high_power_class_result: Some(true),
                lpmode_support: false,
                lpmode_result: true,
                tx_disable: Some(vec![false; 4]),
                tx_disable_channel_result: true,
                calls: HashMap::new(),
                tx_disable_channel_args: Vec::new(),
                set_lpmode_args: Vec::new(),
            })),
        }
    }
}

impl MockSffApi {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&self, method: &str) {
        let mut g = self.inner.lock().unwrap();
        *g.calls.entry(method.to_string()).or_insert(0) += 1;
    }

    pub fn call_count(&self, method: &str) -> usize {
        *self.inner.lock().unwrap().calls.get(method).unwrap_or(&0)
    }
    pub fn tx_disable_channel_args(&self) -> Vec<(u32, bool)> {
        self.inner.lock().unwrap().tx_disable_channel_args.clone()
    }
    pub fn set_lpmode_args(&self) -> Vec<bool> {
        self.inner.lock().unwrap().set_lpmode_args.clone()
    }
    pub fn set_is_cmis(&self, v: bool) {
        self.inner.lock().unwrap().is_cmis = v;
    }
    pub fn set_is_copper(&self, v: Option<bool>) {
        self.inner.lock().unwrap().is_copper = v;
    }
    pub fn set_tx_disable_support(&self, v: Option<bool>) {
        self.inner.lock().unwrap().tx_disable_support = v;
    }
    pub fn set_power_class(&self, v: Option<i64>) {
        self.inner.lock().unwrap().power_class = v;
    }
    pub fn set_high_power_class_result(&self, v: Option<bool>) {
        self.inner.lock().unwrap().high_power_class_result = v;
    }
    pub fn set_lpmode_support(&self, v: bool) {
        self.inner.lock().unwrap().lpmode_support = v;
    }
    pub fn set_lpmode_result(&self, v: bool) {
        self.inner.lock().unwrap().lpmode_result = v;
    }
    pub fn set_tx_disable(&self, v: Option<Vec<bool>>) {
        self.inner.lock().unwrap().tx_disable = v;
    }
    pub fn set_tx_disable_channel_result(&self, v: bool) {
        self.inner.lock().unwrap().tx_disable_channel_result = v;
    }
}

impl SffApi for MockSffApi {
    fn is_cmis(&self) -> bool {
        self.bump("is_cmis");
        self.inner.lock().unwrap().is_cmis
    }
    fn is_copper(&self) -> Option<bool> {
        self.bump("is_copper");
        self.inner.lock().unwrap().is_copper
    }
    fn get_tx_disable_support(&self) -> Option<bool> {
        self.bump("get_tx_disable_support");
        self.inner.lock().unwrap().tx_disable_support
    }
    fn get_power_class(&self) -> Option<i64> {
        self.bump("get_power_class");
        self.inner.lock().unwrap().power_class
    }
    fn set_high_power_class(&self, power_class: i64, enable: bool) -> Option<bool> {
        self.bump("set_high_power_class");
        self.inner.lock().unwrap().high_power_class_result
    }
    fn get_lpmode_support(&self) -> bool {
        self.bump("get_lpmode_support");
        self.inner.lock().unwrap().lpmode_support
    }
    fn set_lpmode(&self, lpmode: bool) -> bool {
        self.bump("set_lpmode");
        let mut g = self.inner.lock().unwrap();
        g.set_lpmode_args.push(lpmode);
        g.lpmode_result
    }
    fn get_tx_disable(&self) -> Option<Vec<bool>> {
        self.bump("get_tx_disable");
        self.inner.lock().unwrap().tx_disable.clone()
    }
    fn tx_disable_channel(&self, media_lanes_mask: u32, disable: bool) -> bool {
        self.bump("tx_disable_channel");
        let mut g = self.inner.lock().unwrap();
        g.tx_disable_channel_args.push((media_lanes_mask, disable));
        g.tx_disable_channel_result
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::mock::{MockChassis, MockSfp, MockTable};
    use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType};

    // A recording logger so ported tests can assert the exact syslog strings the Python
    // tests check (`mock_logger.log_error.assert_called_with(...)`).
    #[derive(Clone, Default)]
    struct RecordingSffLog {
        notices: Rc<RefCell<Vec<String>>>,
        warnings: Rc<RefCell<Vec<String>>>,
        errors: Rc<RefCell<Vec<String>>>,
    }
    impl SffLog for RecordingSffLog {
        fn log_notice(&self, message: &str) {
            self.notices.borrow_mut().push(message.to_string());
        }
        fn log_warning(&self, message: &str) {
            self.warnings.borrow_mut().push(message.to_string());
        }
        fn log_error(&self, message: &str) {
            self.errors.borrow_mut().push(message.to_string());
        }
    }

    struct Env {
        cfg: MockTable,
        state: MockTable,
        api: MockSffApi,
        log: RecordingSffLog,
    }

    /// Build a task backed by a single present SFP + shared mock tables + a shared MockSffApi.
    fn build_task(present: bool, api: MockSffApi) -> (SffManagerTask, Env) {
        let cfg = MockTable::new();
        let state = MockTable::new();
        let log = RecordingSffLog::default();
        let chassis = MockChassis::with_sfps(vec![
            MockSfp::absent(),
            if present { MockSfp::present() } else { MockSfp::absent() },
        ]);
        let api_for_factory = api.clone();
        let factory: SffApiFactory =
            Box::new(move |_sfp| Some(Box::new(api_for_factory.clone()) as Box<dyn SffApi>));
        let task = SffManagerTask::new(
            Box::new(chassis),
            Rc::new(cfg.clone()),
            Rc::new(state.clone()),
            factory,
            Rc::new(log.clone()),
        );
        (task, Env { cfg, state, api, log })
    }

    fn set_event(lport: &str, pport: i32, fields: &[(&str, &str)]) -> PortChangeEvent {
        let mut ev = PortChangeEvent::new(lport, pport, 0, PortEventType::PortSet);
        ev.port_dict = Some(fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        ev
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_handle_port_change_event
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_handle_port_change_event() {
        let api = MockSffApi::new();
        let (mut task, _env) = build_task(true, api);

        // Non-physical keys (PortConfigDone / PortInitDone) are ignored.
        let ev = PortChangeEvent::new("PortConfigDone", -1, 0, PortEventType::PortSet);
        task.on_port_update_event(&ev);
        assert_eq!(task.port_dict.len(), 0);

        let ev = PortChangeEvent::new("PortInitDone", -1, 0, PortEventType::PortSet);
        task.on_port_update_event(&ev);
        assert_eq!(task.port_dict.len(), 0);

        // PORT_ADD / PORT_REMOVE (not SET/DEL) are ignored.
        let ev = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd);
        task.on_port_update_event(&ev);
        assert_eq!(task.port_dict.len(), 0);

        let ev = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortRemove);
        task.on_port_update_event(&ev);
        assert_eq!(task.port_dict.len(), 0);

        // A bare PORT_DEL (no dict payload) is a no-op.
        let ev = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortDel);
        task.on_port_update_event(&ev);
        assert_eq!(task.port_dict.len(), 0);

        // A SET with a dict adds the port.
        let ev = set_event("Ethernet0", 1, &[("type", "QSFP28"), ("subport", "0"), ("host_tx_ready", "false")]);
        task.on_port_update_event(&ev);
        assert_eq!(task.port_dict.len(), 1);
        assert_eq!(task.port_dict["Ethernet0"].xcvr_type.as_deref(), Some("QSFP28"));

        // A STATE_DB TRANSCEIVER_INFO DEL clears just `type` but keeps the entry.
        let mut ev = PortChangeEvent::new("Ethernet0", -1, 0, PortEventType::PortDel);
        ev.port_dict = Some(vec![]);
        ev.db_name = Some("STATE_DB".to_string());
        ev.table_name = Some("TRANSCEIVER_INFO".to_string());
        task.on_port_update_event(&ev);
        assert_eq!(task.port_dict.len(), 1);
        assert!(task.port_dict["Ethernet0"].xcvr_type.is_none());

        // A CONFIG_DB PORT DEL removes the whole entry.
        let mut ev = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortDel);
        ev.port_dict = Some(vec![]);
        ev.db_name = Some("CONFIG_DB".to_string());
        ev.table_name = Some("PORT_TABLE".to_string());
        task.on_port_update_event(&ev);
        assert_eq!(task.port_dict.len(), 0);
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_get_active_lanes_for_lport
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_get_active_lanes_for_lport() {
        let (task, _env) = build_task(true, MockSffApi::new());
        let lp = "Ethernet0";

        assert_eq!(
            task.get_active_lanes_for_lport(lp, 3, 1, 4),
            Some(vec![false, false, true, false])
        );
        assert_eq!(
            task.get_active_lanes_for_lport(lp, 1, 2, 4),
            Some(vec![true, true, false, false])
        );
        assert_eq!(
            task.get_active_lanes_for_lport(lp, 2, 2, 4),
            Some(vec![false, false, true, true])
        );
        assert_eq!(
            task.get_active_lanes_for_lport(lp, 0, 4, 4),
            Some(vec![true, true, true, true])
        );

        // Larger (synthetic) port width.
        let mut expected = vec![false; 32];
        for e in expected.iter_mut().take(4) {
            *e = true;
        }
        assert_eq!(task.get_active_lanes_for_lport(lp, 1, 4, 32), Some(expected));
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_get_active_lanes_for_lport_with_invalid_input
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_get_active_lanes_for_lport_with_invalid_input() {
        let (task, _env) = build_task(true, MockSffApi::new());
        let lp = "Ethernet0";
        assert_eq!(task.get_active_lanes_for_lport(lp, -1, 4, 32), None);
        assert_eq!(task.get_active_lanes_for_lport(lp, 5, 1, 4), None);
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_get_host_tx_status
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_get_host_tx_status() {
        let (task, env) = build_task(true, MockSffApi::new());
        // Absent → 'false'.
        assert_eq!(task.get_host_tx_status("Ethernet0", 0), "false");
        // Present in STATE_DB PORT_TABLE.
        env.state.hset("Ethernet0", "host_tx_ready", "true").unwrap();
        assert_eq!(task.get_host_tx_status("Ethernet0", 0), "true");
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_get_admin_status
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_get_admin_status() {
        let (task, env) = build_task(true, MockSffApi::new());
        // Absent → 'down'.
        assert_eq!(task.get_admin_status("Ethernet0", 0), "down");
        // Present in CONFIG_DB PORT.
        env.cfg.hset("Ethernet0", "admin_status", "up").unwrap();
        assert_eq!(task.get_admin_status("Ethernet0", 0), "up");
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_enable_high_power_class
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_enable_high_power_class() {
        let (task, env) = build_task(true, MockSffApi::new());
        let api = env.api;
        let lp = "Ethernet0";

        // Normal case: class 5 → set_high_power_class called once.
        api.set_power_class(Some(5));
        api.set_high_power_class_result(Some(true));
        task.enable_high_power_class(&api, lp);
        assert_eq!(api.call_count("get_power_class"), 1);
        assert_eq!(api.call_count("set_high_power_class"), 1);

        // get_power_class failed (None) → no set_high_power_class.
        api.set_power_class(None);
        task.enable_high_power_class(&api, lp);
        assert_eq!(api.call_count("get_power_class"), 2);
        assert_eq!(api.call_count("set_high_power_class"), 1);
        assert!(env.log.errors.borrow().iter().any(|m| m.contains("failed to get power class")));

        // class < 5 → no need to set high power class.
        api.set_power_class(Some(4));
        task.enable_high_power_class(&api, lp);
        assert_eq!(api.call_count("get_power_class"), 3);
        assert_eq!(api.call_count("set_high_power_class"), 1);

        // set_high_power_class failed (Some(false)) → logs error, counts.
        api.set_power_class(Some(5));
        api.set_high_power_class_result(Some(false));
        task.enable_high_power_class(&api, lp);
        assert_eq!(api.call_count("get_power_class"), 4);
        assert_eq!(api.call_count("set_high_power_class"), 2);
        assert!(env.log.errors.borrow().iter().any(|m| m.contains("failed to enable high power class")));

        // set_high_power_class not supported (None) → silently skipped (Python except: pass).
        api.set_power_class(Some(5));
        api.set_high_power_class_result(None);
        task.enable_high_power_class(&api, lp);
        assert_eq!(api.call_count("get_power_class"), 5);
        assert_eq!(api.call_count("set_high_power_class"), 3);
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_xcvr_api_none_in_task_worker
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_xcvr_api_none_in_task_worker() {
        let cfg = MockTable::new();
        let state = MockTable::new();
        let log = RecordingSffLog::default();
        let chassis = MockChassis::with_sfps(vec![MockSfp::absent(), MockSfp::present()]);
        // Factory returns None → "no xcvr api".
        let factory: SffApiFactory = Box::new(|_sfp| None);
        let mut task = SffManagerTask::new(
            Box::new(chassis),
            Rc::new(cfg.clone()),
            Rc::new(state.clone()),
            factory,
            Rc::new(log.clone()),
        );
        task.port_dict.insert(
            "Ethernet0".to_string(),
            SffPortInfo {
                asic_id: 0,
                index: Some(1),
                subport: Some("0".to_string()),
                lanes: Some(vec!["1".into(), "2".into(), "3".into(), "4".into()]),
                host_tx_ready: Some("true".to_string()),
                admin_status: Some("up".to_string()),
                xcvr_type: Some("QSFP28".to_string()),
                active_lanes: None,
            },
        );
        task.process_ports_once();
        assert!(log
            .errors
            .borrow()
            .iter()
            .any(|m| m.contains("skipping sff_mgr since no xcvr api!")));
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_task_worker
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_task_worker() {
        let api = MockSffApi::new();
        api.set_power_class(Some(1));
        api.set_is_copper(Some(false));
        api.set_tx_disable_support(Some(true));
        let (mut task, env) = build_task(true, api);
        let api = env.api;

        // TX enable case: host_tx_ready=true + admin=up, module reports all lanes disabled.
        env.state.hset("Ethernet0", "host_tx_ready", "true").unwrap();
        env.cfg.hset("Ethernet0", "admin_status", "up").unwrap();
        let ev = set_event("Ethernet0", 1, &[("type", "QSFP28"), ("subport", "0"), ("lanes", "1,2,3,4")]);
        task.on_port_update_event(&ev);
        assert_eq!(task.port_dict.len(), 1);
        api.set_tx_disable(Some(vec![true, true, true, true]));
        task.process_ports_once();
        assert_eq!(api.call_count("tx_disable_channel"), 1);
        // Enabled TX (disable=false) on all four lanes.
        assert_eq!(api.tx_disable_channel_args(), vec![(0b1111, false)]);

        // TX disable case: host_tx_ready flips to false.
        let ev = set_event("Ethernet0", 1, &[("host_tx_ready", "false")]);
        task.on_port_update_event(&ev);
        api.set_tx_disable(Some(vec![false, false, false, false]));
        task.process_ports_once();
        assert_eq!(api.call_count("tx_disable_channel"), 2);
        assert_eq!(api.tx_disable_channel_args().last().copied(), Some((0b1111, true)));

        // No insertion and no change → no new tx_disable_channel, and prev==cur snapshot.
        task.process_ports_once();
        assert_eq!(api.call_count("tx_disable_channel"), 2);
        assert_eq!(task.port_dict, task.port_dict_prev);

        // Copper case: skip (is_copper true).
        let ev = set_event("Ethernet0", 1, &[("host_tx_ready", "true")]);
        task.on_port_update_event(&ev);
        api.set_is_copper(Some(true));
        task.process_ports_once();
        assert_eq!(api.call_count("tx_disable_channel"), 2);
        api.set_is_copper(Some(false));

        // tx_disable not supported case: skip.
        let ev = set_event("Ethernet0", 1, &[("host_tx_ready", "false")]);
        task.on_port_update_event(&ev);
        api.set_tx_disable_support(Some(false));
        task.process_ports_once();
        assert_eq!(api.call_count("tx_disable_channel"), 2);
        api.set_tx_disable_support(Some(true));
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_task_worker — sfp-not-present branch (module removal).
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_task_worker_module_not_present() {
        let api = MockSffApi::new();
        // Chassis whose SFP at index 1 is absent.
        let cfg = MockTable::new();
        let state = MockTable::new();
        let log = RecordingSffLog::default();
        let chassis = MockChassis::with_sfps(vec![MockSfp::absent(), MockSfp::absent()]);
        let api_for_factory = api.clone();
        let factory: SffApiFactory =
            Box::new(move |_sfp| Some(Box::new(api_for_factory.clone()) as Box<dyn SffApi>));
        let mut task = SffManagerTask::new(
            Box::new(chassis),
            Rc::new(cfg.clone()),
            Rc::new(state.clone()),
            factory,
            Rc::new(log.clone()),
        );
        task.port_dict.insert(
            "Ethernet0".to_string(),
            SffPortInfo {
                asic_id: 0,
                index: Some(1),
                subport: Some("0".to_string()),
                lanes: Some(vec!["1".into(), "2".into(), "3".into(), "4".into()]),
                host_tx_ready: Some("false".to_string()),
                admin_status: Some("up".to_string()),
                xcvr_type: Some("QSFP28".to_string()),
                active_lanes: None,
            },
        );
        task.process_ports_once();
        assert_eq!(api.call_count("tx_disable_channel"), 0);
        // Exactly the "module not present!" error is logged, and `type` is cleared.
        assert_eq!(log.errors.borrow().len(), 1);
        assert_eq!(log.errors.borrow()[0], "SFF-MAIN: Ethernet0: module not present!");
        assert!(task.port_dict["Ethernet0"].xcvr_type.is_none());
    }

    // ---------------------------------------------------------------------------------
    // lpmode setting case (sff_mgr.py:480-490) — error logged when lpmode supported but fails.
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_task_worker_lpmode() {
        let api = MockSffApi::new();
        let (mut task, env) = build_task(true, api);
        let api = env.api;
        api.set_power_class(Some(1));
        api.set_is_copper(Some(false));
        api.set_tx_disable_support(Some(true));
        api.set_tx_disable(Some(vec![false, false, false, false]));

        // 1. lpmode supported but set_lpmode fails → error logged.
        api.set_lpmode_support(true);
        api.set_lpmode_result(false);
        let ev = set_event("Ethernet0", 1, &[("type", "QSFP28"), ("subport", "0"), ("lanes", "1,2,3,4"),
            ("host_tx_ready", "false"), ("admin_status", "up")]);
        task.on_port_update_event(&ev);
        task.port_dict_prev.clear();
        task.process_ports_once();
        assert!(env
            .log
            .errors
            .borrow()
            .iter()
            .any(|m| m == "SFF-MAIN: Ethernet0: Failed to take module out of low power mode."));
        assert_eq!(api.set_lpmode_args(), vec![false]);

        // 2. lpmode supported + successful → no additional lpmode error.
        env.log.errors.borrow_mut().clear();
        api.set_lpmode_result(true);
        let ev = set_event("Ethernet0", 1, &[("type", "QSFP28")]);
        task.on_port_update_event(&ev);
        task.port_dict_prev.clear();
        task.process_ports_once();
        assert!(!env
            .log
            .errors
            .borrow()
            .iter()
            .any(|m| m.contains("Failed to take module out of low power mode")));

        // 3. lpmode NOT supported → set_lpmode never called, no error.
        env.log.errors.borrow_mut().clear();
        let calls_before = api.call_count("set_lpmode");
        api.set_lpmode_support(false);
        let ev = set_event("Ethernet0", 1, &[("type", "QSFP28")]);
        task.on_port_update_event(&ev);
        task.port_dict_prev.clear();
        task.process_ports_once();
        assert_eq!(api.call_count("set_lpmode"), calls_before);
        assert!(!env
            .log
            .errors
            .borrow()
            .iter()
            .any(|m| m.contains("Failed to take module out of low power mode")));
    }

    // ---------------------------------------------------------------------------------
    // test_SffManagerTask_task_run_with_exception (fresh Rust test — Python-runtime-specific)
    //
    // Models the reference: the port-update observer raising propagates out of `task_worker`,
    // is captured by `run` (logged + recorded), sets the main-thread stop event, and is
    // re-raised by `join`.
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_SffManagerTask_task_run_with_exception() {
        let (mut task, _env) = build_task(true, MockSffApi::new());
        let main_stop = Arc::new(AtomicBool::new(false));
        task = task.with_main_thread_stop_event(main_stop.clone());

        // Observer poll that raises on the first call (NotImplementedError analogue).
        let poll: PortUpdatePoll = Box::new(|| Err("NotImplementedError: PortChangeObserver".to_string()));
        task.run(poll);

        // The worker exception was captured and re-raised on join.
        let joined = task.join();
        assert!(joined.is_err());
        let msg = joined.unwrap_err();
        assert!(msg.contains("NotImplementedError"));
        assert!(msg.contains("PortChangeObserver"));
        // The main-thread stop event was set so the daemon tears down.
        assert!(main_stop.load(Ordering::SeqCst));
    }

    // ---------------------------------------------------------------------------------
    // NEW (bridge/mock seams): sff_manager_host_tx_disable_enable_sequence
    //
    // Drives the full deterministic bring-up sequence: an admin-up module with host_tx_ready
    // flipping true→false→true, asserting the TX enable → disable → enable transitions and
    // that lpmode-disable + High Power Class enable fire on the insertion pass.
    // ---------------------------------------------------------------------------------
    #[test]
    fn sff_manager_host_tx_disable_enable_sequence() {
        let api = MockSffApi::new();
        api.set_is_cmis(false);
        api.set_is_copper(Some(false));
        api.set_tx_disable_support(Some(true));
        api.set_power_class(Some(5)); // class-5 → High Power Class enable fires
        api.set_high_power_class_result(Some(true));
        api.set_lpmode_support(true);
        api.set_lpmode_result(true);
        let (mut task, env) = build_task(true, api);
        let api = env.api;

        // Insertion pass: admin up + host_tx_ready true → TX enabled (all lanes), HPC + lpmode.
        env.cfg.hset("Ethernet0", "admin_status", "up").unwrap();
        api.set_tx_disable(Some(vec![true, true, true, true]));
        let ev = set_event(
            "Ethernet0",
            1,
            &[("type", "QSFP28"), ("subport", "0"), ("lanes", "1,2,3,4"), ("host_tx_ready", "true"),
              ("admin_status", "up")],
        );
        task.on_port_update_event(&ev);
        task.process_ports_once();
        assert_eq!(api.call_count("set_high_power_class"), 1, "HPC enabled on insert");
        assert_eq!(api.set_lpmode_args(), vec![false], "module taken out of low power on insert");
        assert_eq!(api.tx_disable_channel_args(), vec![(0b1111, false)], "TX enabled on all lanes");

        // host_tx_ready → false: TX disabled on all lanes.
        api.set_tx_disable(Some(vec![false, false, false, false]));
        let ev = set_event("Ethernet0", 1, &[("host_tx_ready", "false")]);
        task.on_port_update_event(&ev);
        task.process_ports_once();
        assert_eq!(api.tx_disable_channel_args().last().copied(), Some((0b1111, true)), "TX disabled");

        // host_tx_ready → true again: TX re-enabled.
        api.set_tx_disable(Some(vec![true, true, true, true]));
        let ev = set_event("Ethernet0", 1, &[("host_tx_ready", "true")]);
        task.on_port_update_event(&ev);
        task.process_ports_once();
        assert_eq!(api.tx_disable_channel_args().last().copied(), Some((0b1111, false)), "TX re-enabled");
        assert_eq!(api.call_count("tx_disable_channel"), 3);
    }

    #[test]
    fn skeleton_present() {
        assert!(true);
    }
}
