//! Port of `dom/utilities/db/utils.py` — `DBUtils`: the common poster pattern and
//! the flag-metadata machinery shared by the DOM/status/VDM posters.
//!
//! The Python class holds `sfp_obj_dict`/`port_mapping`/`task_stopping_event` and
//! reads the module through `XCVRDUtils`. In Rust the same context is threaded
//! explicitly through the [`Hal`] seam so the posters stay `&dyn`-mockable: the
//! shared [`DbUtils::post_diagnostic_values_to_db`] validates the port, reads a
//! diagnostic dict from the module (or the per-pass `db_cache`), beautifies it,
//! appends the trailing `last_update_time`, and writes the row.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::db::DbTable;
use crate::hal::{Hal, SfpHandle};
use crate::xcvrd_utilities::port_event_helper::PortMapping;

/// Field/value row as posted to a STATE_DB table (order preserved).
pub type Fvs = Vec<(String, String)>;

/// Per-poll cache keyed by physical port (`db_cache` in `post_diagnostic_values_to_db`):
/// avoids re-reading the same EEPROM once per breakout subport in a single pass.
/// `None` records "read returned nothing" so a cache hit still short-circuits.
pub type DbCache = HashMap<usize, Option<Value>>;

/// Render a JSON scalar the way Python `str(value)` would, for STATE_DB fields.
/// (DOM values are numbers or already-formatted strings — unlike the CMIS identity
/// strings in `xcvrd::stringify`, they are not NUL-padded, so no trimming here.)
pub fn value_to_py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// `_strip_unit(value, unit)` — drop a trailing unit suffix from a string value,
/// otherwise `str(value)`.
pub fn strip_unit(v: &Value, unit: &str) -> String {
    if let Value::String(s) = v {
        if let Some(stripped) = s.strip_suffix(unit) {
            return stripped.to_string();
        }
    }
    value_to_py_str(v)
}

/// `DBUtils` — validate/read/beautify/post + flag-metadata helpers.
pub struct DbUtils;

impl DbUtils {
    pub const NEVER: &'static str = "never";
    pub const NOT_AVAILABLE: &'static str = "N/A";

    pub fn new() -> Self {
        DbUtils
    }

    /// `post_diagnostic_values_to_db` — the shared poster: validate the port, read a
    /// dict (or use `db_cache`), beautify, append `last_update_time`, `set`.
    ///
    /// `get_values_func` reads the diagnostic dict for the resolved module handle;
    /// `beautify_func` mutates the dict in place before it is stringified + posted.
    #[allow(clippy::too_many_arguments)]
    pub fn post_diagnostic_values_to_db<G, B>(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        port_mapping: &PortMapping,
        table: &dyn DbTable,
        hal: &dyn Hal,
        get_values_func: G,
        db_cache: Option<&mut DbCache>,
        beautify_func: B,
        enable_flat_memory_check: bool,
    ) where
        G: FnOnce(&dyn SfpHandle) -> Option<Value>,
        B: FnOnce(&mut Map<String, Value>),
    {
        let physical_port = match self.validate_and_get_physical_port(
            stop,
            logical_port_name,
            port_mapping,
            hal,
            enable_flat_memory_check,
        ) {
            Some(p) => p,
            None => return,
        };

        let sfp = match hal.sfp(physical_port) {
            Ok(s) => s,
            Err(_) => return,
        };

        // Read the diagnostic dict, honoring the per-pass cache (mirrors the Python
        // `db_cache` branch: a hit re-uses the value without touching the EEPROM).
        let diagnostic_values: Option<Value> = match db_cache {
            Some(cache) => {
                if let Some(cached) = cache.get(&physical_port) {
                    cached.clone()
                } else {
                    let read = get_values_func(&*sfp);
                    cache.insert(physical_port, read.clone());
                    read
                }
            }
            None => get_values_func(&*sfp),
        };

        // None / empty dict / non-object => nothing to post (matches Python's
        // `if diagnostic_values_dict is not None: if not diagnostic_values_dict: return`).
        let mut obj = match diagnostic_values {
            Some(Value::Object(o)) if !o.is_empty() => o,
            _ => return,
        };

        beautify_func(&mut obj);

        let mut fvs: Fvs = obj.iter().map(|(k, v)| (k.clone(), value_to_py_str(v))).collect();
        fvs.push(("last_update_time".to_string(), self.get_current_time()));
        table.set(logical_port_name, &fvs);
    }

