#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `dom/utilities/vdm/db_utils.py`: `VDMDBUtils` — the writer for the
//! `TRANSCEIVER_VDM_*` tables:
//!   * `TRANSCEIVER_VDM_REAL_VALUE` — the merged basic + statistic observables.
//!   * `TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_THRESHOLD` — the per-type limits.
//!   * `TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_FLAG` — the per-type latched
//!     flags, each with its change-count / set-time / clear-time metadata trio.
//!
//! The raw HAL threshold/flag dicts carry the threshold type inside the key (e.g.
//! `laser_temperature_media_halarm1`, or the unit-test fixture form
//! `laser_temperature_media_1_halarm`); `_post_port_vdm_thresholds_or_flags_to_db`
//! splits them into one sub-dict per type — stripping the `_{type}` token from the
//! key — so each type lands in its own STATE_DB table (`laser_temperature_media1`).
//! All platform/DB access flows through the [`Chassis`]/[`Table`] seams so unit
//! tests inject mocks.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::db::Table;
use crate::dom::utilities::db::utils::{
    beautify_info_dict, get_current_time, py_str, py_truthy, update_flag_metadata_tables, DomLogger,
    NoopDomLogger,
};
use crate::dom::utilities::vdm::utils::VDMUtils;
use crate::dom::utilities::vdm::VDM_THRESHOLD_TYPES;
use crate::hal::Chassis;
use crate::xcvrd::Event;
use crate::xcvrd_utilities::port_event_helper::PortMapping;

/// The per-type VDM flag value row + its change-tracking metadata trio.
pub struct VdmFlagTables {
    pub flag: Rc<dyn Table>,
    pub change_count: Rc<dyn Table>,
    pub set_time: Rc<dyn Table>,
    pub clear_time: Rc<dyn Table>,
}

/// Rust port of the Python `VDMDBUtils`.
pub struct VDMDBUtils {
    chassis: Rc<dyn Chassis>,
    port_mapping: PortMapping,
    vdm_real_value_tbl: Rc<dyn Table>,
    /// One `TRANSCEIVER_VDM_{TYPE}_THRESHOLD` table per lower-case type token.
    threshold_tbls: HashMap<String, Rc<dyn Table>>,
    /// One `TRANSCEIVER_VDM_{TYPE}_FLAG` (+ metadata trio) per type token.
    flag_tbls: HashMap<String, VdmFlagTables>,
    task_stopping_event: Arc<Event>,
    logger: Rc<dyn DomLogger>,
    vdm_utils: VDMUtils,
}

