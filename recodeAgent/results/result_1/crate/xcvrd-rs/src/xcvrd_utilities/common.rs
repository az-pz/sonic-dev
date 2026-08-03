//! Common utilities — port of `xcvrd_utilities/common.py`.
//!
//! Holds the CMIS state enumeration (`CMIS_STATE_*`), the SW-status table helper
//! (`update_port_transceiver_status_table_sw`), the DOM-table delete helper
//! (`del_port_sfp_dom_info_from_db`), and the HAL wrapper fns (`_wrapper_*`) as
//! trait calls. Behavioural bodies are stubs — the Translator fills them.

#![allow(dead_code, unused_variables)]

use crate::hal::SfpApi;
use crate::statedb::{Row, TableApi};

/// Python `NOT_IMPLEMENTED_ERROR = 3` (exit code on unimplemented info/fw path).
pub const NOT_IMPLEMENTED_ERROR: i32 = 3;

/// Default SW-status `error` value (no error). Mirrors `error_descriptions='N/A'`.
pub const NO_ERROR: &str = "N/A";

/// CMIS datapath states (`common.py:22-39`). `as_str` yields the exact STATE_DB
/// string written to `TRANSCEIVER_STATUS_SW.cmis_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmisState {
    Unknown,
    Inserted,
    DpPreInitCheck,
    DpDeinit,
    ApConfigured,
    DpActivation,
    DpInit,
    DpTxOn,
    Ready,
    Removed,
    Failed,
}

impl CmisState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CmisState::Unknown => "UNKNOWN",
            CmisState::Inserted => "INSERTED",
            CmisState::DpPreInitCheck => "DP_PRE_INIT_CHECK",
            CmisState::DpDeinit => "DP_DEINIT",
            CmisState::ApConfigured => "AP_CONFIGURED",
            CmisState::DpActivation => "DP_ACTIVATION",
            CmisState::DpInit => "DP_INIT",
            CmisState::DpTxOn => "DP_TXON",
            CmisState::Ready => "READY",
            CmisState::Removed => "REMOVED",
            CmisState::Failed => "FAILED",
        }
    }

    /// Parse a STATE_DB `cmis_state` string back into the enum (`UNKNOWN` fallback).
    pub fn from_str(s: &str) -> CmisState {
        match s {
            "INSERTED" => CmisState::Inserted,
            "DP_PRE_INIT_CHECK" => CmisState::DpPreInitCheck,
            "DP_DEINIT" => CmisState::DpDeinit,
            "AP_CONFIGURED" => CmisState::ApConfigured,
            "DP_ACTIVATION" => CmisState::DpActivation,
            "DP_INIT" => CmisState::DpInit,
            "DP_TXON" => CmisState::DpTxOn,
            "READY" => CmisState::Ready,
            "REMOVED" => CmisState::Removed,
            "FAILED" => CmisState::Failed,
            _ => CmisState::Unknown,
        }
    }

    /// `CMIS_TERMINAL_STATES` = {FAILED, READY, REMOVED}.
    pub fn is_terminal(&self) -> bool {
        matches!(self, CmisState::Failed | CmisState::Ready | CmisState::Removed)
    }
}

/// `update_port_transceiver_status_table_sw` (`common.py:110`): set the SW
/// `status` (+`error`) fields of `TRANSCEIVER_STATUS_SW` for a logical port.
pub fn update_port_transceiver_status_table_sw<T: TableApi>(
    logical_port_name: &str,
    status_sw_tbl: &T,
    status: &str,
    error_descriptions: &str,
) -> Result<(), crate::statedb::DbError> {
    let mut row = Row::new();
    row.insert("status".to_string(), status.to_string());
    row.insert("error".to_string(), error_descriptions.to_string());
    status_sw_tbl.set(logical_port_name, &row)
}

/// `get_cmis_state_from_state_db` (`common.py:259`): read back `cmis_state`
/// (`hget`), defaulting to `UNKNOWN` when the field/row is absent.
pub fn get_cmis_state_from_state_db<T: TableApi>(
    lport: &str,
    status_sw_tbl: &T,
) -> Result<CmisState, crate::statedb::DbError> {
    Ok(match status_sw_tbl.hget(lport, "cmis_state")? {
        Some(s) => CmisState::from_str(&s),
        None => CmisState::Unknown,
    })
}

