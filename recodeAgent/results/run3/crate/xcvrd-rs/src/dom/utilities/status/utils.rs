//! HW status HAL readers — port of `dom/utilities/status/utils.py` (`StatusUtils`).

#![allow(dead_code, unused_variables)]

use serde_json::{json, Value};

use crate::hal::SfpApi;

/// `StatusUtils` (`status/utils.py:1`).
pub struct StatusUtils;

impl StatusUtils {
    /// `get_transceiver_status` -> `TRANSCEIVER_STATUS`. Python returns the raw
    /// `sfp.get_transceiver_status()` dict and collapses `NotImplementedError` to
    /// `{}`; here any HAL error (incl. `NotImplemented`) yields an empty object so
    /// the generic writer skips it. [M3]
    pub fn get_transceiver_status<S: SfpApi>(sfp: &S) -> Value {
        match sfp.get_transceiver_status() {
            Ok(v) => v,
            Err(_) => json!({}),
        }
    }

    /// `get_transceiver_status_flags`.
    pub fn get_transceiver_status_flags<S: SfpApi>(sfp: &S) -> Value {
        todo!("later: StatusUtils::get_transceiver_status_flags")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSfp;

    /// <- test_get_transceiver_status: a populated status dict passes through; an
    /// empty dict stays empty; NotImplementedError -> `{}`.
    #[test]
    fn get_transceiver_status_populated_empty_and_not_impl() {
        let mut sfp = MockSfp::default();
        sfp.status = Some(json!({"module_state": "ModuleReady", "DP1State": "DataPathActivated"}));
        let v = StatusUtils::get_transceiver_status(&sfp);
        assert_eq!(v["module_state"], json!("ModuleReady"));

        sfp.status = Some(json!({}));
        assert_eq!(StatusUtils::get_transceiver_status(&sfp), json!({}));

        sfp.status = None; // get_transceiver_status -> NotImplemented
        assert_eq!(StatusUtils::get_transceiver_status(&sfp), json!({}));
    }
}
