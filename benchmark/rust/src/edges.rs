//! Config-A edges: a Rust-native transceiver plant that never touches Python.
//!
//! Deliberately NOT `xcvrd_rs::mock::MockSfp`. That type carries a `call_log` that
//! allocates a `String` and takes a `Mutex` on every call (~18ns) and exists to make
//! unit-test assertions, not to be fast; and its sibling `MockDbTable` looks a field
//! up by linear scan (`mock.rs:90`) where the Python mock uses a dict. Those are
//! fine for correctness tests and wrong for timing.
//!
//! Semantics are matched to `benchmark/pymocks/sonic_platform/sfp.py` field for field:
//! same fixture, same payloads, and a fresh owned copy returned from every getter.
//! The copy is not optional on this side -- `SfpHandle` is declared `-> Result<Value>`,
//! so an owned value must be materialised regardless. That is the floor on config A's
//! edge cost (~460ns for the 27-field DOM map) and it cannot be lowered without
//! editing the target crate, which is immutable.
//!
//! Residual asymmetry against the Python edge (~102ns) is therefore ~358ns per call,
//! IN RUST'S DISFAVOUR. That direction is intentional: it makes any measured Rust win
//! a conservative lower bound rather than an artefact of the instrument.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use xcvrd_rs::error::Result;
use xcvrd_rs::hal::{ChangeEvent, Hal, SfpHandle};

/// Fixture-backed payloads, decoded once and shared by every slot.
#[derive(Clone, Debug, Default)]
pub struct Fixture {
    pub presence: bool,
    pub replaceable: bool,
    pub reset_status: bool,
    pub sfp_type: String,
    pub error_description: Option<String>,
    pub lpmode: bool,
    pub info: Value,
    pub dom_real_value: Value,
    pub status: Value,
    pub threshold_info: Value,
    pub json_calls: BTreeMap<String, Value>,
    pub eeprom: BTreeMap<usize, u8>,
}

impl Fixture {
    /// Parse the same JSON file the Python mock loads, so neither side can drift.
    pub fn from_json(v: &Value) -> Self {
        let obj = |k: &str| v.get(k).cloned().unwrap_or(Value::Object(Default::default()));
        Fixture {
            presence: v.get("presence").and_then(Value::as_bool).unwrap_or(true),
            replaceable: v.get("replaceable").and_then(Value::as_bool).unwrap_or(true),
            reset_status: v.get("reset_status").and_then(Value::as_bool).unwrap_or(false),
            sfp_type: v
                .get("sfp_type")
                .and_then(Value::as_str)
                .unwrap_or("QSFP_DD")
                .to_string(),
            error_description: v
                .get("error_description")
                .and_then(Value::as_str)
                .map(str::to_string),
            lpmode: v.get("lpmode").and_then(Value::as_bool).unwrap_or(false),
            info: obj("info"),
            dom_real_value: obj("dom_real_value"),
            status: obj("status"),
            threshold_info: obj("threshold_info"),
            json_calls: v
                .get("json_calls")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, val)| (k.clone(), val.clone())).collect())
                .unwrap_or_default(),
            eeprom: v
                .get("eeprom")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, val)| {
                            Some((k.parse().ok()?, val.as_u64()? as u8))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub fn from_path(path: &str) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Fixture::from_json(&v))
    }
}

pub struct BenchSfp {
    // Arc, not a clone of the Fixture. `hal.sfp(port)` is called at eight sites in
    // the DOM manager alone, per port, per poll -- deep-copying the fixture there
    // measured 4178 ns/handle against the bridge's 206 ns, i.e. config A would have
    // carried a 20x construction penalty that exists nowhere in either daemon. The
    // bridge just wraps a Python object reference, so a refcount bump is the faithful
    // analogue.
    fx: Arc<Fixture>,
    index: usize,
    lpmode: Mutex<bool>,
    eeprom: Mutex<BTreeMap<usize, u8>>,
}