/// `del_port_sfp_dom_info_from_db` (`common.py:335`): delete a logical port's row
/// from each of the given tables (INFO/DOM/STATUS/VDM/PM/FW on removal).
pub fn del_port_sfp_dom_info_from_db<T: TableApi>(
    logical_port_name: &str,
    tbls_to_del: &[&T],
) -> Result<(), crate::statedb::DbError> {
    // Single-ASIC/non-ganged testbed: the physical port name equals the logical
    // port name (`get_physical_port_name` returns `logical_port` when not ganged),
    // so delete the logical port's row directly from each table.
    for tbl in tbls_to_del {
        tbl.del(logical_port_name)?;
    }
    Ok(())
}

/// `_wrapper_get_presence` (`common.py:124`): module present? Trait-call form.
pub fn wrapper_get_presence<S: SfpApi>(sfp: &S) -> bool {
    // NotImplementedError / any bridge error -> treated as "not present" (Python
    // falls through to `return False`).
    sfp.get_presence().unwrap_or(false)
}

/// `_wrapper_is_flat_memory` (`common.py:300`): flat-memory (SFF) module?
/// `Some(true)` flat, `Some(false)` paged, `Some(true)` when there is no xcvr api
/// (Python `if not api: return True`), `None` on `NotImplementedError`.
pub fn wrapper_is_flat_memory<S: SfpApi>(sfp: &S) -> Option<bool> {
    match sfp.is_flat_memory() {
        Ok(Some(b)) => Some(b),
        Ok(None) => Some(true), // no xcvr api -> flat
        Err(_) => None,         // NotImplementedError -> None
    }
}

