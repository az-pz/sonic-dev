#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `dom/utilities/dom_sensor/db_utils.py`: DOMDBUtils — the writer for
//! `TRANSCEIVER_DOM_SENSOR`, `TRANSCEIVER_DOM_TEMPERATURE` and
//! `TRANSCEIVER_DOM_THRESHOLD`.
//!
//! The three unit posters share the
//! `post_diagnostic_values_to_db` engine (DBUtils base) with `_beautify_dom_info_dict`
//! / `_strip_unit`. All platform/DB access flows through the [`Chassis`]/[`Table`]
//! seams so unit tests inject mocks.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::db::Table;
use crate::dom::utilities::db::utils::{
    get_current_time, py_str, update_flag_metadata_tables, DomLogger, NoopDomLogger,
};
use crate::dom::utilities::dom_sensor::utils::DOMUtils;
use crate::hal::Chassis;
use crate::xcvrd::Event;
use crate::xcvrd_utilities::port_event_helper::PortMapping;

/// Unit suffixes stripped by `_beautify_dom_info_dict` (`DOMDBUtils.*_UNIT`).
const TEMP_UNIT: &str = "C";
const VOLT_UNIT: &str = "Volts";
const POWER_UNIT: &str = "dBm";
const BIAS_UNIT: &str = "mA";

/// `re.match('^(tx|rx)[1-8]power$', k)` without a regex dependency.
pub fn is_tx_rx_power(k: &str) -> bool {
    is_lane_key(k, "power")
}

/// `re.match('^(tx|rx)[1-8]bias$', k)` without a regex dependency.
pub fn is_tx_rx_bias(k: &str) -> bool {
    is_lane_key(k, "bias")
}

fn is_lane_key(k: &str, suffix: &str) -> bool {
    let b = k.as_bytes();
    if b.len() != 3 + suffix.len() {
        return false;
    }
    (b[0] == b't' || b[0] == b'r')
        && b[1] == b'x'
        && (b'1'..=b'8').contains(&b[2])
        && &k[3..] == suffix
}

/// `_strip_unit(value, unit)` (db_utils.py:120): trim the trailing `unit` from a
/// string that ends with it, otherwise `str(value)`.
pub fn strip_unit(value: &Value, unit: &str) -> String {
    if let Value::String(s) = value {
        if let Some(stripped) = s.strip_suffix(unit) {
            return stripped.to_string();
        }
    }
    py_str(value)
}

/// `_beautify_dom_info_dict` (db_utils.py:127): strip engineering units from the
/// recognised DOM keys and stringify everything else, in place.
pub fn beautify_dom_info_dict(dom_info_dict: &mut Map<String, Value>) {
    for (k, v) in dom_info_dict.iter_mut() {
        let new_val = if k == "temperature" {
            strip_unit(v, TEMP_UNIT)
        } else if k == "voltage" {
            strip_unit(v, VOLT_UNIT)
        } else if is_tx_rx_power(k) {
            strip_unit(v, POWER_UNIT)
        } else if is_tx_rx_bias(k) {
            strip_unit(v, BIAS_UNIT)
        } else if !v.is_string() {
            py_str(v)
        } else {
            // A string value under an unrecognised key is left unchanged.
            continue;
        };
        *v = Value::String(new_val);
    }
}

/// Beautify a raw DOM `Value` into the STATE_DB field list (units stripped, all
/// values stringified). Pure helper the deployed daemon (`daemon.rs`) reuses to
/// build a `TRANSCEIVER_DOM_SENSOR` / `TRANSCEIVER_DOM_THRESHOLD` row without
/// pulling in the whole `DOMDBUtils` collaborator graph. Returns `None` when the
/// value is not a (non-empty) object — matching the Python `None`/`{}` skips.
pub fn beautify_dom_row(values: &Value) -> Option<Vec<(String, String)>> {
    let obj = values.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut m = obj.clone();
    beautify_dom_info_dict(&mut m);
    Some(m.iter().map(|(k, v)| (k.clone(), py_str(v))).collect())
}

/// Rust port of the Python `DOMDBUtils`. Holds the HAL (`sfp_obj_dict` analogue),
/// the port map, the three destination tables, the stop event, a logger and the
/// DOM decoders.
pub struct DOMDBUtils {
    chassis: Rc<dyn Chassis>,
    port_mapping: PortMapping,
    dom_sensor_tbl: Rc<dyn Table>,
    dom_temperature_tbl: Rc<dyn Table>,
    dom_threshold_tbl: Rc<dyn Table>,
    // TRANSCEIVER_DOM_FLAG value row + its change-tracking metadata trio.
    dom_flag_tbl: Rc<dyn Table>,
    dom_flag_change_count_tbl: Rc<dyn Table>,
    dom_flag_set_time_tbl: Rc<dyn Table>,
    dom_flag_clear_time_tbl: Rc<dyn Table>,
    task_stopping_event: Arc<Event>,
    logger: Rc<dyn DomLogger>,
    dom_utils: DOMUtils,
}

