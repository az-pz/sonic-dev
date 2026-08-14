#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `xcvrd_utilities/xcvr_table_helper.py`: STATE_DB table-name constants + XcvrTableHelper handles.
use serde_json::Value;
use crate::hal::Sfp;
use crate::db::Table;

pub const TRANSCEIVER_INFO_TABLE: &str = "TRANSCEIVER_INFO";
pub const TRANSCEIVER_FIRMWARE_INFO_TABLE: &str = "TRANSCEIVER_FIRMWARE_INFO";
pub const TRANSCEIVER_DOM_SENSOR_TABLE: &str = "TRANSCEIVER_DOM_SENSOR";
pub const TRANSCEIVER_DOM_FLAG_TABLE: &str = "TRANSCEIVER_DOM_FLAG";
pub const TRANSCEIVER_DOM_FLAG_CHANGE_COUNT_TABLE: &str = "TRANSCEIVER_DOM_FLAG_CHANGE_COUNT";
pub const TRANSCEIVER_DOM_FLAG_SET_TIME_TABLE: &str = "TRANSCEIVER_DOM_FLAG_SET_TIME";
pub const TRANSCEIVER_DOM_FLAG_CLEAR_TIME_TABLE: &str = "TRANSCEIVER_DOM_FLAG_CLEAR_TIME";
pub const TRANSCEIVER_DOM_THRESHOLD_TABLE: &str = "TRANSCEIVER_DOM_THRESHOLD";
pub const TRANSCEIVER_DOM_TEMPERATURE_TABLE: &str = "TRANSCEIVER_DOM_TEMPERATURE";
pub const TRANSCEIVER_STATUS_TABLE: &str = "TRANSCEIVER_STATUS";
pub const TRANSCEIVER_STATUS_FLAG_TABLE: &str = "TRANSCEIVER_STATUS_FLAG";
pub const TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT_TABLE: &str = "TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT";
pub const TRANSCEIVER_STATUS_FLAG_SET_TIME_TABLE: &str = "TRANSCEIVER_STATUS_FLAG_SET_TIME";
pub const TRANSCEIVER_STATUS_FLAG_CLEAR_TIME_TABLE: &str = "TRANSCEIVER_STATUS_FLAG_CLEAR_TIME";
pub const TRANSCEIVER_STATUS_SW_TABLE: &str = "TRANSCEIVER_STATUS_SW";
pub const TRANSCEIVER_VDM_REAL_VALUE_TABLE: &str = "TRANSCEIVER_VDM_REAL_VALUE";
pub const TRANSCEIVER_VDM_HALARM_THRESHOLD_TABLE: &str = "TRANSCEIVER_VDM_HALARM_THRESHOLD";
pub const TRANSCEIVER_VDM_LALARM_THRESHOLD_TABLE: &str = "TRANSCEIVER_VDM_LALARM_THRESHOLD";
pub const TRANSCEIVER_VDM_HWARN_THRESHOLD_TABLE: &str = "TRANSCEIVER_VDM_HWARN_THRESHOLD";
pub const TRANSCEIVER_VDM_LWARN_THRESHOLD_TABLE: &str = "TRANSCEIVER_VDM_LWARN_THRESHOLD";
pub const TRANSCEIVER_VDM_HALARM_FLAG: &str = "TRANSCEIVER_VDM_HALARM_FLAG";
pub const TRANSCEIVER_VDM_LALARM_FLAG: &str = "TRANSCEIVER_VDM_LALARM_FLAG";
pub const TRANSCEIVER_VDM_HWARN_FLAG: &str = "TRANSCEIVER_VDM_HWARN_FLAG";
pub const TRANSCEIVER_VDM_LWARN_FLAG: &str = "TRANSCEIVER_VDM_LWARN_FLAG";
pub const TRANSCEIVER_VDM_HALARM_FLAG_CHANGE_COUNT: &str = "TRANSCEIVER_VDM_HALARM_FLAG_CHANGE_COUNT";
pub const TRANSCEIVER_VDM_LALARM_FLAG_CHANGE_COUNT: &str = "TRANSCEIVER_VDM_LALARM_FLAG_CHANGE_COUNT";
pub const TRANSCEIVER_VDM_HWARN_FLAG_CHANGE_COUNT: &str = "TRANSCEIVER_VDM_HWARN_FLAG_CHANGE_COUNT";
pub const TRANSCEIVER_VDM_LWARN_FLAG_CHANGE_COUNT: &str = "TRANSCEIVER_VDM_LWARN_FLAG_CHANGE_COUNT";
pub const TRANSCEIVER_VDM_HALARM_FLAG_SET_TIME: &str = "TRANSCEIVER_VDM_HALARM_FLAG_SET_TIME";
pub const TRANSCEIVER_VDM_LALARM_FLAG_SET_TIME: &str = "TRANSCEIVER_VDM_LALARM_FLAG_SET_TIME";
pub const TRANSCEIVER_VDM_HWARN_FLAG_SET_TIME: &str = "TRANSCEIVER_VDM_HWARN_FLAG_SET_TIME";
pub const TRANSCEIVER_VDM_LWARN_FLAG_SET_TIME: &str = "TRANSCEIVER_VDM_LWARN_FLAG_SET_TIME";
pub const TRANSCEIVER_VDM_HALARM_FLAG_CLEAR_TIME: &str = "TRANSCEIVER_VDM_HALARM_FLAG_CLEAR_TIME";
pub const TRANSCEIVER_VDM_LALARM_FLAG_CLEAR_TIME: &str = "TRANSCEIVER_VDM_LALARM_FLAG_CLEAR_TIME";
pub const TRANSCEIVER_VDM_HWARN_FLAG_CLEAR_TIME: &str = "TRANSCEIVER_VDM_HWARN_FLAG_CLEAR_TIME";
pub const TRANSCEIVER_VDM_LWARN_FLAG_CLEAR_TIME: &str = "TRANSCEIVER_VDM_LWARN_FLAG_CLEAR_TIME";
pub const TRANSCEIVER_PM_TABLE: &str = "TRANSCEIVER_PM";
pub const NPU_SI_SETTINGS_SYNC_STATUS_KEY: &str = "NPU_SI_SETTINGS_SYNC_STATUS";
pub const NPU_SI_SETTINGS_DEFAULT_VALUE: &str = "NPU_SI_SETTINGS_DEFAULT";
pub const NPU_SI_SETTINGS_NOTIFIED_VALUE: &str = "NPU_SI_SETTINGS_NOTIFIED";
pub const VDM_THRESHOLD_TYPES: [&str; 4] = ["halarm", "lalarm", "hwarn", "lwarn"];

