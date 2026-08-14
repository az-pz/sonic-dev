//! Test doubles for the HAL + DB seams, the Rust analogue of
//! `tests/mock_platform.py` (canned SFPs) and `tests/mock_swsscommon.py` (a
//! dict-backed `Table`). Compiled only under `cfg(test)`; the real daemon path
//! (`daemon::run → env::open_* → platform_bridge/swss_common`) is untouched.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

use serde_json::Value;

use crate::db::{DbResult, StateDb, Table};
use crate::hal::{ChangeEvent, Chassis, HalResult, Sfp};

/// A programmable SFP: canned identity/DOM/status `Value`s + presence flag,
/// standing in for `@patch('xcvrd...._wrapper_get_transceiver_info', ...)`.
#[derive(Clone, Default)]
pub struct MockSfp {
    pub present: bool,
    pub replaceable: bool,
    pub reset_status: bool,
    pub sfp_type: String,
    pub error_description: Option<String>,
    pub info: Value,
    pub dom: Value,
    pub status: Value,
    pub thresholds: Value,
    pub lpmode: RefCell<bool>,
    /// Byte-addressed EEPROM store (linear optoe offset → byte), backing
    /// `read_eeprom`/`write_eeprom` so the CMIS control seam can be exercised with a
    /// mock. Unset offsets read back as `0` (the emulator's cleared default). Shared via
    /// `Rc` so a clone (e.g. the handle a `BridgeCmisApi` takes ownership of) observes the
    /// same control writes the test inspects — mirroring [`MockTable`]'s shared rows.
    pub eeprom: Rc<RefCell<BTreeMap<usize, u8>>>,
    /// Canned results for `call_json(method)` (dom_flags, status_flags, pm, …).
    pub json_calls: BTreeMap<String, Value>,
    /// Methods that should raise (return `Err`) — the analogue of
    /// `mock_sfp.<method>.side_effect = NotImplementedError`.
    pub err_methods: RefCell<BTreeSet<String>>,
}

impl MockSfp {
    pub fn present_with_info(info: Value) -> Self {
        MockSfp { present: true, replaceable: true, info, ..Default::default() }
    }
    /// A present module whose identity read fails (EEPROM not ready): mirrors the
    /// emulator's FAULT_READ where `get_transceiver_info()` returns Python `None`.
    pub fn present_eeprom_not_ready() -> Self {
        MockSfp { present: true, replaceable: true, info: Value::Null, ..Default::default() }
    }
    pub fn absent() -> Self {
        MockSfp { present: false, ..Default::default() }
    }
    /// A present, replaceable module with an empty EEPROM (all bytes read `0`).
    pub fn present() -> Self {
        MockSfp { present: true, replaceable: true, ..Default::default() }
    }
    /// Chainable builder: seed one EEPROM byte at a linear offset (page*128+off).
    pub fn with_eeprom(self, offset: usize, byte: u8) -> Self {
        self.eeprom.borrow_mut().insert(offset, byte);
        self
    }
    /// Script a `call_json(method)` result (e.g. `get_temperature`, dom flags).
    pub fn set_json_call(&mut self, method: &str, value: Value) {
        self.json_calls.insert(method.to_string(), value);
    }
    /// Make `method` raise (`NotImplementedError` analogue) on the next call.
    pub fn fail_method(&mut self, method: &str) {
        self.err_methods.borrow_mut().insert(method.to_string());
    }
    fn should_fail(&self, method: &str) -> bool {
        self.err_methods.borrow().contains(method)
    }
}

