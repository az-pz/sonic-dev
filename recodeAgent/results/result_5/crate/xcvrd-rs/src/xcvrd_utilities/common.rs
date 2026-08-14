#![allow(dead_code)]
//! Port of `xcvrd_utilities/common.py`: presence/status_sw writers, CMIS state
//! constants, del/wrapper helpers, and small pure utilities.
use crate::db::Table;
use crate::hal::Chassis;
use crate::cmis::cmis_api::CmisApi;
use crate::xcvrd_utilities::port_event_helper::PortMapping;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const CMIS_STATE_UNKNOWN: &str = "UNKNOWN";
pub const CMIS_STATE_INSERTED: &str = "INSERTED";
pub const CMIS_STATE_DP_PRE_INIT_CHECK: &str = "DP_PRE_INIT_CHECK";
pub const CMIS_STATE_DP_DEINIT: &str = "DP_DEINIT";
pub const CMIS_STATE_AP_CONF: &str = "AP_CONFIGURED";
pub const CMIS_STATE_DP_ACTIVATE: &str = "DP_ACTIVATION";
pub const CMIS_STATE_DP_INIT: &str = "DP_INIT";
pub const CMIS_STATE_DP_TXON: &str = "DP_TXON";
pub const CMIS_STATE_READY: &str = "READY";
pub const CMIS_STATE_REMOVED: &str = "REMOVED";
pub const CMIS_STATE_FAILED: &str = "FAILED";
pub const CMIS_TERMINAL_STATES: [&str; 3] = [CMIS_STATE_FAILED, CMIS_STATE_READY, CMIS_STATE_REMOVED];

/// `CmisManagerTask.CMIS_MODULE_TYPES` (cmis_manager_task.py) — the module-type
/// abbreviations that get the paged CMIS datapath bring-up (everything else is a flat
/// SFF module the SFF/`SffManagerTask` path owns).
pub const CMIS_MODULE_TYPES: [&str; 6] = ["QSFP-DD", "QSFP_DD", "OSFP", "OSFP-8X", "QSFP+C", "CPO"];

/// Write the SW-owned STATUS fields (`status`, `error`) for a logical port,
/// mirroring `update_port_transceiver_status_table_sw`.
pub fn update_port_transceiver_status_table_sw(
    logical_port_name: &str,
    status_sw_tbl: &dyn Table,
    status: &str,
    error_descriptions: &str,
) -> Result<(), String> {
    status_sw_tbl.set(
        logical_port_name,
        &[
            ("status".to_string(), status.to_string()),
            ("error".to_string(), error_descriptions.to_string()),
        ],
    )
}

/// `common.is_copper(physical_port)` — is the transceiver copper (DAC), used only to
/// choose the `COPPER`/`OPTICAL` medium fragment of a media-settings key. The reference
/// calls `get_sfp(port).get_xcvr_api().is_copper()` and, on any
/// `NotImplementedError`/`AttributeError` (or no chassis), logs and **assumes copper**
/// (`True`). Here the seam is [`Sfp::call_json`]`("is_copper")`; any HAL/decode error or a
/// non-boolean result falls back to `true`, and a missing chassis is `true` as well.
pub fn is_copper(chassis: Option<&dyn Chassis>, physical_port: usize) -> bool {
    let chassis = match chassis {
        Some(c) => c,
        None => return true,
    };
    match chassis.sfp(physical_port) {
        Ok(sfp) => match sfp.call_json("is_copper") {
            Ok(Value::Bool(b)) => b,
            _ => true,
        },
        Err(_) => true,
    }
}

/// Wrapper to read SFP presence for a physical port; false on any HAL error
/// (the analogue of the Python `NotImplementedError`/no-platform fallbacks).
pub fn wrapper_get_presence(chassis: &dyn Chassis, physical_port: usize) -> bool {
    match chassis.sfp(physical_port) {
        Ok(sfp) => sfp.get_presence().unwrap_or(false),
        Err(_) => false,
    }
}

/// Minimal STATE_DB read seam for the warm/fast-reboot detectors, mirroring the Python
/// `daemon_base.db_connect("STATE_DB", namespace=...).hget(table, field)`. The real impl
/// wraps a swss `DbConnector` (see [`crate::db`]); unit tests inject a map-backed mock. A
/// missing key/field is `None`, the analogue of the Python `hget` returning `None`.
pub trait StateDbHget {
    fn get_field(&self, key: &str, field: &str) -> Option<String>;
}