impl DOMDBUtils {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chassis: Rc<dyn Chassis>,
        port_mapping: PortMapping,
        dom_sensor_tbl: Rc<dyn Table>,
        dom_temperature_tbl: Rc<dyn Table>,
        dom_threshold_tbl: Rc<dyn Table>,
        task_stopping_event: Arc<Event>,
        logger: Rc<dyn DomLogger>,
    ) -> Self {
        // Default the DOM-flag tables to fresh no-op handles so existing callers
        // (the sensor/threshold posters) construct unchanged; the flag poster
        // wires the real tables via `with_dom_flag_tables`.
        let dom_utils = DOMUtils::new(chassis.clone());
        DOMDBUtils {
            chassis,
            port_mapping,
            dom_sensor_tbl,
            dom_temperature_tbl,
            dom_threshold_tbl,
            dom_flag_tbl: Rc::new(crate::db::NullTable),
            dom_flag_change_count_tbl: Rc::new(crate::db::NullTable),
            dom_flag_set_time_tbl: Rc::new(crate::db::NullTable),
            dom_flag_clear_time_tbl: Rc::new(crate::db::NullTable),
            task_stopping_event,
            logger,
            dom_utils,
        }
    }

    /// Attach the `TRANSCEIVER_DOM_FLAG` value + metadata-trio tables (the analogue
    /// of the `xcvr_table_helper.get_dom_flag_*_tbl(asic)` handles the Python
    /// `DOMDBUtils` resolves). Builder-style so the base constructors stay unchanged.
    pub fn with_dom_flag_tables(
        mut self,
        value: Rc<dyn Table>,
        change_count: Rc<dyn Table>,
        set_time: Rc<dyn Table>,
        clear_time: Rc<dyn Table>,
    ) -> Self {
        self.dom_flag_tbl = value;
        self.dom_flag_change_count_tbl = change_count;
        self.dom_flag_set_time_tbl = set_time;
        self.dom_flag_clear_time_tbl = clear_time;
        self
    }

    /// `post_port_dom_temperature_info_to_db` (db_utils.py:27).
    pub fn post_port_dom_temperature_info_to_db(
        &self,
        logical_port_name: &str,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) {
        if self.port_mapping.get_asic_id_for_logical_port(logical_port_name).is_none() {
            self.logger.log_error(&format!(
                "Post port dom sensor info to db failed for {logical_port_name} as no asic index found"
            ));
            return;
        }
        let tbl = self.dom_temperature_tbl.clone();
        self.post_diagnostic_values_to_db(logical_port_name, &*tbl, db_cache, |pport| {
            self.dom_utils.get_transceiver_dom_temperature(pport as usize)
        });
    }

    /// `post_port_dom_sensor_info_to_db` (db_utils.py:40).
    pub fn post_port_dom_sensor_info_to_db(
        &self,
        logical_port_name: &str,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) {
        if self.port_mapping.get_asic_id_for_logical_port(logical_port_name).is_none() {
            self.logger.log_error(&format!(
                "Post port dom sensor info to db failed for {logical_port_name} as no asic index found"
            ));
            return;
        }
        let tbl = self.dom_sensor_tbl.clone();
        self.post_diagnostic_values_to_db(logical_port_name, &*tbl, db_cache, |pport| {
            self.dom_utils.get_transceiver_dom_sensor_real_value(pport as usize)
        });
    }

    /// `post_port_dom_thresholds_to_db` (db_utils.py:107).
    pub fn post_port_dom_thresholds_to_db(
        &self,
        logical_port_name: &str,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) {
        if self.port_mapping.get_asic_id_for_logical_port(logical_port_name).is_none() {
            self.logger.log_error(&format!(
                "Post port dom thresholds to db failed for {logical_port_name} as no asic index found"
            ));
            return;
        }
        let tbl = self.dom_threshold_tbl.clone();
        self.post_diagnostic_values_to_db(logical_port_name, &*tbl, db_cache, |pport| {
            self.dom_utils.get_transceiver_dom_thresholds(pport as usize)
        });
    }

    /// `post_port_dom_flags_to_db` (dom_sensor/db_utils.py:53): read the module's
    /// latched DOM flags, maintain the `TRANSCEIVER_DOM_FLAG` metadata trio, then
    /// publish the (beautified) flag row with a `last_update_time` stamp.
    ///
    /// The metadata update reads the *previous* flag row from STATE_DB and compares
    /// it to the freshly-read flags BEFORE the new values overwrite it, so a
    /// raise/clear is detected as an edge.
    pub fn post_port_dom_flags_to_db(
        &self,
        logical_port_name: &str,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
    ) {
        if self.port_mapping.get_asic_id_for_logical_port(logical_port_name).is_none() {
            self.logger.log_error(&format!(
                "Post port dom flags to db failed for {logical_port_name} as no asic index found"
            ));
            return;
        }
        let physical_port = match self.validate_and_get_physical_port(logical_port_name, false) {
            Some(p) => p,
            None => return,
        };

        // Read flags (cache-aware); the DOM decoder yields `{}` for a module that
        // can't serve them (the Python `None`/`NotImplementedError` skip).
        let flags = match db_cache {
            Some(cache) => {
                let cached = cache.borrow().get(&physical_port).cloned();
                match cached {
                    Some(v) => v,
                    None => {
                        let v = self.dom_utils.get_transceiver_dom_flags(physical_port as usize);
                        if let Some(obj) = v.as_object() {
                            if !obj.is_empty() {
                                self.update_dom_flag_metadata(logical_port_name, obj);
                            }
                        }
                        cache.borrow_mut().insert(physical_port, v.clone());
                        v
                    }
                }
            }
            None => {
                let v = self.dom_utils.get_transceiver_dom_flags(physical_port as usize);
                if let Some(obj) = v.as_object() {
                    if !obj.is_empty() {
                        self.update_dom_flag_metadata(logical_port_name, obj);
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
        beautify_dom_info_dict(&mut beautified);
        let mut fvs: Vec<(String, String)> =
            beautified.iter().map(|(k, v)| (k.clone(), py_str(v))).collect();
        fvs.push(("last_update_time".to_string(), get_current_time()));
        let _ = self.dom_flag_tbl.set(logical_port_name, &fvs);
    }

    /// `DBUtils._update_flag_metadata_tables(..., "DOM flags")` bound to this
    /// util's DOM-flag tables.
    fn update_dom_flag_metadata(&self, logical_port_name: &str, curr_flags: &Map<String, Value>) {
        update_flag_metadata_tables(
            logical_port_name,
            curr_flags,
            &get_current_time(),
            &*self.dom_flag_tbl,
            &*self.dom_flag_change_count_tbl,
            &*self.dom_flag_set_time_tbl,
            &*self.dom_flag_clear_time_tbl,
            "DOM flags",
            &*self.logger,
        );
    }

    /// `DBUtils.post_diagnostic_values_to_db` (db/utils.py:19). `get_values` yields
    /// a `serde_json::Value`; `Value::Null` models Python `None` (skip) and an
    /// empty object models `{}` (skip). Present values are beautified and posted
    /// with a `last_update_time` stamp.
    pub fn post_diagnostic_values_to_db<F>(
        &self,
        logical_port_name: &str,
        table: &dyn Table,
        db_cache: Option<&RefCell<HashMap<i32, Value>>>,
        get_values: F,
    ) where
        F: Fn(i32) -> Value,
    {
        let physical_port = match self.validate_and_get_physical_port(logical_port_name, false) {
            Some(p) => p,
            None => return,
        };

        let values = match db_cache {
            Some(cache) => {
                // Bind the lookup to a temporary so the shared borrow is dropped
                // before the miss branch takes a mutable borrow to cache the value.
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

        // Python: `if diagnostic_values_dict is not None: if not dict: return`.
        if values.is_null() {
            return;
        }
        let obj = match values.as_object() {
            Some(o) if !o.is_empty() => o.clone(),
            _ => return,
        };

        let mut beautified = obj;
        beautify_dom_info_dict(&mut beautified);
        let mut fvs: Vec<(String, String)> =
            beautified.iter().map(|(k, v)| (k.clone(), py_str(v))).collect();
        fvs.push(("last_update_time".to_string(), get_current_time()));
        let _ = table.set(logical_port_name, &fvs);
    }

    /// `_validate_and_get_physical_port` (db/utils.py:62).
    pub fn validate_and_get_physical_port(
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
        // `physical_port not in self.sfp_obj_dict`
        if self.chassis.sfp(pport as usize).is_err() {
            self.logger.log_error(&format!(
                "Validate and get physical port failed for {logical_port_name} as no sfp object found"
            ));
            return None;
        }
        if !self.get_transceiver_presence(pport) {
            return None;
        }
        // `enable_flat_memory_check` path is unused by the DOM posters (default false).
        Some(pport)
    }

    /// `XCVRDUtils.get_transceiver_presence(physical_port)`.
    fn get_transceiver_presence(&self, physical_port: i32) -> bool {
        self.chassis
            .sfp(physical_port as usize)
            .and_then(|s| s.get_presence())
            .unwrap_or(false)
    }

    /// `_beautify_dom_info_dict` as a method, including the Python `None` warning
    /// (`_beautify_dom_info_dict(None)` logs and returns).
    pub fn beautify_dom_info_dict(&self, dom_info_dict: Option<&mut Map<String, Value>>) {
        match dom_info_dict {
            None => self.logger.log_warning("DOM info dict is None while beautifying"),
            Some(d) => beautify_dom_info_dict(d),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockChassis, MockSfp, MockTable};
    use serde_json::json;
    use std::cell::Cell;

    /// A counting logger so the `_beautify_dom_info_dict(None)` warning is
    /// observable (the Rust analogue of `mock_logger.log_warning.assert_called`).
    #[derive(Default)]
    struct CountingLogger {
        warnings: Cell<u32>,
        last_warning: RefCell<Option<String>>,
    }
    impl DomLogger for CountingLogger {
        fn log_warning(&self, msg: &str) {
            self.warnings.set(self.warnings.get() + 1);
            *self.last_warning.borrow_mut() = Some(msg.to_string());
        }
    }

    fn mapping_eth0() -> PortMapping {
        use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType};
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        pm
    }

    fn build(
        chassis: Rc<dyn Chassis>,
        pm: PortMapping,
        sensor: Rc<dyn Table>,
        temperature: Rc<dyn Table>,
        threshold: Rc<dyn Table>,
        stop: Arc<Event>,
    ) -> DOMDBUtils {
        DOMDBUtils::new(chassis, pm, sensor, temperature, threshold, stop, Rc::new(NoopDomLogger))
    }

    fn full_sensor_values() -> Value {
        let mut m = serde_json::Map::new();
        m.insert("temperature".into(), json!("22.75"));
        m.insert("voltage".into(), json!("0.5"));
        for i in 1..=8 {
            m.insert(format!("rx{i}power"), json!("0.7"));
            m.insert(format!("tx{i}bias"), json!("0.7"));
            m.insert(format!("tx{i}power"), json!("0.7"));
        }
        Value::Object(m)
    }

    /// Port of `tests/test_xcvrd.py::test_post_port_dom_sensor_info_to_db`.
    #[test]
    fn test_post_port_dom_sensor_info_to_db() {
        let sensor = Rc::new(MockTable::new());
        let temperature = Rc::new(MockTable::new());
        let threshold = Rc::new(MockTable::new());
        let stop = Event::new();
        let cache: RefCell<HashMap<i32, Value>> = RefCell::new(HashMap::new());

        let present_sfp = |dom: Value| {
            let mut sfp = MockSfp::present_with_info(json!({}));
            sfp.dom = dom;
            Rc::new(MockChassis::with_sfps(vec![sfp])) as Rc<dyn Chassis>
        };

        // asic index None -> empty (unknown port).
        {
            let d = build(present_sfp(json!({})), PortMapping::new(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_sensor_info_to_db("Ethernet0", None);
            assert_eq!(sensor.get_size().unwrap(), 0);
        }
        // stop event set -> empty.
        {
            stop.set();
            let d = build(present_sfp(json!({})), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_sensor_info_to_db("Ethernet0", None);
            assert_eq!(sensor.get_size().unwrap(), 0);
            stop.clear();
        }
        // transceiver not present -> empty.
        {
            let mut absent = MockSfp::absent();
            absent.dom = full_sensor_values();
            let chassis = Rc::new(MockChassis::with_sfps(vec![absent])) as Rc<dyn Chassis>;
            let d = build(chassis, mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_sensor_info_to_db("Ethernet0", None);
            assert_eq!(sensor.get_size().unwrap(), 0);
        }
        // present but values None (dom = Null) -> empty.
        {
            let d = build(present_sfp(Value::Null), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_sensor_info_to_db("Ethernet0", None);
            assert_eq!(sensor.get_size().unwrap(), 0);
        }
        // present with valid values -> 27 fields (26 + last_update_time); cache populated.
        {
            let d = build(present_sfp(full_sensor_values()), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_sensor_info_to_db("Ethernet0", Some(&cache));
            assert_eq!(sensor.row("Ethernet0").unwrap().len(), 27);
            assert!(cache.borrow().get(&0).is_some());
        }
        // getter now yields None but cache hit re-posts the cached values -> still 27.
        {
            let d = build(present_sfp(Value::Null), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_sensor_info_to_db("Ethernet0", Some(&cache));
            assert_eq!(sensor.row("Ethernet0").unwrap().len(), 27);
        }
    }

    /// Port of `tests/test_xcvrd.py::test_post_port_dom_temperature_info_to_db`.
    #[test]
    fn test_post_port_dom_temperature_info_to_db() {
        let sensor = Rc::new(MockTable::new());
        let temperature = Rc::new(MockTable::new());
        let threshold = Rc::new(MockTable::new());
        let stop = Event::new();
        let cache: RefCell<HashMap<i32, Value>> = RefCell::new(HashMap::new());

        let present_temp = |t: Option<Value>| {
            let mut sfp = MockSfp::present_with_info(json!({}));
            if let Some(v) = t {
                sfp.set_json_call("get_temperature", v);
            } else {
                sfp.fail_method("get_temperature");
            }
            Rc::new(MockChassis::with_sfps(vec![sfp])) as Rc<dyn Chassis>
        };

        // asic index None.
        {
            let d = build(present_temp(Some(json!("68.75"))), PortMapping::new(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_temperature_info_to_db("Ethernet0", None);
            assert_eq!(temperature.get_size().unwrap(), 0);
        }
        // stop set.
        {
            stop.set();
            let d = build(present_temp(Some(json!("68.75"))), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_temperature_info_to_db("Ethernet0", None);
            assert_eq!(temperature.get_size().unwrap(), 0);
            stop.clear();
        }
        // not present.
        {
            let mut absent = MockSfp::absent();
            absent.set_json_call("get_temperature", json!("68.75"));
            let chassis = Rc::new(MockChassis::with_sfps(vec![absent])) as Rc<dyn Chassis>;
            let d = build(chassis, mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_temperature_info_to_db("Ethernet0", None);
            assert_eq!(temperature.get_size().unwrap(), 0);
        }
        // getter raises (NotImplementedError) -> {} -> empty.
        {
            let d = build(present_temp(None), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_temperature_info_to_db("Ethernet0", None);
            assert_eq!(temperature.get_size().unwrap(), 0);
        }
        // valid -> 2 fields (temperature + last_update_time); cache populated.
        {
            let d = build(present_temp(Some(json!("68.75"))), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_temperature_info_to_db("Ethernet0", Some(&cache));
            assert_eq!(temperature.row("Ethernet0").unwrap().len(), 2);
            assert!(cache.borrow().get(&0).is_some());
        }
        // cache hit re-posts -> still 2.
        {
            let d = build(present_temp(None), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_temperature_info_to_db("Ethernet0", Some(&cache));
            assert_eq!(temperature.row("Ethernet0").unwrap().len(), 2);
        }
    }

    /// Port of `tests/test_xcvrd.py::test_post_port_dom_thresholds_to_db`.
    #[test]
    fn test_post_port_dom_thresholds_to_db() {
        let sensor = Rc::new(MockTable::new());
        let temperature = Rc::new(MockTable::new());
        let threshold = Rc::new(MockTable::new());
        let stop = Event::new();
        let cache: RefCell<HashMap<i32, Value>> = RefCell::new(HashMap::new());

        let thr_values = json!({
            "temphighalarm": "75.0", "templowalarm": "-5.0",
            "temphighwarning": "72.0", "templowwarning": "-2.0",
            "vcchighalarm": "3.63", "vcclowalarm": "2.97",
            "vcchighwarning": "3.465", "vcclowwarning": "3.135",
            "rxpowerhighalarm": "6.2", "rxpowerlowalarm": "-11.198",
            "rxpowerhighwarning": "4.2", "rxpowerlowwarning": "-9.201",
        });
        let present_thr = |thr: Value| {
            let mut sfp = MockSfp::present_with_info(json!({}));
            sfp.thresholds = thr;
            Rc::new(MockChassis::with_sfps(vec![sfp])) as Rc<dyn Chassis>
        };

        // asic index None.
        {
            let d = build(present_thr(thr_values.clone()), PortMapping::new(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_thresholds_to_db("Ethernet0", None);
            assert_eq!(threshold.get_size().unwrap(), 0);
        }
        // stop set.
        {
            stop.set();
            let d = build(present_thr(thr_values.clone()), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_thresholds_to_db("Ethernet0", None);
            assert_eq!(threshold.get_size().unwrap(), 0);
            stop.clear();
        }
        // not present.
        {
            let mut absent = MockSfp::absent();
            absent.thresholds = thr_values.clone();
            let chassis = Rc::new(MockChassis::with_sfps(vec![absent])) as Rc<dyn Chassis>;
            let d = build(chassis, mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_thresholds_to_db("Ethernet0", None);
            assert_eq!(threshold.get_size().unwrap(), 0);
        }
        // values None (thresholds = Null) -> empty.
        {
            let d = build(present_thr(Value::Null), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_thresholds_to_db("Ethernet0", None);
            assert_eq!(threshold.get_size().unwrap(), 0);
        }
        // valid -> 13 fields (12 + last_update_time); cache populated.
        {
            let d = build(present_thr(thr_values.clone()), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_thresholds_to_db("Ethernet0", Some(&cache));
            assert_eq!(threshold.row("Ethernet0").unwrap().len(), 13);
            assert!(cache.borrow().get(&0).is_some());
        }
        // cache hit re-posts -> still 13.
        {
            let d = build(present_thr(Value::Null), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_thresholds_to_db("Ethernet0", Some(&cache));
            assert_eq!(threshold.row("Ethernet0").unwrap().len(), 13);
        }
    }

    /// the DOM threshold row builder is the gate the
    /// daemon's per-port threshold backfill keys off. An empty / unreadable read — the
    /// too-early insert-time read of a just-powered CMIS module whose threshold page has
    /// not yet settled — yields `None`, so no `TRANSCEIVER_DOM_THRESHOLD` row is written
    /// and the daemon's `!exists(DOM_THRESHOLD|port)` guard stays armed and re-attempts on
    /// the next DOM pass; a readable read yields `Some(row)`, so the row is published once
    /// and the retry stops. This is the decision that lets a merely-present spare logical
    /// port (never re-inserted) still complete its INFO+threshold pipeline.
    #[test]
    fn dom_threshold_row_none_when_empty_some_when_readable() {
        assert!(beautify_dom_row(&Value::Null).is_none(), "Null read -> no row");
        assert!(beautify_dom_row(&json!({})).is_none(), "empty read -> no row");

        let readable = json!({
            "temphighalarm": "75.0", "templowalarm": "-5.0",
            "vcchighalarm": "3.63", "vcclowalarm": "2.97",
        });
        let row = beautify_dom_row(&readable).expect("readable thresholds -> Some(row)");
        assert_eq!(row.len(), 4);
        let keys: std::collections::HashSet<&str> = row.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains("temphighalarm") && keys.contains("vcclowalarm"));
    }

    /// the multiport threshold backfill converges on a
    /// shared destination table. Models the daemon's DOM-loop re-post gated on
    /// `!exists(DOM_THRESHOLD|port)`: the first, too-early read of a just-powered module's
    /// threshold page returns empty, so the shared threshold table gets NO row (exactly the
    /// gap observed for the spare logical port Ethernet60 — INFO present but
    /// DOM_THRESHOLD absent); a later read, once the page is readable, backfills the row.
    #[test]
    fn dom_threshold_backfill_converges_once_readable() {
        let sensor = Rc::new(MockTable::new());
        let temperature = Rc::new(MockTable::new());
        let threshold = Rc::new(MockTable::new());
        let stop = Event::new();

        let present_thr = |thr: Value| {
            let mut sfp = MockSfp::present_with_info(json!({}));
            sfp.thresholds = thr;
            Rc::new(MockChassis::with_sfps(vec![sfp])) as Rc<dyn Chassis>
        };

        // Insert-time early read: threshold page not yet readable (Null) -> no row.
        {
            let d = build(present_thr(Value::Null), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_thresholds_to_db("Ethernet0", None);
            assert_eq!(threshold.get_size().unwrap(), 0, "empty read must leave the row absent");
        }
        // DOM-loop backfill: page now readable -> row appears (6 fields + last_update_time).
        {
            let readable = json!({
                "temphighalarm": "75.0", "templowalarm": "-5.0",
                "vcchighalarm": "3.63", "vcclowalarm": "2.97",
                "rxpowerhighalarm": "6.2", "rxpowerlowalarm": "-11.198",
            });
            let d = build(present_thr(readable), mapping_eth0(),
                sensor.clone(), temperature.clone(), threshold.clone(), stop.clone());
            d.post_port_dom_thresholds_to_db("Ethernet0", None);
            assert_eq!(threshold.row("Ethernet0").unwrap().len(), 7, "backfill must publish the row");
        }
    }

    /// Port of `tests/test_xcvrd.py::test_beautify_dom_info_dict`.
    #[test]
    fn test_beautify_dom_info_dict() {
        let mut dom_info_dict = json!({ "temperature": "0C", "eSNR": 1.1 })
            .as_object().unwrap().clone();
        let logger = Rc::new(CountingLogger::default());
        let d = DOMDBUtils::new(
            Rc::new(MockChassis::with_sfps(vec![])),
            PortMapping::new(),
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            Event::new(),
            logger.clone(),
        );
        d.beautify_dom_info_dict(Some(&mut dom_info_dict));
        assert_eq!(dom_info_dict.get("temperature"), Some(&json!("0")));
        assert_eq!(dom_info_dict.get("eSNR"), Some(&json!("1.1")));

        // None input logs the warning exactly once.
        d.beautify_dom_info_dict(None);
        assert_eq!(logger.warnings.get(), 1);
        assert_eq!(logger.last_warning.borrow().as_deref(), Some("DOM info dict is None while beautifying"));
    }

    /// the beautify+row builder strips units and
    /// stringifies every value the way the DOM_SENSOR row is posted.
    #[test]
    fn dom_sensor_real_values_to_db_row() {
        let raw = json!({
            "temperature": "22.75C",
            "voltage": "0.5Volts",
            "tx1power": "0.7dBm",
            "rx1power": "0.7dBm",
            "tx1bias": "0.7mA",
            "eSNR": 1.1,
        });
        let row: std::collections::BTreeMap<String, String> =
            beautify_dom_row(&raw).unwrap().into_iter().collect();
        assert_eq!(row.get("temperature").map(String::as_str), Some("22.75"));
        assert_eq!(row.get("voltage").map(String::as_str), Some("0.5"));
        assert_eq!(row.get("tx1power").map(String::as_str), Some("0.7"));
        assert_eq!(row.get("rx1power").map(String::as_str), Some("0.7"));
        assert_eq!(row.get("tx1bias").map(String::as_str), Some("0.7"));
        assert_eq!(row.get("eSNR").map(String::as_str), Some("1.1"));

        // Empty / non-object collapse to None (Python `{}`/`None` skips).
        assert!(beautify_dom_row(&json!({})).is_none());
        assert!(beautify_dom_row(&Value::Null).is_none());
    }

    /// the DOM *flag* row beautify — boolean flag
    /// values render as Python `str(bool)` (`True`/`False`) and the flag keys
    /// (tempHAlarm, vccHAlarm, …) are NOT treated as unit-bearing DOM keys, so no
    /// stripping happens. This is what `post_port_dom_flags_to_db` writes to
    /// `TRANSCEIVER_DOM_FLAG` and what `test_dom_flag_meta.py` reads back.
    #[test]
    fn dom_flag_row_beautifies_bools_without_unit_strip() {
        let raw = json!({
            "tempHAlarm": true,
            "vccHAlarm": false,
            "tempLWarning": true,
        });
        let row: std::collections::BTreeMap<String, String> =
            beautify_dom_row(&raw).unwrap().into_iter().collect();
        assert_eq!(row.get("tempHAlarm").map(String::as_str), Some("True"));
        assert_eq!(row.get("vccHAlarm").map(String::as_str), Some("False"));
        assert_eq!(row.get("tempLWarning").map(String::as_str), Some("True"));
    }

    #[test]
    fn lane_key_regex_matches_python() {
        for i in 1..=8 {
            assert!(is_tx_rx_power(&format!("tx{i}power")));
            assert!(is_tx_rx_power(&format!("rx{i}power")));
            assert!(is_tx_rx_bias(&format!("tx{i}bias")));
            assert!(is_tx_rx_bias(&format!("rx{i}bias")));
        }
        assert!(!is_tx_rx_power("tx9power"));
        assert!(!is_tx_rx_power("tx0power"));
        assert!(!is_tx_rx_power("txpower"));
        assert!(!is_tx_rx_power("tx1powerx"));
        assert!(!is_tx_rx_bias("temperature"));
    }

    /// Build a present SFP whose `get_transceiver_dom_flags` returns `flags`.
    fn present_flags(flags: Value) -> Rc<dyn Chassis> {
        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.set_json_call("get_transceiver_dom_flags", flags);
        Rc::new(MockChassis::with_sfps(vec![sfp])) as Rc<dyn Chassis>
    }

    fn build_with_flags(
        chassis: Rc<dyn Chassis>,
        pm: PortMapping,
        stop: Arc<Event>,
        value: Rc<dyn Table>,
        count: Rc<dyn Table>,
        set_time: Rc<dyn Table>,
        clear_time: Rc<dyn Table>,
    ) -> DOMDBUtils {
        DOMDBUtils::new(
            chassis,
            pm,
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            Rc::new(MockTable::new()),
            stop,
            Rc::new(NoopDomLogger),
        )
        .with_dom_flag_tables(value, count, set_time, clear_time)
    }

    /// Port of `tests/test_xcvrd.py::test_get_transceiver_dom_flags`: the DOM-flag
    /// getter returns the platform dict, and a `NotImplementedError` collapses to
    /// `{}` (so the poster skips the port rather than crashing the poll).
    #[test]
    fn test_get_transceiver_dom_flags() {
        let flags = json!({ "tempHAlarm": true, "vccHAlarm": false });
        let chassis = present_flags(flags.clone());
        let dom_utils = DOMUtils::new(chassis);
        assert_eq!(dom_utils.get_transceiver_dom_flags(0), flags);

        // Platform raises NotImplementedError -> {}.
        let mut sfp = MockSfp::present_with_info(json!({}));
        sfp.fail_method("get_transceiver_dom_flags");
        let dom_utils = DOMUtils::new(Rc::new(MockChassis::with_sfps(vec![sfp])));
        assert_eq!(dom_utils.get_transceiver_dom_flags(0), json!({}));
    }

    /// Port of `tests/test_xcvrd.py::test_post_port_dom_flags_to_db`: publishing the
    /// DOM flags writes the beautified flag row (+ `last_update_time`) to
    /// `TRANSCEIVER_DOM_FLAG` and seeds the metadata trio on the first publish, then
    /// bumps the change count / stamps the set-time when a flag flips to raised.
    #[test]
    fn test_post_port_dom_flags_to_db() {
        let value = Rc::new(MockTable::new());
        let count = Rc::new(MockTable::new());
        let set_time = Rc::new(MockTable::new());
        let clear_time = Rc::new(MockTable::new());
        let stop = Event::new();

        // asic index None (unknown port) -> nothing written.
        {
            let d = build_with_flags(
                present_flags(json!({ "tempHAlarm": false })),
                PortMapping::new(),
                stop.clone(),
                value.clone(),
                count.clone(),
                set_time.clone(),
                clear_time.clone(),
            );
            d.post_port_dom_flags_to_db("Ethernet0", None);
            assert_eq!(value.get_size().unwrap(), 0);
        }

        // First publish: flag row written + metadata seeded (count 0, times never).
        {
            let d = build_with_flags(
                present_flags(json!({ "tempHAlarm": false, "vccHAlarm": false })),
                mapping_eth0(),
                stop.clone(),
                value.clone(),
                count.clone(),
                set_time.clone(),
                clear_time.clone(),
            );
            d.post_port_dom_flags_to_db("Ethernet0", None);
            let row = value.row("Ethernet0").unwrap();
            assert_eq!(row.get("tempHAlarm").map(String::as_str), Some("False"));
            assert_eq!(row.get("vccHAlarm").map(String::as_str), Some("False"));
            assert!(row.contains_key("last_update_time"));
            assert_eq!(count.field("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
            assert_eq!(set_time.field("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
            assert_eq!(clear_time.field("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        }

        // Second publish with tempHAlarm now raised: count bumps to 1, set-time stamped.
        {
            let d = build_with_flags(
                present_flags(json!({ "tempHAlarm": true, "vccHAlarm": false })),
                mapping_eth0(),
                stop.clone(),
                value.clone(),
                count.clone(),
                set_time.clone(),
                clear_time.clone(),
            );
            d.post_port_dom_flags_to_db("Ethernet0", None);
            assert_eq!(value.field("Ethernet0", "tempHAlarm").as_deref(), Some("True"));
            assert_eq!(count.field("Ethernet0", "tempHAlarm").as_deref(), Some("1"));
            assert_ne!(set_time.field("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
            // vccHAlarm did not change -> its count/time stay put.
            assert_eq!(count.field("Ethernet0", "vccHAlarm").as_deref(), Some("0"));
            assert_eq!(clear_time.field("Ethernet0", "vccHAlarm").as_deref(), Some("never"));
        }

        // Empty flags (platform can't serve them) -> no write, no metadata churn.
        {
            let value2 = Rc::new(MockTable::new());
            let d = build_with_flags(
                present_flags(json!({})),
                mapping_eth0(),
                stop.clone(),
                value2.clone(),
                Rc::new(MockTable::new()),
                Rc::new(MockTable::new()),
                Rc::new(MockTable::new()),
            );
            d.post_port_dom_flags_to_db("Ethernet0", None);
            assert_eq!(value2.get_size().unwrap(), 0);
        }
    }
}