impl BenchSfp {
    pub fn new(index: usize, fx: Arc<Fixture>) -> Self {
        let eeprom = fx.eeprom.clone();
        let lpmode = fx.lpmode;
        BenchSfp {
            fx,
            index,
            lpmode: Mutex::new(lpmode),
            eeprom: Mutex::new(eeprom),
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }
}

impl SfpHandle for BenchSfp {
    fn get_presence(&self) -> Result<bool> {
        Ok(self.fx.presence)
    }
    fn is_replaceable(&self) -> Result<bool> {
        Ok(self.fx.replaceable)
    }
    fn get_reset_status(&self) -> Result<bool> {
        Ok(self.fx.reset_status)
    }
    fn sfp_type(&self) -> Result<String> {
        Ok(self.fx.sfp_type.clone())
    }
    fn get_error_description(&self) -> Result<Option<String>> {
        Ok(self.fx.error_description.clone())
    }
    fn get_transceiver_info(&self) -> Result<Value> {
        Ok(self.fx.info.clone())
    }
    fn get_transceiver_dom_real_value(&self) -> Result<Value> {
        Ok(self.fx.dom_real_value.clone())
    }
    fn get_transceiver_status(&self) -> Result<Value> {
        Ok(self.fx.status.clone())
    }
    fn get_transceiver_threshold_info(&self) -> Result<Value> {
        Ok(self.fx.threshold_info.clone())
    }
    fn get_lpmode(&self) -> Result<bool> {
        Ok(*self.lpmode.lock().unwrap())
    }
    fn set_lpmode(&self, on: bool) -> Result<bool> {
        *self.lpmode.lock().unwrap() = on;
        Ok(true)
    }
    fn reset(&self) -> Result<bool> {
        Ok(true)
    }
    fn call_json(&self, method: &str) -> Result<Value> {
        // Unknown getters return null rather than erroring: the Python mock has the
        // same shape (a missing key simply is not bound), and a hard error here would
        // turn a fixture gap into a benchmark crash halfway through a run.
        Ok(self
            .fx
            .json_calls
            .get(method)
            .cloned()
            .unwrap_or(Value::Null))
    }
    fn read_eeprom(&self, offset: usize, num_bytes: usize) -> Result<Option<Vec<u8>>> {
        let eep = self.eeprom.lock().unwrap();
        Ok(Some(
            (0..num_bytes)
                .map(|i| eep.get(&(offset + i)).copied().unwrap_or(0))
                .collect(),
        ))
    }
    fn write_eeprom(&self, offset: usize, data: &[u8]) -> Result<bool> {
        let mut eep = self.eeprom.lock().unwrap();
        for (i, b) in data.iter().enumerate() {
            eep.insert(offset + i, *b);
        }
        Ok(true)
    }
}

pub struct BenchHal {
    fx: Arc<Fixture>,
    num_sfps: usize,
    events: Mutex<Vec<ChangeEvent>>,
}

impl BenchHal {
    pub fn new(fx: Fixture, num_sfps: usize) -> Self {
        BenchHal {
            fx: Arc::new(fx),
            num_sfps,
            events: Mutex::new(Vec::new()),
        }
    }

    /// Stage an event for the next `get_change_event`, mirroring the Python mock's
    /// `queue_change_event`, so plug storms are scenario-driven and reproducible
    /// instead of dependent on wall-clock timing.
    pub fn queue_change_event(&self, ev: ChangeEvent) {
        self.events.lock().unwrap().push(ev);
    }
}

impl Hal for BenchHal {
    fn num_sfps(&self) -> Result<usize> {
        Ok(self.num_sfps)
    }
    fn sfp(&self, index: usize) -> Result<Box<dyn SfpHandle>> {
        Ok(Box::new(BenchSfp::new(index, Arc::clone(&self.fx))))
    }
    fn get_change_event(&self, _timeout_ms: u64) -> Result<ChangeEvent> {
        let mut q = self.events.lock().unwrap();
        if q.is_empty() {
            // Never block: a sleep here would be measured as daemon time.
            return Ok(ChangeEvent {
                status: true,
                sfp: Default::default(),
                sfp_error: Default::default(),
            });
        }
        Ok(q.remove(0))
    }
}
