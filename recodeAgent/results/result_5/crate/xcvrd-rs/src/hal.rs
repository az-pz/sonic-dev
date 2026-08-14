//! HAL trait seam (unit-test strategy).
//!
//! `test_xcvrd.py` mocks the platform by patching the `_wrapper_*` helpers and
//! handing tasks MagicMock SFPs. In Rust we express the same seam as two traits:
//! [`Chassis`] and [`Sfp`]. The **real** implementations ([`RealChassis`],
//! [`RealSfp`]) delegate to the thick PyO3 [`platform_bridge`] HAL (unchanged);
//! the **mock** implementations live in [`crate::mock`] (compiled under `test`).
//!
//! Daemon logic is written against `&dyn Chassis` / `&dyn Sfp` so a unit test can
//! inject a `MockChassis` while the deployed daemon injects the bridge.

use serde_json::Value;

/// Seam error: bridge/DB errors are flattened to a string so the trait stays
/// object-safe and decoupled from `platform_bridge::BridgeError`.
pub type HalResult<T> = std::result::Result<T, String>;

/// One transceiver change-event poll — reuse the bridge's shape so real + mock
/// agree: `{status, sfp{port:code}, sfp_error{port:code}}`.
pub use platform_bridge::ChangeEvent;

/// A single transceiver slot (mirrors `sonic_platform` `SfpBase` + the emulator).
/// Getters map 1:1 to the `_wrapper_*` helpers in `xcvrd.py`/`common.py`.
pub trait Sfp {
    fn get_presence(&self) -> HalResult<bool>;
    fn is_replaceable(&self) -> HalResult<bool>;
    fn get_reset_status(&self) -> HalResult<bool>;
    fn sfp_type(&self) -> HalResult<String>;
    fn get_error_description(&self) -> HalResult<Option<String>>;
    fn get_transceiver_info(&self) -> HalResult<Value>;
    fn get_transceiver_dom_real_value(&self) -> HalResult<Value>;
    fn get_transceiver_status(&self) -> HalResult<Value>;
    fn get_transceiver_threshold_info(&self) -> HalResult<Value>;
    fn get_lpmode(&self) -> HalResult<bool>;
    fn set_lpmode(&self, on: bool) -> HalResult<bool>;
    fn reset(&self) -> HalResult<bool>;
    /// Raw EEPROM read (`SfpBase.read_eeprom(offset, num_bytes)`): `num_bytes` from
    /// linear `offset` (optoe `linear = page*128 + offset`). `None` mirrors the Python
    /// `None` on a read miss. Used by the CMIS control seam to read the page-10h/page-01h
    /// control/advertisement bytes (CMIS *decode* stays in Python; these are raw registers).
    fn read_eeprom(&self, offset: usize, num_bytes: usize) -> HalResult<Option<Vec<u8>>>;
    /// Raw EEPROM write (`SfpBase.write_eeprom(offset, data)`) — the CMIS page-10h datapath
    /// control-register writes (`c_cmis.py` encodings). `true` on success.
    fn write_eeprom(&self, offset: usize, data: &[u8]) -> HalResult<bool>;
    /// Escape hatch for any no-arg Python `Sfp` method not yet given a typed
    /// wrapper (`get_transceiver_dom_flags`, `..._status_flags`, `..._pm`, …).
    fn call_json(&self, method: &str) -> HalResult<Value>;
}

/// The transceiver plant (`platform_chassis`): slot count, per-slot handles, and
/// the change-event poll.
pub trait Chassis {
    fn num_sfps(&self) -> HalResult<usize>;
    fn sfp(&self, index: usize) -> HalResult<Box<dyn Sfp>>;
    fn get_change_event(&self, timeout_ms: u64) -> HalResult<ChangeEvent>;
}

// ---- real implementations (wrap the PyO3 bridge; NOT daemon logic) ----------

/// Real chassis: wraps [`platform_bridge::Platform`].
pub struct RealChassis(pub platform_bridge::Platform);

/// Real SFP: wraps [`platform_bridge::Sfp`].
pub struct RealSfp(pub platform_bridge::Sfp);

impl RealChassis {
    pub fn open() -> HalResult<Self> {
        platform_bridge::Platform::new().map(RealChassis).map_err(|e| e.to_string())
    }
}

impl Chassis for RealChassis {
    fn num_sfps(&self) -> HalResult<usize> {
        self.0.num_sfps().map_err(|e| e.to_string())
    }
    fn sfp(&self, index: usize) -> HalResult<Box<dyn Sfp>> {
        let s = self.0.sfp(index).map_err(|e| e.to_string())?;
        Ok(Box::new(RealSfp(s)))
    }
    fn get_change_event(&self, timeout_ms: u64) -> HalResult<ChangeEvent> {
        self.0.get_change_event(timeout_ms).map_err(|e| e.to_string())
    }
}

impl Sfp for RealSfp {
    fn get_presence(&self) -> HalResult<bool> { self.0.get_presence().map_err(|e| e.to_string()) }
    fn is_replaceable(&self) -> HalResult<bool> { self.0.is_replaceable().map_err(|e| e.to_string()) }
    fn get_reset_status(&self) -> HalResult<bool> { self.0.get_reset_status().map_err(|e| e.to_string()) }
    fn sfp_type(&self) -> HalResult<String> { self.0.sfp_type().map_err(|e| e.to_string()) }
    fn get_error_description(&self) -> HalResult<Option<String>> { self.0.get_error_description().map_err(|e| e.to_string()) }
    fn get_transceiver_info(&self) -> HalResult<Value> { self.0.get_transceiver_info().map_err(|e| e.to_string()) }
    fn get_transceiver_dom_real_value(&self) -> HalResult<Value> { self.0.get_transceiver_dom_real_value().map_err(|e| e.to_string()) }
    fn get_transceiver_status(&self) -> HalResult<Value> { self.0.get_transceiver_status().map_err(|e| e.to_string()) }
    fn get_transceiver_threshold_info(&self) -> HalResult<Value> { self.0.get_transceiver_threshold_info().map_err(|e| e.to_string()) }
    fn get_lpmode(&self) -> HalResult<bool> { self.0.get_lpmode().map_err(|e| e.to_string()) }
    fn set_lpmode(&self, on: bool) -> HalResult<bool> { self.0.set_lpmode(on).map_err(|e| e.to_string()) }
    fn reset(&self) -> HalResult<bool> { self.0.reset().map_err(|e| e.to_string()) }
    fn read_eeprom(&self, offset: usize, num_bytes: usize) -> HalResult<Option<Vec<u8>>> { self.0.read_eeprom(offset, num_bytes).map_err(|e| e.to_string()) }
    fn write_eeprom(&self, offset: usize, data: &[u8]) -> HalResult<bool> { self.0.write_eeprom(offset, data).map_err(|e| e.to_string()) }
    fn call_json(&self, method: &str) -> HalResult<Value> { self.0.call_json(method, ()).map_err(|e| e.to_string()) }
}
