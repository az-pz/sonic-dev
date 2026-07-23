//! Test doubles for the HAL and STATE_DB seams — the Rust counterpart of the
//! Python `tests/mock_platform.py` (inline MagicMock SFPs) and
//! `tests/mock_swsscommon.py` (`Table`). Gated `#[cfg(test)]` so it is compiled
//! only for `cargo test` (`tools/unit_test.sh`), never into the deployed daemon.
//!
//! - `MockSfp` / `MockHal` implement `hal::{SfpApi, Hal}` with programmable
//!   presence / identity / DOM / status / threshold values and a scripted
//!   `get_change_event` queue. A `None` dict field yields `HalError::NotImplemented`
//!   (mirrors `MagicMock.side_effect = NotImplementedError`).
//! - `MockTable` / `MockStateDb` implement `statedb::{TableApi, StateDb}` as an
//!   in-memory `HashMap` (a direct port of `mock_swsscommon.Table`), so a unit
//!   test reads back exactly what the daemon wrote. Handles to the same table
//!   name share one backing store.
//!
//! Unlike the daemon-logic modules, the mock carries REAL (simple) logic — it is
//! test infrastructure, not the ported daemon.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use serde_json::Value;

use crate::hal::{ChangeEvent, Hal, HalError, SfpApi};
use crate::statedb::{DbError, Row, StateDb, TableApi};
use crate::xcvrd_utilities::port_event_helper::{
    PortEventSource, RawPortEvent, SelectResult, SubMeta,
};

// --------------------------------------------------------------------------
// Mock HAL
// --------------------------------------------------------------------------

/// Outcome of `get_xcvr_api().is_flat_memory()` for `MockSfp::is_flat_memory`,
/// modelling the branches `XCVRDUtils.is_transceiver_flat_memory` /
/// `_wrapper_is_flat_memory` handle (api present flat/paged, no api, or error).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlatMem {
    /// api present, flat memory (SFF).
    Flat,
    /// api present, paged memory (CMIS).
    Paged,
    /// `get_xcvr_api()` returned `None`.
    NoApi,
    /// `NotImplementedError` / `KeyError`.
    NotImpl,
}

/// A programmable transceiver slot. Clone-returned by `MockHal::sfp`.
#[derive(Clone)]
pub struct MockSfp {
    pub presence: bool,
    pub replaceable: bool,
    pub reset_status: bool,
    /// Low-power mode. `Some(true/false)` reflects `get_lpmode`; `None` models the
    /// platform not answering (`get_lpmode` raising / returning None) -> `Err`.
    /// `set_lpmode` persists here (interior mutability) so a control-path test can
    /// assert `set_lpmode` -> `get_lpmode` round-trips on this instance.
    pub lpmode: Cell<Option<bool>>,
    pub sfp_type: String,
    pub error_description: Option<String>,
    /// `Some(v)` -> `Ok(v)`; `None` -> `Err(NotImplemented)` (empty-dict path).
    pub info: Option<Value>,
    pub dom_real_value: Option<Value>,
    pub status: Option<Value>,
    pub threshold_info: Option<Value>,
    /// Backs `is_flat_memory` (defaults to `Paged`: the emulator module is CMIS).
    pub flat_memory: FlatMem,
    /// Number of `write_eeprom` calls seen — lets a test assert the daemon's
    /// lpmode read path issues no EEPROM writes, so it can never clobber the
    /// sfputil/plugin's CMIS ModuleGlobalControls (00h:26) writes.
    pub eeprom_writes: Cell<usize>,
}

impl Default for MockSfp {
    fn default() -> Self {
        Self {
            presence: false,
            replaceable: true,
            reset_status: false,
            lpmode: Cell::new(Some(false)),
            sfp_type: "QSFP_DD".to_string(),
            error_description: None,
            info: None,
            dom_real_value: None,
            status: None,
            threshold_info: None,
            flat_memory: FlatMem::Paged,
            eeprom_writes: Cell::new(0),
        }
    }
}

impl MockSfp {
    /// A present module carrying the given identity dict (`TRANSCEIVER_INFO`).
    pub fn present(info: Value) -> Self {
        Self { presence: true, info: Some(info), ..Self::default() }
    }
}

