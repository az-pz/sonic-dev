#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `dom/utilities/status/utils.py`: StatusUtils HAL decoders.
//!
//! Each getter reaches the transceiver through the [`Chassis`]/[`Sfp`] HAL seam
//! (the `sfp_obj_dict` analogue) and returns a `serde_json::Value`. Faithful to
//! the Python: a `NotImplementedError` from the platform (here any HAL error, or
//! an unknown physical port) collapses to an empty object `{}`.

use std::rc::Rc;

use serde_json::{json, Value};

use crate::hal::{Chassis, Sfp};

/// Rust port of the Python `StatusUtils`: transceiver status decoders over the HAL.
pub struct StatusUtils {
    /// The `sfp_obj_dict` analogue: resolves a physical port to its [`Sfp`].
    chassis: Rc<dyn Chassis>,
}

impl StatusUtils {
    pub fn new(chassis: Rc<dyn Chassis>) -> Self {
        StatusUtils { chassis }
    }

    fn sfp(&self, physical_port: usize) -> Option<Box<dyn Sfp>> {
        self.chassis.sfp(physical_port).ok()
    }

    /// `get_transceiver_status` (utils.py:10): the rich CMIS status dict
    /// (`module_state`, `DP1..8State`, per-lane tx/rx status, …), or `{}` if the
    /// module can't serve it (`NotImplementedError` → `{}`).
    pub fn get_transceiver_status(&self, physical_port: usize) -> Value {
        match self.sfp(physical_port) {
            Some(sfp) => sfp.get_transceiver_status().unwrap_or_else(|_| json!({})),
            None => json!({}),
        }
    }

    /// `get_transceiver_status_flags` (utils.py:19): the latched status flags
    /// (`module_firmware_fault`, per-lane `txNfault`/`rxNlos`, …), reached through
    /// the no-arg `call_json` escape hatch. `NotImplementedError` → `{}`.
    pub fn get_transceiver_status_flags(&self, physical_port: usize) -> Value {
        match self.sfp(physical_port) {
            Some(sfp) => sfp
                .call_json("get_transceiver_status_flags")
                .unwrap_or_else(|_| json!({})),
            None => json!({}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockChassis, MockSfp};

    fn chassis_with(sfp: MockSfp) -> Rc<dyn Chassis> {
        Rc::new(MockChassis::with_sfps(vec![sfp]))
    }

    /// Port of `tests/test_xcvrd.py::test_get_transceiver_status`.
    #[test]
    fn test_get_transceiver_status() {
        let status = json!({ "module_state": "ModuleReady", "DP1State": "DataPathActivated" });
        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.status = status.clone();
        let status_utils = StatusUtils::new(chassis_with(sfp));
        assert_eq!(status_utils.get_transceiver_status(0), status);

        // Unknown physical port -> {}.
        assert_eq!(status_utils.get_transceiver_status(9), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_get_transceiver_status_flags`.
    #[test]
    fn test_get_transceiver_status_flags() {
        let flags = json!({ "module_firmware_fault": false, "tx1fault": true });
        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.set_json_call("get_transceiver_status_flags", flags.clone());
        let status_utils = StatusUtils::new(chassis_with(sfp));
        assert_eq!(status_utils.get_transceiver_status_flags(0), flags);

        // Platform raises NotImplementedError -> {}.
        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.fail_method("get_transceiver_status_flags");
        let status_utils = StatusUtils::new(chassis_with(sfp));
        assert_eq!(status_utils.get_transceiver_status_flags(0), json!({}));
    }
}