impl Sfp for MockSfp {
    fn get_presence(&self) -> HalResult<bool> {
        if self.should_fail("get_presence") {
            return Err("get_presence not implemented".to_string());
        }
        Ok(self.present)
    }
    fn is_replaceable(&self) -> HalResult<bool> { Ok(self.replaceable) }
    fn get_reset_status(&self) -> HalResult<bool> { Ok(self.reset_status) }
    fn sfp_type(&self) -> HalResult<String> { Ok(self.sfp_type.clone()) }
    fn get_error_description(&self) -> HalResult<Option<String>> { Ok(self.error_description.clone()) }
    fn get_transceiver_info(&self) -> HalResult<Value> { Ok(self.info.clone()) }
    fn get_transceiver_dom_real_value(&self) -> HalResult<Value> {
        if self.should_fail("get_transceiver_dom_real_value") {
            return Err("get_transceiver_dom_real_value not implemented".to_string());
        }
        Ok(self.dom.clone())
    }
    fn get_transceiver_status(&self) -> HalResult<Value> { Ok(self.status.clone()) }
    fn get_transceiver_threshold_info(&self) -> HalResult<Value> {
        if self.should_fail("get_transceiver_threshold_info") {
            return Err("get_transceiver_threshold_info not implemented".to_string());
        }
        Ok(self.thresholds.clone())
    }
    fn get_lpmode(&self) -> HalResult<bool> {
        if self.should_fail("get_lpmode") {
            return Err("get_lpmode not implemented".to_string());
        }
        Ok(*self.lpmode.borrow())
    }
    fn set_lpmode(&self, on: bool) -> HalResult<bool> { *self.lpmode.borrow_mut() = on; Ok(true) }
    fn reset(&self) -> HalResult<bool> { Ok(true) }
    fn read_eeprom(&self, offset: usize, num_bytes: usize) -> HalResult<Option<Vec<u8>>> {
        if self.should_fail("read_eeprom") {
            return Err("read_eeprom not implemented".to_string());
        }
        let m = self.eeprom.borrow();
        let out = (offset..offset + num_bytes).map(|i| *m.get(&i).unwrap_or(&0)).collect();
        Ok(Some(out))
    }
    fn write_eeprom(&self, offset: usize, data: &[u8]) -> HalResult<bool> {
        if self.should_fail("write_eeprom") {
            return Err("write_eeprom not implemented".to_string());
        }
        let mut m = self.eeprom.borrow_mut();
        for (i, b) in data.iter().enumerate() {
            m.insert(offset + i, *b);
        }
        Ok(true)
    }
    fn call_json(&self, method: &str) -> HalResult<Value> {
        if self.should_fail(method) {
            return Err(format!("{method} not implemented"));
        }
        Ok(self.json_calls.get(method).cloned().unwrap_or(Value::Null))
    }
}

/// A chassis of [`MockSfp`]s plus a scripted change-event queue (FIFO).
#[derive(Default)]
pub struct MockChassis {
    pub sfps: Vec<MockSfp>,
    pub change_events: RefCell<VecDeque<ChangeEvent>>,
}

impl MockChassis {
    pub fn with_sfps(sfps: Vec<MockSfp>) -> Self {
        MockChassis { sfps, change_events: RefCell::new(VecDeque::new()) }
    }
    pub fn push_change_event(&self, ev: ChangeEvent) {
        self.change_events.borrow_mut().push_back(ev);
    }
}

impl Chassis for MockChassis {
    fn num_sfps(&self) -> HalResult<usize> { Ok(self.sfps.len()) }
    fn sfp(&self, index: usize) -> HalResult<Box<dyn Sfp>> {
        self.sfps.get(index).cloned().map(|s| Box::new(s) as Box<dyn Sfp>)
            .ok_or_else(|| format!("no sfp at index {index}"))
    }
    fn get_change_event(&self, _timeout_ms: u64) -> HalResult<ChangeEvent> {
        Ok(self.change_events.borrow_mut().pop_front().unwrap_or_default())
    }
}

/// Shared, dict-backed row store for a [`MockTable`] so every handle minted for a
/// given table name (including snapshots) observes the same writes.
#[derive(Default)]
struct MockTableInner {
    rows: RefCell<BTreeMap<String, BTreeMap<String, String>>>,
    set_count: Cell<usize>,
    del_count: Cell<usize>,
}

/// A `BTreeMap`-backed STATE_DB table: `{key -> {field -> value}}`, mirroring
/// `tests/mock_swsscommon.py`. Cloning shares the underlying rows (via `Rc`) so a
/// producer handle and a test snapshot observe the same state.
#[derive(Default, Clone)]
pub struct MockTable {
    inner: Rc<MockTableInner>,
}

impl MockTable {
    pub fn new() -> Self { MockTable::default() }

    /// Whole-row `set(...)` call count (the Python producers call `table.set`).
    pub fn set_count(&self) -> usize { self.inner.set_count.get() }
    /// `_del(...)` call count.
    pub fn del_count(&self) -> usize { self.inner.del_count.get() }
    /// True iff a row exists for `key`.
    pub fn contains(&self, key: &str) -> bool { self.inner.rows.borrow().contains_key(key) }
    /// Snapshot of one row's fields (for assertions).
    pub fn row(&self, key: &str) -> Option<BTreeMap<String, String>> {
        self.inner.rows.borrow().get(key).cloned()
    }
    /// Convenience: read a single field.
    pub fn field(&self, key: &str, field: &str) -> Option<String> {
        self.inner.rows.borrow().get(key).and_then(|r| r.get(field).cloned())
    }
    /// Number of fields stored under `key` (Python `mock_swsscommon.Table.get_size_for_key`).
    pub fn get_size_for_key(&self, key: &str) -> usize {
        self.inner.rows.borrow().get(key).map_or(0, |r| r.len())
    }
}