/// `is_fast_reboot_enabled(namespace)` (xcvrd_utilities/common.py:212): fast reboot is on
/// iff STATE_DB `FAST_RESTART_ENABLE_TABLE|system.enable` is the (case-insensitive,
/// whitespace-trimmed) string `"true"`. Any other value — or an absent field — is `false`.
/// The `namespace` selection is folded into the injected `db` (the caller connects to the
/// right STATE_DB), matching the single-connection-per-namespace Python contract.
pub fn is_fast_reboot_enabled(db: &dyn StateDbHget) -> bool {
    match db.get_field("FAST_RESTART_ENABLE_TABLE|system", "enable") {
        Some(s) => s.trim().eq_ignore_ascii_case("true"),
        None => false,
    }
}

/// `is_syncd_warm_restore_complete(namespace)` (xcvrd_utilities/common.py:220): a warm
/// reboot is in progress iff EITHER STATE_DB `WARM_RESTART_TABLE|syncd.restore_count` is a
/// positive integer OR `WARM_RESTART_ENABLE_TABLE|system.enable` is `"true"`. Redis stores
/// hash values as strings, so `restore_count` is treated like the Python `str` branch:
/// a whitespace-trimmed all-digit string that parses to `> 0` counts (mirroring
/// `restore_count.strip().isdigit() and int(...) > 0`); a non-numeric value (e.g. `"abc"`)
/// is ignored, as the Python `ValueError` is caught and returns `False`.
pub fn is_syncd_warm_restore_complete(db: &dyn StateDbHget) -> bool {
    let restore_count = db.get_field("WARM_RESTART_TABLE|syncd", "restore_count");
    let system_enabled = db.get_field("WARM_RESTART_ENABLE_TABLE|system", "enable");

    if let Some(rc) = restore_count.as_deref() {
        let t = rc.trim();
        if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) && t.parse::<i64>().unwrap_or(0) > 0
        {
            return true;
        }
    }
    if let Some(se) = system_enabled.as_deref() {
        if se.trim().eq_ignore_ascii_case("true") {
            return true;
        }
    }
    false
}

/// `get_namespace_from_asic_id(asic_id)` (xcvrd_utilities/common.py:87): the STATE_DB
/// namespace for an ASIC — the empty string on a single-ASIC platform, or `"asic{N}"` on a
/// multi-ASIC one. `multi_asic` stands in for the Python `multi_asic.is_multi_asic()`
/// platform probe (always `false` for the deployed single-ASIC KVM target).
pub fn get_namespace_from_asic_id(asic_id: i32, multi_asic: bool) -> String {
    if multi_asic {
        format!("asic{asic_id}")
    } else {
        String::new()
    }
}

/// `common.is_cmis_api(api)` — is this transceiver served by the paged-CMIS api (so the
/// CmisManagerTask owns its datapath bring-up)? The reference keys on the Python
/// `isinstance(api, CmisApi)`; the observable equivalent through our decode seam is the
/// module-type abbreviation being one of [`CMIS_MODULE_TYPES`]. An unreadable/absent type
/// (`None`) is not a CMIS module.
pub fn is_cmis_api(type_abbrv: Option<&str>) -> bool {
    match type_abbrv {
        Some(t) => CMIS_MODULE_TYPES.contains(&t),
        None => false,
    }
}

/// Map a host-electrical-interface name to a port speed (bps/1000), per `get_interface_speed`.
pub fn get_interface_speed(ifname: &str) -> u32 {
    if ifname.contains("1.6T") {
        1_600_000
    } else if ifname.contains("800G") {
        800_000
    } else if ifname.contains("400G") {
        400_000
    } else if ifname.contains("200G") {
        200_000
    } else if ifname.contains("100G") || ifname.contains("CAUI-4") {
        100_000
    } else if ifname.contains("50G") || ifname.contains("LAUI-2") {
        50_000
    } else if ifname.contains("40G") || ifname.contains("XLAUI") || ifname.contains("XLPPI") {
        40_000
    } else if ifname.contains("25G") {
        25_000
    } else if ifname.contains("10G") || ifname.contains("SFI") || ifname.contains("XFI") {
        10_000
    } else if ifname.contains("1000BASE") {
        1_000
    } else {
        0
    }
}