/// Rust port of the Python `XcvrTableHelper`.
#[derive(Default)]
pub struct XcvrTableHelper;

impl XcvrTableHelper {
    pub fn new() -> Self { XcvrTableHelper }

    /// Port of `get_state_db_port_table_val_by_key`: read one field from STATE_DB
    /// `PORT_TABLE|<lport>`. The Python resolves the per-asic `state_port_tbl` from
    /// `port_mapping.get_asic_id_for_logical_port` + `get_state_port_tbl`; here the
    /// resolved table is passed in explicitly (the daemon owns the handles). Returns
    /// `None` when `port_mapping` is absent, the state table is absent, the logical port
    /// has no row, or the row lacks `key`.
    pub fn get_state_db_port_table_val_by_key(
        &self,
        lport: &str,
        port_mapping: Option<&crate::xcvrd_utilities::port_event_helper::PortMapping>,
        state_port_tbl: Option<&dyn Table>,
        key: &str,
    ) -> Option<String> {
        port_mapping?;
        let state_port_tbl = state_port_tbl?;
        let row = state_port_tbl.get(lport).ok().flatten()?;
        row.into_iter().find(|(f, _)| f == key).map(|(_, v)| v)
    }

    /// Port of `is_npu_si_settings_update_required`: NPU SI settings must be (re)applied
    /// when `NPU_SI_SETTINGS_SYNC_STATUS` is absent/unreadable OR still at the DEFAULT
    /// value — i.e. it has not yet been NOTIFIED to APPL_DB for this port.
    pub fn is_npu_si_settings_update_required(
        &self,
        lport: &str,
        port_mapping: Option<&crate::xcvrd_utilities::port_event_helper::PortMapping>,
        state_port_tbl: Option<&dyn Table>,
    ) -> bool {
        match self.get_state_db_port_table_val_by_key(
            lport,
            port_mapping,
            state_port_tbl,
            NPU_SI_SETTINGS_SYNC_STATUS_KEY,
        ) {
            None => true,
            Some(v) => v == NPU_SI_SETTINGS_DEFAULT_VALUE,
        }
    }

