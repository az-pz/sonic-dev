//! Port of `dom/utilities/dom_sensor/{utils,db_utils}.py` — `DOMUtils` (reads the
//! module DOM monitors off the SFP handle) and `DOMDBUtils` (posts them to
//! `TRANSCEIVER_DOM_SENSOR` / `_DOM_TEMPERATURE` / `_DOM_THRESHOLD`).
//!
//! The posters delegate to the shared [`DbUtils::post_diagnostic_values_to_db`]
//! (validate → read → beautify → set); the only DOM-specific piece is
//! [`DomDbUtils::beautify_dom_info_dict`], which strips the engineering units the
//! module reports (`22.0C`/`3.3Volts`/`-1.2dBm`/`7.5mA`) so STATE_DB carries bare
//! numbers, exactly as `_beautify_dom_info_dict` does.

use std::sync::atomic::AtomicBool;

use serde_json::{Map, Value};

use crate::db::DbTable;
use crate::hal::{Hal, SfpHandle};
use crate::xcvrd_utilities::port_event_helper::PortMapping;

use super::db::{strip_unit, value_to_py_str, DbCache, DbUtils};

/// `DOMUtils` — read the module DOM monitor dicts off the SFP handle.
///
/// Each reader mirrors the Python `try: … except NotImplementedError: return {}`:
/// a successful read yields `Some(value)`; a not-implemented/errored read yields
/// `None`. The shared poster treats `None` and an empty dict identically (nothing
/// to post), so the two representations are behaviorally equivalent.
pub struct DomUtils;

impl DomUtils {
    pub fn new() -> Self {
        DomUtils
    }

    /// `get_transceiver_dom_temperature` — `{'temperature': sfp.get_temperature()}`.
    pub fn get_transceiver_dom_temperature(&self, sfp: &dyn SfpHandle) -> Option<Value> {
        match sfp.call_json("get_temperature") {
            Ok(temp) => {
                let mut m = Map::new();
                m.insert("temperature".to_string(), temp);
                Some(Value::Object(m))
            }
            Err(_) => None,
        }
    }

    /// `get_transceiver_dom_sensor_real_value` — `sfp.get_transceiver_dom_real_value()`.
    pub fn get_transceiver_dom_sensor_real_value(&self, sfp: &dyn SfpHandle) -> Option<Value> {
        sfp.get_transceiver_dom_real_value().ok()
    }

    /// `get_transceiver_dom_flags` — `sfp.get_transceiver_dom_flags()`.
    ///
    /// Pure delegate, mirroring `DOMUtils.get_transceiver_dom_flags`
    /// (`try: return sfp.get_transceiver_dom_flags() except NotImplementedError: {}`).
    /// The module temp/vcc alarm-warning group (CMIS byte 00h:9) and every other latched
    /// flag are decoded in the platform's `CmisApi.get_transceiver_dom_flags`
    /// (`get_module_level_flag` reads byte 00h:9 and emits `tempH/L*` + `vccH/L*`
    /// together); the daemon never decodes EEPROM itself. A successful read yields
    /// `Some(dict)`; a not-implemented/errored read yields `None`, which the poster
    /// treats identically to an empty dict ("nothing to post").
    pub fn get_transceiver_dom_flags(&self, sfp: &dyn SfpHandle) -> Option<Value> {
        sfp.call_json("get_transceiver_dom_flags").ok()
    }

    /// `get_transceiver_dom_thresholds` — `sfp.get_transceiver_threshold_info()`.
    pub fn get_transceiver_dom_thresholds(&self, sfp: &dyn SfpHandle) -> Option<Value> {
        sfp.get_transceiver_threshold_info().ok()
    }
}

impl Default for DomUtils {
    fn default() -> Self {
        DomUtils::new()
    }
}

/// `DOMDBUtils` — the DOM posters (`TRANSCEIVER_DOM_SENSOR/_TEMPERATURE/_THRESHOLD`).
pub struct DomDbUtils;

