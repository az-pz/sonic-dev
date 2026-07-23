//! HW status DB posters — port of `dom/utilities/status/db_utils.py`
//! (`StatusDBUtils`). Posts `TRANSCEIVER_STATUS` / `TRANSCEIVER_STATUS_FLAG`.

#![allow(dead_code, unused_variables)]

use crate::dom::utilities::db::utils::DbUtils;
use crate::dom::utilities::status::utils::StatusUtils;
use crate::hal::SfpApi;
use crate::statedb::{DbError, TableApi};
use crate::xcvrd_utilities::common::wrapper_get_presence;

/// `StatusDBUtils` (`status/db_utils.py:6`).
pub struct StatusDbUtils;

impl StatusDbUtils {
    /// `post_port_transceiver_hw_status_to_db` -> `TRANSCEIVER_STATUS`. Reads the
    /// module status via the HAL, beautifies (`str()`), appends `last_update_time`
    /// and `set`s the row. Skips absent modules and empty reads; returns `true`
    /// iff a row was written. [M3]
    pub fn post_port_transceiver_hw_status_to_db<S: SfpApi, T: TableApi>(
        logical_port_name: &str,
        sfp: &S,
        status_tbl: &T,
    ) -> Result<bool, DbError> {
        if !wrapper_get_presence(sfp) {
            return Ok(false);
        }
        let values = StatusUtils::get_transceiver_status(sfp);
        DbUtils::post_diagnostic_values_to_db(
            logical_port_name,
            status_tbl,
            &values,
            DbUtils::beautify_info_dict,
        )
    }

    /// `post_port_transceiver_hw_status_flags_to_db` (+ flag metadata).
    pub fn post_port_transceiver_hw_status_flags_to_db<S: SfpApi, T: TableApi>(
        logical_port_name: &str,
        sfp: &S,
        status_flag_tbl: &T,
    ) -> Result<(), DbError> {
        todo!("later: StatusDbUtils::post_port_transceiver_hw_status_flags_to_db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockSfp, MockStateDb};
    use crate::statedb::StateDb;
    use crate::xcvrd_utilities::xcvr_table_helper::TRANSCEIVER_STATUS_TABLE;
    use serde_json::json;

    /// <- test_post_port_transceiver_hw_status_to_db: absent / empty read -> no
    /// row; a present module with a 12-field status dict -> 12 + last_update_time.
    #[test]
    fn post_hw_status_absent_present_and_empty() {
        let db = MockStateDb::new();
        let status_tbl = db.table(TRANSCEIVER_STATUS_TABLE).unwrap();

        // Absent -> nothing published.
        let mut sfp = MockSfp::default();
        sfp.presence = false;
        sfp.status = Some(json!({"module_state": "ModuleReady"}));
        assert!(!StatusDbUtils::post_port_transceiver_hw_status_to_db("Ethernet0", &sfp, &status_tbl).unwrap());
        assert!(status_tbl.get("Ethernet0").unwrap().is_none());

        // Present + 12 status fields -> 13 fields (12 + last_update_time).
        sfp.presence = true;
        sfp.status = Some(json!({
            "cmis_state": "READY", "module_state": "ModuleReady",
            "module_fault_cause": "No Fault detected",
            "DP1State": "DataPathActivated", "DP2State": "DataPathActivated",
            "DP3State": "DataPathActivated", "DP4State": "DataPathActivated",
            "DP5State": "DataPathActivated", "DP6State": "DataPathActivated",
            "DP7State": "DataPathActivated", "DP8State": "DataPathActivated",
            "some_flag": true
        }));
        assert!(StatusDbUtils::post_port_transceiver_hw_status_to_db("Ethernet0", &sfp, &status_tbl).unwrap());
        let r = status_tbl.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.len(), 13);
        assert_eq!(r.get("module_state").map(String::as_str), Some("ModuleReady"));
        // Non-str value beautified with str() -> "True".
        assert_eq!(r.get("some_flag").map(String::as_str), Some("True"));
        assert!(r.contains_key("last_update_time"));

        // Present but empty read -> no write (prior row preserved).
        sfp.status = Some(json!({}));
        assert!(!StatusDbUtils::post_port_transceiver_hw_status_to_db("Ethernet0", &sfp, &status_tbl).unwrap());
        assert_eq!(status_tbl.get("Ethernet0").unwrap().unwrap().len(), 13);

        // NotImplemented status read -> treated as empty -> no write.
        sfp.status = None;
        assert!(!StatusDbUtils::post_port_transceiver_hw_status_to_db("Ethernet0", &sfp, &status_tbl).unwrap());
    }
}