    /// `get_gearbox_line_lanes_dict` — build `{logical_port: line_lane_count}` from a gearbox
    /// APPL_DB `_GEARBOX_TABLE`. Only `interface:` keys are processed; a row contributes an
    /// entry only when it has BOTH a non-empty `name` and non-empty `line_lanes`, with the
    /// count = number of comma-separated `line_lanes` entries. Malformed rows are skipped.
    pub fn get_gearbox_line_lanes_dict(
        &self,
        gearbox_tbl: &dyn Table,
    ) -> std::collections::HashMap<String, u32> {
        let mut dict = std::collections::HashMap::new();
        let keys = match gearbox_tbl.get_keys() {
            Ok(k) => k,
            Err(_) => return dict,
        };
        for key in keys {
            if !key.starts_with("interface:") {
                continue;
            }
            let Ok(Some(row)) = gearbox_tbl.get(&key) else {
                continue;
            };
            let fvs: std::collections::HashMap<String, String> = row.into_iter().collect();
            let name = fvs.get("name").map(String::as_str).unwrap_or("");
            let line_lanes = fvs.get("line_lanes").map(String::as_str).unwrap_or("");
            if !name.is_empty() && !line_lanes.is_empty() {
                dict.insert(name.to_string(), line_lanes.split(',').count() as u32);
            }
        }
        dict
    }

}


