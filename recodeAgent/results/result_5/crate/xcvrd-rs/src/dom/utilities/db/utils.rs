#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `dom/utilities/db/utils.py`: DBUtils base (diagnostic writes, flag-metadata trio, beautify, get_current_time).
//!
//! `get_current_time` renders the exact UTC
//! `strftime("%a %b %d %H:%M:%S %Y")` the `last_update_time` field carries and
//! `beautify_info_dict` performs the `str(value)` rendering of non-string values.
//! The `chrono` crate is intentionally avoided (no new deps): the
//! UTC calendar conversion is a small, testable std-only routine.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::db::Table;

/// `"N/A"` / `"never"` sentinels (`DBUtils.NOT_AVAILABLE` / `NEVER`).
pub const NOT_AVAILABLE: &str = "N/A";
pub const NEVER: &str = "never";

/// The logger seam the DOM utilities take (`self.logger`), mirroring the Python
/// `syslogger`/`helper_logger` argument. Only the levels the ported code uses are
/// declared; all default to no-ops so the deployed daemon can pass a silent sink.
pub trait DomLogger {
    fn log_error(&self, _msg: &str) {}
    fn log_warning(&self, _msg: &str) {}
    fn log_notice(&self, _msg: &str) {}
    fn log_info(&self, _msg: &str) {}
    fn log_debug(&self, _msg: &str) {}
}

/// A silent logger for the deployed daemon path.
#[derive(Default)]
pub struct NoopDomLogger;
impl DomLogger for NoopDomLogger {}

/// Render a JSON value as Python's `str(value)` would, the way the diagnostic
/// writers stringify field values before posting to STATE_DB.
///
/// Strings are used verbatim (no NUL trimming here — DOM values are engineering
/// numbers/`N/A`, not fixed-width CMIS identity strings); booleans render
/// `True`/`False`; numbers reuse serde's shortest round-trip form (matching
/// `str(float)`/`str(int)`); `null` → `None`.
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True".to_string() } else { "False".to_string() },
        Value::Number(n) => n.to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// Python truthiness of a JSON value (`if value:`): `null`/`false`/`0`/`""`/`[]`/`{}`
/// are falsy, everything else truthy. Used to decide whether a DOM flag is *raised*
/// (`if curr_flag_value:` in `_update_flag_metadata`) — the flags decode to Python
/// bools, so this is `Value::Bool(b) -> b` in practice, with the other arms matching
/// Python for defensive parity.
pub fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// One edge transition of a DOM flag, produced by [`compute_flag_metadata_plan`]:
/// the flag `key` changed value and `raised` says whether the new value is set
/// (→ stamp `*_SET_TIME`) or cleared (→ stamp `*_CLEAR_TIME`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagEdge {
    pub key: String,
    pub raised: bool,
}

/// The metadata mutation implied by comparing the just-read flag dict against the
/// values currently in STATE_DB — the pure core of `_update_flag_metadata_tables`
/// (db/utils.py:107). Either the flag value row does not exist yet (`initialize` →
/// seed every key to `0`/`never`/`never`) or a set of `edges` fired (each bumps
/// that flag's change count and stamps a set/clear time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagMetadataPlan {
    pub initialize: bool,
    pub edges: Vec<FlagEdge>,
}

/// `_update_flag_metadata_tables` decision logic, pure and DB-agnostic so it is
/// unit-testable without STATE_DB.
///
/// `prev_flag_values` is the beautified flag row currently in STATE_DB
/// (`flag_value_table.get(port)`), keyed by flag name → `"True"`/`"False"` (it may
/// also carry `last_update_time`, which is ignored). An **empty** map means the row
/// was not found → first publish → `initialize`. Otherwise, for each current flag
/// whose stringified value differs from what STATE_DB holds (skipping `"N/A"`), an
/// edge is recorded; `raised` reflects the flag's Python truthiness.
pub fn compute_flag_metadata_plan(
    prev_flag_values: &HashMap<String, String>,
    curr_flags: &Map<String, Value>,
) -> FlagMetadataPlan {
    if prev_flag_values.is_empty() {
        return FlagMetadataPlan { initialize: true, edges: Vec::new() };
    }
    let mut edges = Vec::new();
    for (key, val) in curr_flags {
        // Skip "N/A" values (str(curr_flag_value).strip() == NOT_AVAILABLE).
        if py_str(val).trim() == NOT_AVAILABLE {
            continue;
        }
        if let Some(prev) = prev_flag_values.get(key) {
            if *prev != py_str(val) {
                edges.push(FlagEdge { key: key.clone(), raised: py_truthy(val) });
            }
        }
    }
    FlagMetadataPlan { initialize: false, edges }
}