impl VDMDBUtils {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chassis: Rc<dyn Chassis>,
        port_mapping: PortMapping,
        vdm_real_value_tbl: Rc<dyn Table>,
        threshold_tbls: HashMap<String, Rc<dyn Table>>,
        flag_tbls: HashMap<String, VdmFlagTables>,
        task_stopping_event: Arc<Event>,
        logger: Rc<dyn DomLogger>,
    ) -> Self {
        let vdm_utils = VDMUtils::with_logger(chassis.clone(), logger.clone());
        VDMDBUtils {
            chassis,
            port_mapping,
            vdm_real_value_tbl,
            threshold_tbls,
            flag_tbls,
            task_stopping_event,
            logger,
            vdm_utils,
        }
    }

    /// `post_port_vdm_real_values_from_dict_to_db` (vdm/db_utils.py:25): post the
    /// pre-merged VDM real values (basic + statistic) to `TRANSCEIVER_VDM_REAL_VALUE`
    /// in one operation with a single trailing `last_update_time`. A `None`/empty dict
    /// writes nothing.
    pub fn post_port_vdm_real_values_from_dict_to_db(
        &self,
        logical_port_name: &str,
        vdm_real_values_dict: &Value,
    ) {
        // Basic validation of the port (flat-memory modules have no VDM page).
        if self.validate_and_get_physical_port(logical_port_name, true).is_none() {
            return;
        }
        if self.port_mapping.get_asic_id_for_logical_port(logical_port_name).is_none() {
            self.logger.log_error(&format!(
                "Post port vdm real values from dict to db failed for {logical_port_name} as no asic index found"
            ));
            return;
        }
        // `if not vdm_real_values_dict: return` — Null OR empty object both skip.
        let obj = match vdm_real_values_dict.as_object() {
            Some(o) if !o.is_empty() => o.clone(),
            _ => return,
        };
        let mut beautified = obj;
        beautify_info_dict(&mut beautified);
        let mut fvs: Vec<(String, String)> =
            beautified.iter().map(|(k, v)| (k.clone(), py_str(v))).collect();
        fvs.push(("last_update_time".to_string(), get_current_time()));
        let _ = self.vdm_real_value_tbl.set(logical_port_name, &fvs);
    }

    /// `post_port_vdm_thresholds_to_db` (vdm/db_utils.py:62).
    pub fn post_port_vdm_thresholds_to_db(
        &self,
        logical_port_name: &str,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) {
        self.post_thresholds_or_flags(logical_port_name, false, db_cache);
    }

    /// `post_port_vdm_flags_to_db` (vdm/db_utils.py:58).
    pub fn post_port_vdm_flags_to_db(
        &self,
        logical_port_name: &str,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) {
        self.post_thresholds_or_flags(logical_port_name, true, db_cache);
    }

    /// `_post_port_vdm_thresholds_or_flags_to_db` (vdm/db_utils.py:67): read the raw
    /// per-type-suffixed dict, split it into one sub-dict per threshold type, maintain
    /// the flag metadata trio (flags only, on a fresh read), then publish each
    /// non-empty type row — stopping at the first empty category.
    fn post_thresholds_or_flags(
        &self,
        logical_port_name: &str,
        flag_data: bool,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) {
        let physical_port = match self.validate_and_get_physical_port(logical_port_name, true) {
            Some(p) => p,
            None => return,
        };

        // Resolve the per-type split, from the cache if present else a fresh HAL read.
        let cached = db_cache.and_then(|c| c.borrow().get(&physical_port).cloned());
        let split: Vec<(&'static str, Map<String, Value>)> = match cached {
            Some(v) => self.split_from_cache(&v),
            None => {
                let raw = if flag_data {
                    self.vdm_utils.get_vdm_flags(physical_port as usize)
                } else {
                    self.vdm_utils.get_vdm_thresholds(physical_port as usize)
                };
                // `if vdm_values_dict is None: return` — the platform reported no VDM.
                if raw.is_null() {
                    self.logger.log_error(&format!(
                        "Post port vdm thresholds or flags to db failed for {logical_port_name} \
                         as no vdm values found with flag_data {flag_data}"
                    ));
                    return;
                }
                let raw_map = raw.as_object().cloned().unwrap_or_default();
                let update_time = get_current_time();
                let split = self.split_by_type(&raw_map);
                // Flag metadata: update the trio for each populated type BEFORE the
                // value row is overwritten (so a raise/clear is detected as an edge).
                if flag_data {
                    for (ttype, dict) in &split {
                        if !dict.is_empty() {
                            self.update_flag_metadata(logical_port_name, ttype, dict, &update_time);
                        }
                    }
                }
                if let Some(cache) = db_cache {
                    cache.borrow_mut().insert(physical_port, split_to_value(&split));
                }
                split
            }
        };

        // Post each type's row in order; stop at the first empty category.
        for (ttype, dict) in &split {
            if dict.is_empty() {
                return;
            }
            let mut beautified = dict.clone();
            beautify_info_dict(&mut beautified);
            let mut fvs: Vec<(String, String)> =
                beautified.iter().map(|(k, v)| (k.clone(), py_str(v))).collect();
            fvs.push(("last_update_time".to_string(), get_current_time()));
            let table = if flag_data {
                self.flag_tbls.get(*ttype).map(|t| &t.flag)
            } else {
                self.threshold_tbls.get(*ttype)
            };
            if let Some(table) = table {
                let _ = table.set(logical_port_name, &fvs);
            }
        }
    }

    /// Split the raw type-suffixed dict into one sub-dict per threshold type, in the
    /// canonical `VDM_THRESHOLD_TYPES` order. A key belongs to type `t` iff it
    /// contains the `_{t}` token; the token is stripped from the stored key
    /// (`laser_temperature_media_halarm1` → `laser_temperature_media1`).
    fn split_by_type(&self, raw: &Map<String, Value>) -> Vec<(&'static str, Map<String, Value>)> {
        let mut out: Vec<(&'static str, Map<String, Value>)> =
            VDM_THRESHOLD_TYPES.iter().map(|t| (*t, Map::new())).collect();
        for (key, value) in raw {
            for (i, ttype) in VDM_THRESHOLD_TYPES.iter().enumerate() {
                let token = format!("_{ttype}");
                if key.contains(&token) {
                    let new_key = key.replace(&token, "");
                    out[i].1.insert(new_key, value.clone());
                }
            }
        }
        out
    }

    /// Reconstruct the per-type split from the `db_cache` value (an object of objects).
    fn split_from_cache(&self, cached: &Value) -> Vec<(&'static str, Map<String, Value>)> {
        VDM_THRESHOLD_TYPES
            .iter()
            .map(|t| {
                let d = cached.get(*t).and_then(|v| v.as_object()).cloned().unwrap_or_default();
                (*t, d)
            })
            .collect()
    }

    /// `DBUtils._update_flag_metadata_tables(..., "VDM {type}")` bound to one type's
    /// flag tables.
    fn update_flag_metadata(
        &self,
        logical_port_name: &str,
        ttype: &str,
        dict: &Map<String, Value>,
        update_time: &str,
    ) {
        let Some(tbls) = self.flag_tbls.get(ttype) else {
            return;
        };
        update_flag_metadata_tables(
            logical_port_name,
            dict,
            update_time,
            &*tbls.flag,
            &*tbls.change_count,
            &*tbls.set_time,
            &*tbls.clear_time,
            &format!("VDM {ttype}"),
            &*self.logger,
        );
    }

    /// `_validate_and_get_physical_port` (db/utils.py:62) with the VDM flat-memory
    /// check (`enable_flat_memory_check=True`).
    fn validate_and_get_physical_port(
        &self,
        logical_port_name: &str,
        enable_flat_memory_check: bool,
    ) -> Option<i32> {
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
        if enable_flat_memory_check && self.is_transceiver_flat_memory(pport) {
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

    /// `XCVRDUtils.is_transceiver_flat_memory(physical_port)` — a flat-memory (page-0
    /// only) module has no VDM page, so VDM writes are skipped. Any error → `false`.
    fn is_transceiver_flat_memory(&self, physical_port: i32) -> bool {
        self.chassis
            .sfp(physical_port as usize)
            .ok()
            .and_then(|s| s.call_json("is_flat_memory").ok())
            .map(|v| py_truthy(&v))
            .unwrap_or(false)
    }
}

/// Serialize the per-type split to a `db_cache` value (an object of objects).
fn split_to_value(split: &[(&'static str, Map<String, Value>)]) -> Value {
    let mut m = Map::new();
    for (t, d) in split {
        m.insert((*t).to_string(), Value::Object(d.clone()));
    }
    Value::Object(m)
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

    /// A mapping whose logical port resolves to a physical port but has **no** asic
    /// index (the `get_asic_id_for_logical_port() is None` skip path).
    fn mapping_eth0_no_asic() -> PortMapping {
        let mut pm = PortMapping::new();
        pm.logical_to_physical.insert("Ethernet0".to_string(), 0);
        pm.physical_to_logical.insert(0, vec!["Ethernet0".to_string()]);
        pm
    }

    fn chassis_with(sfp: MockSfp) -> Rc<dyn Chassis> {
        Rc::new(MockChassis::with_sfps(vec![sfp]))
    }

    // Helper to build a full set of per-type mock tables, returning both the trait
    // handles (for the util) and the concrete MockTables (for assertions).
    struct Tables {
        real: MockTable,
        thresholds: HashMap<String, MockTable>,
        flags: HashMap<String, (MockTable, MockTable, MockTable, MockTable)>,
    }

    fn build_tables() -> (Tables, Rc<dyn Table>, HashMap<String, Rc<dyn Table>>, HashMap<String, VdmFlagTables>) {
        let real = MockTable::new();
        let mut thresholds = HashMap::new();
        let mut threshold_handles: HashMap<String, Rc<dyn Table>> = HashMap::new();
        let mut flags = HashMap::new();
        let mut flag_handles: HashMap<String, VdmFlagTables> = HashMap::new();
        for t in VDM_THRESHOLD_TYPES {
            let thr = MockTable::new();
            threshold_handles.insert(t.to_string(), Rc::new(thr.clone()));
            thresholds.insert(t.to_string(), thr);

            let (f, c, s, cl) = (MockTable::new(), MockTable::new(), MockTable::new(), MockTable::new());
            flag_handles.insert(
                t.to_string(),
                VdmFlagTables {
                    flag: Rc::new(f.clone()),
                    change_count: Rc::new(c.clone()),
                    set_time: Rc::new(s.clone()),
                    clear_time: Rc::new(cl.clone()),
                },
            );
            flags.insert(t.to_string(), (f, c, s, cl));
        }
        let real_handle: Rc<dyn Table> = Rc::new(real.clone());
        (Tables { real, thresholds, flags }, real_handle, threshold_handles, flag_handles)
    }

    fn build(
        sfp: MockSfp,
        pm: PortMapping,
        stop: Arc<Event>,
    ) -> (VDMDBUtils, Tables) {
        let (tables, real_handle, threshold_handles, flag_handles) = build_tables();
        let d = VDMDBUtils::new(
            chassis_with(sfp),
            pm,
            real_handle,
            threshold_handles,
            flag_handles,
            stop,
            Rc::new(NoopDomLogger),
        );
        (d, tables)
    }

    fn threshold_fixture() -> Value {
        // The upstream unit-test fixture: `{prefix}_{i}_{type}`.
        let mut m = Map::new();
        for i in 1..=8 {
            m.insert(format!("laser_temperature_media_{i}_halarm"), json!(90.0));
            m.insert(format!("laser_temperature_media_{i}_lalarm"), json!(-5.0));
            m.insert(format!("laser_temperature_media_{i}_hwarn"), json!(85.0));
            m.insert(format!("laser_temperature_media_{i}_lwarn"), json!(0.0));
        }
        Value::Object(m)
    }

    /// Port of `tests/test_xcvrd.py::test_post_port_vdm_thresholds_to_db`: the four
    /// per-type tables stay empty for every validation-fail path (stop event, not
    /// present, flat memory, `None` values) and are each populated with 9 fields
    /// (8 observables + `last_update_time`) once valid values are read; a db_cache
    /// hit re-posts the same 9 fields.
    #[test]
    fn test_post_port_vdm_thresholds_to_db() {
        // Ensure tables are empty if the stop event is set.
        let stop = Event::new();
        stop.set();
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_vdm_thresholds", threshold_fixture());
        let (d, tables) = build(sfp, mapping_eth0(), stop);
        d.post_port_vdm_thresholds_to_db("Ethernet0", None);
        for t in VDM_THRESHOLD_TYPES {
            assert_eq!(tables.thresholds[t].get_size().unwrap(), 0);
        }

        // Ensure tables are empty if the transceiver is not present.
        let mut sfp = MockSfp::absent();
        sfp.set_json_call("get_transceiver_vdm_thresholds", threshold_fixture());
        let (d, tables) = build(sfp, mapping_eth0(), Event::new());
        d.post_port_vdm_thresholds_to_db("Ethernet0", None);
        for t in VDM_THRESHOLD_TYPES {
            assert_eq!(tables.thresholds[t].get_size().unwrap(), 0);
        }

        // Ensure tables are empty if the transceiver is flat memory.
        let mut sfp = MockSfp::present();
        sfp.set_json_call("is_flat_memory", json!(true));
        sfp.set_json_call("get_transceiver_vdm_thresholds", threshold_fixture());
        let (d, tables) = build(sfp, mapping_eth0(), Event::new());
        d.post_port_vdm_thresholds_to_db("Ethernet0", None);
        for t in VDM_THRESHOLD_TYPES {
            assert_eq!(tables.thresholds[t].get_size().unwrap(), 0);
        }

        // Ensure tables are empty if the HAL returns None (no VDM values).
        let sfp = MockSfp::present(); // get_transceiver_vdm_thresholds unset → Null
        let (d, tables) = build(sfp, mapping_eth0(), Event::new());
        d.post_port_vdm_thresholds_to_db("Ethernet0", None);
        for t in VDM_THRESHOLD_TYPES {
            assert_eq!(tables.thresholds[t].get_size().unwrap(), 0);
        }

        // Ensure tables are populated (9 fields each) when valid values are read, and
        // that a db_cache hit re-posts the same 9 fields.
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_vdm_thresholds", threshold_fixture());
        let (d, tables) = build(sfp, mapping_eth0(), Event::new());
        let cache: RefCell<HashMap<i32, Value>> = RefCell::new(HashMap::new());
        d.post_port_vdm_thresholds_to_db("Ethernet0", Some(&cache));
        for t in VDM_THRESHOLD_TYPES {
            assert_eq!(tables.thresholds[t].row("Ethernet0").map(|r| r.len()), Some(9));
        }
        assert!(cache.borrow().contains_key(&0));

        d.post_port_vdm_thresholds_to_db("Ethernet0", Some(&cache));
        for t in VDM_THRESHOLD_TYPES {
            assert_eq!(tables.thresholds[t].row("Ethernet0").map(|r| r.len()), Some(9));
        }
    }

    /// Port of `tests/test_xcvrd.py::test_post_port_vdm_real_values_from_dict_to_db`:
    /// the real-value row is skipped when the asic index is `None`, when the dict is
    /// `None`, and when it is empty; a valid dict is posted (16 observables +
    /// `last_update_time`), with `N/A` values preserved verbatim, and a subsequent
    /// update writes the new values through.
    ///
    /// NOTE: the Python mock `Table.set` *replaces* the whole row (so it asserts the
    /// row shrinks 17 → 3 on the 2-field update); the real `swss_common::Table::set`
    /// and this crate's `MockTable` both *merge* per field (HSET semantics), so here
    /// we assert the merged outcome — the two updated fields carry their new values.
    #[test]
    fn test_post_port_vdm_real_values_from_dict_to_db() {
        let real_values = {
            let mut m = Map::new();
            for i in 1..=8 {
                m.insert(
                    format!("laser_temperature_media{i}"),
                    if i <= 4 { json!(38) } else { json!("N/A") },
                );
            }
            for i in 1..=8 {
                m.insert(format!("esnr_media_input{i}"), json!(23.1171875));
            }
            Value::Object(m)
        };

        // asic index None → nothing written.
        let mut sfp = MockSfp::present();
        sfp.set_json_call("is_flat_memory", json!(false));
        let (d, tables) = build(sfp, mapping_eth0_no_asic(), Event::new());
        d.post_port_vdm_real_values_from_dict_to_db("Ethernet0", &real_values);
        assert_eq!(tables.real.get_size().unwrap(), 0);

        // asic index set: None dict → skip, empty dict → skip.
        let mut sfp = MockSfp::present();
        sfp.set_json_call("is_flat_memory", json!(false));
        let (d, tables) = build(sfp, mapping_eth0(), Event::new());
        d.post_port_vdm_real_values_from_dict_to_db("Ethernet0", &Value::Null);
        assert_eq!(tables.real.get_size().unwrap(), 0);
        d.post_port_vdm_real_values_from_dict_to_db("Ethernet0", &json!({}));
        assert_eq!(tables.real.get_size().unwrap(), 0);

        // Valid dict → 16 observables + last_update_time = 17 fields, N/A preserved.
        d.post_port_vdm_real_values_from_dict_to_db("Ethernet0", &real_values);
        let row = tables.real.row("Ethernet0").unwrap();
        assert_eq!(row.len(), 17);
        assert_eq!(row.get("laser_temperature_media1").map(String::as_str), Some("38"));
        assert_eq!(row.get("laser_temperature_media5").map(String::as_str), Some("N/A"));
        assert!(row.contains_key("last_update_time"));

        // Update writes the new values through (merge semantics).
        let updated = json!({ "laser_temperature_media1": 40, "esnr_media_input1": 25.0 });
        d.post_port_vdm_real_values_from_dict_to_db("Ethernet0", &updated);
        assert_eq!(tables.real.field("Ethernet0", "laser_temperature_media1").as_deref(), Some("40"));
        assert_eq!(tables.real.field("Ethernet0", "esnr_media_input1").as_deref(), Some("25.0"));
    }

    /// the fan-out split
    /// handles the **real-HAL** key form `{prefix}_{type}{lane}` (as opposed to
    /// the upstream unit-test fixture form `{prefix}_{lane}_{type}`), stripping the
    /// `_{type}` token so each per-type table stores `laser_temperature_media1`.
    #[test]
    fn vdm_thresholds_to_db_rows() {
        let mut raw = Map::new();
        // Real-HAL form: prefix + `_type` + lane.
        raw.insert("laser_temperature_media_halarm1".to_string(), json!(80.0));
        raw.insert("laser_temperature_media_lalarm1".to_string(), json!(-5.0));
        raw.insert("laser_temperature_media_hwarn1".to_string(), json!(75.0));
        raw.insert("laser_temperature_media_lwarn1".to_string(), json!(0.0));
        raw.insert("esnr_media_input_halarm1".to_string(), json!(35.0));
        raw.insert("esnr_media_input_lalarm1".to_string(), json!(5.0));
        raw.insert("esnr_media_input_hwarn1".to_string(), json!(30.0));
        raw.insert("esnr_media_input_lwarn1".to_string(), json!(7.0));

        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_vdm_thresholds", Value::Object(raw));
        let (d, tables) = build(sfp, mapping_eth0(), Event::new());
        d.post_port_vdm_thresholds_to_db("Ethernet0", None);

        // Each per-type table has the two observables (token stripped) + last_update_time.
        let halarm = tables.thresholds["halarm"].row("Ethernet0").unwrap();
        assert_eq!(halarm.get("laser_temperature_media1").map(String::as_str), Some("80.0"));
        assert_eq!(halarm.get("esnr_media_input1").map(String::as_str), Some("35.0"));
        assert!(halarm.contains_key("last_update_time"));
        assert!(!halarm.contains_key("laser_temperature_media_halarm1"));

        assert_eq!(
            tables.thresholds["lwarn"].field("Ethernet0", "laser_temperature_media1").as_deref(),
            Some("0.0")
        );
        assert_eq!(
            tables.thresholds["hwarn"].field("Ethernet0", "esnr_media_input1").as_deref(),
            Some("30.0")
        );
    }

    /// Port of `tests/test_xcvrd.py::test_post_port_vdm_flags_to_db` behaviour: the
    /// first flag publish writes the per-type flag rows and seeds the metadata trio;
    /// a second publish with a raised flag bumps the change count + stamps the set
    /// time (the same change-tracking machinery as DOM/STATUS flags).
    #[test]
    fn test_post_port_vdm_flags_to_db() {
        let flags_all_false = || {
            let mut m = Map::new();
            m.insert("laser_temperature_media_halarm1".to_string(), json!(false));
            m.insert("laser_temperature_media_lalarm1".to_string(), json!(false));
            m.insert("laser_temperature_media_hwarn1".to_string(), json!(false));
            m.insert("laser_temperature_media_lwarn1".to_string(), json!(false));
            Value::Object(m)
        };

        // Build a util whose tables persist across two posts (shared handles).
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_vdm_flags", flags_all_false());
        let (tables, real_handle, threshold_handles, flag_handles) = build_tables();
        let d = VDMDBUtils::new(
            chassis_with(sfp),
            mapping_eth0(),
            real_handle,
            threshold_handles,
            flag_handles,
            Event::new(),
            Rc::new(NoopDomLogger),
        );

        // First publish: flag row written False, metadata seeded (count 0 / never).
        d.post_port_vdm_flags_to_db("Ethernet0", None);
        let (flag, count, set_time, _clear) = &tables.flags["halarm"];
        assert_eq!(flag.field("Ethernet0", "laser_temperature_media1").as_deref(), Some("False"));
        assert_eq!(count.field("Ethernet0", "laser_temperature_media1").as_deref(), Some("0"));
        assert_eq!(set_time.field("Ethernet0", "laser_temperature_media1").as_deref(), Some("never"));

        // Second publish: halarm raised → count 1, set-time stamped (not "never").
        let mut raised = Map::new();
        raised.insert("laser_temperature_media_halarm1".to_string(), json!(true));
        raised.insert("laser_temperature_media_lalarm1".to_string(), json!(false));
        raised.insert("laser_temperature_media_hwarn1".to_string(), json!(false));
        raised.insert("laser_temperature_media_lwarn1".to_string(), json!(false));
        // Re-point the SFP's flag read at the raised dict by rebuilding the util over
        // the SAME tables (fresh chassis, shared table handles).
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_vdm_flags", Value::Object(raised));
        let flag_handles2: HashMap<String, VdmFlagTables> = VDM_THRESHOLD_TYPES
            .iter()
            .map(|t| {
                let (f, c, s, cl) = &tables.flags[*t];
                (
                    t.to_string(),
                    VdmFlagTables {
                        flag: Rc::new(f.clone()),
                        change_count: Rc::new(c.clone()),
                        set_time: Rc::new(s.clone()),
                        clear_time: Rc::new(cl.clone()),
                    },
                )
            })
            .collect();
        let threshold_handles2: HashMap<String, Rc<dyn Table>> = VDM_THRESHOLD_TYPES
            .iter()
            .map(|t| (t.to_string(), Rc::new(tables.thresholds[*t].clone()) as Rc<dyn Table>))
            .collect();
        let d2 = VDMDBUtils::new(
            chassis_with(sfp),
            mapping_eth0(),
            Rc::new(tables.real.clone()),
            threshold_handles2,
            flag_handles2,
            Event::new(),
            Rc::new(NoopDomLogger),
        );
        d2.post_port_vdm_flags_to_db("Ethernet0", None);
        assert_eq!(flag.field("Ethernet0", "laser_temperature_media1").as_deref(), Some("True"));
        assert_eq!(count.field("Ethernet0", "laser_temperature_media1").as_deref(), Some("1"));
        assert_ne!(set_time.field("Ethernet0", "laser_temperature_media1").as_deref(), Some("never"));
    }
}
