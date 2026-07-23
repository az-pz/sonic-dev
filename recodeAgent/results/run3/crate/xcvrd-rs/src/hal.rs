//! HAL seam — the mockable transceiver hardware boundary.
//!
//! Port of the Python platform-access surface xcvrd uses (`common._wrapper_*`,
//! `chassis.get_sfp/get_num_sfps/get_change_event`, `sfp.get_transceiver_*`). The
//! daemon logic (SfpStateUpdateTask, DomInfoUpdateTask, CmisManagerTask, …) is
//! written against these traits so it can run against the real Python plugin on
//! the DUT *and* against `mock::MockHal` under `cargo test` (mirroring the Python
//! tests' MagicMock SFPs). This is the Part-B unit-test seam from analysis §3.6.
//!
//! - `trait SfpApi`  — one transceiver slot (identity/DOM/status/threshold/lpmode/eeprom).
//! - `trait Hal`     — the chassis (num_sfps, sfp(i), get_change_event).
//! - `PlatformHal` / `PlatformSfp` — the REAL impl: 1:1 thin delegation to
//!   `platform_bridge::{Platform, Sfp}` (the PyO3 → sonic_platform → xcvr-emu path).
//!
//! Only the trait definitions + the thin real wrappers live here — no daemon
//! decision logic (that is the Translator's job in the task modules).

use serde_json::Value;
use std::sync::Arc;

pub use platform_bridge::ChangeEvent;

/// Error surfaced by a HAL call. `NotImplemented` models the Python
/// `NotImplementedError` the daemon treats as "feature absent" (skip / empty).
#[derive(Debug)]
pub enum HalError {
    /// The platform does not implement this call (Python `NotImplementedError`).
    NotImplemented,
    /// Any failure from the underlying bridge / plugin (with context string).
    Bridge(String),
    /// Mock-injected failure (unit tests only).
    Mock(String),
}

impl std::fmt::Display for HalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HalError::NotImplemented => write!(f, "hal: not implemented"),
            HalError::Bridge(s) => write!(f, "hal bridge error: {s}"),
            HalError::Mock(s) => write!(f, "hal mock error: {s}"),
        }
    }
}

impl std::error::Error for HalError {}