/// Read a [`Table`] row into an owned `String` map (`{field -> value}`), the
/// analogue of `dict(table.get(port)[1])`. An absent row is an empty map.
fn table_row_strings(table: &dyn Table, logical_port_name: &str) -> HashMap<String, String> {
    match table.get(logical_port_name) {
        Ok(Some(fvs)) => fvs.into_iter().collect(),
        _ => HashMap::new(),
    }
}

/// `DBUtils._update_flag_metadata_tables` (db/utils.py:107) over the [`Table`]
/// seam: compare the freshly-read `curr_flags` against the flag values currently
/// in `flag_value_table` and maintain the change-tracking metadata trio.
///
/// On the **first** publish (the flag-value row does not exist yet) every flag key
/// is seeded (`_initialize_metadata_tables`): change count `0`, set/clear time
/// `never`. On a subsequent publish, each flag whose stringified value changed
/// (skipping `"N/A"`) bumps that flag's cumulative change count and stamps its
/// set-time (raised) or clear-time (cleared). This mirrors the daemon's inline
/// `update_flag_metadata_tables` but writes through the mockable `Table` trait so
/// unit tests can assert the metadata without STATE_DB.
#[allow(clippy::too_many_arguments)]
pub fn update_flag_metadata_tables(
    logical_port_name: &str,
    curr_flags: &Map<String, Value>,
    flag_values_dict_update_time: &str,
    flag_value_table: &dyn Table,
    flag_change_count_table: &dyn Table,
    flag_last_set_time_table: &dyn Table,
    flag_last_clear_time_table: &dyn Table,
    table_name_for_logging: &str,
    logger: &dyn DomLogger,
) {
    let prev = table_row_strings(flag_value_table, logical_port_name);
    let plan = compute_flag_metadata_plan(&prev, curr_flags);

    if plan.initialize {
        // `_initialize_metadata_tables`: seed every current flag key.
        for key in curr_flags.keys() {
            let _ = flag_change_count_table.set(logical_port_name, &[(key.clone(), "0".to_string())]);
            let _ = flag_last_set_time_table.set(logical_port_name, &[(key.clone(), NEVER.to_string())]);
            let _ = flag_last_clear_time_table.set(logical_port_name, &[(key.clone(), NEVER.to_string())]);
        }
        return;
    }

    if plan.edges.is_empty() {
        return;
    }

    // `_update_flag_metadata` per changed flag: read the cumulative change count,
    // bump it, and stamp the set/clear time.
    let counts = table_row_strings(flag_change_count_table, logical_port_name);
    if flag_change_count_table.get(logical_port_name).ok().flatten().is_none() {
        logger.log_warning(&format!(
            "Failed to get the change count for table {table_name_for_logging} port {logical_port_name}"
        ));
        return;
    }
    for edge in &plan.edges {
        let next = counts.get(&edge.key).and_then(|c| c.parse::<i64>().ok()).unwrap_or(0) + 1;
        let _ = flag_change_count_table.set(logical_port_name, &[(edge.key.clone(), next.to_string())]);
        if edge.raised {
            let _ = flag_last_set_time_table
                .set(logical_port_name, &[(edge.key.clone(), flag_values_dict_update_time.to_string())]);
        } else {
            let _ = flag_last_clear_time_table
                .set(logical_port_name, &[(edge.key.clone(), flag_values_dict_update_time.to_string())]);
        }
    }
}


/// value in place (`if not isinstance(v, str): info_dict[k] = str(v)`).
pub fn beautify_info_dict(info_dict: &mut Map<String, Value>) {
    for (_k, v) in info_dict.iter_mut() {
        if !v.is_string() {
            let s = py_str(v);
            *v = Value::String(s);
        }
    }
}

/// Beautify a raw diagnostic `Value` into the STATE_DB field list using the base
/// (non-unit-stripping) `beautify_info_dict` — the pure helper the deployed daemon
/// (`daemon.rs`) reuses to build a `TRANSCEIVER_STATUS` row without pulling in the
/// whole `StatusDBUtils` collaborator graph. Returns `None` when the value is not
/// a (non-empty) object, matching the Python `None`/`{}` skips in
/// `post_diagnostic_values_to_db`.
pub fn beautify_info_row(values: &Value) -> Option<Vec<(String, String)>> {
    let obj = values.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut m = obj.clone();
    beautify_info_dict(&mut m);
    Some(m.iter().map(|(k, v)| (k.clone(), py_str(v))).collect())
}

