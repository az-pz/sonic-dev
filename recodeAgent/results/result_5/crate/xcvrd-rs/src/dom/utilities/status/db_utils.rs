#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `dom/utilities/status/db_utils.py`: StatusDBUtils — the writer for
//! `TRANSCEIVER_STATUS` and `TRANSCEIVER_STATUS_FLAG` (+ its change-count / set-time
//! / clear-time metadata trio).
//!
//! Unlike `DOMDBUtils`, the status writers use the **base** `beautify_info_dict`
//! (no unit stripping): status fields are enum strings (`ModuleReady`,
//! `DataPathActivated`, …) and booleans, rendered with Python `str(...)`. All
//! platform/DB access flows through the [`Chassis`]/[`Table`] seams so unit tests
//! inject mocks.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::db::Table;
use crate::dom::utilities::db::utils::{
    beautify_info_dict, get_current_time, py_str, update_flag_metadata_tables, DomLogger,
    NoopDomLogger,
};
use crate::dom::utilities::status::utils::StatusUtils;
use crate::hal::Chassis;
use crate::xcvrd::Event;
use crate::xcvrd_utilities::port_event_helper::PortMapping;

/// Rust port of the Python `StatusDBUtils`.
pub struct StatusDBUtils {
    chassis: Rc<dyn Chassis>,
    port_mapping: PortMapping,
    status_tbl: Rc<dyn Table>,
    // TRANSCEIVER_STATUS_FLAG value row + its change-tracking metadata trio.
    status_flag_tbl: Rc<dyn Table>,
    status_flag_change_count_tbl: Rc<dyn Table>,
    status_flag_set_time_tbl: Rc<dyn Table>,
    status_flag_clear_time_tbl: Rc<dyn Table>,
    task_stopping_event: Arc<Event>,
    logger: Rc<dyn DomLogger>,
    status_utils: StatusUtils,
}