/// Python-style bool rendering, matching `str(bool)` the reference daemon writes.
pub fn pybool(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

/// Fields the CMIS manager's `post_port_active_apsel_to_db`
/// (`cmis/cmis_manager_task.py:751-782`) owns — NOT the identity publish: every
/// `active_apsel_hostlane{n}`, plus `host_lane_count` and `media_lane_count`
/// (which follow the active application). The emulated `get_transceiver_info()`
/// identity dict carries their raw numeric values, but `TRANSCEIVER_INFO` must
/// keep the manager's authoritative value (`'N/A'` while no datapath is active).
/// Both identity-publish paths — `xcvrd::build_info_row` and the daemon's
/// `sync_port` — skip these via THIS predicate so the skip-set can't drift apart.
/// (Guarded against over-matching: `host_lane_assignment_options` etc. are NOT
/// owned, so this matches `host_lane_count`/`media_lane_count` exactly, not by
/// prefix.)
pub fn is_cmis_manager_owned_field(field: &str) -> bool {
    field.starts_with("active_apsel_hostlane")
        || field == "host_lane_count"
        || field == "media_lane_count"
}

/// The CMIS-manager-owned `TRANSCEIVER_INFO` fields written `'N/A'` for a present
/// CMIS module with no active datapath (`host_lanes_mask == 0` / `reset_apsel`):
/// every `active_apsel_hostlane{1..=max_host_lanes}` plus `host_lane_count` and
/// `media_lane_count`. Mirrors `post_port_active_apsel_to_db`'s reset branch
/// (`cmis/cmis_manager_task.py:759-782`). The daemon runs the reduced CMIS driver
/// (no full manager task), so `sync_port` writes these inline; every field it
/// returns also satisfies `is_cmis_manager_owned_field`, so the identity loop
/// skips exactly what this set re-establishes as `'N/A'`.
pub fn cmis_no_datapath_na_fields(max_host_lanes: usize) -> Vec<String> {
    let mut fields: Vec<String> = (1..=max_host_lanes)
        .map(|lane| format!("active_apsel_hostlane{lane}"))
        .collect();
    fields.push("host_lane_count".to_string());
    fields.push("media_lane_count".to_string());
    fields
}

/// Render a HAL identity/DOM/etc. field as the STATE_DB string the reference
/// daemon writes (`str(value)`), stripping NUL padding on CMIS strings.
///
/// Faithfulness rules pinned by the M6 golden projection:
/// - **Strings**: NUL padding removed (CMIS fields are fixed-width, NUL-padded;
///   the e2e harness strips NULs on read). Trailing SPACES are kept — e.g.
///   `vendor_date` "2024-12-14 " keeps its trailing space.
/// - **bool** -> `True`/`False`; **number** -> canonical (`100.0`, `4`).
/// - **dict/list** (e.g. `application_advertisement`) -> Python `repr`, not JSON,
///   because the reference does `str(value)` on the original Python object
///   (`{1: {'host_...': 'XLAUI...', ...}}`).
/// - **null** -> field skipped (the golden module emits no null identity fields).
pub fn stringify_field(value: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match value {
        Value::Null => None,
        Value::String(s) => Some(strip_nuls(s)),
        Value::Bool(b) => Some(pybool(*b).to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(py_repr(other)),
    }
}

/// Remove NUL padding from a CMIS string (the e2e harness does the same on read:
/// `out.replace("\x00", "")`). Spaces are intentionally preserved.
fn strip_nuls(s: &str) -> String {
    s.replace('\0', "")
}

/// Python `repr()` of a JSON value — used for nested dict/list identity fields so
/// the STATE_DB string matches the reference daemon's `str(value)` on the original
/// Python object (e.g. `application_advertisement`).
fn py_repr(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(b) => pybool(*b).to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_str_repr(s),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(py_repr).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(o) => {
            // preserve_order keeps insertion (CMIS-decode) order — the golden
            // application_advertisement pins that exact inner ordering.
            let items: Vec<String> = o
                .iter()
                .map(|(k, v)| format!("{}: {}", py_key_repr(k), py_repr(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// Python `repr()` of a string: single-quoted by default, switching to double
/// quotes only when the string has a single quote but no double quote (CPython's
/// rule), escaping backslashes / the quote char / common control chars.
fn py_str_repr(s: &str) -> String {
    let s = strip_nuls(s);
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// Python `repr()` of a dict key. `json.dumps` stringifies non-string keys, so a
/// key that round-trips through `i64` (e.g. the `application_advertisement`
/// app-selector `1`) was an int in Python and is rendered unquoted; anything else
/// is a quoted string.
fn py_key_repr(k: &str) -> String {
    if k.parse::<i64>().map(|n| n.to_string() == *k).unwrap_or(false) {
        k.to_string()
    } else {
        py_str_repr(k)
    }
}

/// Convert a HAL dict `Value` into a STATE_DB `Row` (field -> stringified value).
pub fn value_to_row(value: &serde_json::Value) -> Row {
    let mut row = Row::new();
    if let Some(obj) = value.as_object() {
        for (field, v) in obj {
            if let Some(s) = stringify_field(v) {
                row.insert(field.clone(), s);
            }
        }
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockSfp, MockStateDb};
    use crate::statedb::StateDb;
    use serde_json::json;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn cmis_state_string_roundtrip() {
        // The exact STATE_DB string matters for the oracle (cmis_state == "READY").
        assert_eq!(CmisState::Ready.as_str(), "READY");
        assert_eq!(CmisState::from_str("READY"), CmisState::Ready);
        assert_eq!(CmisState::from_str("bogus"), CmisState::Unknown);
        for s in [CmisState::Inserted, CmisState::DpTxOn, CmisState::Failed] {
            assert_eq!(CmisState::from_str(s.as_str()), s);
        }
    }

    #[test]
    fn cmis_terminal_states() {
        assert!(CmisState::Ready.is_terminal());
        assert!(CmisState::Failed.is_terminal());
        assert!(CmisState::Removed.is_terminal());
        assert!(!CmisState::Inserted.is_terminal());
    }

    /// NUL-strip + `str(bool)` / `str(number)` rendering, and Python-repr for
    /// nested dict/list identity fields (the M6 golden rendering rules).
    #[test]
    fn stringify_strips_nul_and_renders_python_scalars() {
        // CMIS strings are fixed-width, NUL-padded; NULs removed, spaces KEPT.
        assert_eq!(stringify_field(&json!("EMU-40G-LR4\0\0\0")).as_deref(), Some("EMU-40G-LR4"));
        // Trailing spaces preserved (golden vendor_date is "2024-12-14 ").
        assert_eq!(stringify_field(&json!("2024-12-14 ")).as_deref(), Some("2024-12-14 "));
        assert_eq!(stringify_field(&json!("ACME  ")).as_deref(), Some("ACME  "));
        // str(bool) -> "True"/"False".
        assert_eq!(stringify_field(&json!(true)).as_deref(), Some("True"));
        assert_eq!(stringify_field(&json!(false)).as_deref(), Some("False"));
        // str(number): ints plain, floats keep the decimal point.
        assert_eq!(stringify_field(&json!(196100)).as_deref(), Some("196100"));
        assert_eq!(stringify_field(&json!(-15.0)).as_deref(), Some("-15.0"));
        assert_eq!(stringify_field(&json!(100.0)).as_deref(), Some("100.0"));
        // JSON null -> no field written (skipped).
        assert!(stringify_field(&json!(null)).is_none());
        assert_eq!(pybool(true), "True");
        assert_eq!(pybool(false), "False");
    }

    /// A nested dict field (application_advertisement) renders as a Python
    /// `str(dict)` repr — int keys unquoted, string values single-quoted, inner
    /// order preserved (needs serde_json `preserve_order`).
    #[test]
    fn stringify_renders_application_advertisement_as_python_repr() {
        let adv = json!({
            "1": {
                "host_electrical_interface_id": "XLAUI C2M (Annex 83B)",
                "module_media_interface_id": "40GBASE-LR4 (Cl 87)",
                "media_lane_count": 4,
                "host_lane_count": 4,
                "host_lane_assignment_options": 1,
                "media_lane_assignment_options": 1
            }
        });
        assert_eq!(
            stringify_field(&adv).as_deref(),
            Some("{1: {'host_electrical_interface_id': 'XLAUI C2M (Annex 83B)', \
'module_media_interface_id': '40GBASE-LR4 (Cl 87)', 'media_lane_count': 4, \
'host_lane_count': 4, 'host_lane_assignment_options': 1, \
'media_lane_assignment_options': 1}}")
        );
        // A list renders with Python repr elements too.
        assert_eq!(stringify_field(&json!([1, "a", true])).as_deref(), Some("[1, 'a', True]"));
    }

    #[test]
    fn value_to_row_maps_object_fields_and_skips_null() {
        let r = value_to_row(&json!({"model": "EMU\0", "cmis_rev": "5.0", "absent": null}));
        assert_eq!(r.get("model").map(String::as_str), Some("EMU"));
        assert_eq!(r.get("cmis_rev").map(String::as_str), Some("5.0"));
        assert!(!r.contains_key("absent"));
    }

    /// <- common.update_port_transceiver_status_table_sw: seeds ('status','error').
    #[test]
    fn update_status_sw_writes_status_and_error() {
        let db = MockStateDb::new();
        let tbl = db.table("TRANSCEIVER_STATUS_SW").unwrap();
        update_port_transceiver_status_table_sw("Ethernet0", &tbl, "1", NO_ERROR).unwrap();
        assert_eq!(tbl.hget("Ethernet0", "status").unwrap().as_deref(), Some("1"));
        assert_eq!(tbl.hget("Ethernet0", "error").unwrap().as_deref(), Some("N/A"));
    }

    /// <- test_del_port_sfp_dom_info_from_db: each table's row for the port is removed.
    #[test]
    fn del_removes_port_rows_from_each_table() {
        let db = MockStateDb::new();
        let info = db.table("TRANSCEIVER_INFO").unwrap();
        let dom = db.table("TRANSCEIVER_DOM_SENSOR").unwrap();
        info.set("Ethernet0", &row(&[("model", "x")])).unwrap();
        dom.set("Ethernet0", &row(&[("temperature", "30")])).unwrap();

        del_port_sfp_dom_info_from_db("Ethernet0", &[&info, &dom]).unwrap();
        assert!(info.get("Ethernet0").unwrap().is_none());
        assert!(dom.get("Ethernet0").unwrap().is_none());
    }

    /// <- test_wrapper_get_presence (retargeted onto SfpApi): present true, absent false.
    #[test]
    fn wrapper_presence_reflects_sfp() {
        let mut sfp = MockSfp::default();
        sfp.presence = true;
        assert!(wrapper_get_presence(&sfp));
        sfp.presence = false;
        assert!(!wrapper_get_presence(&sfp));
    }

    /// <- _wrapper_is_flat_memory: paged/flat pass through; no-api -> Some(true);
    /// NotImplementedError -> None.
    #[test]
    fn wrapper_is_flat_memory_branches() {
        use crate::mock::FlatMem;
        let mut sfp = MockSfp::default();
        sfp.flat_memory = FlatMem::Paged;
        assert_eq!(wrapper_is_flat_memory(&sfp), Some(false));
        sfp.flat_memory = FlatMem::Flat;
        assert_eq!(wrapper_is_flat_memory(&sfp), Some(true));
        sfp.flat_memory = FlatMem::NoApi;
        assert_eq!(wrapper_is_flat_memory(&sfp), Some(true));
        sfp.flat_memory = FlatMem::NotImpl;
        assert_eq!(wrapper_is_flat_memory(&sfp), None);
    }

    /// <- test_get_cmis_state_from_state_db: reads back `cmis_state`, UNKNOWN when
    /// the field/row is absent.
    #[test]
    fn get_cmis_state_reads_back_or_unknown() {
        let db = MockStateDb::new();
        let sw = db.table("TRANSCEIVER_STATUS_SW").unwrap();
        // Absent -> UNKNOWN.
        assert_eq!(get_cmis_state_from_state_db("Ethernet0", &sw).unwrap(), CmisState::Unknown);
        // Present READY -> READY.
        sw.set("Ethernet0", &row(&[("cmis_state", "READY")])).unwrap();
        assert_eq!(get_cmis_state_from_state_db("Ethernet0", &sw).unwrap(), CmisState::Ready);
        // Row present but no cmis_state field -> UNKNOWN.
        sw.set("Ethernet4", &row(&[("status", "1")])).unwrap();
        assert_eq!(get_cmis_state_from_state_db("Ethernet4", &sw).unwrap(), CmisState::Unknown);
    }

    /// <- M6 golden: the CMIS-manager-owned identity fields — every
    /// active_apsel_hostlaneN, host_lane_count, media_lane_count — are recognised;
    /// look-alikes (host_lane_assignment_options, media_lane_*) and ordinary
    /// identity fields are NOT, so the identity publish never over-filters.
    #[test]
    fn is_cmis_manager_owned_field_matches_manager_fields_only() {
        for f in [
            "active_apsel_hostlane1",
            "active_apsel_hostlane4",
            "active_apsel_hostlane8",
            "host_lane_count",
            "media_lane_count",
        ] {
            assert!(is_cmis_manager_owned_field(f), "{f} should be manager-owned");
        }
        for f in [
            "cmis_rev",
            "model",
            "manufacturer",
            "host_lane_assignment_options",
            "media_lane_assignment_options",
            "application_advertisement",
            "is_replaceable",
        ] {
            assert!(!is_cmis_manager_owned_field(f), "{f} should NOT be manager-owned");
        }
    }

    /// <- M6 golden (daemon.rs sync_port reduced CMIS driver): the no-datapath
    /// 'N/A' set is exactly the 8 active_apsel host lanes + host_lane_count +
    /// media_lane_count — the set the golden 40G-LR4 pins — and the ANTI-DRIFT
    /// invariant: every field written 'N/A' is also one the identity loop skips
    /// (is_cmis_manager_owned_field), so the raw numeric value can never leak.
    #[test]
    fn cmis_no_datapath_na_fields_matches_golden_and_skip_set() {
        let fields = cmis_no_datapath_na_fields(8);
        let expected = [
            "active_apsel_hostlane1",
            "active_apsel_hostlane2",
            "active_apsel_hostlane3",
            "active_apsel_hostlane4",
            "active_apsel_hostlane5",
            "active_apsel_hostlane6",
            "active_apsel_hostlane7",
            "active_apsel_hostlane8",
            "host_lane_count",
            "media_lane_count",
        ];
        assert_eq!(fields, expected);
        for f in &fields {
            assert!(
                is_cmis_manager_owned_field(f),
                "{f} is written 'N/A' but not in the identity-publish skip-set (drift!)"
            );
        }
    }
}
