#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `dom/utilities/vdm/utils.py`: `VDMUtils` — the HAL decoders and the
//! freeze/unfreeze confirm cycle for CMIS VDM.
//!
//! Each getter reaches the transceiver through the [`Chassis`]/[`Sfp`] HAL seam
//! (the `sfp_obj_dict` analogue). Faithful to the Python: a capability probe that
//! raises (`NotImplementedError`/`AttributeError`, here any HAL error, or an
//! unknown physical port) is `False`, and a value decoder that raises collapses to
//! an empty object `{}` (the raw value is otherwise passed through verbatim so a
//! platform `None`/`{}` is preserved for the caller's `... or {}` merge).
//!
//! The freeze/unfreeze confirm loop (`_vdm_action_and_confirm`) waits `settle` for
//! the module to clear the done bit, then polls the done status until it confirms
//! or the `budget` elapses. The three timings are injectable ([`VDMUtils::with_timeouts`])
//! so a unit test can exercise the "never confirms → False" path without the 1 s
//! production budget; the deployed daemon uses the CMIS-spec defaults.

use std::rc::Rc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::dom::utilities::db::utils::{py_truthy, DomLogger, NoopDomLogger};
use crate::hal::{Chassis, Sfp};

/// `MAX_tVDMF_TIME_MSECS` — settle time after issuing freeze/unfreeze before the
/// done bit is polled (vdm/utils.py:4).
const MAX_TVDMF_TIME_MSECS: u64 = 10;
/// `MAX_VDM_FREEZE_UNFREEZE_TIME_MSECS` — total budget to confirm the done bit.
const MAX_VDM_FREEZE_UNFREEZE_TIME_MSECS: u64 = 1000;
/// `FREEZE_UNFREEZE_DONE_POLLING_INTERVAL_MSECS` — done-bit poll interval.
const FREEZE_UNFREEZE_DONE_POLLING_INTERVAL_MSECS: u64 = 1;

/// Rust port of the Python `VDMUtils`.
pub struct VDMUtils {
    /// The `sfp_obj_dict` analogue: resolves a physical port to its [`Sfp`].
    chassis: Rc<dyn Chassis>,
    logger: Rc<dyn DomLogger>,
    settle: Duration,
    budget: Duration,
    poll: Duration,
}

impl VDMUtils {
    pub fn new(chassis: Rc<dyn Chassis>) -> Self {
        VDMUtils::with_logger(chassis, Rc::new(NoopDomLogger))
    }

    pub fn with_logger(chassis: Rc<dyn Chassis>, logger: Rc<dyn DomLogger>) -> Self {
        VDMUtils {
            chassis,
            logger,
            settle: Duration::from_millis(MAX_TVDMF_TIME_MSECS),
            budget: Duration::from_millis(MAX_VDM_FREEZE_UNFREEZE_TIME_MSECS),
            poll: Duration::from_millis(FREEZE_UNFREEZE_DONE_POLLING_INTERVAL_MSECS),
        }
    }

    /// Test constructor: shrink the freeze/unfreeze timings so the "never confirms"
    /// path resolves in milliseconds instead of the 1 s production budget.
    pub fn with_timeouts(chassis: Rc<dyn Chassis>, settle_ms: u64, budget_ms: u64, poll_ms: u64) -> Self {
        VDMUtils {
            chassis,
            logger: Rc::new(NoopDomLogger),
            settle: Duration::from_millis(settle_ms),
            budget: Duration::from_millis(budget_ms),
            poll: Duration::from_millis(poll_ms),
        }
    }

    fn sfp(&self, physical_port: usize) -> Option<Box<dyn Sfp>> {
        self.chassis.sfp(physical_port).ok()
    }

    /// `is_transceiver_vdm_supported` (vdm/utils.py:17): capability bool,
    /// `NotImplementedError`/absent → `False`.
    pub fn is_transceiver_vdm_supported(&self, physical_port: usize) -> bool {
        self.probe(physical_port, "is_transceiver_vdm_supported")
    }

    /// `is_vdm_statistic_supported` (vdm/utils.py:23): capability bool,
    /// `NotImplementedError`/`AttributeError`/absent → `False`.
    pub fn is_vdm_statistic_supported(&self, physical_port: usize) -> bool {
        self.probe(physical_port, "is_vdm_statistic_supported")
    }

    /// `get_vdm_real_values_basic` (vdm/utils.py:29): the basic (instantaneous)
    /// observables, error → `{}`.
    pub fn get_vdm_real_values_basic(&self, physical_port: usize) -> Value {
        self.decode(physical_port, "get_transceiver_vdm_real_value_basic")
    }

