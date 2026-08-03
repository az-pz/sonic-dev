//! DOM sensor DB posters — port of `dom/utilities/dom_sensor/db_utils.py`
//! (`DOMDBUtils`). Posts `TRANSCEIVER_DOM_SENSOR` / `_THRESHOLD` / `_FLAG` and
//! owns the DOM `_beautify_dom_info_dict` unit-strip.

#![allow(dead_code, unused_variables)]

use serde_json::{Map, Value};

use crate::dom::utilities::db::utils::{python_str, DbUtils};
use crate::dom::utilities::dom_sensor::utils::DomUtils;
use crate::hal::SfpApi;
use crate::statedb::{DbError, Row, TableApi};
use crate::xcvrd_utilities::common::wrapper_get_presence;

const TEMP_UNIT: &str = "C";
const VOLT_UNIT: &str = "Volts";
const POWER_UNIT: &str = "dBm";
const BIAS_UNIT: &str = "mA";

/// `DOMDBUtils` (`dom_sensor/db_utils.py:7`).
pub struct DomDbUtils;

/// `_strip_unit`: if `v` is a string ending in `unit`, drop the suffix; otherwise
/// render it with `str()` (Python `_strip_unit`).
fn strip_unit(v: &Value, unit: &str) -> String {
    if let Value::String(s) = v {
        if let Some(stripped) = s.strip_suffix(unit) {
            return stripped.to_string();
        }
    }
    python_str(v)
}

/// `re.match('^(tx|rx)[1-8]<suffix>$', k)` without a regex dependency.
fn is_txrx(k: &str, suffix: &str) -> bool {
    if k.len() != 3 + suffix.len() {
        return false;
    }
    let prefix = &k[0..2];
    if prefix != "tx" && prefix != "rx" {
        return false;
    }
    let digit = k.as_bytes()[2];
    if !(b'1'..=b'8').contains(&digit) {
        return false;
    }
    &k[3..] == suffix
}

impl DomDbUtils {
    /// `_beautify_dom_info_dict`: strip units — `temperature` C, `voltage` Volts,
    /// `(tx|rx)[1-8]power` dBm, `(tx|rx)[1-8]bias` mA; every other value -> `str()`.
    /// Operates on the raw DOM dict and yields the STATE_DB `Row`.
    pub fn beautify_dom_info_dict(dom: &Map<String, Value>) -> Row {
        let mut row = Row::new();
        for (k, v) in dom {
            let s = if k == "temperature" {
                strip_unit(v, TEMP_UNIT)
            } else if k == "voltage" {
                strip_unit(v, VOLT_UNIT)
            } else if is_txrx(k, "power") {
                strip_unit(v, POWER_UNIT)
            } else if is_txrx(k, "bias") {
                strip_unit(v, BIAS_UNIT)
            } else {
                python_str(v)
            };
            row.insert(k.clone(), s);
        }
        row
    }

    /// `post_port_dom_sensor_info_to_db` -> `TRANSCEIVER_DOM_SENSOR`. Skips absent
    /// modules and empty reads; returns `true` iff a row was written. [M2]
    pub fn post_port_dom_sensor_info_to_db<S: SfpApi, T: TableApi>(
        logical_port_name: &str,
        sfp: &S,
        dom_tbl: &T,
    ) -> Result<bool, DbError> {
        if !wrapper_get_presence(sfp) {
            return Ok(false);
        }
        let values = DomUtils::get_transceiver_dom_sensor_real_value(sfp);
        DbUtils::post_diagnostic_values_to_db(
            logical_port_name,
            dom_tbl,
            &values,
            Self::beautify_dom_info_dict,
        )
    }