impl DomDbUtils {
    pub const TEMP_UNIT: &'static str = "C";
    pub const VOLT_UNIT: &'static str = "Volts";
    pub const POWER_UNIT: &'static str = "dBm";
    pub const BIAS_UNIT: &'static str = "mA";

    pub fn new() -> Self {
        DomDbUtils
    }

    /// `post_port_dom_sensor_info_to_db` → `TRANSCEIVER_DOM_SENSOR`.
    pub fn post_port_dom_sensor_info_to_db(
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
        let dom = DomUtils;
        DbUtils.post_diagnostic_values_to_db(
            stop,
            logical_port_name,
            port_mapping,
            table,
            hal,
            |sfp| dom.get_transceiver_dom_sensor_real_value(sfp),
            db_cache,
            Self::beautify_dom_info_dict,
            false,
        );
    }

    /// `post_port_dom_temperature_info_to_db` → `TRANSCEIVER_DOM_TEMPERATURE`.
    pub fn post_port_dom_temperature_info_to_db(
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
        let dom = DomUtils;
        DbUtils.post_diagnostic_values_to_db(
            stop,
            logical_port_name,
            port_mapping,
            table,
            hal,
            |sfp| dom.get_transceiver_dom_temperature(sfp),
            db_cache,
            Self::beautify_dom_info_dict,
            false,
        );
    }

    /// `post_port_dom_thresholds_to_db` → `TRANSCEIVER_DOM_THRESHOLD` (at insert).
    pub fn post_port_dom_thresholds_to_db(
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
        let dom = DomUtils;
        DbUtils.post_diagnostic_values_to_db(
            stop,
            logical_port_name,
            port_mapping,
            table,
            hal,
            |sfp| dom.get_transceiver_dom_thresholds(sfp),
            db_cache,
            Self::beautify_dom_info_dict,
            false,
        );
    }

    /// `post_port_dom_flags_to_db` (`dom_sensor/db_utils.py:53`) → `TRANSCEIVER_DOM_
    /// FLAG` + its change-count / set-time / clear-time metadata tables. Reads the
    /// module's latched DOM monitor flags (`get_transceiver_dom_flags`), stamps flag
    /// change-tracking metadata on every transition, and publishes the (unit-free)
    /// flag row. Shares [`DbUtils::post_flags_to_db`] with the status-flag poster.
    #[allow(clippy::too_many_arguments)]
    pub fn post_port_dom_flags_to_db(
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
        let dom = DomUtils;
        DbUtils.post_flags_to_db(
            stop,
            logical_port_name,
            port_mapping,
            hal,
            flag_tbl,
            flag_change_count_tbl,
            flag_set_time_tbl,
            flag_clear_time_tbl,
            |sfp| dom.get_transceiver_dom_flags(sfp),
            Self::beautify_dom_info_dict,
            "DOM flags",
            db_cache,
        );
    }

    /// `_beautify_dom_info_dict` — strip the reported engineering unit from the DOM
    /// monitor values (`temperature`→drop `C`, `voltage`→drop `Volts`,
    /// `(tx|rx)[1-8]power`→drop `dBm`, `(tx|rx)[1-8]bias`→drop `mA`), and `str()` any
    /// remaining non-string value. Keys are preserved (an `N/A` value that lacks the
    /// unit stays `N/A`).
    pub fn beautify_dom_info_dict(dom_info_dict: &mut Map<String, Value>) {
        for (k, v) in dom_info_dict.iter_mut() {
            if k == "temperature" {
                let s = strip_unit(v, Self::TEMP_UNIT);
                *v = Value::String(s);
            } else if k == "voltage" {
                let s = strip_unit(v, Self::VOLT_UNIT);
                *v = Value::String(s);
            } else if is_lane_key(k, "power") {
                let s = strip_unit(v, Self::POWER_UNIT);
                *v = Value::String(s);
            } else if is_lane_key(k, "bias") {
                let s = strip_unit(v, Self::BIAS_UNIT);
                *v = Value::String(s);
            } else if !v.is_string() {
                *v = Value::String(value_to_py_str(v));
            }
        }
    }
}