/// `DBUtils.get_current_time` (db/utils.py:161): the current UTC time formatted
/// with `"%a %b %d %H:%M:%S %Y"` (e.g. `Wed Jul 29 18:34:54 2026`).
pub fn get_current_time() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc_strftime(secs)
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The pure kernel of [`get_current_time`]: format a Unix timestamp (UTC) exactly
/// as `strftime("%a %b %d %H:%M:%S %Y")`. Split out so the format is unit-testable
/// against fixed epochs without a clock dependency.
pub fn format_utc_strftime(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // 1970-01-01 was a Thursday; index 4 with Sunday=0.
    let weekday = (((days % 7) + 4) % 7) as usize;
    let (year, month, day) = civil_from_days(days);

    format!(
        "{} {} {:02} {:02}:{:02}:{:02} {}",
        WEEKDAYS[weekday],
        MONTHS[(month - 1) as usize],
        day,
        hour,
        minute,
        second,
        year
    )
}

/// Convert a day count since 1970-01-01 into a `(year, month, day)` civil date
/// (Howard Hinnant's `civil_from_days`, valid across the full proleptic Gregorian
/// range). `month` is 1..=12, `day` is 1..=31.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Rust port of the Python `DBUtils` (beautify + current-time).
/// Carries only the logger seam it needs; the platform/DB collaborators are held
/// by the concrete `DOMDBUtils` subclass.
pub struct DBUtils;

impl DBUtils {
    pub fn new() -> Self {
        DBUtils
    }

    /// `DBUtils.beautify_info_dict` as a method (some callers hold a DBUtils).
    pub fn beautify_info_dict(&self, info_dict: &mut Map<String, Value>) {
        beautify_info_dict(info_dict);
    }

    /// `DBUtils.get_current_time`.
    pub fn get_current_time(&self) -> String {
        get_current_time()
    }
}

