//! DOM sensor HAL readers — port of `dom/utilities/dom_sensor/utils.py` (`DOMUtils`).
//!
//! Thin wrappers over the SFP HAL that return the DOM dicts (`serde_json::Value`).
//! Python catches `NotImplementedError` and returns `{}`; here any HAL error
//! (including `NotImplemented`) or a non-object value collapses to an empty object
//! so the generic writer skips it.

#![allow(dead_code, unused_variables)]

use serde_json::{json, Value};

use crate::hal::SfpApi;

/// `DOMUtils` (`dom_sensor/utils.py:1`).
pub struct DomUtils;

/// Normalise a HAL DOM read to an object `Value`: any error -> `{}` (the Python
/// `except NotImplementedError: return {}`), a non-object result -> `{}` as well
/// so the downstream "empty -> skip" gate behaves.
fn dict_or_empty(result: crate::hal::Result<Value>) -> Value {
    match result {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

impl DomUtils {
    /// `get_transceiver_dom_temperature` -> `{'temperature': ...}` (opt path).
    pub fn get_transceiver_dom_temperature<S: SfpApi>(sfp: &S) -> Value {
        // The bridge exposes the full DOM dict; the module-temperature-only path
        // is not needed for M2 (DomThermalInfoUpdateTask is optional).
        match sfp.get_transceiver_dom_real_value() {
            Ok(v) => match v.get("temperature") {
                Some(t) => {
                    let mut m = serde_json::Map::new();
                    m.insert("temperature".to_string(), t.clone());
                    Value::Object(m)
                }
                None => json!({}),
            },
            Err(_) => json!({}),
        }
    }

    /// `get_transceiver_dom_sensor_real_value` -> `TRANSCEIVER_DOM_SENSOR`. [M2]
    pub fn get_transceiver_dom_sensor_real_value<S: SfpApi>(sfp: &S) -> Value {
        dict_or_empty(sfp.get_transceiver_dom_real_value())
    }

    /// `get_transceiver_dom_flags`.
    pub fn get_transceiver_dom_flags<S: SfpApi>(sfp: &S) -> Value {
        // No dedicated HAL flag getter yet; DOM flags are a later milestone.
        json!({})
    }

    /// `get_transceiver_dom_thresholds` -> `TRANSCEIVER_DOM_THRESHOLD`. [M2]
    pub fn get_transceiver_dom_thresholds<S: SfpApi>(sfp: &S) -> Value {
        dict_or_empty(sfp.get_transceiver_threshold_info())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSfp;
    use serde_json::json;

    /// <- test_get_transceiver_dom_sensor_real_value: populated dict passes
    /// through; empty dict stays empty; NotImplemented -> empty.
    #[test]
    fn dom_sensor_real_value_populated_empty_and_not_impl() {
        let mut sfp = MockSfp::default();
        sfp.dom_real_value = Some(json!({"temperature": 22.75, "voltage": 3.3}));
        let v = DomUtils::get_transceiver_dom_sensor_real_value(&sfp);
        assert_eq!(v["temperature"], json!(22.75));

        sfp.dom_real_value = Some(json!({}));
        assert_eq!(DomUtils::get_transceiver_dom_sensor_real_value(&sfp), json!({}));

        sfp.dom_real_value = None; // get_transceiver_dom_real_value -> NotImplemented
        assert_eq!(DomUtils::get_transceiver_dom_sensor_real_value(&sfp), json!({}));
    }

    /// <- test_get_transceiver_dom_thresholds: same shape over threshold info.
    #[test]
    fn dom_thresholds_populated_empty_and_not_impl() {
        let mut sfp = MockSfp::default();
        sfp.threshold_info = Some(json!({"temphighalarm": "75.0"}));
        assert_eq!(
            DomUtils::get_transceiver_dom_thresholds(&sfp)["temphighalarm"],
            json!("75.0")
        );

        sfp.threshold_info = Some(json!({}));
        assert_eq!(DomUtils::get_transceiver_dom_thresholds(&sfp), json!({}));

        sfp.threshold_info = None;
        assert_eq!(DomUtils::get_transceiver_dom_thresholds(&sfp), json!({}));
    }
}
