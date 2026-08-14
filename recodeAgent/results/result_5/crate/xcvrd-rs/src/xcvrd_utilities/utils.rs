#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `xcvrd_utilities/utils.py`: `XCVRDUtils` presence / low-power / flat-memory
//! helpers over the `sfp_obj_dict`.
//!
//! The Python `XCVRDUtils(sfp_obj_dict, logger)` indexes `sfp_obj_dict[physical_port]`
//! and calls the SFP's `get_presence()` / `get_xcvr_api().is_flat_memory()` /
//! `get_lpmode()`. In Rust the `sfp_obj_dict` is the HAL [`Chassis`] seam
//! (`chassis.sfp(physical_port)`), so the same code drives the real PyO3 bridge in the
//! daemon and a `MockChassis` in the unit tests. A missing slot (`chassis.sfp` `Err`)
//! is the Python `KeyError`; a failing getter (`Err`) is the Python
//! `NotImplementedError` — both fall back to the Python default for that helper.

use std::rc::Rc;

use serde_json::Value;

use crate::hal::Chassis;

/// The logger seam `XCVRDUtils` takes (`self.logger`), mirroring the Python
/// `helper_logger` argument. Only the level the ported code uses is exercised;
/// everything defaults to a no-op so the deployed daemon can pass a silent sink.
pub trait XcvrdLogger {
    fn log_error(&self, _msg: &str) {}
    fn log_warning(&self, _msg: &str) {}
    fn log_notice(&self, _msg: &str) {}
}

/// A silent logger for the deployed daemon path (and any caller that does not care
/// about the diagnostic strings).
#[derive(Default)]
pub struct NoopXcvrdLogger;
impl XcvrdLogger for NoopXcvrdLogger {}

/// Rust port of the Python `XCVRDUtils`. Holds the transceiver plant (`sfp_obj_dict`
/// → [`Chassis`]) and a logger, exactly like `XCVRDUtils(sfp_obj_dict, logger)`.
pub struct XCVRDUtils {
    chassis: Rc<dyn Chassis>,
    logger: Rc<dyn XcvrdLogger>,
}

impl XCVRDUtils {
    /// `XCVRDUtils.__init__(self, sfp_obj_dict, logger)`.
    pub fn new(chassis: Rc<dyn Chassis>, logger: Rc<dyn XcvrdLogger>) -> Self {
        XCVRDUtils { chassis, logger }
    }

    /// `get_transceiver_presence(physical_port)`:
    /// `self.sfp_obj_dict[physical_port].get_presence()`, defaulting to `False` on a
    /// `KeyError` (no such slot) or `NotImplementedError` (getter unsupported).
    pub fn get_transceiver_presence(&self, physical_port: i32) -> bool {
        match self.chassis.sfp(physical_port as usize) {
            Ok(sfp) => match sfp.get_presence() {
                Ok(present) => present,
                Err(_) => {
                    self.logger
                        .log_error(&format!("Failed to get presence for port {physical_port}"));
                    false
                }
            },
            Err(_) => {
                self.logger
                    .log_error(&format!("Failed to get presence for port {physical_port}"));
                false
            }
        }
    }

    /// `is_transceiver_flat_memory(physical_port)`:
    /// `api = sfp.get_xcvr_api(); return True if not api else api.is_flat_memory()`,
    /// defaulting to `True` on a `KeyError`/`NotImplementedError`. A flat-memory (or
    /// unknown) module has no upper pages, so `True` is the safe answer that suppresses
    /// paged reads. The `get_xcvr_api()` + `is_flat_memory()` chain is reached through
    /// the SFP seam's `call_json("is_flat_memory")`: a missing api / raising getter
    /// surfaces as `Err` (→ `True`), and an api-less module reads back as JSON `null`
    /// (→ `True`, the Python `if not api` branch).
    pub fn is_transceiver_flat_memory(&self, physical_port: i32) -> bool {
        match self.chassis.sfp(physical_port as usize) {
            Ok(sfp) => match sfp.call_json("is_flat_memory") {
                Ok(v) => v.as_bool().unwrap_or(true),
                Err(_) => {
                    self.logger
                        .log_error(&format!("Failed to check flat memory for port {physical_port}"));
                    true
                }
            },
            Err(_) => {
                self.logger
                    .log_error(&format!("Failed to check flat memory for port {physical_port}"));
                true
            }
        }
    }

