//! Misc SFP helpers — port of `xcvrd_utilities/utils.py` (`XCVRDUtils`).
//!
//! Presence / flat-memory / lpmode helpers over the HAL seam. Stubs only.

#![allow(dead_code, unused_variables)]

use crate::hal::SfpApi;

/// `XCVRDUtils` (`utils.py:1`): thin HAL-backed predicates.
pub struct XcvrdUtils;

impl XcvrdUtils {
    /// `get_transceiver_presence`.
    pub fn get_transceiver_presence<S: SfpApi>(sfp: &S) -> bool {
        // NotImplementedError / bridge error -> not present (Python catches and
        // returns False).
        sfp.get_presence().unwrap_or(false)
    }

    /// `is_transceiver_flat_memory`: paged (CMIS) vs flat (SFF). Python defaults to
    /// flat (`True`) when there is no xcvr api or the call raises
    /// `KeyError`/`NotImplementedError`.
    pub fn is_transceiver_flat_memory<S: SfpApi>(sfp: &S) -> bool {
        match sfp.is_flat_memory() {
            Ok(Some(b)) => b,
            Ok(None) => true, // no xcvr api -> flat
            Err(_) => true,   // KeyError / NotImplementedError -> flat
        }
    }

    /// `is_transceiver_lpmode_on` (`utils.py:27`): reflect the module's low-power
    /// mode. Python returns the raw `get_lpmode()` value and logs + returns
    /// `False` on any exception (a `None` return is itself falsy). The real bridge
    /// extracts a bool (`call_bool`), so a Python `None`/`NotImplementedError`
    /// surfaces here as `Err` — both collapse to `false`, matching Python. This is
    /// a pure read: it never writes EEPROM, so it cannot clobber the sfputil/plugin
    /// CMIS ModuleGlobalControls (00h:26) lpmode/reset bits.
    pub fn is_transceiver_lpmode_on<S: SfpApi>(sfp: &S) -> bool {
        sfp.get_lpmode().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{FlatMem, MockSfp};

    /// <- test_xcvrd_utils_get_transceiver_presence.
    #[test]
    fn get_transceiver_presence_reflects_sfp() {
        let mut sfp = MockSfp::default();
        sfp.presence = true;
        assert!(XcvrdUtils::get_transceiver_presence(&sfp));
        sfp.presence = false;
        assert!(!XcvrdUtils::get_transceiver_presence(&sfp));
    }

    /// <- test_is_transceiver_flat_memory: no-api -> True; flat True; paged False;
    /// KeyError/NotImplementedError -> True.
    #[test]
    fn is_transceiver_flat_memory_cases() {
        let mut sfp = MockSfp::default();
        // get_xcvr_api returns None -> True.
        sfp.flat_memory = FlatMem::NoApi;
        assert!(XcvrdUtils::is_transceiver_flat_memory(&sfp));
        // is_flat_memory returns True -> True.
        sfp.flat_memory = FlatMem::Flat;
        assert!(XcvrdUtils::is_transceiver_flat_memory(&sfp));
        // is_flat_memory returns False -> False.
        sfp.flat_memory = FlatMem::Paged;
        assert!(!XcvrdUtils::is_transceiver_flat_memory(&sfp));
        // NotImplementedError / KeyError -> True.
        sfp.flat_memory = FlatMem::NotImpl;
        assert!(XcvrdUtils::is_transceiver_flat_memory(&sfp));
    }

    /// <- test_is_transceiver_lpmode_on: get_lpmode True -> on; False -> off; a
    /// `None` return, `NotImplementedError`, or the missing-port `KeyError` all
    /// surface here as `Err` (the real bridge extracts a bool) -> off. Covers the
    /// five Python sub-cases (None/True/False/KeyError/NotImplementedError).
    #[test]
    fn is_transceiver_lpmode_on_cases() {
        let sfp = MockSfp::default();
        // get_lpmode -> True.
        sfp.lpmode.set(Some(true));
        assert!(XcvrdUtils::is_transceiver_lpmode_on(&sfp));
        // get_lpmode -> False.
        sfp.lpmode.set(Some(false));
        assert!(!XcvrdUtils::is_transceiver_lpmode_on(&sfp));
        // get_lpmode -> None / NotImplementedError / KeyError -> off.
        sfp.lpmode.set(None);
        assert!(!XcvrdUtils::is_transceiver_lpmode_on(&sfp));
    }

    /// M4 don't-regress guard: reflecting lpmode is a pure read — it must not
    /// issue any EEPROM write that could clobber the sfputil/plugin's CMIS
    /// ModuleGlobalControls (00h:26) writes (reset->0x08, lpmode-on->0x10).
    #[test]
    fn is_transceiver_lpmode_on_never_writes_eeprom() {
        let sfp = MockSfp::default();
        sfp.lpmode.set(Some(true));
        let _ = XcvrdUtils::is_transceiver_lpmode_on(&sfp);
        sfp.lpmode.set(None);
        let _ = XcvrdUtils::is_transceiver_lpmode_on(&sfp);
        assert_eq!(sfp.eeprom_writes.get(), 0);
    }
}