impl StatusDBUtils {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chassis: Rc<dyn Chassis>,
        port_mapping: PortMapping,
        status_tbl: Rc<dyn Table>,
        status_flag_tbl: Rc<dyn Table>,
        status_flag_change_count_tbl: Rc<dyn Table>,
        status_flag_set_time_tbl: Rc<dyn Table>,
        status_flag_clear_time_tbl: Rc<dyn Table>,
        task_stopping_event: Arc<Event>,
        logger: Rc<dyn DomLogger>,
    ) -> Self {
        let status_utils = StatusUtils::new(chassis.clone());
        StatusDBUtils {
            chassis,
            port_mapping,
            status_tbl,
            status_flag_tbl,
            status_flag_change_count_tbl,
            status_flag_set_time_tbl,
            status_flag_clear_time_tbl,
            task_stopping_event,
            logger,
            status_utils,
        }
    }

    /// `post_port_transceiver_hw_status_to_db` (status/db_utils.py:21): post the
    /// rich `TRANSCEIVER_STATUS` row via the base diagnostic engine (no unit strip).
    pub fn post_port_transceiver_hw_status_to_db(
        &self,
        logical_port_name: &str,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) {
        if self.port_mapping.get_asic_id_for_logical_port(logical_port_name).is_none() {
            self.logger.log_error(&format!(
                "Post port transceiver hw status to db failed for {logical_port_name} as no asic index found"
            ));
            return;
        }
        self.post_diagnostic_values_to_db(logical_port_name, &*self.status_tbl, db_cache, |p| {
            self.status_utils.get_transceiver_status(p as usize)
        });
    }

    /// `post_port_transceiver_hw_status_flags_to_db` (status/db_utils.py:41): read
    /// the latched status flags, maintain the `TRANSCEIVER_STATUS_FLAG` metadata
    /// trio, then publish the (beautified) flag row with a `last_update_time` stamp.
    pub fn post_port_transceiver_hw_status_flags_to_db(
        &self,
        logical_port_name: &str,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) {
        if self.port_mapping.get_asic_id_for_logical_port(logical_port_name).is_none() {
            self.logger.log_error(&format!(
                "Post port transceiver hw status flags to db failed for {logical_port_name} as no asic index found"
            ));
            return;
        }
        let physical_port = match self.validate_and_get_physical_port(logical_port_name) {
            Some(p) => p,
            None => return,
        };

        let flags = match db_cache {
            Some(cache) => {
                let cached = cache.borrow().get(&physical_port).cloned();
                match cached {
                    Some(v) => v,
                    None => {
                        let v = self.status_utils.get_transceiver_status_flags(physical_port as usize);
                        if let Some(obj) = v.as_object() {
                            if !obj.is_empty() {
                                self.update_status_flag_metadata(logical_port_name, obj);
                            }
                        }
                        cache.borrow_mut().insert(physical_port, v.clone());
                        v
                    }
                }
            }
            None => {
                let v = self.status_utils.get_transceiver_status_flags(physical_port as usize);
                if let Some(obj) = v.as_object() {
                    if !obj.is_empty() {
                        self.update_status_flag_metadata(logical_port_name, obj);
                    }
                }
                v
            }
        };

        let obj = match flags.as_object() {
            Some(o) if !o.is_empty() => o.clone(),
            _ => return,
        };
        let mut beautified = obj;
        beautify_info_dict(&mut beautified);
        let mut fvs: Vec<(String, String)> =
            beautified.iter().map(|(k, v)| (k.clone(), py_str(v))).collect();
        fvs.push(("last_update_time".to_string(), get_current_time()));
        let _ = self.status_flag_tbl.set(logical_port_name, &fvs);
    }

    /// `DBUtils._update_flag_metadata_tables(..., "Status flags")` bound to this
    /// util's status-flag tables.
    fn update_status_flag_metadata(&self, logical_port_name: &str, curr_flags: &Map<String, Value>) {
        update_flag_metadata_tables(
            logical_port_name,
            curr_flags,
            &get_current_time(),
            &*self.status_flag_tbl,
            &*self.status_flag_change_count_tbl,
            &*self.status_flag_set_time_tbl,
            &*self.status_flag_clear_time_tbl,
            "Status flags",
            &*self.logger,
        );
    }

    /// `DBUtils.post_diagnostic_values_to_db` with the **base** `beautify_info_dict`
    /// (status values keep their units — there are none — and non-string values are
    /// `str(...)`-rendered).
    fn post_diagnostic_values_to_db<F>(
        &self,
        logical_port_name: &str,
        table: &dyn Table,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
        get_values: F,
    ) where
        F: Fn(i32) -> Value,
    {
        let physical_port = match self.validate_and_get_physical_port(logical_port_name) {
            Some(p) => p,
            None => return,
        };

        let values = match db_cache {
            Some(cache) => {
                let cached = cache.borrow().get(&physical_port).cloned();
                match cached {
                    Some(v) => v,
                    None => {
                        let v = get_values(physical_port);
                        cache.borrow_mut().insert(physical_port, v.clone());
                        v
                    }
                }
            }
            None => get_values(physical_port),
        };

        if values.is_null() {
            return;
        }
        let obj = match values.as_object() {
            Some(o) if !o.is_empty() => o.clone(),
            _ => return,
        };
        let mut beautified = obj;
        beautify_info_dict(&mut beautified);
        let mut fvs: Vec<(String, String)> =
            beautified.iter().map(|(k, v)| (k.clone(), py_str(v))).collect();
        fvs.push(("last_update_time".to_string(), get_current_time()));
        let _ = table.set(logical_port_name, &fvs);
    }

    /// `_validate_and_get_physical_port` (db/utils.py:62), status subset.
    fn validate_and_get_physical_port(&self, logical_port_name: &str) -> Option<i32> {
        if self.task_stopping_event.is_set() {
            return None;
        }
        let pport = match self.port_mapping.get_logical_to_physical(logical_port_name) {
            Some(list) if !list.is_empty() => list[0],
            _ => {
                self.logger.log_error(&format!(
                    "Validate and get physical port failed for {logical_port_name} as no physical port found"
                ));
                return None;
            }
        };
        if self.chassis.sfp(pport as usize).is_err() {
            self.logger.log_error(&format!(
                "Validate and get physical port failed for {logical_port_name} as no sfp object found"
            ));
            return None;
        }
        if !self.get_transceiver_presence(pport) {
            return None;
        }
        Some(pport)
    }

    fn get_transceiver_presence(&self, physical_port: i32) -> bool {
        self.chassis
            .sfp(physical_port as usize)
            .and_then(|s| s.get_presence())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockChassis, MockSfp, MockTable};
    use serde_json::json;

    fn mapping_eth0() -> PortMapping {
        use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType};
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        pm
    }

    fn chassis_with(sfp: MockSfp) -> Rc<dyn Chassis> {
        Rc::new(MockChassis::with_sfps(vec![sfp]))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        chassis: Rc<dyn Chassis>,
        pm: PortMapping,
        status: Rc<dyn Table>,
        flag: Rc<dyn Table>,
        count: Rc<dyn Table>,
        set_time: Rc<dyn Table>,
        clear_time: Rc<dyn Table>,
        stop: Arc<Event>,
    ) -> StatusDBUtils {
        StatusDBUtils::new(
            chassis, pm, status, flag, count, set_time, clear_time, stop, Rc::new(NoopDomLogger),
        )
    }

    /// Port of `tests/test_xcvrd.py::test_get_transceiver_status`.
    #[test]
    fn test_get_transceiver_status() {
        let status = json!({ "module_state": "ModuleReady", "DP1State": "DataPathActivated" });
        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.status = status.clone();
        let utils = StatusUtils::new(chassis_with(sfp));
        assert_eq!(utils.get_transceiver_status(0), status);
        assert_eq!(utils.get_transceiver_status(9), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_get_transceiver_status_flags`.
    #[test]
    fn test_get_transceiver_status_flags() {
        let flags = json!({ "module_firmware_fault": false, "tx1fault": true });
        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.set_json_call("get_transceiver_status_flags", flags.clone());
        let utils = StatusUtils::new(chassis_with(sfp));
        assert_eq!(utils.get_transceiver_status_flags(0), flags);

        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.fail_method("get_transceiver_status_flags");
        let utils = StatusUtils::new(chassis_with(sfp));
        assert_eq!(utils.get_transceiver_status_flags(0), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_post_port_transceiver_hw_status_to_db`:
    /// the rich status row is posted verbatim (no unit strip) + `last_update_time`.
    #[test]
    fn test_post_port_transceiver_hw_status_to_db() {
        let status = json!({
            "module_state": "ModuleReady",
            "DP1State": "DataPathActivated",
            "tx1disable": false,
        });
        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.status = status;
        let status_tbl = Rc::new(MockTable::new());
        let d = build(
            chassis_with(sfp),
            mapping_eth0(),
            status_tbl.clone(),
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            Event::new(),
        );
        d.post_port_transceiver_hw_status_to_db("Ethernet0", None);
        let row = status_tbl.row("Ethernet0").unwrap();
        assert_eq!(row.get("module_state").map(String::as_str), Some("ModuleReady"));
        assert_eq!(row.get("DP1State").map(String::as_str), Some("DataPathActivated"));
        // Non-string value stringified Python-style (`str(False)` == "False").
        assert_eq!(row.get("tx1disable").map(String::as_str), Some("False"));
        assert!(row.contains_key("last_update_time"));

        // Unknown port (no asic index) -> nothing written.
        let status_tbl2 = Rc::new(MockTable::new());
        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.status = json!({ "module_state": "ModuleReady" });
        let d = build(
            chassis_with(sfp),
            PortMapping::new(),
            status_tbl2.clone(),
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            Event::new(),
        );
        d.post_port_transceiver_hw_status_to_db("Ethernet0", None);
        assert_eq!(status_tbl2.get_size().unwrap(), 0);
    }

    /// Port of `tests/test_xcvrd.py::test_post_port_transceiver_hw_status_flags_to_db`:
    /// first publish writes the flag row + seeds the metadata trio; a subsequent
    /// publish with a flipped flag bumps the change count + stamps the set-time.
    #[test]
    fn test_post_port_transceiver_hw_status_flags_to_db() {
        let flag = Rc::new(MockTable::new());
        let count = Rc::new(MockTable::new());
        let set_time = Rc::new(MockTable::new());
        let clear_time = Rc::new(MockTable::new());

        let present_flags = |flags: Value| {
            let mut sfp = MockSfp::present_with_info(json!({}));
            sfp.set_json_call("get_transceiver_status_flags", flags);
            chassis_with(sfp)
        };

        // First publish: row + metadata seed.
        {
            let d = build(
                present_flags(json!({ "module_firmware_fault": false, "tx1fault": false })),
                mapping_eth0(),
                Rc::new(MockTable::new()),
                flag.clone(),
                count.clone(),
                set_time.clone(),
                clear_time.clone(),
                Event::new(),
            );
            d.post_port_transceiver_hw_status_flags_to_db("Ethernet0", None);
            let row = flag.row("Ethernet0").unwrap();
            assert_eq!(row.get("module_firmware_fault").map(String::as_str), Some("False"));
            assert_eq!(row.get("tx1fault").map(String::as_str), Some("False"));
            assert!(row.contains_key("last_update_time"));
            assert_eq!(count.field("Ethernet0", "tx1fault").as_deref(), Some("0"));
            assert_eq!(set_time.field("Ethernet0", "tx1fault").as_deref(), Some("never"));
            assert_eq!(clear_time.field("Ethernet0", "tx1fault").as_deref(), Some("never"));
        }

        // Second publish: tx1fault raised -> count 1 + set-time stamped.
        {
            let d = build(
                present_flags(json!({ "module_firmware_fault": false, "tx1fault": true })),
                mapping_eth0(),
                Rc::new(MockTable::new()),
                flag.clone(),
                count.clone(),
                set_time.clone(),
                clear_time.clone(),
                Event::new(),
            );
            d.post_port_transceiver_hw_status_flags_to_db("Ethernet0", None);
            assert_eq!(flag.field("Ethernet0", "tx1fault").as_deref(), Some("True"));
            assert_eq!(count.field("Ethernet0", "tx1fault").as_deref(), Some("1"));
            assert_ne!(set_time.field("Ethernet0", "tx1fault").as_deref(), Some("never"));
        }

        // Empty flags (unimplemented) -> nothing written.
        {
            let flag2 = Rc::new(MockTable::new());
            let d = build(
                present_flags(json!({})),
                mapping_eth0(),
                Rc::new(MockTable::new()),
                flag2.clone(),
                Rc::new(MockTable::new()),
                Rc::new(MockTable::new()),
                Rc::new(MockTable::new()),
                Event::new(),
            );
            d.post_port_transceiver_hw_status_flags_to_db("Ethernet0", None);
            assert_eq!(flag2.get_size().unwrap(), 0);
        }
    }

    /// the status-flag projection renders booleans as
    /// Python `str(bool)` and carries no unit stripping (status keys are not DOM
    /// unit-bearing keys), which is what `TRANSCEIVER_STATUS_FLAG` stores.
    #[test]
    fn hw_status_flags_projection() {
        let mut m = json!({
            "module_firmware_fault": false,
            "datapath_firmware_fault": true,
            "module_state_changed": false,
        })
        .as_object()
        .unwrap()
        .clone();
        beautify_info_dict(&mut m);
        assert_eq!(m.get("module_firmware_fault"), Some(&json!("False")));
        assert_eq!(m.get("datapath_firmware_fault"), Some(&json!("True")));
        assert_eq!(m.get("module_state_changed"), Some(&json!("False")));
    }
}
