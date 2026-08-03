//! Port of `dom/utilities/status/{utils,db_utils}.py` — `StatusUtils` (reads) and
//! `StatusDBUtils` (posts) for `TRANSCEIVER_STATUS` and `TRANSCEIVER_STATUS_FLAG`
//! (+ `_CHANGE_COUNT/_SET_TIME/_CLEAR_TIME`).
//!
//! Both posters delegate to the shared base helpers: the hardware-status poster to
//! [`DbUtils::post_diagnostic_values_to_db`] and the status-flag poster to the flag
//! machinery in [`DbUtils::post_flags_to_db`]. `StatusDBUtils` does **not** override
//! `beautify_info_dict`, so both use the base (stringify-only) beautifier — status
//! fields carry no engineering unit to strip (unlike the DOM sensor values).

use std::sync::atomic::AtomicBool;

use serde_json::{Map, Value};

use crate::db::DbTable;
use crate::hal::{Hal, SfpHandle};
use crate::xcvrd_utilities::port_event_helper::PortMapping;

use super::db::{DbCache, DbUtils};

/// `StatusUtils` — read the transceiver hardware-status dicts off the SFP handle.
///
/// Mirrors the Python `try: … except NotImplementedError: return {}`: a successful
/// read yields `Some(value)`, a not-implemented/errored read yields `None`. The
/// shared posters treat `None` and an empty dict identically (nothing to post).
pub struct StatusUtils;

impl StatusUtils {
    pub fn new() -> Self {
        StatusUtils
    }

    /// `get_transceiver_status` — `sfp.get_transceiver_status()`.
    pub fn get_transceiver_status(&self, sfp: &dyn SfpHandle) -> Option<Value> {
        sfp.get_transceiver_status().ok()
    }

    /// `get_transceiver_status_flags` — `sfp.get_transceiver_status_flags()`.
    pub fn get_transceiver_status_flags(&self, sfp: &dyn SfpHandle) -> Option<Value> {
        sfp.call_json("get_transceiver_status_flags").ok()
    }
}

impl Default for StatusUtils {
    fn default() -> Self {
        StatusUtils::new()
    }
}

/// `StatusDBUtils` — the status posters (`TRANSCEIVER_STATUS` /
/// `TRANSCEIVER_STATUS_FLAG` + metadata).
pub struct StatusDbUtils;

impl StatusDbUtils {
    pub fn new() -> Self {
        StatusDbUtils
    }

    /// `post_port_transceiver_hw_status_to_db` → `TRANSCEIVER_STATUS`
    /// (+`last_update_time`). Reads the hardware-status dict and posts it with the
    /// base (stringify-only) beautifier.
    pub fn post_port_transceiver_hw_status_to_db(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        port_mapping: &PortMapping,
        table: &dyn DbTable,
        hal: &dyn Hal,
        db_cache: Option<&mut DbCache>,
    ) {
        if port_mapping
            .get_asic_id_for_logical_port(logical_port_name)
            .is_none()
        {
            return;
        }
        let status = StatusUtils;
        DbUtils.post_diagnostic_values_to_db(
            stop,
            logical_port_name,
            port_mapping,
            table,
            hal,
            |sfp| status.get_transceiver_status(sfp),
            db_cache,
            |m: &mut Map<String, Value>| DbUtils.beautify_info_dict(m),
            false,
        );
    }