    /// `post_port_dom_thresholds_to_db` -> `TRANSCEIVER_DOM_THRESHOLD`. [M2]
    pub fn post_port_dom_thresholds_to_db<S: SfpApi, T: TableApi>(
        logical_port_name: &str,
        sfp: &S,
        threshold_tbl: &T,
    ) -> Result<bool, DbError> {
        if !wrapper_get_presence(sfp) {
            return Ok(false);
        }
        let values = DomUtils::get_transceiver_dom_thresholds(sfp);
        DbUtils::post_diagnostic_values_to_db(
            logical_port_name,
            threshold_tbl,
            &values,
            Self::beautify_dom_info_dict,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockSfp, MockStateDb};
    use crate::statedb::StateDb;
    use crate::xcvrd_utilities::xcvr_table_helper::{
        TRANSCEIVER_DOM_SENSOR_TABLE, TRANSCEIVER_DOM_THRESHOLD_TABLE,
    };
    use serde_json::json;

    fn dom_values() -> Value {
        // 26 fields (temperature, voltage, rx1-8power, tx1-8bias, tx1-8power).
        json!({
            "temperature": "22.75", "voltage": "0.5",
            "rx1power": "0.7", "rx2power": "0.7", "rx3power": "0.7", "rx4power": "0.7",
            "rx5power": "0.7", "rx6power": "0.7", "rx7power": "0.7", "rx8power": "0.7",
            "tx1bias": "0.7", "tx2bias": "0.7", "tx3bias": "0.7", "tx4bias": "0.7",
            "tx5bias": "0.7", "tx6bias": "0.7", "tx7bias": "0.7", "tx8bias": "0.7",
            "tx1power": "0.7", "tx2power": "0.7", "tx3power": "0.7", "tx4power": "0.7",
            "tx5power": "0.7", "tx6power": "0.7", "tx7power": "0.7", "tx8power": "0.7"
        })
    }

    /// <- test_beautify_dom_info_dict: temperature unit stripped, non-str -> str.
    #[test]
    fn beautify_dom_info_dict_strips_units() {
        let obj = json!({"temperature": "0C", "eSNR": 1.1});
        let row = DomDbUtils::beautify_dom_info_dict(obj.as_object().unwrap());
        assert_eq!(row.get("temperature").map(String::as_str), Some("0"));
        assert_eq!(row.get("eSNR").map(String::as_str), Some("1.1"));
    }

    /// Every unit class is trimmed (voltage/power/bias), numbers pass through.
    #[test]
    fn beautify_dom_info_dict_all_unit_classes() {
        let obj = json!({
            "temperature": "30.5C", "voltage": "3.3Volts",
            "rx1power": "-2.1dBm", "tx8bias": "6.5mA",
            "temperature_float": 30.5, "notaunit": "12.3"
        });
        let row = DomDbUtils::beautify_dom_info_dict(obj.as_object().unwrap());
        assert_eq!(row.get("temperature").map(String::as_str), Some("30.5"));
        assert_eq!(row.get("voltage").map(String::as_str), Some("3.3"));
        assert_eq!(row.get("rx1power").map(String::as_str), Some("-2.1"));
        assert_eq!(row.get("tx8bias").map(String::as_str), Some("6.5"));
        // Not one of the special keys -> str() unchanged.
        assert_eq!(row.get("temperature_float").map(String::as_str), Some("30.5"));
        assert_eq!(row.get("notaunit").map(String::as_str), Some("12.3"));
    }

    /// `txNpower` where the value has no unit suffix is left as-is (str()).
    #[test]
    fn beautify_dom_numeric_power_without_unit() {
        let obj = json!({"tx1power": 0.7, "rx8power": "0.7"});
        let row = DomDbUtils::beautify_dom_info_dict(obj.as_object().unwrap());
        assert_eq!(row.get("tx1power").map(String::as_str), Some("0.7"));
        assert_eq!(row.get("rx8power").map(String::as_str), Some("0.7"));
    }

    /// <- test_post_port_dom_sensor_info_to_db: absent -> nothing; present + valid
    /// -> 26 fields + last_update_time == 27; empty read leaves the prior row.
    #[test]
    fn post_dom_sensor_absent_present_and_empty() {
        let db = MockStateDb::new();
        let dom_tbl = db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap();

        // Absent -> skip.
        let mut sfp = MockSfp::default();
        sfp.presence = false;
        sfp.dom_real_value = Some(dom_values());
        assert!(!DomDbUtils::post_port_dom_sensor_info_to_db("Ethernet0", &sfp, &dom_tbl).unwrap());
        assert!(dom_tbl.get("Ethernet0").unwrap().is_none());

        // Present + valid -> 27 fields (26 + last_update_time).
        sfp.presence = true;
        assert!(DomDbUtils::post_port_dom_sensor_info_to_db("Ethernet0", &sfp, &dom_tbl).unwrap());
        let r = dom_tbl.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.len(), 27);
        assert_eq!(r.get("temperature").map(String::as_str), Some("22.75"));
        assert_eq!(r.get("voltage").map(String::as_str), Some("0.5"));

        // Present but empty read -> no write (prior row preserved, Python skip).
        sfp.dom_real_value = Some(json!({}));
        assert!(!DomDbUtils::post_port_dom_sensor_info_to_db("Ethernet0", &sfp, &dom_tbl).unwrap());
        assert_eq!(dom_tbl.get("Ethernet0").unwrap().unwrap().len(), 27);
    }

    /// <- test_post_port_dom_thresholds_to_db: present + 12 thresholds -> 13 fields.
    #[test]
    fn post_dom_thresholds_present() {
        let db = MockStateDb::new();
        let thr_tbl = db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE).unwrap();
        let mut sfp = MockSfp::default();
        sfp.presence = true;
        sfp.threshold_info = Some(json!({
            "temphighalarm": "75.0", "templowalarm": "-5.0",
            "temphighwarning": "72.0", "templowwarning": "-2.0",
            "vcchighalarm": "3.63", "vcclowalarm": "2.97",
            "vcchighwarning": "3.465", "vcclowwarning": "3.135",
            "rxpowerhighalarm": "6.2", "rxpowerlowalarm": "-11.198",
            "rxpowerhighwarning": "4.2", "rxpowerlowwarning": "-9.201"
        }));
        assert!(DomDbUtils::post_port_dom_thresholds_to_db("Ethernet0", &sfp, &thr_tbl).unwrap());
        let r = thr_tbl.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.len(), 13);
        assert_eq!(r.get("temphighalarm").map(String::as_str), Some("75.0"));
    }

    /// <- test_post_port_dom_temperature_info_to_db (via the generic path): a
    /// module-temperature-only read yields a single field + last_update_time.
    #[test]
    fn post_dom_temperature_single_field() {
        let db = MockStateDb::new();
        let dom_tbl = db.table("TRANSCEIVER_DOM_TEMPERATURE").unwrap();
        let mut sfp = MockSfp::default();
        sfp.presence = true;
        sfp.dom_real_value = Some(json!({"temperature": "68.75C", "voltage": "3.3Volts"}));
        let vals = DomUtils::get_transceiver_dom_temperature(&sfp);
        assert!(DbUtils::post_diagnostic_values_to_db(
            "Ethernet0",
            &dom_tbl,
            &vals,
            DomDbUtils::beautify_dom_info_dict
        )
        .unwrap());
        let r = dom_tbl.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.len(), 2); // temperature + last_update_time
        assert_eq!(r.get("temperature").map(String::as_str), Some("68.75"));
    }
}
