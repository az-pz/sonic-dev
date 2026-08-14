//! Unit tests for `optics_si_parser` — the Rust translation of
//! `tests/test_xcvrd.py::TestOpticSiParser` (plus the `TestXcvrdScript` optics cases),
//! run against the crate's mock `CmisApi` seam instead of `MagicMock()`.
use super::*;
use crate::cmis::cmis_api::MockCmisApi;
use serde_json::json;

/// The `optics_si_settings.json` fixture (crate testdata copy), the analogue of the
/// Python module-level `optics_si_settings_dict`.
fn optics_settings() -> Value {
    serde_json::from_str(include_str!("testdata/optics_si_settings.json")).unwrap()
}

/// `optics_si_settings_with_comma_dict`: GLOBAL `0-31` re-keyed as a comma/range list and
/// PORT_MEDIA_SETTINGS removed.
fn optics_settings_with_comma() -> Value {
    let mut d = optics_settings();
    let obj = d.as_object_mut().unwrap();
    let global_si = obj
        .get_mut(GLOBAL_MEDIA_SETTINGS_KEY)
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("0-31")
        .unwrap();
    obj.remove(PORT_MEDIA_SETTINGS_KEY);
    obj.get_mut(GLOBAL_MEDIA_SETTINGS_KEY)
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("0-5,6,7-20,21-31".to_string(), global_si);
    d
}

/// `port_optics_si_settings`: only the PORT_MEDIA_SETTINGS block (no GLOBAL).
fn port_optics_settings() -> Value {
    let mut d = optics_settings();
    let port_ms = d.as_object_mut().unwrap().remove(PORT_MEDIA_SETTINGS_KEY).unwrap();
    json!({ PORT_MEDIA_SETTINGS_KEY: port_ms })
}

/// A mock module api advertising the given vendor name / part number.
fn vendor_api(manufacturer: Option<&str>, model: Option<&str>) -> MockCmisApi {
    let api = MockCmisApi::new();
    api.set_manufacturer(manufacturer);
    api.set_model(model);
    api
}

// ---- _match_optics_si_key -------------------------------------------------------------

#[test]
fn test_match_optics_si_key_regex_error() {
    // Invalid regex (unclosed bracket) → fall back to exact string comparison.
    let dict_key = "[invalid regex";
    assert!(!match_optics_si_key(dict_key, "VENDOR-1234", "VENDOR"));
    // Exact string match after the regex error.
    assert!(match_optics_si_key(dict_key, dict_key, "VENDOR"));
}

#[test]
fn test_match_optics_si_key_fallback_string_match() {
    let key = "VENDOR-1234";
    let vendor = "VENDOR";
    assert!(match_optics_si_key(key, key, vendor)); // exact key
    assert!(match_optics_si_key(vendor, key, vendor)); // vendor name
    assert!(match_optics_si_key("VENDOR", key, vendor)); // split-prefix
}

/// NEW: a real regex key (`VENDOR-(1234|5678)`) matches by full key and by vendor-prefix, and
/// the vendor-prefix path resolves even when the full key differs.
#[test]
fn optics_si_key_regex_and_fallback() {
    assert!(match_optics_si_key("VENDOR-(1234|5678)", "VENDOR-5678", "VENDOR"));
    assert!(!match_optics_si_key("VENDOR-(1234|5678)", "VENDOR-9999", "VENDOR"));
    // A `.*` suffix pattern (as the vendor JSON uses: "XCVR-EMU.*").
    assert!(match_optics_si_key("XCVR-EMU.*", "XCVR-EMU\0\0", "XCVR-EMU\0\0"));
}

// ---- _get_port_media_settings ---------------------------------------------------------

#[test]
fn test_get_port_media_settings_speed_key_missing() {
    // Port 5 exists but is empty → len(optics_si_dict)==0 → return the non-empty default.
    let g = json!({ "PORT_MEDIA_SETTINGS": { "5": {} } });
    let default = json!({ "default": "value" });
    let r = get_port_media_settings(&g, 5, 25, "VENDOR-1234", "VENDOR", &default);
    assert_eq!(r, default);
}

#[test]
fn test_get_port_media_settings_no_values_with_empty_default() {
    // Port 5 empty AND empty default → {}.
    let g = json!({ "PORT_MEDIA_SETTINGS": { "5": {} } });
    let r = get_port_media_settings(&g, 5, 25, "VENDOR-1234", "VENDOR", &empty_obj());
    assert_eq!(r, empty_obj());
}

// ---- get_module_vendor_key ------------------------------------------------------------