    /// `is_transceiver_lpmode_on(physical_port)`:
    /// `self.sfp_obj_dict[physical_port].get_lpmode()`, defaulting to `False` on any
    /// exception. The Python getter may return `None` (unsupported low-power read),
    /// which is falsy — the bool seam models that as `false`.
    pub fn is_transceiver_lpmode_on(&self, physical_port: i32) -> bool {
        match self.chassis.sfp(physical_port as usize) {
            Ok(sfp) => match sfp.get_lpmode() {
                Ok(on) => on,
                Err(_) => {
                    self.logger.log_error(&format!(
                        "Failed to get low power mode for port {physical_port}"
                    ));
                    false
                }
            },
            Err(_) => {
                self.logger.log_error(&format!(
                    "Failed to get low power mode for port {physical_port}"
                ));
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hal::{Chassis, Sfp};
    use crate::mock::{MockChassis, MockSfp};
    use serde_json::json;

    /// Build an `XCVRDUtils` whose `sfp_obj_dict` has `sfp` at physical port 1 (the key
    /// the Python tests use), mirroring `XCVRDUtils({1: mock_sfp}, logger)`. Slot 0 is a
    /// filler absent module so the vec-backed `MockChassis` indexes 1 the same way.
    fn utils_with_sfp_at_1(sfp: MockSfp) -> XCVRDUtils {
        let chassis =
            Rc::new(MockChassis::with_sfps(vec![MockSfp::absent(), sfp])) as Rc<dyn Chassis>;
        XCVRDUtils::new(chassis, Rc::new(NoopXcvrdLogger))
    }

    /// An `XCVRDUtils` with an empty `sfp_obj_dict` — `chassis.sfp(1)` is the Python
    /// `KeyError` (`xcvrd_util.sfp_obj_dict = {}`).
    fn utils_empty() -> XCVRDUtils {
        let chassis = Rc::new(MockChassis::with_sfps(vec![])) as Rc<dyn Chassis>;
        XCVRDUtils::new(chassis, Rc::new(NoopXcvrdLogger))
    }

    // ---- translated from tests/test_xcvrd.py -------------------------------------

    #[test]
    fn test_xcvrd_utils_get_transceiver_presence() {
        // get_presence() -> True
        assert!(utils_with_sfp_at_1(MockSfp::present()).get_transceiver_presence(1));

        // get_presence() -> False
        assert!(!utils_with_sfp_at_1(MockSfp::absent()).get_transceiver_presence(1));

        // get_presence() raises NotImplementedError -> False
        let mut boom = MockSfp::present();
        boom.fail_method("get_presence");
        assert!(!utils_with_sfp_at_1(boom).get_transceiver_presence(1));

        // physical_port not in sfp_obj_dict (KeyError) -> False
        assert!(!utils_empty().get_transceiver_presence(1));
    }

    #[test]
    fn test_is_transceiver_flat_memory() {
        // get_xcvr_api() returns None -> True (no scripted is_flat_memory -> JSON null)
        assert!(utils_with_sfp_at_1(MockSfp::present()).is_transceiver_flat_memory(1));

        // api.is_flat_memory() returns True -> True
        let mut flat = MockSfp::present();
        flat.set_json_call("is_flat_memory", json!(true));
        assert!(utils_with_sfp_at_1(flat).is_transceiver_flat_memory(1));

        // api.is_flat_memory() returns False -> False
        let mut paged = MockSfp::present();
        paged.set_json_call("is_flat_memory", json!(false));
        assert!(!utils_with_sfp_at_1(paged).is_transceiver_flat_memory(1));

        // get_xcvr_api() raises KeyError (empty sfp_obj_dict) -> True
        assert!(utils_empty().is_transceiver_flat_memory(1));

        // is_flat_memory() raises NotImplementedError -> True
        let mut boom = MockSfp::present();
        boom.fail_method("is_flat_memory");
        assert!(utils_with_sfp_at_1(boom).is_transceiver_flat_memory(1));
    }

    #[test]
    fn test_is_transceiver_lpmode_on() {
        // get_lpmode() returns None (falsy) -> False (MockSfp lpmode defaults to false)
        assert!(!utils_with_sfp_at_1(MockSfp::present()).is_transceiver_lpmode_on(1));

        // get_lpmode() returns True -> True
        let on = MockSfp::present();
        on.set_lpmode(true).unwrap();
        assert!(utils_with_sfp_at_1(on).is_transceiver_lpmode_on(1));

        // get_lpmode() returns False -> False
        let off = MockSfp::present();
        off.set_lpmode(false).unwrap();
        assert!(!utils_with_sfp_at_1(off).is_transceiver_lpmode_on(1));

        // physical_port not in sfp_obj_dict (KeyError) -> False
        assert!(!utils_empty().is_transceiver_lpmode_on(1));

        // get_lpmode() raises NotImplementedError -> False
        let mut boom = MockSfp::present();
        boom.fail_method("get_lpmode");
        assert!(!utils_with_sfp_at_1(boom).is_transceiver_lpmode_on(1));
    }

    // ---- NEW unit tests over the bridge/mock seams -------------------------------

    /// `set_lpmode_via_mock_sfp`: the HAL `set_lpmode` write flips the module's
    /// low-power state through the mock SFP, and `XCVRDUtils.is_transceiver_lpmode_on`
    /// then reflects it — the round-trip the CMIS/DOM low-power gating depends on.
    #[test]
    fn set_lpmode_via_mock_sfp() {
        let sfp = MockSfp::present();
        // Drive the module into low power via the seam (as sfputil / CMIS bring-up do).
        assert!(sfp.set_lpmode(true).unwrap());
        assert!(sfp.get_lpmode().unwrap(), "mock SFP must latch the lpmode write");
        assert!(utils_with_sfp_at_1(sfp).is_transceiver_lpmode_on(1));

        // ...and clearing it takes the module back out of low power.
        let sfp = MockSfp::present();
        sfp.set_lpmode(true).unwrap();
        assert!(sfp.set_lpmode(false).unwrap());
        assert!(!sfp.get_lpmode().unwrap());
        assert!(!utils_with_sfp_at_1(sfp).is_transceiver_lpmode_on(1));
    }

    /// `is_transceiver_lpmode_on_reads_flat_memory`: the low-power and flat-memory
    /// helpers read independent registers through the same SFP seam — a module can be in
    /// low power while still being paged (VDM-capable), so the two helpers must not be
    /// conflated. This guards the DOM freeze gate, which pairs
    /// `is_vdm_statistic_supported` (a paged read) with `is_transceiver_lpmode_on`.
    #[test]
    fn is_transceiver_lpmode_on_reads_flat_memory() {
        // Paged (non-flat) module currently in low power: lpmode on, flat_memory false.
        let mut sfp = MockSfp::present();
        sfp.set_json_call("is_flat_memory", json!(false));
        sfp.set_lpmode(true).unwrap();
        let utils = utils_with_sfp_at_1(sfp);
        assert!(utils.is_transceiver_lpmode_on(1), "reads get_lpmode, not is_flat_memory");
        assert!(!utils.is_transceiver_flat_memory(1), "reads is_flat_memory, not get_lpmode");

        // Flat-memory module out of low power: lpmode off, flat_memory true.
        let mut sfp = MockSfp::present();
        sfp.set_json_call("is_flat_memory", json!(true));
        sfp.set_lpmode(false).unwrap();
        let utils = utils_with_sfp_at_1(sfp);
        assert!(!utils.is_transceiver_lpmode_on(1));
        assert!(utils.is_transceiver_flat_memory(1));
    }
}