/// `get_cmis_application(host_lane_count, speed, app_advert)` — pick the app code from a
/// module's application advertisement by matching the port's host lane count AND host
/// electrical interface speed. Apps are scanned in numeric-ascending index order (mirroring
/// Python dict insertion order 1..15); the first match's index (masked to 4 bits) is the
/// desired application code. `None` when nothing matches (→ the caller latches FAILED).
pub fn get_cmis_application(host_lane_count: u32, speed: u32, app_advert: &Value) -> Option<u32> {
    if speed == 0 || host_lane_count == 0 {
        return None;
    }
    let map = app_advert.as_object()?;
    let mut entries: Vec<(u32, &Value)> = map
        .iter()
        .filter_map(|(k, v)| k.parse::<u32>().ok().map(|idx| (idx, v)))
        .collect();
    entries.sort_by_key(|(idx, _)| *idx);
    for (index, app_info) in entries {
        let advert_lane_count = app_info.get("host_lane_count").and_then(|v| v.as_u64());
        let ifname = app_info
            .get("host_electrical_interface_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if advert_lane_count == Some(host_lane_count as u64) && get_interface_speed(ifname) == speed {
            return Some(index & 0xf);
        }
    }
    None
}

/// `get_cmis_application_desired(api, host_lane_count, speed)` — the module-advertisement
/// app-select wrapper: reads the api's application advertisement and delegates to
/// [`get_cmis_application`]. `None` on no match / missing inputs.
pub fn get_cmis_application_desired(
    api: &dyn CmisApi,
    host_lane_count: u32,
    speed: u32,
) -> Option<u32> {
    if speed == 0 || host_lane_count == 0 {
        return None;
    }
    get_cmis_application(host_lane_count, speed, &api.get_application_advertisement())
}

/// Read the cached `cmis_state` for `lport`, defaulting to `UNKNOWN`.
pub fn get_cmis_state_from_state_db(lport: &str, status_sw_tbl: &dyn Table) -> String {
    match status_sw_tbl.hget(lport, "cmis_state") {
        Ok(Some(state)) => state,
        _ => CMIS_STATE_UNKNOWN.to_string(),
    }
}

/// Physical port name for STATE_DB keys (`logical` or `logical:N (ganged)`).
pub fn get_physical_port_name(logical_port: &str, physical_port: i32, ganged: bool) -> String {
    if ganged {
        format!("{logical_port}:{physical_port} (ganged)")
    } else {
        logical_port.to_string()
    }
}

/// `{physical_port_index -> physical_port_name}` for a logical port (empty if unmapped).
pub fn get_physical_port_name_dict(logical_port_name: &str, port_mapping: &PortMapping) -> BTreeMap<i32, String> {
    let mut port_name_dict = BTreeMap::new();
    let physical_port_list = match port_mapping.logical_port_name_to_physical_port_list(logical_port_name) {
        Some(list) => list,
        None => return port_name_dict,
    };
    let ganged_port = physical_port_list.len() > 1;
    let mut ganged_member_num = 1;
    for physical_port in physical_port_list {
        let port_name = get_physical_port_name(logical_port_name, ganged_member_num, ganged_port);
        ganged_member_num += 1;
        port_name_dict.insert(physical_port, port_name);
    }
    port_name_dict
}

/// `get_pluggable_obj_dict(port_mapping_data)` (common.py:173) — the set of physical
/// ports backed by a pluggable SFP object. Iterates `physical_to_logical` and, for each
/// physical port, includes it when `get_port_device` succeeds. In Rust `get_port_device`
/// is `chassis.sfp(physical_port)` (there is no CPO seam, so every port is pluggable),
/// and a failing `sfp()` — the Python `get_sfp` raising — drops the port. A `None`
/// mapping yields an empty set (and never touches the chassis).
pub fn get_pluggable_obj_dict(
    port_mapping: Option<&PortMapping>,
    chassis: &dyn Chassis,
) -> BTreeSet<i32> {
    let mut obj_dict = BTreeSet::new();
    let port_mapping = match port_mapping {
        Some(pm) => pm,
        None => return obj_dict,
    };
    for &physical_port in port_mapping.physical_to_logical.keys() {
        if chassis.sfp(physical_port as usize).is_ok() {
            obj_dict.insert(physical_port);
        }
    }
    obj_dict
}