impl Default for DBUtils {
    fn default() -> Self {
        DBUtils
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Port of `tests/test_xcvrd.py::test_beautify_info_dict`: a non-string value
    /// is stringified in place, a string value is left untouched.
    #[test]
    fn test_beautify_info_dict() {
        let mut dom_info_dict = json!({ "eSNR": 1.1 }).as_object().unwrap().clone();
        beautify_info_dict(&mut dom_info_dict);
        assert_eq!(dom_info_dict.get("eSNR"), Some(&Value::String("1.1".to_string())));

        // A DBUtils instance exposes the same behavior.
        let mut d2 = json!({ "eSNR": 1.1, "vendor": "EMU" }).as_object().unwrap().clone();
        DBUtils::new().beautify_info_dict(&mut d2);
        assert_eq!(d2.get("eSNR"), Some(&Value::String("1.1".to_string())));
        assert_eq!(d2.get("vendor"), Some(&Value::String("EMU".to_string())));
    }

    /// the `last_update_time` stamp reproduces the
    /// exact `strftime("%a %b %d %H:%M:%S %Y")` `test_last_update_time.py` parses.
    #[test]
    fn last_update_time_strftime_format() {
        // 2026-07-29 18:34:54 UTC == 1785350094 (a Wednesday) — the doc example.
        assert_eq!(format_utc_strftime(1_785_350_094), "Wed Jul 29 18:34:54 2026");
        // Epoch itself: 1970-01-01 00:00:00 UTC was a Thursday.
        assert_eq!(format_utc_strftime(0), "Thu Jan 01 00:00:00 1970");
        // A single-digit day is zero-padded (%d): 2000-01-02 03:04:05 UTC.
        assert_eq!(format_utc_strftime(946_782_245), "Sun Jan 02 03:04:05 2000");
        // Leap day: 2024-02-29 12:00:00 UTC (a Thursday).
        assert_eq!(format_utc_strftime(1_709_208_000), "Thu Feb 29 12:00:00 2024");
    }

    /// `get_current_time()` (the live clock) yields a value that round-trips the
    /// same format (structure/length), guarding the wiring, not the instant.
    #[test]
    fn get_current_time_is_well_formed() {
        let now = get_current_time();
        let parts: Vec<&str> = now.split(' ').collect();
        assert_eq!(parts.len(), 5, "unexpected shape: {now:?}");
        assert!(WEEKDAYS.contains(&parts[0]));
        assert!(MONTHS.contains(&parts[1]));
        assert_eq!(parts[2].len(), 2); // %d zero-padded
        assert_eq!(parts[3].len(), 8); // HH:MM:SS
        assert_eq!(parts[4].len(), 4); // %Y
    }

    #[test]
    fn py_str_matches_python_str() {
        assert_eq!(py_str(&json!("hi")), "hi");
        assert_eq!(py_str(&json!(1.1)), "1.1");
        assert_eq!(py_str(&json!(5)), "5");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(false)), "False");
    }

    #[test]
    fn py_truthy_matches_python() {
        assert!(py_truthy(&json!(true)));
        assert!(!py_truthy(&json!(false)));
        assert!(!py_truthy(&Value::Null));
        assert!(!py_truthy(&json!(0)));
        assert!(py_truthy(&json!(1)));
        assert!(!py_truthy(&json!("")));
        assert!(py_truthy(&json!("x")));
    }

    /// first publish (no prior flag row) initializes
    /// the metadata for every flag key; no edges are reported.
    #[test]
    fn flag_metadata_first_publish_initializes() {
        let prev: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let curr = json!({ "tempHAlarm": false, "vccHAlarm": false })
            .as_object()
            .unwrap()
            .clone();
        let plan = compute_flag_metadata_plan(&prev, &curr);
        assert!(plan.initialize);
        assert!(plan.edges.is_empty());
    }

    /// on a subsequent publish only the flags whose
    /// value CHANGED are reported as edges, with `raised` reflecting the new value;
    /// unchanged flags and "N/A" values are skipped.
    #[test]
    fn flag_metadata_edges_on_transition_only() {
        let mut prev = std::collections::HashMap::new();
        prev.insert("tempHAlarm".to_string(), "False".to_string());
        prev.insert("vccHAlarm".to_string(), "True".to_string());
        prev.insert("last_update_time".to_string(), "Wed Jul 29 18:34:54 2026".to_string());
        // tempHAlarm False->True (raised), vccHAlarm True->True (no change),
        // txLOS N/A (skipped).
        let curr = json!({ "tempHAlarm": true, "vccHAlarm": true, "txLOS": "N/A" })
            .as_object()
            .unwrap()
            .clone();
        let plan = compute_flag_metadata_plan(&prev, &curr);
        assert!(!plan.initialize);
        assert_eq!(plan.edges, vec![FlagEdge { key: "tempHAlarm".to_string(), raised: true }]);
    }

    /// a clear transition reports `raised: false`
    /// (→ CLEAR_TIME), and a no-op (identical value) reports no edge.
    #[test]
    fn flag_metadata_clear_and_noop() {
        let mut prev = std::collections::HashMap::new();
        prev.insert("tempHAlarm".to_string(), "True".to_string());
        let cleared = json!({ "tempHAlarm": false }).as_object().unwrap().clone();
        assert_eq!(
            compute_flag_metadata_plan(&prev, &cleared).edges,
            vec![FlagEdge { key: "tempHAlarm".to_string(), raised: false }]
        );

        let mut prev_raised = std::collections::HashMap::new();
        prev_raised.insert("tempHAlarm".to_string(), "True".to_string());
        let same = json!({ "tempHAlarm": true }).as_object().unwrap().clone();
        assert!(compute_flag_metadata_plan(&prev_raised, &same).edges.is_empty());
    }

    /// `beautify_info_row` (the daemon's
    /// `TRANSCEIVER_STATUS` builder) stringifies every value via the base beautify
    /// — no unit stripping — and skips `None`/`{}` like the Python status poster.
    #[test]
    fn beautify_info_row_stringifies_without_unit_strip() {
        // A mix of number / bool / string values: all become their str() form,
        // and a would-be-unit key like "temperature" is NOT stripped here (base
        // beautify only runs in the DOM-specific path).
        let row = beautify_info_row(&json!({
            "temperature": 41.5,
            "cmis_state": "READY",
            "tx_disable": false,
        }))
        .expect("non-empty object beautifies to a row");
        let map: std::collections::HashMap<_, _> = row.into_iter().collect();
        assert_eq!(map.get("temperature").map(String::as_str), Some("41.5"));
        assert_eq!(map.get("cmis_state").map(String::as_str), Some("READY"));
        assert_eq!(map.get("tx_disable").map(String::as_str), Some("False"));

        // None (JSON null) and {} both skip (return None), matching the Python
        // post_diagnostic_values_to_db guards.
        assert!(beautify_info_row(&Value::Null).is_none());
        assert!(beautify_info_row(&json!({})).is_none());
    }

    /// Port of `tests/test_xcvrd.py::test_update_flag_metadata_tables`: the
    /// Table-seam metadata engine. First publish initializes the trio (count `0`,
    /// times `never`); a later raise/clear bumps the count and stamps set/clear
    /// time only for the changed flag.
    #[test]
    fn test_update_flag_metadata_tables() {
        use crate::mock::MockTable;
        let flag_value = MockTable::new();
        let count = MockTable::new();
        let set_time = MockTable::new();
        let clear_time = MockTable::new();
        let logger = NoopDomLogger;

        // First publish: the flag-value row does not exist yet -> initialize.
        let curr = json!({ "tempHAlarm": false, "vccHAlarm": false }).as_object().unwrap().clone();
        update_flag_metadata_tables(
            "Ethernet0", &curr, "Wed Jul 29 18:34:54 2026",
            &flag_value, &count, &set_time, &clear_time, "DOM flags", &logger,
        );
        assert_eq!(count.field("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(count.field("Ethernet0", "vccHAlarm").as_deref(), Some("0"));
        assert_eq!(set_time.field("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        assert_eq!(clear_time.field("Ethernet0", "tempHAlarm").as_deref(), Some("never"));

        // Now the flag-value row exists (simulate the poster having written it).
        flag_value.set("Ethernet0", &[
            ("tempHAlarm".into(), "False".into()),
            ("vccHAlarm".into(), "False".into()),
        ]).unwrap();

        // Raise tempHAlarm: count 0 -> 1, SET_TIME stamped, CLEAR_TIME untouched.
        let raised = json!({ "tempHAlarm": true, "vccHAlarm": false }).as_object().unwrap().clone();
        update_flag_metadata_tables(
            "Ethernet0", &raised, "Wed Jul 29 18:35:00 2026",
            &flag_value, &count, &set_time, &clear_time, "DOM flags", &logger,
        );
        assert_eq!(count.field("Ethernet0", "tempHAlarm").as_deref(), Some("1"));
        assert_eq!(set_time.field("Ethernet0", "tempHAlarm").as_deref(), Some("Wed Jul 29 18:35:00 2026"));
        assert_eq!(clear_time.field("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        // vccHAlarm did not change -> its count stays 0.
        assert_eq!(count.field("Ethernet0", "vccHAlarm").as_deref(), Some("0"));

        // Reflect the new value in the flag-value row, then clear tempHAlarm.
        flag_value.hset("Ethernet0", "tempHAlarm", "True").unwrap();
        let cleared = json!({ "tempHAlarm": false, "vccHAlarm": false }).as_object().unwrap().clone();
        update_flag_metadata_tables(
            "Ethernet0", &cleared, "Wed Jul 29 18:36:00 2026",
            &flag_value, &count, &set_time, &clear_time, "DOM flags", &logger,
        );
        assert_eq!(count.field("Ethernet0", "tempHAlarm").as_deref(), Some("2"));
        assert_eq!(clear_time.field("Ethernet0", "tempHAlarm").as_deref(), Some("Wed Jul 29 18:36:00 2026"));
        // SET_TIME from the earlier raise is preserved.
        assert_eq!(set_time.field("Ethernet0", "tempHAlarm").as_deref(), Some("Wed Jul 29 18:35:00 2026"));
    }

    /// the first write seeds the change count to `0`
    /// and both set/clear times to `never` for EVERY flag key, and a subsequent
    /// no-op / `"N/A"` publish leaves the seeded counts untouched.
    #[test]
    fn flag_metadata_first_write_inits_count_set_clear_time() {
        use crate::mock::MockTable;
        let (flag_value, count, set_time, clear_time) =
            (MockTable::new(), MockTable::new(), MockTable::new(), MockTable::new());
        let logger = NoopDomLogger;

        let curr = json!({ "tempHAlarm": false, "rxLOS": "N/A" }).as_object().unwrap().clone();
        update_flag_metadata_tables(
            "Ethernet0", &curr, "Wed Jul 29 18:34:54 2026",
            &flag_value, &count, &set_time, &clear_time, "DOM flags", &logger,
        );
        for key in ["tempHAlarm", "rxLOS"] {
            assert_eq!(count.field("Ethernet0", key).as_deref(), Some("0"), "count init {key}");
            assert_eq!(set_time.field("Ethernet0", key).as_deref(), Some("never"), "set init {key}");
            assert_eq!(clear_time.field("Ethernet0", key).as_deref(), Some("never"), "clear init {key}");
        }

        // With the flag-value row now present, an unchanged + an "N/A" flag produce
        // no edges -> the seeded counts are untouched.
        flag_value.set("Ethernet0", &[("tempHAlarm".into(), "False".into()), ("rxLOS".into(), "N/A".into())]).unwrap();
        update_flag_metadata_tables(
            "Ethernet0", &curr, "Wed Jul 29 18:35:00 2026",
            &flag_value, &count, &set_time, &clear_time, "DOM flags", &logger,
        );
        assert_eq!(count.field("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(count.field("Ethernet0", "rxLOS").as_deref(), Some("0"));
    }
}