impl From<platform_bridge::BridgeError> for HalError {
    fn from(e: platform_bridge::BridgeError) -> Self {
        HalError::Bridge(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, HalError>;

/// One transceiver slot. Mirrors the Python `Sfp` methods xcvrd calls; dict
/// getters return `serde_json::Value` exactly like `platform-bridge`.
pub trait SfpApi {
    fn get_presence(&self) -> Result<bool>;
    fn is_replaceable(&self) -> Result<bool>;
    fn get_reset_status(&self) -> Result<bool>;
    fn sfp_type(&self) -> Result<String>;
    fn get_error_description(&self) -> Result<Option<String>>;
    /// `TRANSCEIVER_INFO` source. [M1]
    fn get_transceiver_info(&self) -> Result<Value>;
    /// Paged (CMIS) vs flat (SFF) memory, via the Python xcvr api
    /// (`_wrapper_is_flat_memory` / `XCVRDUtils.is_transceiver_flat_memory`).
    /// `Ok(Some(true))` flat, `Ok(Some(false))` paged, `Ok(None)` no xcvr api,
    /// `Err(NotImplemented)` when the platform can't answer. [M3]
    fn is_flat_memory(&self) -> Result<Option<bool>>;
    /// `TRANSCEIVER_DOM_SENSOR` source. [M2]
    fn get_transceiver_dom_real_value(&self) -> Result<Value>;
    /// `TRANSCEIVER_STATUS` source. [M3]
    fn get_transceiver_status(&self) -> Result<Value>;
    /// `TRANSCEIVER_DOM_THRESHOLD` source. [M2/M6]
    fn get_transceiver_threshold_info(&self) -> Result<Value>;
    fn get_lpmode(&self) -> Result<bool>;
    fn set_lpmode(&self, on: bool) -> Result<bool>;
    fn reset(&self) -> Result<bool>;
    fn read_eeprom(&self, offset: usize, num_bytes: usize) -> Result<Option<Vec<u8>>>;
    fn write_eeprom(&self, offset: usize, data: &[u8]) -> Result<bool>;
}

/// The transceiver chassis: enumerate slots and poll plug/unplug/error events.
pub trait Hal {
    type Sfp: SfpApi;
    fn num_sfps(&self) -> Result<usize>;
    fn sfp(&self, index: usize) -> Result<Self::Sfp>;
    fn get_change_event(&self, timeout_ms: u64) -> Result<ChangeEvent>;
}

/// Shared HAL: a single chassis handed to several spawned task threads
/// (`SfpStateUpdateTask`, `DomInfoUpdateTask`, …) — the Rust analogue of the one
/// global `platform_chassis` all Python tasks call. Requires the wrapped `Hal`
/// to be `Send + Sync` (the real `PlatformHal` is). [M5]
impl<H: Hal> Hal for Arc<H> {
    type Sfp = H::Sfp;
    fn num_sfps(&self) -> Result<usize> {
        (**self).num_sfps()
    }
    fn sfp(&self, index: usize) -> Result<H::Sfp> {
        (**self).sfp(index)
    }
    fn get_change_event(&self, timeout_ms: u64) -> Result<ChangeEvent> {
        (**self).get_change_event(timeout_ms)
    }
}

// --------------------------------------------------------------------------
// Real implementation: thin 1:1 delegation to platform-bridge (no logic).
// --------------------------------------------------------------------------

/// Real HAL over the PyO3 `platform_bridge::Platform` (sonic_platform → xcvr-emu).
pub struct PlatformHal {
    inner: platform_bridge::Platform,
}

impl PlatformHal {
    /// Wrap an already-constructed bridge `Platform`.
    pub fn new(inner: platform_bridge::Platform) -> Self {
        Self { inner }
    }

    /// Open the platform via the shared env seed (`env::open_platform`).
    pub fn open() -> Result<Self> {
        Ok(Self::new(crate::env::open_platform()?))
    }
}

impl Hal for PlatformHal {
    type Sfp = PlatformSfp;

    fn num_sfps(&self) -> Result<usize> {
        Ok(self.inner.num_sfps()?)
    }

    fn sfp(&self, index: usize) -> Result<PlatformSfp> {
        Ok(PlatformSfp { inner: self.inner.sfp(index)? })
    }

    fn get_change_event(&self, timeout_ms: u64) -> Result<ChangeEvent> {
        Ok(self.inner.get_change_event(timeout_ms)?)
    }
}

/// Real SFP handle over `platform_bridge::Sfp`.
pub struct PlatformSfp {
    inner: platform_bridge::Sfp,
}

impl SfpApi for PlatformSfp {
    fn get_presence(&self) -> Result<bool> {
        Ok(self.inner.get_presence()?)
    }
    fn is_replaceable(&self) -> Result<bool> {
        Ok(self.inner.is_replaceable()?)
    }
    fn get_reset_status(&self) -> Result<bool> {
        Ok(self.inner.get_reset_status()?)
    }
    fn sfp_type(&self) -> Result<String> {
        Ok(self.inner.sfp_type()?)
    }
    fn get_error_description(&self) -> Result<Option<String>> {
        Ok(self.inner.get_error_description()?)
    }
    fn get_transceiver_info(&self) -> Result<Value> {
        Ok(self.inner.get_transceiver_info()?)
    }
    fn is_flat_memory(&self) -> Result<Option<bool>> {
        // The Python xcvr api exposes `is_flat_memory()`. CMIS decode stays in
        // Python, so reach it through the bridge escape hatch rather than decoding
        // here; a missing api / NotImplementedError collapses to `None`/`Err`, which
        // the `_wrapper_is_flat_memory` callers treat as "assume flat".
        match self.inner.call_json("is_flat_memory", ()) {
            Ok(Value::Bool(b)) => Ok(Some(b)),
            Ok(Value::Null) => Ok(None),
            Ok(_) => Ok(None),
            Err(_) => Err(HalError::NotImplemented),
        }
    }
    fn get_transceiver_dom_real_value(&self) -> Result<Value> {
        Ok(self.inner.get_transceiver_dom_real_value()?)
    }
    fn get_transceiver_status(&self) -> Result<Value> {
        Ok(self.inner.get_transceiver_status()?)
    }
    fn get_transceiver_threshold_info(&self) -> Result<Value> {
        // The JSON bridge can't marshal non-finite floats (rx/tx-power thresholds
        // are -inf when the limit is 0 mW -> json.dumps '-Infinity', which
        // serde_json rejects), so read + str() them Python-side. Empty on
        // error/absent api -> the caller skips the write (NotImplementedError path).
        Ok(read_thresholds_stringified(self.inner.index()))
    }
    fn get_lpmode(&self) -> Result<bool> {
        Ok(self.inner.get_lpmode()?)
    }
    fn set_lpmode(&self, on: bool) -> Result<bool> {
        Ok(self.inner.set_lpmode(on)?)
    }
    fn reset(&self) -> Result<bool> {
        Ok(self.inner.reset()?)
    }
    fn read_eeprom(&self, offset: usize, num_bytes: usize) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.read_eeprom(offset, num_bytes)?)
    }
    fn write_eeprom(&self, offset: usize, data: &[u8]) -> Result<bool> {
        Ok(self.inner.write_eeprom(offset, data)?)
    }
}

/// Compile-time proof that the real HAL handles are `Send + Sync`, so the daemon
/// can share one chassis (`Arc<PlatformHal>`) across spawned task threads and move
/// per-port `PlatformSfp` handles between them (M5 concurrency).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PlatformHal>();
    assert_send_sync::<PlatformSfp>();
    assert_send_sync::<Arc<PlatformHal>>();
};