    /// `get_vdm_real_values_statistic` (vdm/utils.py:36): the min/max/avg statistic
    /// observables (read under freeze), error → `{}`.
    pub fn get_vdm_real_values_statistic(&self, physical_port: usize) -> Value {
        self.decode(physical_port, "get_transceiver_vdm_real_value_statistic")
    }

    /// `get_vdm_flags` (vdm/utils.py:43): the VDM flag dict (keys suffixed with the
    /// threshold type), error → `{}`.
    pub fn get_vdm_flags(&self, physical_port: usize) -> Value {
        self.decode(physical_port, "get_transceiver_vdm_flags")
    }

    /// `get_vdm_thresholds` (vdm/utils.py:50): the VDM threshold dict (keys suffixed
    /// with the threshold type), error → `{}`.
    pub fn get_vdm_thresholds(&self, physical_port: usize) -> Value {
        self.decode(physical_port, "get_transceiver_vdm_thresholds")
    }

    /// A capability probe: `bool(sfp.<method>())`, any error / absent module → `False`.
    fn probe(&self, physical_port: usize, method: &str) -> bool {
        match self.sfp(physical_port) {
            Some(sfp) => match sfp.call_json(method) {
                Ok(v) => py_truthy(&v),
                Err(_) => false,
            },
            None => false,
        }
    }

    /// A value decoder: `sfp.<method>()` passed through verbatim, any error / absent
    /// module → `{}`.
    fn decode(&self, physical_port: usize, method: &str) -> Value {
        match self.sfp(physical_port) {
            Some(sfp) => sfp.call_json(method).unwrap_or_else(|_| json!({})),
            None => json!({}),
        }
    }

    /// `_vdm_action_and_confirm` (vdm/utils.py:68): run a freeze/unfreeze action,
    /// wait `settle`, then poll its done-status until it confirms (within `budget`).
    /// Returns `false` if the action reports failure, never confirms, the module is
    /// absent, or the HAL call errors (`KeyError`/`NotImplementedError`).
    pub fn vdm_action_and_confirm(
        &self,
        physical_port: usize,
        action_method: &str,
        status_method: &str,
        action_name: &str,
    ) -> bool {
        let sfp = match self.sfp(physical_port) {
            Some(s) => s,
            None => {
                self.logger.log_error(&format!(
                    "VDM {action_name} failed for port {physical_port} as no sfp object found"
                ));
                return false;
            }
        };
        // status = action(); if not status: return False
        match sfp.call_json(action_method) {
            Ok(v) if py_truthy(&v) => {}
            Ok(_) => {
                self.logger.log_error(&format!(
                    "Failed to {action_name} VDM stats for port {physical_port}"
                ));
                return false;
            }
            Err(e) => {
                self.logger.log_error(&format!(
                    "VDM {action_name} failed for port {physical_port} with exception {e}"
                ));
                return false;
            }
        }
        // Wait MAX_tVDMF to allow the module to clear the done bit.
        sleep(self.settle);
        // Poll for the done bit within the budget.
        let start = Instant::now();
        while start.elapsed() < self.budget {
            match sfp.call_json(status_method) {
                Ok(v) if py_truthy(&v) => return true,
                Ok(_) => {}
                Err(e) => {
                    self.logger.log_error(&format!(
                        "VDM {action_name} failed for port {physical_port} with exception {e}"
                    ));
                    return false;
                }
            }
            sleep(self.poll);
        }
        self.logger.log_error(&format!(
            "Failed to confirm VDM {action_name} status for port {physical_port}"
        ));
        false
    }

    /// `_freeze_vdm_stats_and_confirm` (vdm/utils.py:103).
    pub fn freeze_vdm_stats_and_confirm(&self, physical_port: usize) -> bool {
        self.vdm_action_and_confirm(physical_port, "freeze_vdm_stats", "get_vdm_freeze_status", "freeze")
    }

    /// `_unfreeze_vdm_stats_and_confirm` (vdm/utils.py:119).
    pub fn unfreeze_vdm_stats_and_confirm(&self, physical_port: usize) -> bool {
        self.vdm_action_and_confirm(physical_port, "unfreeze_vdm_stats", "get_vdm_unfreeze_status", "unfreeze")
    }

