//! Generic diagnostic DB writer — port of `dom/utilities/db/utils.py` (`DBUtils`).
//!
//! `post_diagnostic_values_to_db` is the shared write path: take a `{field:value}`
//! dict read from the SFP, beautify (stringify / strip units), append
//! `last_update_time`, then `table.set`. Flag tables additionally maintain
//! change-count / set-time / clear-time metadata (`_update_flag_metadata_tables`,
//! left for a later milestone).
//!
//! Design note: beautification runs on the raw `serde_json::Value` object so it
//! matches the Python semantics exactly — a string value that ends in its unit is
//! trimmed, everything else is rendered with `str()`. The generic writer therefore
//! takes the raw `Value` plus a `BeautifyFn` that produces the final STATE_DB `Row`.

#![allow(dead_code, unused_variables)]

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::statedb::{DbError, Row, TableApi};
use crate::xcvrd_utilities::common::pybool;

/// A beautifier: turn a raw diagnostic dict (`serde_json` object) into the final
/// STATE_DB `Row`. Mirrors the Python `beautify_func` parameter — the default is
/// [`DbUtils::beautify_info_dict`]; DOM uses `DomDbUtils::beautify_dom_info_dict`.
pub type BeautifyFn = fn(&Map<String, Value>) -> Row;

/// `str(value)` as CPython renders it for the scalar types the platform bridge
/// yields. Strings pass through unchanged (Python `str(str)` is identity — note
/// this does NOT trim, unlike the CMIS-identity `stringify_field`); bools become
/// `True`/`False`; numbers use their canonical rendering; `null` -> `None`.
pub fn python_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => pybool(*b).to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `DBUtils` (`db/utils.py:5`): base diagnostic writer + flag-metadata engine.
pub struct DbUtils;

impl DbUtils {
    /// `NEVER` sentinel used by the flag-metadata tables.
    pub const NEVER: &'static str = "never";
    /// `NOT_AVAILABLE` sentinel (`N/A`).
    pub const NOT_AVAILABLE: &'static str = "N/A";

    /// `get_current_time` — `%a %b %d %H:%M:%S %Y` in UTC (the `last_update_time`
    /// field). The oracle drops this volatile field, so this is implemented with
    /// `std::time` alone (no extra crate) rather than `chrono`.
    pub fn get_current_time() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format_utc(secs)
    }

    /// `beautify_info_dict` — the default beautifier: render every value with
    /// `str()` (non-`str` -> `str`). No unit stripping.
    pub fn beautify_info_dict(info: &Map<String, Value>) -> Row {
        let mut row = Row::new();
        for (k, v) in info {
            row.insert(k.clone(), python_str(v));
        }
        row
    }

    /// `post_diagnostic_values_to_db` — beautify `values`, append
    /// `last_update_time`, and `table.set(logical_port_name, ...)`. Returns
    /// `true` iff a row was written (skipped when the dict is `None`/empty, like
    /// the Python early-returns). The caller has already resolved the SFP and the
    /// target table (presence/asic gating happens there).
    pub fn post_diagnostic_values_to_db<T: TableApi>(
        logical_port_name: &str,
        table: &T,
        values: &Value,
        beautify: BeautifyFn,
    ) -> Result<bool, DbError> {
        // Python: `if diagnostic_values_dict is not None: if not <dict>: return`.
        // None / non-object / empty object -> nothing published.
        let obj = match values.as_object() {
            Some(o) if !o.is_empty() => o,
            _ => return Ok(false),
        };

        let mut row = beautify(obj);
        row.insert("last_update_time".to_string(), Self::get_current_time());
        table.set(logical_port_name, &row)?;
        Ok(true)
    }
}

// --- self-contained UTC time formatting -----------------------------------

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Render `secs` (Unix epoch seconds, UTC) as `%a %b %d %H:%M:%S %Y`
/// (e.g. `Wed Jul 22 18:37:25 2026`), matching Python's default `time_format`.
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;

    // 1970-01-01 was a Thursday (index 4 with 0 = Sunday).
    let weekday = ((days % 7 + 7) % 7 + 4) % 7;

    let (year, month, day) = civil_from_days(days);

    format!(
        "{} {} {:02} {:02}:{:02}:{:02} {}",
        WEEKDAYS[weekday as usize],
        MONTHS[(month - 1) as usize],
        day,
        hour,
        minute,
        second,
        year,
    )
}

/// Convert a day count since the Unix epoch into a `(year, month, day)` civil
/// date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockStateDb;
    use crate::statedb::StateDb;
    use serde_json::json;

    /// <- test_beautify_info_dict: non-`str` -> `str()` (`1.1` -> `"1.1"`).
    #[test]
    fn beautify_info_dict_stringifies_non_str() {
        let obj = json!({"eSNR": 1.1, "note": "ok", "flag": true});
        let row = DbUtils::beautify_info_dict(obj.as_object().unwrap());
        assert_eq!(row.get("eSNR").map(String::as_str), Some("1.1"));
        assert_eq!(row.get("note").map(String::as_str), Some("ok"));
        assert_eq!(row.get("flag").map(String::as_str), Some("True"));
    }

    /// Populated dict is written with `last_update_time` appended.
    #[test]
    fn post_diagnostic_writes_row_with_timestamp() {
        let db = MockStateDb::new();
        let tbl = db.table("TRANSCEIVER_PM").unwrap();
        let vals = json!({"a": 1, "b": "x"});
        let wrote =
            DbUtils::post_diagnostic_values_to_db("Ethernet0", &tbl, &vals, DbUtils::beautify_info_dict)
                .unwrap();
        assert!(wrote);
        let r = tbl.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.get("a").map(String::as_str), Some("1"));
        assert_eq!(r.get("b").map(String::as_str), Some("x"));
        assert!(r.contains_key("last_update_time"));
        // 2 fields + last_update_time.
        assert_eq!(r.len(), 3);
    }

    /// `None`/empty dict -> nothing published (Python early-return).
    #[test]
    fn post_diagnostic_skips_empty_and_null() {
        let db = MockStateDb::new();
        let tbl = db.table("TRANSCEIVER_PM").unwrap();
        assert!(!DbUtils::post_diagnostic_values_to_db(
            "Ethernet0",
            &tbl,
            &json!({}),
            DbUtils::beautify_info_dict
        )
        .unwrap());
        assert!(!DbUtils::post_diagnostic_values_to_db(
            "Ethernet0",
            &tbl,
            &Value::Null,
            DbUtils::beautify_info_dict
        )
        .unwrap());
        assert!(tbl.get("Ethernet0").unwrap().is_none());
    }

    #[test]
    fn get_current_time_is_formatted() {
        // Fixed reference instants exercise the civil-date + weekday math
        // deterministically (the field itself is dropped by the oracle).
        assert_eq!(format_utc(1_609_459_200), "Fri Jan 01 00:00:00 2021");
        assert_eq!(format_utc(1_784_745_445), "Wed Jul 22 18:37:25 2026");
        assert_eq!(format_utc(0), "Thu Jan 01 00:00:00 1970");
        assert_eq!(DbUtils::get_current_time().len(), "Thu Jan 01 00:00:00 1970".len());
    }
}