    /// `post_port_transceiver_hw_status_flags_to_db` → `TRANSCEIVER_STATUS_FLAG`
    /// + its change-count / set-time / clear-time metadata tables. Reads the latched
    /// hardware-status flags, stamps flag change-tracking metadata on every
    /// transition, and publishes the flag row. Shares [`DbUtils::post_flags_to_db`]
    /// with the DOM-flag poster (base beautifier — status flags carry no unit).
    #[allow(clippy::too_many_arguments)]
    pub fn post_port_transceiver_hw_status_flags_to_db(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        port_mapping: &PortMapping,
        hal: &dyn Hal,
        flag_tbl: &dyn DbTable,
        flag_change_count_tbl: &dyn DbTable,
        flag_set_time_tbl: &dyn DbTable,
        flag_clear_time_tbl: &dyn DbTable,
        db_cache: Option<&mut DbCache>,
    ) {
        if port_mapping
            .get_asic_id_for_logical_port(logical_port_name)
            .is_none()
        {
            return;
        }
        let status = StatusUtils;
        DbUtils.post_flags_to_db(
            stop,
            logical_port_name,
            port_mapping,
            hal,
            flag_tbl,
            flag_change_count_tbl,
            flag_set_time_tbl,
            flag_clear_time_tbl,
            |sfp| status.get_transceiver_status_flags(sfp),
            |m: &mut Map<String, Value>| DbUtils.beautify_info_dict(m),
            "Status flags",
            db_cache,
        );
    }
}

impl Default for StatusDbUtils {
    fn default() -> Self {
        StatusDbUtils::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDbTable, MockHal, MockSfp};
    use crate::xcvrd_utilities::port_event_helper::{
        PortChangeEvent, PortChangeEventType, PortMapping,
    };
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    fn mapping_with(ports: &[(&str, usize)]) -> PortMapping {
        let mut pm = PortMapping::new();
        for (name, phys) in ports {
            pm.handle_port_change_event(&PortChangeEvent::new(
                *name,
                *phys as i32,
                0,
                PortChangeEventType::PortAdd,
            ));
        }
        pm
    }

    // TRANSCEIVER_STATUS: the read dict is stringified (base beautify — no unit
    // strip) and posted with a trailing last_update_time.
    #[test]
    fn test_post_port_transceiver_hw_status_to_db() {
        let lport = "Ethernet0";
        let pm = mapping_with(&[(lport, 0)]);
        let tbl = MockDbTable::new("TRANSCEIVER_STATUS");
        let db = StatusDbUtils;
        let stop = AtomicBool::new(false);

        let mut sfp = MockSfp::present();
        sfp.status = json!({"status": "1", "cmis_state": "READY", "error": 0});
        let hal = MockHal::with_sfps(vec![sfp]);

        db.post_port_transceiver_hw_status_to_db(&stop, lport, &pm, &tbl, &hal, None);
        assert_eq!(tbl.hget(lport, "status").as_deref(), Some("1"));
        assert_eq!(tbl.hget(lport, "cmis_state").as_deref(), Some("READY"));
        // Non-string value stringified via the base beautifier.
        assert_eq!(tbl.hget(lport, "error").as_deref(), Some("0"));
        assert!(tbl.hget(lport, "last_update_time").is_some());

        // Absent module -> nothing posted.
        let tbl2 = MockDbTable::new("TRANSCEIVER_STATUS");
        let hal_absent = MockHal::with_sfps(vec![MockSfp::default()]);
        db.post_port_transceiver_hw_status_to_db(&stop, lport, &pm, &tbl2, &hal_absent, None);
        assert_eq!(tbl2.get_size(), 0);

        // Unknown asic -> skip.
        let tbl3 = MockDbTable::new("TRANSCEIVER_STATUS");
        db.post_port_transceiver_hw_status_to_db(&stop, "Ethernet999", &pm, &tbl3, &hal, None);
        assert_eq!(tbl3.get_size(), 0);
    }

    // tests/test_xcvrd.py:test_get_transceiver_status_flags — the getter returns the
    // module's status-flag dict on a successful read. Where the Python passthrough
    // would yield `{}` (an empty dict, or a NotImplementedError), the Rust seam yields
    // an empty `Some` or `None`; the poster treats both as "nothing to post".
    #[test]
    fn test_get_transceiver_status_flags() {
        let status = StatusUtils;
        let sfp = MockSfp::present()
            .with_json("get_transceiver_status_flags", json!({"tx_fault": true}));
        assert_eq!(
            status.get_transceiver_status_flags(&sfp),
            Some(json!({"tx_fault": true}))
        );
        let sfp_empty =
            MockSfp::present().with_json("get_transceiver_status_flags", json!({}));
        assert_eq!(status.get_transceiver_status_flags(&sfp_empty), Some(json!({})));
        let sfp_err = MockSfp::present();
        assert_eq!(status.get_transceiver_status_flags(&sfp_err), None);
    }

