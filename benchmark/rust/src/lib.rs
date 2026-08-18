//! Call-trace recording decorators for the xcvrd benchmark harness.
//!
//! These wrap the daemon's two seams -- [`Hal`]/[`SfpHandle`] (platform side) and
//! [`DbTable`] (STATE_DB side) -- and record every call as a JSONL record per
//! `benchmark/schema/trace.md`, then delegate to the wrapped implementation.
//!
//! Why a decorator rather than counters inside the mocks: the target crate's
//! `MockSfp` already counts a few things (`presence_calls`, `eeprom_writes`,
//! `call_log`) but not the other getters, and `recodeAgent/results/*` are recorded
//! pipeline artifacts that must stay immutable. Wrapping the *traits* instead gives
//! full coverage without touching the target, and -- because it is generic over the
//! trait, not the mock -- the same recorder can wrap `BridgeHal` on a live DUT. That
//! is what makes the Mock-vs-Bridge delta (the PyO3 + GIL tax) measurable with one
//! piece of code.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use xcvrd_rs::db::DbTable;
use xcvrd_rs::error::Result;
use xcvrd_rs::hal::{ChangeEvent, Hal, SfpHandle};

/// Thread-safe JSONL trace sink shared by every decorator in a run.
///
/// `seq` is global and monotonic so a single-threaded scenario can be compared
/// with `--strict-order`; concurrent scenarios interleave nondeterministically and
/// are compared as multisets instead (see `schema/trace.md`).
#[derive(Default)]
pub struct Recorder {
    seq: AtomicU64,
    lines: Mutex<Vec<String>>,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Recorder::default())
    }

    fn push(&self, mut rec: Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Some(obj) = rec.as_object_mut() {
            obj.insert("seq".to_string(), json!(seq));
        }
        // Serialize inside the lock so the Vec order matches `seq` order for
        // single-threaded runs; contention here is irrelevant because tracing runs
        // are never the runs we time.
        if let Ok(mut lines) = self.lines.lock() {
            lines.push(rec.to_string());
        }
    }

    pub fn hal(&self, port: usize, op: &str) {
        self.push(json!({"kind": "hal", "port": port, "op": op}));
    }

    pub fn hal_global(&self, op: &str) {
        self.push(json!({"kind": "hal", "op": op}));
    }

    pub fn db(&self, table: &str, key: &str, op: &str, nfields: Option<usize>) {
        match nfields {
            Some(n) => self.push(json!({"kind":"db","table":table,"key":key,"op":op,"nfields":n})),
            None => self.push(json!({"kind":"db","table":table,"key":key,"op":op})),
        }
    }

    pub fn eeprom(&self, port: usize, write: bool, offset: usize, len: usize) {
        let kind = if write { "eeprom_write" } else { "eeprom_read" };
        self.push(json!({"kind": kind, "port": port, "offset": offset, "len": len}));
    }

    /// The recorded trace as JSONL (one record per line, no trailing newline).
    pub fn to_jsonl(&self) -> String {
        self.lines.lock().map(|l| l.join("\n")).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.lines.lock().map(|l| l.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// Platform seam
// ---------------------------------------------------------------------------

/// Wraps any [`Hal`], recording `num_sfps` / `sfp` / `get_change_event` and handing
/// out [`CountingSfp`] handles so per-port calls are attributed to their slot.
pub struct CountingHal {
    inner: Arc<dyn Hal>,
    rec: Arc<Recorder>,
}

impl CountingHal {
    pub fn new(inner: Arc<dyn Hal>, rec: Arc<Recorder>) -> Self {
        CountingHal { inner, rec }
    }
}

impl Hal for CountingHal {
    fn num_sfps(&self) -> Result<usize> {
        self.rec.hal_global("num_sfps");
        self.inner.num_sfps()
    }

    fn sfp(&self, index: usize) -> Result<Box<dyn SfpHandle>> {
        self.rec.hal(index, "sfp");
        let inner = self.inner.sfp(index)?;
        Ok(Box::new(CountingSfp {
            inner,
            port: index,
            rec: self.rec.clone(),
        }))
    }

    fn get_change_event(&self, timeout_ms: u64) -> Result<ChangeEvent> {
        self.rec.hal_global("get_change_event");
        self.inner.get_change_event(timeout_ms)
    }
}

/// Per-slot handle recording all 15 [`SfpHandle`] operations.
///
/// EEPROM traffic is recorded as its own record kind (with offset/length) rather
/// than a bare call count: read amplification and the exact register set touched
/// are the highest-signal, most machine-independent comparison the harness makes.
pub struct CountingSfp {
    inner: Box<dyn SfpHandle>,
    port: usize,
    rec: Arc<Recorder>,
}

macro_rules! traced {
    ($self:ident, $op:literal, $call:expr) => {{
        $self.rec.hal($self.port, $op);
        $call
    }};
}

impl SfpHandle for CountingSfp {
    fn get_presence(&self) -> Result<bool> {
        traced!(self, "get_presence", self.inner.get_presence())
    }
    fn is_replaceable(&self) -> Result<bool> {
        traced!(self, "is_replaceable", self.inner.is_replaceable())
    }
    fn get_reset_status(&self) -> Result<bool> {
        traced!(self, "get_reset_status", self.inner.get_reset_status())
    }
    fn sfp_type(&self) -> Result<String> {
        traced!(self, "sfp_type", self.inner.sfp_type())
    }
    fn get_error_description(&self) -> Result<Option<String>> {
        traced!(
            self,
            "get_error_description",
            self.inner.get_error_description()
        )
    }
    fn get_transceiver_info(&self) -> Result<Value> {
        traced!(
            self,
            "get_transceiver_info",
            self.inner.get_transceiver_info()
        )
    }
    fn get_transceiver_dom_real_value(&self) -> Result<Value> {
        traced!(
            self,
            "get_transceiver_dom_real_value",
            self.inner.get_transceiver_dom_real_value()
        )
    }
    fn get_transceiver_status(&self) -> Result<Value> {
        traced!(
            self,
            "get_transceiver_status",
            self.inner.get_transceiver_status()
        )
    }
    fn get_transceiver_threshold_info(&self) -> Result<Value> {
        traced!(
            self,
            "get_transceiver_threshold_info",
            self.inner.get_transceiver_threshold_info()
        )
    }
    fn get_lpmode(&self) -> Result<bool> {
        traced!(self, "get_lpmode", self.inner.get_lpmode())
    }
    fn set_lpmode(&self, on: bool) -> Result<bool> {
        traced!(self, "set_lpmode", self.inner.set_lpmode(on))
    }
    fn reset(&self) -> Result<bool> {
        traced!(self, "reset", self.inner.reset())
    }

    /// Recorded as `call_json:<method>` so the specific Python method is preserved --
    /// a bare `call_json` count would hide which getters diverged.
    fn call_json(&self, method: &str) -> Result<Value> {
        self.rec.hal(self.port, &format!("call_json:{method}"));
        self.inner.call_json(method)
    }

    fn read_eeprom(&self, offset: usize, num_bytes: usize) -> Result<Option<Vec<u8>>> {
        self.rec.eeprom(self.port, false, offset, num_bytes);
        self.inner.read_eeprom(offset, num_bytes)
    }

    fn write_eeprom(&self, offset: usize, data: &[u8]) -> Result<bool> {
        self.rec.eeprom(self.port, true, offset, data.len());
        self.inner.write_eeprom(offset, data)
    }
}

// ---------------------------------------------------------------------------
// STATE_DB seam
// ---------------------------------------------------------------------------

/// Wraps any [`DbTable`], recording every row/field operation.
///
/// Writes carry a field count (never field values): that mirrors what the reference
/// unit tests assert (`get_size_for_key("Ethernet0") == 27`), and value-level parity
/// is already covered by the `xcvrd-tests` behavioural suite.
pub struct CountingDbTable {
    inner: Arc<dyn DbTable>,
    table: String,
    rec: Arc<Recorder>,
}

impl CountingDbTable {
    pub fn new(inner: Arc<dyn DbTable>, table: impl Into<String>, rec: Arc<Recorder>) -> Self {
        CountingDbTable {
            inner,
            table: table.into(),
            rec,
        }
    }
}

impl DbTable for CountingDbTable {
    fn set(&self, key: &str, fvs: &[(String, String)]) {
        self.rec.db(&self.table, key, "set", Some(fvs.len()));
        self.inner.set(key, fvs)
    }
    fn hset(&self, key: &str, field: &str, value: &str) {
        self.rec.db(&self.table, key, "hset", Some(1));
        self.inner.hset(key, field, value)
    }
    fn get(&self, key: &str) -> Option<Vec<(String, String)>> {
        self.rec.db(&self.table, key, "get", None);
        self.inner.get(key)
    }
    fn hget(&self, key: &str, field: &str) -> Option<String> {
        self.rec.db(&self.table, key, "hget", None);
        self.inner.hget(key, field)
    }
    fn del(&self, key: &str) {
        self.rec.db(&self.table, key, "del", None);
        self.inner.del(key)
    }
    fn hdel(&self, key: &str, field: &str) {
        self.rec.db(&self.table, key, "hdel", None);
        self.inner.hdel(key, field)
    }
    fn get_keys(&self) -> Vec<String> {
        self.rec.db(&self.table, "", "get_keys", None);
        self.inner.get_keys()
    }
    fn get_size(&self) -> usize {
        self.rec.db(&self.table, "", "get_size", None);
        self.inner.get_size()
    }
    fn get_size_for_key(&self, key: &str) -> usize {
        let n = self.inner.get_size_for_key(key);
        self.rec.db(&self.table, key, "get_size_for_key", Some(n));
        n
    }
    fn get_size_for_key_checked(&self, key: &str) -> Option<usize> {
        let n = self.inner.get_size_for_key_checked(key);
        self.rec
            .db(&self.table, key, "get_size_for_key_checked", n);
        n
    }
    fn as_any(&self) -> &dyn std::any::Any {
        // Deliberately exposes the DECORATOR, not the wrapped table: production never
        // downcasts, and a bench that reached through the wrapper would bypass tracing.
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xcvrd_rs::mock::{MockDbTable, MockHal, MockSfp};

    #[test]
    fn hal_calls_are_recorded_and_delegated() {
        let rec = Recorder::new();
        let hal = Arc::new(MockHal::with_sfps(vec![MockSfp::present()]));
        let counting = CountingHal::new(hal, rec.clone());

        assert_eq!(counting.num_sfps().unwrap(), 1);
        let sfp = counting.sfp(0).unwrap();
        assert!(sfp.get_presence().unwrap(), "delegation must preserve value");

        let jsonl = rec.to_jsonl();
        assert!(jsonl.contains(r#""op":"num_sfps""#));
        assert!(jsonl.contains(r#""op":"sfp""#));
        assert!(jsonl.contains(r#""op":"get_presence""#));
        assert!(jsonl.contains(r#""port":0"#));
    }

    #[test]
    fn db_writes_record_field_counts() {
        let rec = Recorder::new();
        let inner = Arc::new(MockDbTable::new("TRANSCEIVER_DOM_SENSOR"));
        let t = CountingDbTable::new(inner, "TRANSCEIVER_DOM_SENSOR", rec.clone());

        t.set(
            "Ethernet0",
            &[
                ("temperature".to_string(), "45.0".to_string()),
                ("voltage".to_string(), "3.3".to_string()),
            ],
        );

        let jsonl = rec.to_jsonl();
        assert!(jsonl.contains(r#""nfields":2"#));
        assert!(jsonl.contains(r#""table":"TRANSCEIVER_DOM_SENSOR""#));
        // Delegation must actually have happened.
        assert_eq!(t.get_size_for_key("Ethernet0"), 2);
    }

    #[test]
    fn seq_is_monotonic() {
        let rec = Recorder::new();
        rec.hal_global("a");
        rec.hal_global("b");
        let jsonl = rec.to_jsonl();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert!(lines[0].contains(r#""seq":0"#));
        assert!(lines[1].contains(r#""seq":1"#));
    }
}