    /// `_validate_and_get_physical_port` — stop not set → logical maps to a physical
    /// port → the SFP exists → the transceiver is present (→ optionally not flat
    /// memory). Returns the physical port index on success.
    pub fn validate_and_get_physical_port(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        port_mapping: &PortMapping,
        hal: &dyn Hal,
        _enable_flat_memory_check: bool,
    ) -> Option<usize> {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let pport_list = port_mapping.get_logical_to_physical(logical_port_name)?;
        let physical_port = *pport_list.first()?;
        let sfp = hal.sfp(physical_port).ok()?;
        if !sfp.get_presence().unwrap_or(false) {
            return None;
        }
        // `enable_flat_memory_check` gates the PM poster only (M5); the DOM sensor /
        // threshold / temperature posters all pass `false`, so the flat-memory check
        // is intentionally not wired here yet.
        Some(physical_port)
    }

    /// `beautify_info_dict` — stringify non-string values in place (the default
    /// beautifier). Values already strings are left untouched.
    pub fn beautify_info_dict(&self, info_dict: &mut Map<String, Value>) {
        for (_k, v) in info_dict.iter_mut() {
            if !v.is_string() {
                *v = Value::String(value_to_py_str(v));
            }
        }
    }

    /// `get_current_time` — UTC `"%a %b %d %H:%M:%S %Y"` (e.g. `Thu Jan 01 …`).
    pub fn get_current_time(&self) -> String {
        format_utc(SystemTime::now())
    }

    /// Shared flag poster (`post_port_dom_flags_to_db` / `post_port_transceiver_hw_
    /// status_flags_to_db` have byte-identical bodies in Python beyond the reader /
    /// beautifier / tables). Validate the port, read the flag dict off the module
    /// (honoring the per-pass `db_cache`), update the change-count / set-time /
    /// clear-time metadata **before** overwriting the value row, then beautify +
    /// post `<TABLE>|<port>` with the trailing `last_update_time`.
    ///
    /// A `None` read (module raised / returned nothing) posts nothing, matching the
    /// Python `if …_dict is None: return` and the `if not …_dict: return` empty-dict
    /// guard — only a non-empty dict updates metadata and is published.
    #[allow(clippy::too_many_arguments)]
    pub fn post_flags_to_db<G, B>(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        port_mapping: &PortMapping,
        hal: &dyn Hal,
        flag_value_table: &dyn DbTable,
        flag_change_count_table: &dyn DbTable,
        flag_last_set_time_table: &dyn DbTable,
        flag_last_clear_time_table: &dyn DbTable,
        get_flags_func: G,
        beautify_func: B,
        table_name_for_logging: &str,
        db_cache: Option<&mut DbCache>,
    ) where
        G: FnOnce(&dyn SfpHandle) -> Option<Value>,
        B: FnOnce(&mut Map<String, Value>),
    {
        let physical_port = match self.validate_and_get_physical_port(
            stop,
            logical_port_name,
            port_mapping,
            hal,
            false,
        ) {
            Some(p) => p,
            None => return,
        };
        let sfp = match hal.sfp(physical_port) {
            Ok(s) => s,
            Err(_) => return,
        };

        // Read the flag dict once per pass (a cache hit re-uses it without touching
        // the EEPROM); on a fresh read, a non-empty dict updates the metadata tables
        // before the value row is overwritten. `None` short-circuits (nothing posted).
        let read_and_update = |read: &Option<Value>| {
            if let Some(Value::Object(o)) = read {
                if !o.is_empty() {
                    let update_time = self.get_current_time();
                    self.update_flag_metadata_tables(
                        logical_port_name,
                        o,
                        &update_time,
                        flag_value_table,
                        flag_change_count_table,
                        flag_last_set_time_table,
                        flag_last_clear_time_table,
                        table_name_for_logging,
                    );
                }
            }
        };

        let flags: Option<Value> = match db_cache {
            Some(cache) => {
                if let Some(cached) = cache.get(&physical_port) {
                    cached.clone()
                } else {
                    let read = get_flags_func(&*sfp);
                    if read.is_none() {
                        return;
                    }
                    read_and_update(&read);
                    cache.insert(physical_port, read.clone());
                    read
                }
            }
            None => {
                let read = get_flags_func(&*sfp);
                if read.is_none() {
                    return;
                }
                read_and_update(&read);
                read
            }
        };

        let mut obj = match flags {
            Some(Value::Object(o)) if !o.is_empty() => o,
            _ => return,
        };
        beautify_func(&mut obj);
        let mut fvs: Fvs = obj.iter().map(|(k, v)| (k.clone(), value_to_py_str(v))).collect();
        fvs.push(("last_update_time".to_string(), self.get_current_time()));
        flag_value_table.set(logical_port_name, &fvs);
    }