/// memory)? Reads CMIS lower-page 00h:2 bit 7 (FlatMem) via the raw EEPROM seam — that byte
/// is in lower memory, readable even on a flat module. `false` on any HAL error / absent slot
/// (the Python `None`/`NotImplementedError` fallback: not flat).
pub fn wrapper_is_flat_memory(chassis: &dyn Chassis, physical_port: usize) -> bool {
    const FLAT_MEM_LINEAR: usize = 2;
    const FLAT_MEM_BIT: u8 = 0x80;
    match chassis.sfp(physical_port) {
        Ok(sfp) => sfp
            .read_eeprom(FLAT_MEM_LINEAR, 1)
            .ok()
            .flatten()
            .and_then(|v| v.first().copied())
            .map(|b| b & FLAT_MEM_BIT != 0)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// `_wrapper_is_flat_memory(physical_port)` (common.py:367) — the faithful port of the
/// api-based helper the unit tests exercise: `sfp.get_xcvr_api().is_flat_memory()`, with
/// `True` when there is no api and `None` on a `NotImplementedError`/absent slot. The
/// `get_xcvr_api()`/`is_flat_memory()` chain is reached through the SFP `call_json`
/// seam (an unset call ⇒ JSON `null` ⇒ "no api" ⇒ `Some(true)`; a raising method ⇒ `None`).
/// (The byte-addressed [`wrapper_is_flat_memory`] above is the deployed realization the PM
/// path uses; this one mirrors the Python control flow for parity.)
pub fn wrapper_is_flat_memory_api(chassis: &dyn Chassis, physical_port: usize) -> Option<bool> {
    match chassis.sfp(physical_port) {
        Ok(sfp) => match sfp.call_json("is_flat_memory") {
            Ok(Value::Bool(b)) => Some(b),
            Ok(_) => Some(true),
            Err(_) => None,
        },
        Err(_) => None,
    }
}


/// firmware versions via the HAL seam. Returns an empty object on a HAL error / absent slot
/// (the Python `NotImplementedError`/no-chassis fallback → `{}`).
pub fn wrapper_get_transceiver_firmware_info(chassis: &dyn Chassis, physical_port: usize) -> Value {
    match chassis.sfp(physical_port) {
        Ok(sfp) => sfp
            .call_json("get_transceiver_info_firmware_versions")
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
        Err(_) => Value::Object(serde_json::Map::new()),
    }
}

/// `_wrapper_get_transceiver_pm(physical_port)` — the module's coherent performance-monitoring
/// values via the HAL seam. Returns an empty object on a HAL error / absent slot (the Python
/// `NotImplementedError`/no-chassis fallback → `{}`); a genuine `null` from the API is preserved
/// so the caller can distinguish "not ready" from "N/A".
pub fn wrapper_get_transceiver_pm(chassis: &dyn Chassis, physical_port: usize) -> Value {
    match chassis.sfp(physical_port) {
        Ok(sfp) => sfp
            .call_json("get_transceiver_pm")
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
        Err(_) => Value::Object(serde_json::Map::new()),
    }
}

/// Delete a logical port's DOM/SFP rows from every non-null table in the list,
/// mirroring `del_port_sfp_dom_info_from_db`.
pub fn del_port_sfp_dom_info_from_db(
    logical_port_name: &str,
    port_mapping: &PortMapping,
    tbl_to_del_list: &[Option<&dyn Table>],
) -> Result<(), String> {
    let physical_port_names = get_physical_port_name_dict(logical_port_name, port_mapping);
    for physical_port_name in physical_port_names.values() {
        for tbl in tbl_to_del_list.iter().flatten() {
            tbl.del(physical_port_name)?;
        }
    }
    Ok(())
}

/// True when `physical_port` lies within an inclusive `"start-end"` range string.
pub fn check_port_in_range(range_str: &str, physical_port: i32) -> bool {
    const RANGE_SEPARATOR: char = '-';
    let range_list: Vec<&str> = range_str.split(RANGE_SEPARATOR).collect();
    if range_list.len() < 2 {
        return false;
    }
    let start_num: i32 = match range_list[0].trim().parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    let end_num: i32 = match range_list[1].trim().parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    start_num <= physical_port && physical_port <= end_num
}

/// `re.match(pattern, text)` — anchor `pattern` at the **start** of `text` (Python
/// `re.match` semantics: unanchored at the end). Returns `false` when the pattern fails
/// to compile (a safe divergence: the media-settings fixtures are always valid, and the
/// production parser never wants a bad key to spuriously match).
pub fn re_match(pattern: &str, text: &str) -> bool {
    match fancy_regex::Regex::new(&format!("^(?:{pattern})")) {
        Ok(re) => re.is_match(text).unwrap_or(false),
        Err(_) => false,
    }
}

/// `re.fullmatch(pattern, text)` — anchor `pattern` at **both** ends of `text`. Returns
/// `false` on a compile error (see [`re_match`]).
pub fn re_fullmatch(pattern: &str, text: &str) -> bool {
    match fancy_regex::Regex::new(&format!("^(?:{pattern})$")) {
        Ok(re) => re.is_match(text).unwrap_or(false),
        Err(_) => false,
    }
}

/// `re.fullmatch(pattern, text)` that reports a **compile error** to the caller (via
/// `Err(())`) so it can fall back to a literal string comparison, mirroring the optics-SI
/// parser's `try: re.fullmatch(...) except re.error: dict_key == key`.
pub fn re_fullmatch_checked(pattern: &str, text: &str) -> Result<bool, ()> {
    match fancy_regex::Regex::new(&format!("^(?:{pattern})$")) {
        Ok(re) => Ok(re.is_match(text).unwrap_or(false)),
        Err(_) => Err(()),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockChassis, MockSfp, MockTable};
    use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType};

    /// Port of `tests/test_xcvrd.py::test_wrapper_get_presence`.
    #[test]
    fn test_wrapper_get_presence() {
        let present = MockChassis::with_sfps(vec![MockSfp { present: true, ..Default::default() }]);
        assert!(wrapper_get_presence(&present, 0));

        let absent = MockChassis::with_sfps(vec![MockSfp { present: false, ..Default::default() }]);
        assert!(!wrapper_get_presence(&absent, 0));

        // Missing SFP (HAL error) → false, mirroring the NotImplementedError fallback.
        assert!(!wrapper_get_presence(&absent, 5));
    }

    /// Port of `tests/test_xcvrd.py::test_wrapper_get_transceiver_firmware_info`.
    #[test]
    fn test_wrapper_get_transceiver_firmware_info() {
        // Present module with a firmware read → the wrapper returns the (truthy) dict.
        let fw = serde_json::json!({"active_firmware": "2.1.1", "inactive_firmware": "1.2.4"});
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_info_firmware_versions", fw.clone());
        let chassis = MockChassis::with_sfps(vec![sfp]);
        assert_eq!(wrapper_get_transceiver_firmware_info(&chassis, 0), fw);

        // API returns {} → the wrapper returns {} (falsy).
        let mut sfp = MockSfp::present();
        sfp.set_json_call("get_transceiver_info_firmware_versions", serde_json::json!({}));
        let chassis = MockChassis::with_sfps(vec![sfp]);
        assert_eq!(
            wrapper_get_transceiver_firmware_info(&chassis, 0),
            serde_json::json!({})
        );

        // Missing SFP (HAL error / NotImplementedError on get_sfp) → {}.
        let empty = MockChassis::with_sfps(vec![]);
        assert_eq!(
            wrapper_get_transceiver_firmware_info(&empty, 0),
            serde_json::json!({})
        );

        // Present module whose firmware method is not implemented → {}.
        let mut sfp = MockSfp::present();
        sfp.fail_method("get_transceiver_info_firmware_versions");
        let chassis = MockChassis::with_sfps(vec![sfp]);
        assert_eq!(
            wrapper_get_transceiver_firmware_info(&chassis, 0),
            serde_json::json!({})
        );
    }

    #[test]
    fn test_update_port_transceiver_status_table_sw() {
        let tbl = MockTable::new();
        update_port_transceiver_status_table_sw("Ethernet0", &tbl, "1", "N/A").unwrap();
        assert_eq!(tbl.field("Ethernet0", "status").as_deref(), Some("1"));
        assert_eq!(tbl.field("Ethernet0", "error").as_deref(), Some("N/A"));
    }

    #[test]
    fn test_get_interface_speed() {
        assert_eq!(get_interface_speed("400GAUI-8"), 400_000);
        assert_eq!(get_interface_speed("100G-CWDM4"), 100_000);
        assert_eq!(get_interface_speed("CAUI-4"), 100_000);
        assert_eq!(get_interface_speed("40GBASE-CR4"), 40_000);
        assert_eq!(get_interface_speed("unknown"), 0);
    }

    #[test]
    fn test_get_cmis_state_from_state_db() {
        let tbl = MockTable::new();
        assert_eq!(get_cmis_state_from_state_db("Ethernet0", &tbl), CMIS_STATE_UNKNOWN);
        tbl.hset("Ethernet0", "cmis_state", CMIS_STATE_READY).unwrap();
        assert_eq!(get_cmis_state_from_state_db("Ethernet0", &tbl), CMIS_STATE_READY);
    }

    /// A fresh round-trip: writing each CMIS state to STATE_DB and reading it back through
    /// `get_cmis_state_from_state_db` returns exactly that state; an unwritten port is
    /// `UNKNOWN`.
    #[test]
    fn get_cmis_state_from_state_db_roundtrip() {
        let tbl = MockTable::new();
        assert_eq!(get_cmis_state_from_state_db("Ethernet4", &tbl), CMIS_STATE_UNKNOWN);
        for state in [
            CMIS_STATE_INSERTED,
            CMIS_STATE_DP_PRE_INIT_CHECK,
            CMIS_STATE_DP_DEINIT,
            CMIS_STATE_AP_CONF,
            CMIS_STATE_DP_INIT,
            CMIS_STATE_DP_TXON,
            CMIS_STATE_DP_ACTIVATE,
            CMIS_STATE_READY,
            CMIS_STATE_FAILED,
            CMIS_STATE_REMOVED,
        ] {
            tbl.hset("Ethernet4", "cmis_state", state).unwrap();
            assert_eq!(get_cmis_state_from_state_db("Ethernet4", &tbl), state);
        }
        // The cmis_state field-merge must not clobber sibling status/error fields.
        tbl.hset("Ethernet4", "status", "1").unwrap();
        tbl.hset("Ethernet4", "cmis_state", CMIS_STATE_READY).unwrap();
        assert_eq!(tbl.field("Ethernet4", "status").as_deref(), Some("1"));
    }

    /// Port of `tests/test_xcvrd.py::test_is_cmis_api`: the paged-CMIS module types are
    /// recognised, flat SFF types and an unreadable (`None`) type are not.
    #[test]
    fn test_is_cmis_api() {
        for t in ["QSFP-DD", "QSFP_DD", "OSFP", "OSFP-8X", "QSFP+C", "CPO"] {
            assert!(is_cmis_api(Some(t)), "{t} should be CMIS");
        }
        for t in ["SFP", "QSFP+", "QSFP28", "SFP28", "unknown", ""] {
            assert!(!is_cmis_api(Some(t)), "{t} should not be CMIS");
        }
        assert!(!is_cmis_api(None));
    }

    #[test]
    fn test_check_port_in_range() {
        assert!(check_port_in_range("0-31", 5));
        assert!(check_port_in_range("0 - 31", 0));
        assert!(check_port_in_range("0-31", 31));
        assert!(!check_port_in_range("0-31", 32));
    }

    /// Port of `tests/test_xcvrd.py::test_is_copper_exception`: when reading the xcvr api
    /// raises (here: no SFP at the slot), `is_copper` logs and assumes copper (`true`).
    #[test]
    fn test_is_copper_exception() {
        let chassis = MockChassis::with_sfps(vec![]);
        assert!(is_copper(Some(&chassis), 0));
    }

    /// `is_copper` reflects the module's `is_copper()` when it is readable, and assumes
    /// copper on any decode error / absent chassis.
    #[test]
    fn is_copper_reflects_api_and_defaults() {
        let mut copper = MockSfp::present();
        copper.set_json_call("is_copper", serde_json::json!(true));
        let mut optical = MockSfp::present();
        optical.set_json_call("is_copper", serde_json::json!(false));
        let chassis = MockChassis::with_sfps(vec![copper, optical]);
        assert!(is_copper(Some(&chassis), 0));
        assert!(!is_copper(Some(&chassis), 1));
        // No `is_copper` seam value (null) → assume copper.
        let bare = MockChassis::with_sfps(vec![MockSfp::present()]);
        assert!(is_copper(Some(&bare), 0));
        // No chassis → assume copper.
        assert!(is_copper(None, 0));
    }

    #[test]
    fn re_helpers_match_python_semantics() {
        // re.match anchors at start, not end.
        assert!(re_match("QSFP(\\+|28|-DD)", "QSFP-DD-sm_media_interface"));
        assert!(!re_match("sm_media", "QSFP-DD-sm_media_interface"));
        // re.fullmatch anchors both ends.
        assert!(re_fullmatch("COPPER[0-9]+", "COPPER50"));
        assert!(!re_fullmatch("COPPER", "COPPER50"));
        // Look-ahead compiles (fancy-regex) where std `regex` would reject it.
        assert!(re_match("(QSFP-DD)-(?!.*CR).*", "QSFP-DD-sm_media_interface"));
        assert!(!re_match("(QSFP-DD)-(?!.*CR).*", "QSFP-DD-100GBASE-CR2"));
        // A bad pattern is a compile error for the checked variant, false otherwise.
        assert_eq!(re_fullmatch_checked("[unterminated", "x"), Err(()));
        assert!(!re_match("[unterminated", "x"));
    }

    #[test]
    fn test_get_physical_port_name_dict_and_del() {
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));
        let dict = get_physical_port_name_dict("Ethernet0", &pm);
        assert_eq!(dict.get(&0).map(String::as_str), Some("Ethernet0"));

        let tbl = MockTable::new();
        tbl.set("Ethernet0", &[("temperature".into(), "30".into())]).unwrap();
        del_port_sfp_dom_info_from_db("Ethernet0", &pm, &[Some(&tbl as &dyn Table), None]).unwrap();
        assert!(!tbl.contains("Ethernet0"));
        assert_eq!(tbl.del_count(), 1);
    }

    /// Port of `tests/test_xcvrd.py::test_del_port_sfp_dom_info_from_db`: the purge
    /// deletes the logical port's row from *every* non-null table in the list
    /// (INFO + DOM + threshold + PM + firmware here — the removal set), and
    /// tolerates `None` entries for tables that are not wired.
    #[test]
    fn test_del_port_sfp_dom_info_from_db() {
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortAdd));

        let init_tbl = MockTable::new();
        let dom_tbl = MockTable::new();
        let dom_threshold_tbl = MockTable::new();
        let pm_tbl = MockTable::new();
        let firmware_info_tbl = MockTable::new();
        for tbl in [&init_tbl, &dom_tbl, &dom_threshold_tbl, &pm_tbl, &firmware_info_tbl] {
            tbl.set("Ethernet0", &[("field".into(), "value".into())]).unwrap();
        }
        // A different port must be left untouched.
        dom_tbl.set("Ethernet4", &[("temperature".into(), "31".into())]).unwrap();

        del_port_sfp_dom_info_from_db(
            "Ethernet0",
            &pm,
            &[
                Some(&init_tbl as &dyn Table),
                Some(&dom_tbl as &dyn Table),
                None,
                Some(&dom_threshold_tbl as &dyn Table),
                Some(&pm_tbl as &dyn Table),
                Some(&firmware_info_tbl as &dyn Table),
            ],
        )
        .unwrap();

        for tbl in [&init_tbl, &dom_tbl, &dom_threshold_tbl, &pm_tbl, &firmware_info_tbl] {
            assert!(!tbl.contains("Ethernet0"), "Ethernet0 row must be deleted");
        }
        assert_eq!(dom_tbl.get_size().unwrap(), 1); // only the other port remains
        assert!(dom_tbl.contains("Ethernet4"));
    }

    /// A map-backed [`StateDbHget`] double: the Rust analogue of the Python
    /// `MagicMock()` whose `hget(table, key)` side-effect returns the canned
    /// `restore_count` / `system_enabled` (mirroring
    /// `patch("...common.daemon_base.db_connect", return_value=mock_db)`).
    #[derive(Default)]
    struct MockRebootDb {
        fields: std::collections::HashMap<(String, String), String>,
    }
    impl MockRebootDb {
        fn set(mut self, key: &str, field: &str, value: &str) -> Self {
            self.fields.insert((key.to_string(), field.to_string()), value.to_string());
            self
        }
    }
    impl StateDbHget for MockRebootDb {
        fn get_field(&self, key: &str, field: &str) -> Option<String> {
            self.fields.get(&(key.to_string(), field.to_string())).cloned()
        }
    }

    /// Port of `tests/test_xcvrd.py::test_is_syncd_warm_restore_complete_valid_cases`:
    /// a positive `restore_count` OR `system_enabled == "true"` means a warm restore is
    /// underway. Redis stores hash values as strings, so `restore_count` is exercised via
    /// its string branch (`"1"`/`"2"` → true, `"0"` → false).
    #[test]
    fn test_is_syncd_warm_restore_complete_valid_cases() {
        // (restore_count, system_enabled, expected)
        let cases: &[(Option<&str>, Option<&str>, bool)] = &[
            (Some("1"), None, true),
            (Some("0"), None, false),
            (Some("2"), None, true),
            (None, Some("true"), true),
            (None, Some("false"), false),
            (None, None, false),
        ];
        for (rc, se, expected) in cases {
            let mut db = MockRebootDb::default();
            if let Some(rc) = rc {
                db = db.set("WARM_RESTART_TABLE|syncd", "restore_count", rc);
            }
            if let Some(se) = se {
                db = db.set("WARM_RESTART_ENABLE_TABLE|system", "enable", se);
            }
            assert_eq!(
                is_syncd_warm_restore_complete(&db),
                *expected,
                "restore_count={rc:?} system_enabled={se:?}"
            );
        }
    }

    /// Port of `test_is_syncd_warm_restore_complete_invalid_restore_count`: a non-numeric
    /// `restore_count` (`"abc"`) is ignored (the Python `int("abc")` `ValueError` is caught)
    /// and, with no `system_enabled`, the result is `false`.
    #[test]
    fn test_is_syncd_warm_restore_complete_invalid_restore_count() {
        let db = MockRebootDb::default().set("WARM_RESTART_TABLE|syncd", "restore_count", "abc");
        assert!(!is_syncd_warm_restore_complete(&db));
    }

    /// Port of `test_is_syncd_warm_restore_complete_with_namespace`: the namespace is folded
    /// into the injected STATE_DB seam (the caller connects to the right namespace), so the
    /// detector's result is driven purely by that namespace's `restore_count`.
    #[test]
    fn test_is_syncd_warm_restore_complete_with_namespace() {
        // (namespace, restore_count, expected) — namespace is informational here since the
        // seam already targets that namespace's STATE_DB.
        let cases: &[(&str, &str, bool)] = &[
            ("", "1", true),
            ("asic0", "1", true),
            ("asic1", "1", true),
            ("asic0", "0", false),
            ("asic1", "0", false),
        ];
        for (namespace, rc, expected) in cases {
            let db = MockRebootDb::default().set("WARM_RESTART_TABLE|syncd", "restore_count", rc);
            assert_eq!(
                is_syncd_warm_restore_complete(&db),
                *expected,
                "namespace={namespace} restore_count={rc}"
            );
        }
    }

    /// `is_fast_reboot_enabled` is the case-insensitive, whitespace-trimmed `"true"` test on
    /// STATE_DB `FAST_RESTART_ENABLE_TABLE|system.enable`; anything else (or absent) is false.
    #[test]
    fn test_is_fast_reboot_enabled() {
        for (val, expected) in [("true", true), (" TRUE ", true), ("false", false), ("1", false)] {
            let db = MockRebootDb::default().set("FAST_RESTART_ENABLE_TABLE|system", "enable", val);
            assert_eq!(is_fast_reboot_enabled(&db), expected, "enable={val:?}");
        }
        // Absent field → false.
        assert!(!is_fast_reboot_enabled(&MockRebootDb::default()));
    }

    /// `get_namespace_from_asic_id`: empty on single-ASIC, `asic{N}` on multi-ASIC.
    #[test]
    fn test_get_namespace_from_asic_id() {
        assert_eq!(get_namespace_from_asic_id(0, false), "");
        assert_eq!(get_namespace_from_asic_id(3, false), "");
        assert_eq!(get_namespace_from_asic_id(1, true), "asic1");
        assert_eq!(get_namespace_from_asic_id(2, true), "asic2");
    }
}
