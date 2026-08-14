#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `dom/utilities/dom_sensor/utils.py`: DOMUtils HAL decoders.
//!
//! Each getter reaches the transceiver through the [`Chassis`]/[`Sfp`] HAL seam
//! (the `sfp_obj_dict` analogue) and returns a `serde_json::Value`. Faithful to
//! the Python: a `NotImplementedError` from the platform (here any HAL error, or
//! an unknown physical port) collapses to an empty object `{}`.

use std::rc::Rc;

use serde_json::{json, Value};

use crate::hal::{Chassis, Sfp};

/// Rust port of the Python `DOMUtils`: DOM value decoders over the HAL.
pub struct DOMUtils {
    /// The `sfp_obj_dict` analogue: resolves a physical port to its [`Sfp`].
    chassis: Rc<dyn Chassis>,
}

impl DOMUtils {
    pub fn new(chassis: Rc<dyn Chassis>) -> Self {
        DOMUtils { chassis }
    }

    fn sfp(&self, physical_port: usize) -> Option<Box<dyn Sfp>> {
        self.chassis.sfp(physical_port).ok()
    }

    /// `get_transceiver_dom_temperature` (utils.py:10): `{'temperature': <val>}`,
    /// or `{}` if the module can't serve it. `get_temperature` is reached through
    /// the no-arg `call_json` escape hatch (it is not one of the typed wrappers).
    pub fn get_transceiver_dom_temperature(&self, physical_port: usize) -> Value {
        match self.sfp(physical_port) {
            Some(sfp) => match sfp.call_json("get_temperature") {
                Ok(v) => json!({ "temperature": v }),
                Err(_) => json!({}),
            },
            None => json!({}),
        }
    }

    /// `get_transceiver_dom_sensor_real_value` (utils.py:18).
    pub fn get_transceiver_dom_sensor_real_value(&self, physical_port: usize) -> Value {
        match self.sfp(physical_port) {
            Some(sfp) => sfp.get_transceiver_dom_real_value().unwrap_or_else(|_| json!({})),
            None => json!({}),
        }
    }

    /// `get_transceiver_dom_flags` (utils.py:24).
    pub fn get_transceiver_dom_flags(&self, physical_port: usize) -> Value {
        match self.sfp(physical_port) {
            Some(sfp) => sfp.call_json("get_transceiver_dom_flags").unwrap_or_else(|_| json!({})),
            None => json!({}),
        }
    }

    /// `get_transceiver_dom_thresholds` (utils.py:30) → `get_transceiver_threshold_info`.
    pub fn get_transceiver_dom_thresholds(&self, physical_port: usize) -> Value {
        match self.sfp(physical_port) {
            Some(sfp) => sfp.get_transceiver_threshold_info().unwrap_or_else(|_| json!({})),
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

    /// Port of `tests/test_xcvrd.py::test_get_transceiver_dom_temperature`.
    #[test]
    fn test_get_transceiver_dom_temperature() {
        let mut sfp = MockSfp::default();
        sfp.set_json_call("get_temperature", json!(42.0));
        let dom_utils = DOMUtils::new(chassis_with(sfp));
        assert!(dom_utils.get_transceiver_dom_temperature(0).get("temperature").is_some());

        // NotImplementedError from the platform → {}.
        let mut sfp = MockSfp::default();
        sfp.fail_method("get_temperature");
        let dom_utils = DOMUtils::new(chassis_with(sfp));
        assert_eq!(dom_utils.get_transceiver_dom_temperature(0), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_get_transceiver_dom_sensor_real_value`.
    #[test]
    fn test_get_transceiver_dom_sensor_real_value() {
        let mut sfp = MockSfp::default();
        sfp.dom = json!(true);
        let dom_utils = DOMUtils::new(chassis_with(sfp));
        assert_eq!(dom_utils.get_transceiver_dom_sensor_real_value(0), json!(true));

        let mut sfp = MockSfp::default();
        sfp.dom = json!({});
        let dom_utils = DOMUtils::new(chassis_with(sfp));
        assert_eq!(dom_utils.get_transceiver_dom_sensor_real_value(0), json!({}));

        let mut sfp = MockSfp::default();
        sfp.fail_method("get_transceiver_dom_real_value");
        let dom_utils = DOMUtils::new(chassis_with(sfp));
        assert_eq!(dom_utils.get_transceiver_dom_sensor_real_value(0), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_get_transceiver_dom_thresholds`.
    #[test]
    fn test_get_transceiver_dom_thresholds() {
        let mut sfp = MockSfp::default();
        sfp.thresholds = json!(true);
        let dom_utils = DOMUtils::new(chassis_with(sfp));
        assert_eq!(dom_utils.get_transceiver_dom_thresholds(0), json!(true));

        let mut sfp = MockSfp::default();
        sfp.thresholds = json!({});
        let dom_utils = DOMUtils::new(chassis_with(sfp));
        assert_eq!(dom_utils.get_transceiver_dom_thresholds(0), json!({}));

        let mut sfp = MockSfp::default();
        sfp.fail_method("get_transceiver_threshold_info");
        let dom_utils = DOMUtils::new(chassis_with(sfp));
        assert_eq!(dom_utils.get_transceiver_dom_thresholds(0), json!({}));
    }

    /// Unknown physical port (no SFP) collapses to `{}`, like an absent dict key.
    #[test]
    fn unknown_physical_port_yields_empty() {
        let dom_utils = DOMUtils::new(Rc::new(MockChassis::with_sfps(vec![])));
        assert_eq!(dom_utils.get_transceiver_dom_sensor_real_value(3), json!({}));
    }
}