    /// `_update_flag_metadata_tables` — compare the freshly-read flag dict against the
    /// value row already in STATE_DB and maintain the three side tables. On the very
    /// first publish (value row absent) it seeds the metadata (count `0`, times
    /// `never`); afterwards it bumps the per-flag change count and stamps set/clear
    /// time only for flags whose value actually **transitioned**. `"N/A"` values are
    /// skipped (`curr_flag_dict` carries the RAW pre-beautify values).
    #[allow(clippy::too_many_arguments)]
    pub fn update_flag_metadata_tables(
        &self,
        logical_port_name: &str,
        curr_flag_dict: &Map<String, Value>,
        flag_values_dict_update_time: &str,
        flag_value_table: &dyn DbTable,
        flag_change_count_table: &dyn DbTable,
        flag_last_set_time_table: &dyn DbTable,
        flag_last_clear_time_table: &dyn DbTable,
        table_name_for_logging: &str,
    ) {
        // `Table.get` -> None == "row not present" (Python `found == False`).
        let db_flags_value_dict: HashMap<String, String> = match flag_value_table.get(logical_port_name)
        {
            None => {
                self.initialize_metadata_tables(
                    logical_port_name,
                    curr_flag_dict,
                    flag_change_count_table,
                    flag_last_set_time_table,
                    flag_last_clear_time_table,
                );
                return;
            }
            Some(rows) => rows.into_iter().collect(),
        };

        for (flag_key, curr_flag_value) in curr_flag_dict {
            let curr_str = value_to_py_str(curr_flag_value);
            if curr_str.trim() == Self::NOT_AVAILABLE {
                continue; // Skip "N/A" values
            }
            if let Some(db_val) = db_flags_value_dict.get(flag_key) {
                if db_val != &curr_str {
                    self.update_flag_metadata(
                        logical_port_name,
                        flag_key,
                        curr_flag_value,
                        flag_values_dict_update_time,
                        flag_change_count_table,
                        flag_last_set_time_table,
                        flag_last_clear_time_table,
                        table_name_for_logging,
                    );
                }
            }
        }
    }

    /// `_initialize_metadata_tables` — clean-slate seed on the first publish: every
    /// flag key gets change count `0` and set/clear time `never`. Written one field
    /// at a time so the real (merging) `Table.set` accumulates all keys into one row.
    fn initialize_metadata_tables(
        &self,
        logical_port_name: &str,
        curr_flag_dict: &Map<String, Value>,
        flag_change_count_table: &dyn DbTable,
        flag_last_set_time_table: &dyn DbTable,
        flag_last_clear_time_table: &dyn DbTable,
    ) {
        for key in curr_flag_dict.keys() {
            flag_change_count_table.set(logical_port_name, &[(key.clone(), "0".to_string())]);
            flag_last_set_time_table.set(logical_port_name, &[(key.clone(), Self::NEVER.to_string())]);
            flag_last_clear_time_table
                .set(logical_port_name, &[(key.clone(), Self::NEVER.to_string())]);
        }
    }

    /// `_update_flag_metadata` — one transitioned flag: increment its change count and
    /// stamp the set time (flag now truthy) or clear time (flag now falsy).
    #[allow(clippy::too_many_arguments)]
    fn update_flag_metadata(
        &self,
        logical_port_name: &str,
        flag_key: &str,
        curr_flag_value: &Value,
        flag_values_dict_update_time: &str,
        flag_change_count_table: &dyn DbTable,
        flag_last_set_time_table: &dyn DbTable,
        flag_last_clear_time_table: &dyn DbTable,
        table_name_for_logging: &str,
    ) {
        let db_change_count_dict: HashMap<String, String> =
            match flag_change_count_table.get(logical_port_name) {
                None => {
                    eprintln!(
                        "xcvrd-rs: failed to get change count for {table_name_for_logging} port {logical_port_name}"
                    );
                    return;
                }
                Some(rows) => rows.into_iter().collect(),
            };
        let db_change_count = db_change_count_dict
            .get(flag_key)
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        flag_change_count_table
            .set(logical_port_name, &[(flag_key.to_string(), db_change_count.to_string())]);

        if py_truthy(curr_flag_value) {
            flag_last_set_time_table.set(
                logical_port_name,
                &[(flag_key.to_string(), flag_values_dict_update_time.to_string())],
            );
        } else {
            flag_last_clear_time_table.set(
                logical_port_name,
                &[(flag_key.to_string(), flag_values_dict_update_time.to_string())],
            );
        }
    }
}