#[test]
fn test_get_module_vendor_key() {
    // 'Credo ' (trailing space) + 'CAC82X321HW' → ('CREDO-CAC82X321HW', 'CREDO').
    let api = vendor_api(Some("Credo "), Some("CAC82X321HW"));
    let r = get_module_vendor_key(1, Some(&api));
    assert_eq!(r, Some(("CREDO-CAC82X321HW".to_string(), "CREDO".to_string())));
}

#[test]
fn test_get_module_vendor_key_api_none() {
    assert_eq!(get_module_vendor_key(1, None), None);
}

#[test]
fn test_get_module_vendor_key_vendor_name_none() {
    let api = vendor_api(None, Some("CAC82X321HW"));
    assert_eq!(get_module_vendor_key(1, Some(&api)), None);
}

#[test]
fn test_get_module_vendor_key_vendor_pn_none() {
    let api = vendor_api(Some("VENDOR"), None);
    assert_eq!(get_module_vendor_key(1, Some(&api)), None);
}

// ---- fetch_optics_si_setting ----------------------------------------------------------

/// The mocked-`get_module_vendor_key` positive path: present module + a valid vendor key runs
/// the lookup and returns a JSON object (`_check_fetch_optics_si_setting`).
fn check_fetch(g: &Value, index: i32) {
    let api = vendor_api(Some("Credo"), Some("CAC82X321M"));
    let r = fetch_optics_si_setting(g, index, 100, Some(&api), true);
    assert!(r.is_object(), "fetch must return a JSON object");
}

#[test]
fn test_fetch_optics_si_setting() {
    check_fetch(&optics_settings(), 1);
}

#[test]
fn test_fetch_optics_si_setting_with_comma() {
    let g = optics_settings_with_comma();
    check_fetch(&g, 1);
    check_fetch(&g, 6);
}

#[test]
fn test_fetch_optics_si_setting_with_port() {
    check_fetch(&port_optics_settings(), 1);
}

#[test]
fn test_fetch_optics_si_setting_negative() {
    // get_module_vendor_key → None (unreadable vendor) → fetch returns empty.
    let api = vendor_api(None, None);
    let r = fetch_optics_si_setting(&port_optics_settings(), 1, 100, Some(&api), true);
    assert!(!optics_si_present(&r), "unknown vendor key must yield an empty SI dict");
}

/// NEW: a matching vendor key returns the concrete per-vendor SI sub-dict from GLOBAL.
#[test]
fn fetch_optics_si_setting_returns_matching_vendor_dict() {
    let g = optics_settings();
    // GLOBAL '0-31'/'100G_SPEED' advertises vendor 'CREDO-CAC82X321M2MC0HW'.
    let api = vendor_api(Some("Credo"), Some("CAC82X321M2MC0HW"));
    let r = fetch_optics_si_setting(&g, 1, 100, Some(&api), true);
    assert!(is_nonempty_obj(&r), "expected a vendor-matched SI dict");
    assert!(r.get("OutputEqPreCursorTargetRx").is_some());
}

#[test]
fn fetch_optics_si_setting_not_present_returns_empty() {
    let g = optics_settings();
    let api = vendor_api(Some("Credo"), Some("CAC82X321M2MC0HW"));
    // Module absent → empty (no lookup).
    let r = fetch_optics_si_setting(&g, 1, 100, Some(&api), false);
    assert!(!optics_si_present(&r));
}

#[test]
fn fetch_optics_si_setting_no_settings_loaded_returns_empty() {
    let r = fetch_optics_si_setting(&empty_obj(), 1, 100, None, true);
    assert!(!optics_si_present(&r));
}

// ---- load / present -------------------------------------------------------------------

#[test]
fn test_load_optical_si_settings_missing_file() {
    assert_eq!(load_optics_si_settings("/invalid/path", "/invalid/path"), empty_obj());
}

#[test]
fn test_load_optics_si_settings_no_file() {
    assert_eq!(
        load_optics_si_settings("/nonexistent/platform", "/nonexistent/hwsku"),
        empty_obj()
    );
}

#[test]
fn test_load_optical_si_file_from_platform_folder() {
    let testdata = concat!(env!("CARGO_MANIFEST_DIR"), "/src/xcvrd_utilities/testdata");
    assert!(optics_si_present(&load_optics_si_settings(testdata, "/invalid/path")));
}

#[test]
fn test_load_optical_si_file_from_hwsku_folder() {
    let testdata = concat!(env!("CARGO_MANIFEST_DIR"), "/src/xcvrd_utilities/testdata");
    assert!(optics_si_present(&load_optics_si_settings("/invalid/path", testdata)));
}

#[test]
fn test_optics_si_present_empty_dict() {
    assert!(!optics_si_present(&empty_obj()));
}

#[test]
fn test_optics_si_present_with_data() {
    assert!(optics_si_present(&json!({ "some": "data" })));
}