fn dict_or_not_impl(v: &Option<Value>) -> Result<Value, HalError> {
    v.clone().ok_or(HalError::NotImplemented)
}

impl SfpApi for MockSfp {
    fn get_presence(&self) -> Result<bool, HalError> {
        Ok(self.presence)
    }
    fn is_replaceable(&self) -> Result<bool, HalError> {
        Ok(self.replaceable)
    }
    fn get_reset_status(&self) -> Result<bool, HalError> {
        Ok(self.reset_status)
    }
    fn sfp_type(&self) -> Result<String, HalError> {
        Ok(self.sfp_type.clone())
    }
    fn get_error_description(&self) -> Result<Option<String>, HalError> {
        Ok(self.error_description.clone())
    }
    fn get_transceiver_info(&self) -> Result<Value, HalError> {
        dict_or_not_impl(&self.info)
    }
    fn is_flat_memory(&self) -> Result<Option<bool>, HalError> {
        match self.flat_memory {
            FlatMem::Flat => Ok(Some(true)),
            FlatMem::Paged => Ok(Some(false)),
            FlatMem::NoApi => Ok(None),
            FlatMem::NotImpl => Err(HalError::NotImplemented),
        }
    }
    fn get_transceiver_dom_real_value(&self) -> Result<Value, HalError> {
        dict_or_not_impl(&self.dom_real_value)
    }
    fn get_transceiver_status(&self) -> Result<Value, HalError> {
        dict_or_not_impl(&self.status)
    }
    fn get_transceiver_threshold_info(&self) -> Result<Value, HalError> {
        dict_or_not_impl(&self.threshold_info)
    }
    fn get_lpmode(&self) -> Result<bool, HalError> {
        // `None` models the platform not answering (Python `get_lpmode` returning
        // None / raising) -> Err; is_transceiver_lpmode_on collapses that to off.
        self.lpmode.get().ok_or(HalError::NotImplemented)
    }
    fn set_lpmode(&self, on: bool) -> Result<bool, HalError> {
        // Persist so set_lpmode -> get_lpmode round-trips (M4 lpmode-state).
        self.lpmode.set(Some(on));
        Ok(true)
    }
    fn reset(&self) -> Result<bool, HalError> {
        Ok(true)
    }
    fn read_eeprom(&self, _offset: usize, _num_bytes: usize) -> Result<Option<Vec<u8>>, HalError> {
        Ok(None)
    }
    fn write_eeprom(&self, _offset: usize, _data: &[u8]) -> Result<bool, HalError> {
        self.eeprom_writes.set(self.eeprom_writes.get() + 1);
        Ok(true)
    }
}

/// A programmable chassis of `MockSfp`s plus a scripted change-event queue.
pub struct MockHal {
    pub sfps: Vec<MockSfp>,
    events: RefCell<VecDeque<ChangeEvent>>,
}

impl MockHal {
    pub fn new(sfps: Vec<MockSfp>) -> Self {
        Self { sfps, events: RefCell::new(VecDeque::new()) }
    }

    /// `n` empty (absent) slots.
    pub fn with_ports(n: usize) -> Self {
        Self::new(vec![MockSfp::default(); n])
    }

    /// Queue a change event to be returned by the next `get_change_event`.
    pub fn push_event(&self, ev: ChangeEvent) {
        self.events.borrow_mut().push_back(ev);
    }
}

impl Hal for MockHal {
    type Sfp = MockSfp;

    fn num_sfps(&self) -> Result<usize, HalError> {
        Ok(self.sfps.len())
    }

    fn sfp(&self, index: usize) -> Result<MockSfp, HalError> {
        self.sfps
            .get(index)
            .cloned()
            .ok_or_else(|| HalError::Mock(format!("no sfp at index {index}")))
    }

    fn get_change_event(&self, _timeout_ms: u64) -> Result<ChangeEvent, HalError> {
        Ok(self.events.borrow_mut().pop_front().unwrap_or_default())
    }
}

// --------------------------------------------------------------------------
// Mock STATE_DB  (direct port of tests/mock_swsscommon.py Table)
// --------------------------------------------------------------------------

type Store = Rc<RefCell<HashMap<String, HashMap<String, Row>>>>;