/// Python truthiness of a JSON value — the set-vs-clear-time decision keys on the RAW
/// flag value (`if curr_flag_value:`). Module DOM/status flags are booleans, so this
/// is normally just the bool; the full rule keeps parity if a reader ever yields a
/// number/string/None.
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

impl Default for DbUtils {
    fn default() -> Self {
        DbUtils::new()
    }
}

const WDAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MON: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Render a `SystemTime` as `datetime.utcnow().strftime("%a %b %d %H:%M:%S %Y")`
/// without pulling in `chrono` (the suite only asserts the field's presence, but the
/// byte-exact format keeps parity with the Python daemon). Uses Howard Hinnant's
/// days→civil algorithm.
fn format_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (hour, min, sec) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (year, month, day) = civil_from_days(days);
    let wday = (days.rem_euclid(7) + 4).rem_euclid(7) as usize; // 1970-01-01 = Thursday
    format!(
        "{} {} {:02} {:02}:{:02}:{:02} {}",
        WDAY[wday],
        MON[(month - 1) as usize],
        day,
        hour,
        min,
        sec,
        year
    )
}

/// Convert a count of days since 1970-01-01 to `(year, month[1-12], day[1-31])`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
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
    use serde_json::json;

    // Port of tests/test_xcvrd.py:test_beautify_info_dict — non-string values are
    // stringified, strings left as-is.
    #[test]
    fn test_beautify_info_dict() {
        let mut dict = json!({"eSNR": 1.1}).as_object().unwrap().clone();
        DbUtils.beautify_info_dict(&mut dict);
        assert_eq!(dict.get("eSNR"), Some(&json!("1.1")));
    }

    // The UTC timestamp renders in the exact strftime shape the tables carry.
    #[test]
    fn test_get_current_time_format() {
        // Epoch -> the reference wall string.
        assert_eq!(format_utc(UNIX_EPOCH), "Thu Jan 01 00:00:00 1970");
        // A known later instant (2021-01-01 00:00:00 UTC = 1609459200).
        assert_eq!(
            format_utc(UNIX_EPOCH + std::time::Duration::from_secs(1_609_459_200)),
            "Fri Jan 01 00:00:00 2021"
        );
        // Live value is well-formed and non-empty.
        assert!(!DbUtils.get_current_time().is_empty());
    }

    #[test]
    fn test_value_to_py_str_and_strip_unit() {
        assert_eq!(value_to_py_str(&json!(1.1)), "1.1");
        assert_eq!(value_to_py_str(&json!(true)), "True");
        assert_eq!(value_to_py_str(&json!("N/A")), "N/A");
        assert_eq!(strip_unit(&json!("0C"), "C"), "0");
        assert_eq!(strip_unit(&json!("N/A"), "C"), "N/A");
        assert_eq!(strip_unit(&json!(25.0), "C"), "25.0");
        assert_eq!(strip_unit(&json!("3.3Volts"), "Volts"), "3.3");
    }

    #[test]
    fn test_py_truthy() {
        assert!(py_truthy(&json!(true)));
        assert!(!py_truthy(&json!(false)));
        assert!(!py_truthy(&json!(0)));
        assert!(py_truthy(&json!(1)));
        assert!(!py_truthy(&Value::Null));
    }

    fn single(key: &str, v: Value) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(key.to_string(), v);
        m
    }

    // First publish (value row absent) seeds the metadata: count '0', set/clear
    // 'never'. Mirrors tests/test_xcvrd.py:_initialize_metadata_tables path.
    #[test]
    fn test_update_flag_metadata_tables_first_publish_initializes() {
        use crate::mock::MockDbTable;
        let value = MockDbTable::new("TRANSCEIVER_DOM_FLAG");
        let count = MockDbTable::new("TRANSCEIVER_DOM_FLAG_CHANGE_COUNT");
        let set_time = MockDbTable::new("TRANSCEIVER_DOM_FLAG_SET_TIME");
        let clear_time = MockDbTable::new("TRANSCEIVER_DOM_FLAG_CLEAR_TIME");
        let flags = single("tempHAlarm", json!(false));

        DbUtils.update_flag_metadata_tables(
            "Ethernet0",
            &flags,
            "Thu Jan 01 00:00:00 1970",
            &value,
            &count,
            &set_time,
            &clear_time,
            "DOM flags",
        );
        assert_eq!(count.hget("Ethernet0", "tempHAlarm").as_deref(), Some("0"));
        assert_eq!(set_time.hget("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
        assert_eq!(clear_time.hget("Ethernet0", "tempHAlarm").as_deref(), Some("never"));
    }

    // A real transition bumps the change count and stamps set-time (raise) /
    // clear-time (clear); an unchanged re-publish (no-op) does neither.
    #[test]
    fn test_update_flag_metadata_tables_transition_and_noop() {
        use crate::mock::MockDbTable;
        let value = MockDbTable::new("TRANSCEIVER_DOM_FLAG");
        let count = MockDbTable::new("TRANSCEIVER_DOM_FLAG_CHANGE_COUNT");
        let set_time = MockDbTable::new("TRANSCEIVER_DOM_FLAG_SET_TIME");
        let clear_time = MockDbTable::new("TRANSCEIVER_DOM_FLAG_CLEAR_TIME");

        // Seed the value row + metadata as a first publish (flag False).
        value.set("Ethernet0", &[("tempHAlarm".to_string(), "False".to_string())]);
        count.set("Ethernet0", &[("tempHAlarm".to_string(), "0".to_string())]);

        // Raise: False -> True is a transition. count 0 -> 1, set-time stamped.
        DbUtils.update_flag_metadata_tables(
            "Ethernet0",
            &single("tempHAlarm", json!(true)),
            "RAISE_TIME",
            &value,
            &count,
            &set_time,
            &clear_time,
            "DOM flags",
        );
        assert_eq!(count.hget("Ethernet0", "tempHAlarm").as_deref(), Some("1"));
        assert_eq!(set_time.hget("Ethernet0", "tempHAlarm").as_deref(), Some("RAISE_TIME"));
        assert!(clear_time.hget("Ethernet0", "tempHAlarm").is_none());

        // Value row now reflects the raised flag (as the poster would write it).
        value.set("Ethernet0", &[("tempHAlarm".to_string(), "True".to_string())]);

        // No-op: True -> True. count stays 1, set-time unchanged.
        DbUtils.update_flag_metadata_tables(
            "Ethernet0",
            &single("tempHAlarm", json!(true)),
            "NOOP_TIME",
            &value,
            &count,
            &set_time,
            &clear_time,
            "DOM flags",
        );
        assert_eq!(count.hget("Ethernet0", "tempHAlarm").as_deref(), Some("1"));
        assert_eq!(set_time.hget("Ethernet0", "tempHAlarm").as_deref(), Some("RAISE_TIME"));

        // Clear: True -> False. count 1 -> 2, clear-time stamped.
        DbUtils.update_flag_metadata_tables(
            "Ethernet0",
            &single("tempHAlarm", json!(false)),
            "CLEAR_TIME",
            &value,
            &count,
            &set_time,
            &clear_time,
            "DOM flags",
        );
        assert_eq!(count.hget("Ethernet0", "tempHAlarm").as_deref(), Some("2"));
        assert_eq!(clear_time.hget("Ethernet0", "tempHAlarm").as_deref(), Some("CLEAR_TIME"));
    }

    // An "N/A" flag value is skipped entirely (no change count / time change).
    #[test]
    fn test_update_flag_metadata_tables_skips_na() {
        use crate::mock::MockDbTable;
        let value = MockDbTable::new("TRANSCEIVER_DOM_FLAG");
        let count = MockDbTable::new("TRANSCEIVER_DOM_FLAG_CHANGE_COUNT");
        let set_time = MockDbTable::new("TRANSCEIVER_DOM_FLAG_SET_TIME");
        let clear_time = MockDbTable::new("TRANSCEIVER_DOM_FLAG_CLEAR_TIME");
        value.set("Ethernet0", &[("tempHAlarm".to_string(), "False".to_string())]);
        count.set("Ethernet0", &[("tempHAlarm".to_string(), "5".to_string())]);

        DbUtils.update_flag_metadata_tables(
            "Ethernet0",
            &single("tempHAlarm", json!("N/A")),
            "T",
            &value,
            &count,
            &set_time,
            &clear_time,
            "DOM flags",
        );
        // Unchanged: "N/A" is skipped before the diff.
        assert_eq!(count.hget("Ethernet0", "tempHAlarm").as_deref(), Some("5"));
        assert!(set_time.hget("Ethernet0", "tempHAlarm").is_none());
        assert!(clear_time.hget("Ethernet0", "tempHAlarm").is_none());
    }
}