impl Default for DomDbUtils {
    fn default() -> Self {
        DomDbUtils::new()
    }
}

/// `^(tx|rx)[1-8]<suffix>$` — the per-lane DOM key shape (`tx3power`, `rx8bias`, …).
fn is_lane_key(k: &str, suffix: &str) -> bool {
    let b = k.as_bytes();
    k.len() == 3 + suffix.len()
        && (b[0] == b't' || b[0] == b'r')
        && b[1] == b'x'
        && (b'1'..=b'8').contains(&b[2])
        && &k[3..] == suffix
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Result as XResult, XcvrdError};
    use crate::hal::SfpHandle;
    use crate::mock::{MockDbTable, MockHal, MockSfp};
    use crate::xcvrd_utilities::port_event_helper::{
        PortChangeEvent, PortChangeEventType, PortMapping,
    };
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::collections::HashMap;

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

    // An SFP whose every read raises the NotImplementedError analogue (`Err`), to
    // exercise the DOMUtils `except NotImplementedError: return {}` -> `None` path.
    struct ErrSfp;
    impl SfpHandle for ErrSfp {
        fn get_presence(&self) -> XResult<bool> {
            Ok(true)
        }
        fn is_replaceable(&self) -> XResult<bool> {
            Ok(true)
        }
        fn get_reset_status(&self) -> XResult<bool> {
            Ok(false)
        }
        fn sfp_type(&self) -> XResult<String> {
            Ok("QSFP_DD".to_string())
        }
        fn get_error_description(&self) -> XResult<Option<String>> {
            Ok(None)
        }
        fn get_transceiver_info(&self) -> XResult<Value> {
            Err(XcvrdError::NotImplemented)
        }
        fn get_transceiver_dom_real_value(&self) -> XResult<Value> {
            Err(XcvrdError::NotImplemented)
        }
        fn get_transceiver_status(&self) -> XResult<Value> {
            Err(XcvrdError::NotImplemented)
        }
        fn get_transceiver_threshold_info(&self) -> XResult<Value> {
            Err(XcvrdError::NotImplemented)
        }
        fn get_lpmode(&self) -> XResult<bool> {
            Ok(false)
        }
        fn set_lpmode(&self, _on: bool) -> XResult<bool> {
            Ok(true)
        }
        fn reset(&self) -> XResult<bool> {
            Ok(true)
        }
        fn call_json(&self, _method: &str) -> XResult<Value> {
            Err(XcvrdError::NotImplemented)
        }
        fn read_eeprom(&self, _offset: usize, _num_bytes: usize) -> XResult<Option<Vec<u8>>> {
            Ok(None)
        }
        fn write_eeprom(&self, _offset: usize, _data: &[u8]) -> XResult<bool> {
            Err(XcvrdError::NotImplemented)
        }
    }

    fn dom_sensor_values() -> Value {
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

    // --- DOMUtils reads (tests/test_xcvrd.py:test_get_transceiver_dom_*) ---------

    #[test]
    fn test_get_transceiver_dom_temperature() {
        let dom = DomUtils;
        let sfp = MockSfp::present().with_json("get_temperature", json!(42.0));
        let v = dom.get_transceiver_dom_temperature(&sfp).unwrap();
        assert!(v.get("temperature").is_some());
        // NotImplementedError analogue -> None.
        assert!(dom.get_transceiver_dom_temperature(&ErrSfp).is_none());
    }

    #[test]
    fn test_get_transceiver_dom_sensor_real_value() {
        let dom = DomUtils;
        let sfp = MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
        };
        assert!(dom.get_transceiver_dom_sensor_real_value(&sfp).is_some());
        // empty dict stays empty
        let empty = MockSfp {
            dom_real_value: json!({}),
            ..MockSfp::present()
        };
        assert_eq!(
            dom.get_transceiver_dom_sensor_real_value(&empty),
            Some(json!({}))
        );
        // NotImplementedError analogue -> None.
        assert!(dom.get_transceiver_dom_sensor_real_value(&ErrSfp).is_none());
    }

    #[test]
    fn test_get_transceiver_dom_thresholds() {
        let dom = DomUtils;
        let sfp = MockSfp {
            threshold_info: json!({"temphighalarm": "75.0"}),
            ..MockSfp::present()
        };
        assert!(dom.get_transceiver_dom_thresholds(&sfp).is_some());
        assert!(dom.get_transceiver_dom_thresholds(&ErrSfp).is_none());
    }

    // --- beautify (tests/test_xcvrd.py:test_beautify_dom_info_dict) --------------

    #[test]
    fn test_beautify_dom_info_dict() {
        let mut dict = json!({"temperature": "0C", "eSNR": 1.1})
            .as_object()
            .unwrap()
            .clone();
        DomDbUtils::beautify_dom_info_dict(&mut dict);
        assert_eq!(dict.get("temperature"), Some(&json!("0")));
        assert_eq!(dict.get("eSNR"), Some(&json!("1.1")));
    }

    #[test]
    fn test_beautify_dom_info_dict_units_and_na() {
        let mut dict = json!({
            "temperature": "22.0C",
            "voltage": "3.3Volts",
            "rx1power": "-1.2dBm",
            "tx8power": "N/A",
            "tx1bias": "7.5mA",
        })
        .as_object()
        .unwrap()
        .clone();
        DomDbUtils::beautify_dom_info_dict(&mut dict);
        assert_eq!(dict.get("temperature"), Some(&json!("22.0")));
        assert_eq!(dict.get("voltage"), Some(&json!("3.3")));
        assert_eq!(dict.get("rx1power"), Some(&json!("-1.2")));
        assert_eq!(dict.get("tx1bias"), Some(&json!("7.5")));
        // A value without the unit (N/A) is preserved verbatim; the key stays.
        assert_eq!(dict.get("tx8power"), Some(&json!("N/A")));
    }

    // --- posters (tests/test_xcvrd.py:test_post_port_dom_sensor_info_to_db) ------

    #[test]
    fn test_post_port_dom_sensor_info_to_db() {
        let logical_port_name = "Ethernet0";
        let pm = mapping_with(&[(logical_port_name, 0)]);
        let dom_tbl = MockDbTable::new("TRANSCEIVER_DOM_SENSOR");
        let dom = DomDbUtils;

        // Absent transceiver -> nothing posted.
        let hal_absent = MockHal::with_sfps(vec![MockSfp::default()]);
        let stop = AtomicBool::new(false);
        dom.post_port_dom_sensor_info_to_db(&stop, logical_port_name, &pm, &dom_tbl, &hal_absent, None);
        assert_eq!(dom_tbl.get_size(), 0);

        // Stop set -> nothing posted (even though present + values available).
        let hal = MockHal::with_sfps(vec![MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
        }]);
        let stop_set = AtomicBool::new(true);
        dom.post_port_dom_sensor_info_to_db(&stop_set, logical_port_name, &pm, &dom_tbl, &hal, None);
        assert_eq!(dom_tbl.get_size(), 0);

        // Empty read -> nothing posted.
        let hal_empty = MockHal::with_sfps(vec![MockSfp {
            dom_real_value: json!({}),
            ..MockSfp::present()
        }]);
        dom.post_port_dom_sensor_info_to_db(&stop, logical_port_name, &pm, &dom_tbl, &hal_empty, None);
        assert_eq!(dom_tbl.get_size(), 0);

        // Valid read -> 26 sensor fields + last_update_time = 27.
        let mut db_cache: DbCache = HashMap::new();
        dom.post_port_dom_sensor_info_to_db(
            &stop,
            logical_port_name,
            &pm,
            &dom_tbl,
            &hal,
            Some(&mut db_cache),
        );
        assert_eq!(dom_tbl.get_size_for_key(logical_port_name), 27);
        assert!(db_cache.get(&0).is_some());

        // A cache hit re-posts the same field set without re-reading the module.
        let hal_none = MockHal::with_sfps(vec![MockSfp::present()]); // would read nothing
        dom.post_port_dom_sensor_info_to_db(
            &stop,
            logical_port_name,
            &pm,
            &dom_tbl,
            &hal_none,
            Some(&mut db_cache),
        );
        assert_eq!(dom_tbl.get_size_for_key(logical_port_name), 27);

        // Unknown asic (port not in mapping) -> skip.
        dom.post_port_dom_sensor_info_to_db(&stop, "Ethernet999", &pm, &dom_tbl, &hal, None);
        assert!(dom_tbl.get("Ethernet999").is_none());
    }

    #[test]
    fn test_post_port_dom_temperature_info_to_db() {
        let logical_port_name = "Ethernet0";
        let pm = mapping_with(&[(logical_port_name, 0)]);
        let tbl = MockDbTable::new("TRANSCEIVER_DOM_TEMPERATURE");
        let dom = DomDbUtils;
        let stop = AtomicBool::new(false);

        // Not present -> nothing.
        let hal_absent = MockHal::with_sfps(vec![MockSfp::default()]);
        dom.post_port_dom_temperature_info_to_db(&stop, logical_port_name, &pm, &tbl, &hal_absent, None);
        assert_eq!(tbl.get_size(), 0);

        // Present + temperature available -> temperature + last_update_time = 2.
        let hal = MockHal::with_sfps(vec![
            MockSfp::present().with_json("get_temperature", json!("68.75")),
        ]);
        let mut db_cache: DbCache = HashMap::new();
        dom.post_port_dom_temperature_info_to_db(
            &stop,
            logical_port_name,
            &pm,
            &tbl,
            &hal,
            Some(&mut db_cache),
        );
        assert_eq!(tbl.get_size_for_key(logical_port_name), 2);
        assert!(db_cache.get(&0).is_some());
    }

    #[test]
    fn test_post_port_dom_thresholds_to_db() {
        let logical_port_name = "Ethernet0";
        let pm = mapping_with(&[(logical_port_name, 0)]);
        let tbl = MockDbTable::new("TRANSCEIVER_DOM_THRESHOLD");
        let dom = DomDbUtils;
        let stop = AtomicBool::new(false);

        let thresholds = json!({
            "temphighalarm": "75.0", "templowalarm": "-5.0",
            "temphighwarning": "72.0", "templowwarning": "-2.0",
            "vcchighalarm": "3.63", "vcclowalarm": "2.97",
            "vcchighwarning": "3.465", "vcclowwarning": "3.135",
            "rxpowerhighalarm": "6.2", "rxpowerlowalarm": "-11.198",
            "rxpowerhighwarning": "4.2", "rxpowerlowwarning": "-9.201",
        });
        let hal = MockHal::with_sfps(vec![MockSfp {
            threshold_info: thresholds,
            ..MockSfp::present()
        }]);
        let mut db_cache: DbCache = HashMap::new();
        dom.post_port_dom_thresholds_to_db(
            &stop,
            logical_port_name,
            &pm,
            &tbl,
            &hal,
            Some(&mut db_cache),
        );
        // 12 threshold fields + last_update_time = 13.
        assert_eq!(tbl.get_size_for_key(logical_port_name), 13);

        // Cache hit re-posts identically.
        let hal_none = MockHal::with_sfps(vec![MockSfp::present()]);
        dom.post_port_dom_thresholds_to_db(
            &stop,
            logical_port_name,
            &pm,
            &tbl,
            &hal_none,
            Some(&mut db_cache),
        );
        assert_eq!(tbl.get_size_for_key(logical_port_name), 13);
    }

    // tests/test_xcvrd.py:test_get_transceiver_dom_flags — the getter returns the
    // module's DOM flag dict on a successful read. Where the Python passthrough would
    // yield `{}` (an empty dict, or a NotImplementedError), the Rust seam yields an
    // empty `Some` or `None`; `post_flags_to_db` treats both as "nothing to post".
    #[test]
    fn test_get_transceiver_dom_flags() {
        let dom = DomUtils;
        // A populated flag dict is returned as-is.
        let sfp =
            MockSfp::present().with_json("get_transceiver_dom_flags", json!({"tempHAlarm": true}));
        assert_eq!(
            dom.get_transceiver_dom_flags(&sfp),
            Some(json!({"tempHAlarm": true}))
        );
        // An empty dict is preserved (Some(empty)).
        let sfp_empty = MockSfp::present().with_json("get_transceiver_dom_flags", json!({}));
        assert_eq!(dom.get_transceiver_dom_flags(&sfp_empty), Some(json!({})));
        // A failed read (no canned result, i.e. the NotImplementedError path) -> None.
        let sfp_err = MockSfp::present();
        assert_eq!(dom.get_transceiver_dom_flags(&sfp_err), None);
    }

    // The module temp/vcc alarm-warning group (CMIS byte 00h:9) is decoded in the
    // PLATFORM (`CmisApi.get_module_level_flag` reads the single latched byte and emits
    // the temperature half (bits 0-3) AND the supply-voltage half (bits 4-7) together in
    // one `dict.update()`); the daemon is a pure delegate and never decodes EEPROM itself.
    // So whatever the platform surfaces is published verbatim: a quiescent module yields
    // the clean both-False temp+vcc baseline test_dom_flag_groups_temp_and_vcc waits on,
    // and a raised byte yields both halves True — they derive from the SAME read and can
    // never be published one-without-the-other.
    #[test]
    fn test_get_transceiver_dom_flags_temp_vcc_group_passthrough() {
        let dom = DomUtils;

        // Quiescent baseline: the platform decodes the whole temp+vcc group as False from
        // byte 00h:9; the delegate publishes every field unchanged.
        let baseline_flags = json!({
            "tempHAlarm": false, "tempLAlarm": false, "tempHWarn": false, "tempLWarn": false,
            "vccHAlarm": false, "vccLAlarm": false, "vccHWarn": false, "vccLWarn": false,
        });
        let sfp_baseline =
            MockSfp::present().with_json("get_transceiver_dom_flags", baseline_flags.clone());
        assert_eq!(
            dom.get_transceiver_dom_flags(&sfp_baseline),
            Some(baseline_flags)
        );

        // Raised (byte 00h:9 bits 0+4): the platform surfaces BOTH tempHAlarm and vccHAlarm
        // True from the one latched read; the delegate passes both through together.
        let raised_flags = json!({
            "tempHAlarm": true, "tempLAlarm": false, "tempHWarn": false, "tempLWarn": false,
            "vccHAlarm": true, "vccLAlarm": false, "vccHWarn": false, "vccLWarn": false,
        });
        let sfp_raised =
            MockSfp::present().with_json("get_transceiver_dom_flags", raised_flags.clone());
        let raised = dom.get_transceiver_dom_flags(&sfp_raised).unwrap();
        assert_eq!(raised["tempHAlarm"], json!(true));
        assert_eq!(raised["vccHAlarm"], json!(true));
        assert_eq!(raised, raised_flags);
    }

    // --- DOM flag poster (post_port_dom_flags_to_db) -----------------------------

    #[test]
    fn test_post_port_dom_flags_to_db() {
        let lport = "Ethernet0";
        let pm = mapping_with(&[(lport, 0)]);
        let flag = MockDbTable::new("TRANSCEIVER_DOM_FLAG");
        let cc = MockDbTable::new("TRANSCEIVER_DOM_FLAG_CHANGE_COUNT");
        let st = MockDbTable::new("TRANSCEIVER_DOM_FLAG_SET_TIME");
        let ct = MockDbTable::new("TRANSCEIVER_DOM_FLAG_CLEAR_TIME");
        let dom = DomDbUtils;
        let stop = AtomicBool::new(false);

        // Absent module -> nothing posted (presence gate).
        let hal_absent = MockHal::with_sfps(vec![MockSfp::default()]);
        dom.post_port_dom_flags_to_db(&stop, lport, &pm, &hal_absent, &flag, &cc, &st, &ct, None);
        assert_eq!(flag.get_size(), 0);

        // First publish (tempHAlarm False): value row posted (bools -> "False"),
        // metadata seeded (count '0', set/clear 'never').
        let hal_false = MockHal::with_sfps(vec![
            MockSfp::present().with_json("get_transceiver_dom_flags", json!({"tempHAlarm": false})),
        ]);
        dom.post_port_dom_flags_to_db(&stop, lport, &pm, &hal_false, &flag, &cc, &st, &ct, None);
        assert_eq!(flag.hget(lport, "tempHAlarm").as_deref(), Some("False"));
        assert!(flag.hget(lport, "last_update_time").is_some());
        assert_eq!(cc.hget(lport, "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(st.hget(lport, "tempHAlarm").as_deref(), Some("never"));
        assert_eq!(ct.hget(lport, "tempHAlarm").as_deref(), Some("never"));

        // Re-publish the SAME value (no-op): change count is not bumped.
        dom.post_port_dom_flags_to_db(&stop, lport, &pm, &hal_false, &flag, &cc, &st, &ct, None);
        assert_eq!(cc.hget(lport, "tempHAlarm").as_deref(), Some("0"));

        // Raise the flag (False -> True): count 0 -> 1, set-time stamped (not 'never').
        let hal_true = MockHal::with_sfps(vec![
            MockSfp::present().with_json("get_transceiver_dom_flags", json!({"tempHAlarm": true})),
        ]);
        dom.post_port_dom_flags_to_db(&stop, lport, &pm, &hal_true, &flag, &cc, &st, &ct, None);
        assert_eq!(flag.hget(lport, "tempHAlarm").as_deref(), Some("True"));
        assert_eq!(cc.hget(lport, "tempHAlarm").as_deref(), Some("1"));
        assert_ne!(st.hget(lport, "tempHAlarm").as_deref(), Some("never"));

        // Clear the flag (True -> False): count 1 -> 2, clear-time stamped.
        dom.post_port_dom_flags_to_db(&stop, lport, &pm, &hal_false, &flag, &cc, &st, &ct, None);
        assert_eq!(cc.hget(lport, "tempHAlarm").as_deref(), Some("2"));
        assert_ne!(ct.hget(lport, "tempHAlarm").as_deref(), Some("never"));

        // Stop set -> nothing posted even when a flag would be read.
        let stop_set = AtomicBool::new(true);
        let flag2 = MockDbTable::new("TRANSCEIVER_DOM_FLAG");
        dom.post_port_dom_flags_to_db(&stop_set, lport, &pm, &hal_true, &flag2, &cc, &st, &ct, None);
        assert_eq!(flag2.get_size(), 0);

        // Unknown asic (port not in mapping) -> skip.
        dom.post_port_dom_flags_to_db(&stop, "Ethernet999", &pm, &hal_true, &flag, &cc, &st, &ct, None);
        assert!(flag.get("Ethernet999").is_none());
    }

    #[test]
    fn test_post_skips_when_asic_index_none() {
        // A logical port present on the HAL but absent from the port map (no asic) is
        // skipped before any read (mirrors the `asic_index is None` guard).
        let pm = PortMapping::new();
        let tbl = MockDbTable::new("TRANSCEIVER_DOM_SENSOR");
        let hal = MockHal::with_sfps(vec![MockSfp {
            dom_real_value: dom_sensor_values(),
            ..MockSfp::present()
        }]);
        let stop = AtomicBool::new(false);
        DomDbUtils.post_port_dom_sensor_info_to_db(&stop, "Ethernet0", &pm, &tbl, &hal, None);
        assert_eq!(tbl.get_size(), 0);
        let _ = stop.load(Ordering::Relaxed);
    }
}