/// In-memory STATE_DB. Handles to the same table name share one backing store.
#[derive(Clone, Default)]
pub struct MockStateDb {
    store: Store,
}

impl MockStateDb {
    pub fn new() -> Self {
        Self::default()
    }
}

impl StateDb for MockStateDb {
    type Table = MockTable;

    fn table(&self, name: &str) -> Result<MockTable, DbError> {
        self.store
            .borrow_mut()
            .entry(name.to_string())
            .or_default();
        Ok(MockTable { store: self.store.clone(), name: name.to_string() })
    }
}

/// One in-memory table; every op goes through the shared store keyed by name.
pub struct MockTable {
    store: Store,
    name: String,
}

impl TableApi for MockTable {
    fn set(&self, key: &str, fields: &Row) -> Result<(), DbError> {
        let mut store = self.store.borrow_mut();
        let tbl = store.entry(self.name.clone()).or_default();
        tbl.insert(key.to_string(), fields.clone());
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<Row>, DbError> {
        let store = self.store.borrow();
        Ok(store.get(&self.name).and_then(|t| t.get(key)).cloned())
    }

    fn hget(&self, key: &str, field: &str) -> Result<Option<String>, DbError> {
        Ok(self.get(key)?.and_then(|row| row.get(field).cloned()))
    }

    fn hdel(&self, key: &str, field: &str) -> Result<(), DbError> {
        let mut store = self.store.borrow_mut();
        if let Some(tbl) = store.get_mut(&self.name) {
            if let Some(row) = tbl.get_mut(key) {
                row.remove(field);
                if row.is_empty() {
                    tbl.remove(key);
                }
            }
        }
        Ok(())
    }

    fn del(&self, key: &str) -> Result<(), DbError> {
        let mut store = self.store.borrow_mut();
        if let Some(tbl) = store.get_mut(&self.name) {
            tbl.remove(key);
        }
        Ok(())
    }

    fn keys(&self) -> Result<Vec<String>, DbError> {
        let store = self.store.borrow();
        Ok(store
            .get(&self.name)
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default())
    }
}

// --------------------------------------------------------------------------
// Mock runtime PORT-change source (scripted Select + SubscriberStateTable.pop)
// --------------------------------------------------------------------------

/// A scripted `PortEventSource`: `selects` feeds `select()` (FIFO; `Object` once
/// drained) and `tables` feeds one `drain_tables()` (consumed, so a follow-up call
/// with no re-script yields nothing). Mirrors the Python tests scripting
/// `swsscommon.Select.select` + `SubscriberStateTable.pop` side effects.
#[derive(Default)]
pub struct MockPortEventSource {
    pub selects: VecDeque<SelectResult>,
    pub tables: Vec<(SubMeta, Vec<RawPortEvent>)>,
}

impl MockPortEventSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue the next `select()` result.
    pub fn push_select(&mut self, result: SelectResult) {
        self.selects.push_back(result);
    }

    /// Script one subscribed table's pending events for the next `drain_tables()`.
    pub fn set_table(&mut self, meta: SubMeta, events: Vec<RawPortEvent>) {
        self.tables = vec![(meta, events)];
    }
}

impl PortEventSource for MockPortEventSource {
    fn select(&mut self, _timeout_ms: u64) -> SelectResult {
        self.selects.pop_front().unwrap_or(SelectResult::Object)
    }