impl Table for MockTable {
    fn set(&self, key: &str, fvs: &[(String, String)]) -> DbResult<()> {
        self.inner.set_count.set(self.inner.set_count.get() + 1);
        let mut rows = self.inner.rows.borrow_mut();
        let row = rows.entry(key.to_string()).or_default();
        for (f, v) in fvs { row.insert(f.clone(), v.clone()); }
        Ok(())
    }
    fn get(&self, key: &str) -> DbResult<Option<Vec<(String, String)>>> {
        Ok(self.inner.rows.borrow().get(key)
            .map(|r| r.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
    }
    fn hget(&self, key: &str, field: &str) -> DbResult<Option<String>> {
        Ok(self.inner.rows.borrow().get(key).and_then(|r| r.get(field).cloned()))
    }
    fn hset(&self, key: &str, field: &str, value: &str) -> DbResult<()> {
        self.inner.rows.borrow_mut().entry(key.to_string()).or_default()
            .insert(field.to_string(), value.to_string());
        Ok(())
    }
    fn hdel(&self, key: &str, field: &str) -> DbResult<()> {
        if let Some(r) = self.inner.rows.borrow_mut().get_mut(key) { r.remove(field); }
        Ok(())
    }
    fn del(&self, key: &str) -> DbResult<()> {
        self.inner.del_count.set(self.inner.del_count.get() + 1);
        self.inner.rows.borrow_mut().remove(key);
        Ok(())
    }
    fn get_keys(&self) -> DbResult<Vec<String>> {
        Ok(self.inner.rows.borrow().keys().cloned().collect())
    }
    fn get_size(&self) -> DbResult<usize> {
        Ok(self.inner.rows.borrow().len())
    }
}

/// A `StateDb` whose tables are shared `MockTable`s so a test can inspect what a
/// producer wrote (handles minted for the same name share rows).
#[derive(Default)]
pub struct MockStateDb {
    pub tables: RefCell<BTreeMap<String, MockTable>>,
}

impl MockStateDb {
    pub fn new() -> Self { MockStateDb::default() }
    /// Snapshot handle for a table (shares rows with what the producer wrote).
    pub fn table_snapshot(&self, name: &str) -> MockTable {
        self.tables.borrow_mut().entry(name.to_string()).or_default().clone()
    }
}

impl StateDb for MockStateDb {
    fn table(&self, name: &str) -> DbResult<Rc<dyn Table>> {
        let t = self.tables.borrow_mut().entry(name.to_string()).or_default().clone();
        Ok(Rc::new(t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_table_roundtrip() {
        let t = MockTable::new();
        t.hset("TRANSCEIVER_INFO|Ethernet0", "model", "EMU-40G-LR4").unwrap();
        assert_eq!(t.hget("TRANSCEIVER_INFO|Ethernet0", "model").unwrap().as_deref(), Some("EMU-40G-LR4"));
        assert_eq!(t.get_size().unwrap(), 1);
        t.del("TRANSCEIVER_INFO|Ethernet0").unwrap();
        assert_eq!(t.get_size().unwrap(), 0);
    }

    #[test]
    fn mock_table_clone_shares_rows() {
        // A handle minted by MockStateDb and a snapshot must see the same writes.
        let db = MockStateDb::new();
        let handle = db.table("TRANSCEIVER_INFO").unwrap();
        handle.set("Ethernet0", &[("model".into(), "EMU".into())]).unwrap();
        let snap = db.table_snapshot("TRANSCEIVER_INFO");
        assert_eq!(snap.field("Ethernet0", "model").as_deref(), Some("EMU"));
        assert_eq!(snap.set_count(), 1);
    }

    #[test]
    fn mock_sfp_reports_canned_identity() {
        let sfp = MockSfp::present_with_info(serde_json::json!({"model": "EMU-40G-LR4"}));
        assert!(sfp.get_presence().unwrap());
        assert_eq!(sfp.get_transceiver_info().unwrap()["model"], "EMU-40G-LR4");
    }
}