/// Read DOM thresholds for the module at `index` via `sonic_platform`, stringifying
/// each value in Python so they survive JSON marshaling.
///
/// Why not the bridge: `Sfp::get_transceiver_threshold_info()` marshals the dict
/// with `json.dumps` -> `serde_json`, but rx/tx-power thresholds are `float('-inf')`
/// (0 mW limit) which `json.dumps` renders as `-Infinity`, a token serde_json
/// rejects — so that call fails for the golden module and `TRANSCEIVER_DOM_THRESHOLD`
/// is never published. Reading through Python and applying `str()` to every value
/// first yields valid JSON strings (`"-inf"`, `"0.0"`), reproducing the reference
/// daemon's `str(value)` output byte-for-byte (M6 golden). This calls the same
/// `sonic_platform.sfp.Sfp` the bridge/chassis uses (`chassis.get_sfp(i)` returns
/// `Sfp(i)`), so it is the platform's own CMIS decode — not a re-implementation.
///
/// Any error / absent API / `NotImplementedError` -> empty object, so the caller
/// skips the write exactly like the Python `except NotImplementedError` path.
pub fn read_thresholds_stringified(index: usize) -> Value {
    use pyo3::prelude::*;
    use pyo3::types::PyDict;

    Python::with_gil(|py| -> PyResult<Value> {
        let sfp = py
            .import_bound("sonic_platform.sfp")?
            .getattr("Sfp")?
            .call1((index,))?;
        let thr = sfp.call_method0("get_transceiver_threshold_info")?;
        let dict = match thr.downcast::<PyDict>() {
            Ok(d) => d,
            // None / non-dict (feature absent) -> nothing to publish.
            Err(_) => return Ok(serde_json::json!({})),
        };
        let str_fn = py.import_bound("builtins")?.getattr("str")?;
        let mut map = serde_json::Map::new();
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            let val: String = str_fn.call1((v,))?.extract()?;
            map.insert(key, Value::String(val));
        }
        Ok(Value::Object(map))
    })
    .unwrap_or_else(|_| serde_json::json!({}))
}