#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn skeleton_present() {
        // Sanity: the module compiles and is wired into the crate.
        assert!(true);
    }

    #[test]
    fn test_get_state_db_port_table_val_by_key() {
        use crate::mock::MockTable;
        use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType, PortMapping};

        let helper = XcvrTableHelper::new();
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd));

        // port_mapping is None -> None.
        assert_eq!(
            helper.get_state_db_port_table_val_by_key("Ethernet0", None, None, NPU_SI_SETTINGS_SYNC_STATUS_KEY),
            None
        );

        // state_port_tbl is None -> None.
        assert_eq!(
            helper.get_state_db_port_table_val_by_key("Ethernet0", Some(&pm), None, NPU_SI_SETTINGS_SYNC_STATUS_KEY),
            None
        );

        // Row not found -> None.
        let tbl = MockTable::new();
        assert_eq!(
            helper.get_state_db_port_table_val_by_key(
                "Ethernet0",
                Some(&pm),
                Some(&tbl as &dyn Table),
                NPU_SI_SETTINGS_SYNC_STATUS_KEY
            ),
            None
        );

        // Row found but key absent -> None.
        tbl.hset("Ethernet0", "A", "B").unwrap();
        assert_eq!(
            helper.get_state_db_port_table_val_by_key(
                "Ethernet0",
                Some(&pm),
                Some(&tbl as &dyn Table),
                NPU_SI_SETTINGS_SYNC_STATUS_KEY
            ),
            None
        );

        // Row found with key -> value.
        tbl.hset("Ethernet0", NPU_SI_SETTINGS_SYNC_STATUS_KEY, NPU_SI_SETTINGS_DEFAULT_VALUE)
            .unwrap();
        assert_eq!(
            helper.get_state_db_port_table_val_by_key(
                "Ethernet0",
                Some(&pm),
                Some(&tbl as &dyn Table),
                NPU_SI_SETTINGS_SYNC_STATUS_KEY
            ),
            Some(NPU_SI_SETTINGS_DEFAULT_VALUE.to_string())
        );
    }

    #[test]
    fn test_is_npu_si_settings_update_required() {
        use crate::mock::MockTable;
        use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType, PortMapping};

        let helper = XcvrTableHelper::new();
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd));
        let tbl = MockTable::new();

        // Key absent (None) -> update required.
        assert!(helper.is_npu_si_settings_update_required("Ethernet0", Some(&pm), Some(&tbl as &dyn Table)));

        // Already NOTIFIED -> not required.
        tbl.hset("Ethernet0", NPU_SI_SETTINGS_SYNC_STATUS_KEY, NPU_SI_SETTINGS_NOTIFIED_VALUE)
            .unwrap();
        assert!(!helper.is_npu_si_settings_update_required("Ethernet0", Some(&pm), Some(&tbl as &dyn Table)));

        // DEFAULT -> required again.
        tbl.hset("Ethernet0", NPU_SI_SETTINGS_SYNC_STATUS_KEY, NPU_SI_SETTINGS_DEFAULT_VALUE)
            .unwrap();
        assert!(helper.is_npu_si_settings_update_required("Ethernet0", Some(&pm), Some(&tbl as &dyn Table)));
    }

    #[test]
    fn test_XcvrTableHelper_get_gearbox_line_lanes_dict() {
        use crate::mock::MockTable;
        use std::collections::HashMap;

        // Build a _GEARBOX_TABLE mock from interface rows, then assert the derived
        // {logical_port: line_lane_count} map (mirrors the 5 parametrized upstream cases).
        let helper = XcvrTableHelper::new();
        let build = |rows: &[(&str, &[(&str, &str)])]| -> MockTable {
            let t = MockTable::new();
            for (key, fields) in rows {
                for (f, v) in *fields {
                    t.hset(key, f, v).unwrap();
                }
            }
            t
        };
        let expect = |pairs: &[(&str, u32)]| -> HashMap<String, u32> {
            pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
        };

        // Case 1: gearbox port with 2 line lanes.
        let t = build(&[(
            "interface:0",
            &[("name", "Ethernet0"), ("index", "0"), ("phy_id", "1"),
              ("system_lanes", "300,301,302,303"), ("line_lanes", "304,305")],
        )]);
        assert_eq!(helper.get_gearbox_line_lanes_dict(&t), expect(&[("Ethernet0", 2)]));

        // Case 2: multiple gearbox ports.
        let t = build(&[
            ("interface:0", &[("name", "Ethernet0"), ("line_lanes", "304,305,306,307")]),
            ("interface:200", &[("name", "Ethernet200"), ("line_lanes", "404,405")]),
        ]);
        assert_eq!(
            helper.get_gearbox_line_lanes_dict(&t),
            expect(&[("Ethernet0", 4), ("Ethernet200", 2)])
        );

        // Case 3: empty gearbox data.
        let t = build(&[]);
        assert_eq!(helper.get_gearbox_line_lanes_dict(&t), HashMap::new());

        // Case 4: interface with empty line_lanes is skipped.
        let t = build(&[(
            "interface:0",
            &[("name", "Ethernet0"), ("system_lanes", "300,301,302,303"), ("line_lanes", "")],
        )]);
        assert_eq!(helper.get_gearbox_line_lanes_dict(&t), HashMap::new());

        // Case 5: non-interface keys are ignored.
        let t = build(&[
            ("interface:0", &[("name", "Ethernet0"), ("line_lanes", "304,305")]),
            ("phy:1", &[("name", "phy1"), ("some_field", "some_value")]),
        ]);
        assert_eq!(helper.get_gearbox_line_lanes_dict(&t), expect(&[("Ethernet0", 2)]));
    }

}