    /// `vdm_freeze_context` (vdm/utils.py:57): freeze + confirm, run `body(frozen)`,
    /// then *always* unfreeze (the contextmanager `finally`). `body` receives whether
    /// the freeze confirmed (the Python `yield True/False`).
    pub fn with_vdm_freeze<F, R>(&self, physical_port: usize, body: F) -> R
    where
        F: FnOnce(bool) -> R,
    {
        let frozen = self.freeze_vdm_stats_and_confirm(physical_port);
        if !frozen {
            self.logger.log_error(&format!(
                "Failed to freeze VDM stats in contextmanager for port {physical_port}"
            ));
        }
        let result = body(frozen);
        if !self.unfreeze_vdm_stats_and_confirm(physical_port) {
            self.logger.log_error(&format!(
                "Failed to unfreeze VDM stats in contextmanager for port {physical_port}"
            ));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockChassis, MockSfp};

    fn chassis_with(sfp: MockSfp) -> Rc<dyn Chassis> {
        Rc::new(MockChassis::with_sfps(vec![sfp]))
    }

    /// Port of `tests/test_xcvrd.py::test_wrapper_is_transceiver_vdm_supported`:
    /// NotImplementedError → False, and a bool return is passed through.
    #[test]
    fn test_wrapper_is_transceiver_vdm_supported() {
        // NotImplementedError from the platform → False.
        let mut sfp = MockSfp::default();
        sfp.fail_method("is_transceiver_vdm_supported");
        let vdm = VDMUtils::new(chassis_with(sfp));
        assert!(!vdm.is_transceiver_vdm_supported(0));

        // Return False → False.
        let mut sfp = MockSfp::default();
        sfp.set_json_call("is_transceiver_vdm_supported", json!(false));
        let vdm = VDMUtils::new(chassis_with(sfp));
        assert!(!vdm.is_transceiver_vdm_supported(0));

        // Return True → True.
        let mut sfp = MockSfp::default();
        sfp.set_json_call("is_transceiver_vdm_supported", json!(true));
        let vdm = VDMUtils::new(chassis_with(sfp));
        assert!(vdm.is_transceiver_vdm_supported(0));
    }

    /// Port of `tests/test_xcvrd.py::test_is_vdm_statistic_supported`: True/False
    /// pass through; NotImplementedError/AttributeError → False.
    #[test]
    fn test_is_vdm_statistic_supported() {
        let mut sfp = MockSfp::default();
        sfp.set_json_call("is_vdm_statistic_supported", json!(true));
        assert!(VDMUtils::new(chassis_with(sfp)).is_vdm_statistic_supported(0));

        let mut sfp = MockSfp::default();
        sfp.set_json_call("is_vdm_statistic_supported", json!(false));
        assert!(!VDMUtils::new(chassis_with(sfp)).is_vdm_statistic_supported(0));

        // NotImplementedError / AttributeError (any HAL error) → False.
        let mut sfp = MockSfp::default();
        sfp.fail_method("is_vdm_statistic_supported");
        assert!(!VDMUtils::new(chassis_with(sfp)).is_vdm_statistic_supported(0));
    }

    /// Port of `tests/test_xcvrd.py::test_get_vdm_real_values_basic`: dict passes
    /// through; any error → `{}`.
    #[test]
    fn test_get_vdm_real_values_basic() {
        let mut sfp = MockSfp::default();
        sfp.set_json_call("get_transceiver_vdm_real_value_basic", json!({"basic_key": "basic_value"}));
        assert_eq!(
            VDMUtils::new(chassis_with(sfp)).get_vdm_real_values_basic(0),
            json!({"basic_key": "basic_value"})
        );

        let mut sfp = MockSfp::default();
        sfp.set_json_call("get_transceiver_vdm_real_value_basic", json!({}));
        assert_eq!(VDMUtils::new(chassis_with(sfp)).get_vdm_real_values_basic(0), json!({}));

        let mut sfp = MockSfp::default();
        sfp.fail_method("get_transceiver_vdm_real_value_basic");
        assert_eq!(VDMUtils::new(chassis_with(sfp)).get_vdm_real_values_basic(0), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_get_vdm_real_values_statistic`.
    #[test]
    fn test_get_vdm_real_values_statistic() {
        let mut sfp = MockSfp::default();
        sfp.set_json_call("get_transceiver_vdm_real_value_statistic", json!({"stat_key": "stat_value"}));
        assert_eq!(
            VDMUtils::new(chassis_with(sfp)).get_vdm_real_values_statistic(0),
            json!({"stat_key": "stat_value"})
        );

        let mut sfp = MockSfp::default();
        sfp.set_json_call("get_transceiver_vdm_real_value_statistic", json!({}));
        assert_eq!(VDMUtils::new(chassis_with(sfp)).get_vdm_real_values_statistic(0), json!({}));

        let mut sfp = MockSfp::default();
        sfp.fail_method("get_transceiver_vdm_real_value_statistic");
        assert_eq!(VDMUtils::new(chassis_with(sfp)).get_vdm_real_values_statistic(0), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_get_vdm_thresholds`: truthy value passes
    /// through; NotImplementedError → `{}`.
    #[test]
    fn test_get_vdm_thresholds() {
        let mut sfp = MockSfp::default();
        sfp.set_json_call("get_transceiver_vdm_thresholds", json!(true));
        assert!(py_truthy(&VDMUtils::new(chassis_with(sfp)).get_vdm_thresholds(0)));

        let mut sfp = MockSfp::default();
        sfp.set_json_call("get_transceiver_vdm_thresholds", json!({}));
        assert_eq!(VDMUtils::new(chassis_with(sfp)).get_vdm_thresholds(0), json!({}));

        let mut sfp = MockSfp::default();
        sfp.fail_method("get_transceiver_vdm_thresholds");
        assert_eq!(VDMUtils::new(chassis_with(sfp)).get_vdm_thresholds(0), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_get_vdm_flags`: truthy passes through; a
    /// HAL error → `{}`.
    #[test]
    fn test_get_vdm_flags() {
        let mut sfp = MockSfp::default();
        sfp.set_json_call("get_transceiver_vdm_flags", json!(true));
        assert!(py_truthy(&VDMUtils::new(chassis_with(sfp)).get_vdm_flags(0)));

        let mut sfp = MockSfp::default();
        sfp.set_json_call("get_transceiver_vdm_flags", json!({}));
        assert_eq!(VDMUtils::new(chassis_with(sfp)).get_vdm_flags(0), json!({}));

        let mut sfp = MockSfp::default();
        sfp.fail_method("get_transceiver_vdm_flags");
        assert_eq!(VDMUtils::new(chassis_with(sfp)).get_vdm_flags(0), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_vdm_action_and_confirm` (the three rows):
    /// action ok + status ok → True; action ok + status never confirms → False (after
    /// the budget); action fails → False.
    #[test]
    fn test_vdm_action_and_confirm() {
        // action completes and status confirms → True.
        let mut sfp = MockSfp::default();
        sfp.set_json_call("freeze_vdm_stats", json!(true));
        sfp.set_json_call("get_vdm_freeze_status", json!(true));
        let vdm = VDMUtils::with_timeouts(chassis_with(sfp), 0, 20, 0);
        assert!(vdm.vdm_action_and_confirm(0, "freeze_vdm_stats", "get_vdm_freeze_status", "freeze"));

        // action completes but status never confirms → False after the budget.
        let mut sfp = MockSfp::default();
        sfp.set_json_call("freeze_vdm_stats", json!(true));
        sfp.set_json_call("get_vdm_freeze_status", json!(false));
        let vdm = VDMUtils::with_timeouts(chassis_with(sfp), 0, 5, 1);
        assert!(!vdm.vdm_action_and_confirm(0, "freeze_vdm_stats", "get_vdm_freeze_status", "freeze"));

        // action reports failure → False (no polling).
        let mut sfp = MockSfp::default();
        sfp.set_json_call("freeze_vdm_stats", json!(false));
        let vdm = VDMUtils::with_timeouts(chassis_with(sfp), 0, 5, 1);
        assert!(!vdm.vdm_action_and_confirm(0, "freeze_vdm_stats", "get_vdm_freeze_status", "freeze"));
    }

    /// Port of `tests/test_xcvrd.py::test_vdm_action_and_confirm_exception`: the
    /// action itself raising (`NotImplementedError`) → False.
    #[test]
    fn test_vdm_action_and_confirm_exception() {
        let mut sfp = MockSfp::default();
        sfp.fail_method("freeze_vdm_stats");
        let vdm = VDMUtils::with_timeouts(chassis_with(sfp), 0, 5, 1);
        assert!(!vdm.vdm_action_and_confirm(0, "freeze_vdm_stats", "get_vdm_freeze_status", "freeze"));
    }

    /// The freeze context always unfreezes (the contextmanager `finally`), and hands
    /// `body` whether the freeze confirmed.
    #[test]
    fn with_vdm_freeze_always_unfreezes() {
        let mut sfp = MockSfp::default();
        sfp.set_json_call("freeze_vdm_stats", json!(true));
        sfp.set_json_call("get_vdm_freeze_status", json!(true));
        sfp.set_json_call("unfreeze_vdm_stats", json!(true));
        sfp.set_json_call("get_vdm_unfreeze_status", json!(true));
        let vdm = VDMUtils::with_timeouts(chassis_with(sfp), 0, 20, 0);
        let frozen = vdm.with_vdm_freeze(0, |frozen| frozen);
        assert!(frozen);

        // Freeze never confirms → body sees false, unfreeze still attempted.
        let mut sfp = MockSfp::default();
        sfp.set_json_call("freeze_vdm_stats", json!(true));
        sfp.set_json_call("get_vdm_freeze_status", json!(false));
        sfp.set_json_call("unfreeze_vdm_stats", json!(true));
        sfp.set_json_call("get_vdm_unfreeze_status", json!(true));
        let vdm = VDMUtils::with_timeouts(chassis_with(sfp), 0, 5, 1);
        let frozen = vdm.with_vdm_freeze(0, |frozen| frozen);
        assert!(!frozen);
    }
}