    // TRANSCEIVER_STATUS_FLAG: flag row + metadata, transition bumps the change
    // count, no-op does not. Uses a single-key dict (mock tables REPLACE the row).
    #[test]
    fn test_post_port_transceiver_hw_status_flags_to_db() {
        let lport = "Ethernet0";
        let pm = mapping_with(&[(lport, 0)]);
        let flag = MockDbTable::new("TRANSCEIVER_STATUS_FLAG");
        let cc = MockDbTable::new("TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT");
        let st = MockDbTable::new("TRANSCEIVER_STATUS_FLAG_SET_TIME");
        let ct = MockDbTable::new("TRANSCEIVER_STATUS_FLAG_CLEAR_TIME");
        let db = StatusDbUtils;
        let stop = AtomicBool::new(false);

        // First publish (flag False): value row + metadata seeded.
        let hal_false = MockHal::with_sfps(vec![MockSfp::present()
            .with_json("get_transceiver_status_flags", json!({"tx_fault": false}))]);
        db.post_port_transceiver_hw_status_flags_to_db(
            &stop, lport, &pm, &hal_false, &flag, &cc, &st, &ct, None,
        );
        assert_eq!(flag.hget(lport, "tx_fault").as_deref(), Some("False"));
        assert!(flag.hget(lport, "last_update_time").is_some());
        assert_eq!(cc.hget(lport, "tx_fault").as_deref(), Some("0"));
        assert_eq!(st.hget(lport, "tx_fault").as_deref(), Some("never"));

        // No-op re-publish: change count not bumped.
        db.post_port_transceiver_hw_status_flags_to_db(
            &stop, lport, &pm, &hal_false, &flag, &cc, &st, &ct, None,
        );
        assert_eq!(cc.hget(lport, "tx_fault").as_deref(), Some("0"));

        // Raise: count 0 -> 1, set-time stamped.
        let hal_true = MockHal::with_sfps(vec![MockSfp::present()
            .with_json("get_transceiver_status_flags", json!({"tx_fault": true}))]);
        db.post_port_transceiver_hw_status_flags_to_db(
            &stop, lport, &pm, &hal_true, &flag, &cc, &st, &ct, None,
        );
        assert_eq!(flag.hget(lport, "tx_fault").as_deref(), Some("True"));
        assert_eq!(cc.hget(lport, "tx_fault").as_deref(), Some("1"));
        assert_ne!(st.hget(lport, "tx_fault").as_deref(), Some("never"));

        // Unknown asic -> skip.
        db.post_port_transceiver_hw_status_flags_to_db(
            &stop, "Ethernet999", &pm, &hal_true, &flag, &cc, &st, &ct, None,
        );
        assert!(flag.get("Ethernet999").is_none());
    }

    // Port of tests/test_xcvrd.py:test_get_transceiver_status — StatusUtils reads the
    // hardware-status dict off the SFP handle. A populated read yields Some(dict); an
    // empty read yields Some({}) (the poster then treats it as nothing to post). A
    // not-implemented/errored read (the Python `except NotImplementedError: return {}`)
    // maps to None via the trait's Result -> Option.
    #[test]
    fn test_get_transceiver_status() {
        let status_utils = StatusUtils;

        let sfp = MockSfp {
            status: json!({"module_state": "ModuleLowPwr", "DP1State": "DataPathDeactivated"}),
            ..MockSfp::present()
        };
        let got = status_utils.get_transceiver_status(&sfp).expect("Some(dict)");
        assert_eq!(got["module_state"], "ModuleLowPwr");

        // Empty status dict is still Some (the poster short-circuits on empty).
        let empty = MockSfp {
            status: json!({}),
            ..MockSfp::present()
        };
        assert_eq!(status_utils.get_transceiver_status(&empty), Some(json!({})));
    }
}