    fn drain_tables(&mut self) -> Vec<(SubMeta, Vec<RawPortEvent>)> {
        std::mem::take(&mut self.tables)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn mock_table_roundtrip() {
        // Mirrors mock_swsscommon.Table: set -> get/hget -> del.
        let db = MockStateDb::new();
        let tbl = db.table("TRANSCEIVER_INFO").unwrap();
        tbl.set("Ethernet100", &row(&[("manufacturer", "xcvr-emu"), ("model", "EMU-40G-LR4")]))
            .unwrap();

        assert_eq!(tbl.hget("Ethernet100", "manufacturer").unwrap().as_deref(), Some("xcvr-emu"));
        assert_eq!(tbl.get("Ethernet100").unwrap().unwrap().len(), 2);
        assert_eq!(tbl.keys().unwrap(), vec!["Ethernet100".to_string()]);

        tbl.del("Ethernet100").unwrap();
        assert!(tbl.get("Ethernet100").unwrap().is_none());
    }

    #[test]
    fn mock_table_shares_store_across_handles() {
        // A second handle to the same name (the daemon vs. the test) sees writes.
        let db = MockStateDb::new();
        db.table("TRANSCEIVER_STATUS_SW")
            .unwrap()
            .set("Ethernet0", &row(&[("status", "1")]))
            .unwrap();
        let reader = db.table("TRANSCEIVER_STATUS_SW").unwrap();
        assert_eq!(reader.hget("Ethernet0", "status").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn mock_hal_present_and_not_implemented() {
        let hal = MockHal::new(vec![MockSfp::present(json!({"model": "EMU-40G-LR4"}))]);
        assert_eq!(hal.num_sfps().unwrap(), 1);
        let sfp = hal.sfp(0).unwrap();
        assert!(sfp.get_presence().unwrap());
        assert_eq!(sfp.get_transceiver_info().unwrap()["model"], "EMU-40G-LR4");
        // Unprogrammed dict getter models NotImplementedError.
        assert!(matches!(sfp.get_transceiver_status(), Err(HalError::NotImplemented)));
    }

    #[test]
    fn mock_table_hdel_removes_empty_row() {
        // Ports mock_swsscommon.Table.hdel (lines 27-28): deleting the LAST field
        // of a row removes the row/key entirely; deleting one of several leaves the
        // rest. The daemon relies on this when clearing STATUS_SW/DOM fields.
        let db = MockStateDb::new();
        let tbl = db.table("TRANSCEIVER_STATUS_SW").unwrap();
        tbl.set("Ethernet0", &row(&[("status", "1"), ("cmis_state", "READY")]))
            .unwrap();

        tbl.hdel("Ethernet0", "cmis_state").unwrap();
        // Row still present with the remaining field.
        assert_eq!(tbl.hget("Ethernet0", "status").unwrap().as_deref(), Some("1"));
        assert!(tbl.hget("Ethernet0", "cmis_state").unwrap().is_none());

        tbl.hdel("Ethernet0", "status").unwrap();
        // Last field gone -> whole row/key removed (get_size_for_key == 0 -> _del).
        assert!(tbl.get("Ethernet0").unwrap().is_none());
        assert!(tbl.keys().unwrap().is_empty());
    }

    #[test]
    fn mock_hal_change_event_queue() {
        // The scripted get_change_event queue is the Rust analogue of the Python
        // tests' _wrapper_get_transceiver_change_event side_effect list: pushed
        // events pop in FIFO order, then a default (empty) event is returned so the
        // event loop sees no spurious presence changes once the script is drained.
        let hal = MockHal::with_ports(2);
        let mut insert = ChangeEvent::default();
        insert.status = true;
        insert.sfp.insert("0".to_string(), "1".to_string());
        hal.push_event(insert);

        let ev = hal.get_change_event(0).unwrap();
        assert!(ev.status);
        assert_eq!(ev.sfp.get("0").map(String::as_str), Some("1"));

        // Queue drained -> default empty event.
        let empty = hal.get_change_event(0).unwrap();
        assert!(empty.sfp.is_empty());
    }

    #[test]
    fn mock_sfp_lpmode_set_get_roundtrip() {
        // MockSfp models the CMIS lpmode control path: set_lpmode persists so
        // get_lpmode reflects it (set_lpmode -> get_lpmode round-trip). An unset
        // (`None`) lpmode models the platform not answering -> Err(NotImplemented)
        // (Python `get_lpmode` raising / returning None). Neither set nor get is an
        // EEPROM write, so the 00h:26 MGC bits are never touched by this path.
        let sfp = MockSfp::default();
        assert!(!sfp.get_lpmode().unwrap()); // default: off
        sfp.set_lpmode(true).unwrap();
        assert!(sfp.get_lpmode().unwrap());
        sfp.set_lpmode(false).unwrap();
        assert!(!sfp.get_lpmode().unwrap());
        sfp.lpmode.set(None);
        assert!(matches!(sfp.get_lpmode(), Err(HalError::NotImplemented)));
        assert_eq!(sfp.eeprom_writes.get(), 0);
    }
}
