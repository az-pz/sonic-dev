//! Unit tests for `media_settings_parser` — ports of `tests/test_xcvrd.py`
//! (media-settings behaviors) against the crate's mock HAL/DB seams.
//!
//! The large `get_media_settings_value` fixture matrix and the notify fixtures are
//! generated 1:1 from the Python module-level fixtures (see `testdata/*.json`) so the
//! Rust assertions match the Python expectations exactly.

use super::*;
use crate::cmis::cmis_api::MockCmisApi;
use crate::mock::MockTable;
use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType};
use serde_json::json;
use std::collections::BTreeMap;

const TESTDATA_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/xcvrd_utilities/testdata");

fn fixtures() -> Value {
    serde_json::from_str(include_str!("testdata/media_fixtures.json")).unwrap()
}

fn value_cases() -> Vec<Value> {
    serde_json::from_str(include_str!("testdata/media_settings_value_cases.json")).unwrap()
}

fn gearbox_settings() -> Value {
    serde_json::from_str(include_str!("testdata/gearbox_media_settings.json")).unwrap()
}

fn key_from_json(k: &Value) -> MediaSettingsKey {
    MediaSettingsKey {
        vendor_key: k.get("vendor_key").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        media_key: k.get("media_key").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        lane_speed_key: k
            .get("lane_speed_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        medium_lane_speed_key: k
            .get("medium_lane_speed_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

// ---------------------------------------------------------------------------
// test_get_media_settings_value — data-driven over the 55 generated cases.
// ---------------------------------------------------------------------------
#[test]
fn test_get_media_settings_value() {
    for (i, case) in value_cases().iter().enumerate() {
        let g = &case["g_dict"];
        let port = case["port"].as_i64().unwrap() as i32;
        let key = key_from_json(&case["key"]);
        let expected = &case["expected"];
        let result = get_media_settings_value(g, port, &key);
        assert_eq!(
            &result, expected,
            "case {i}: port={port} key={:?}\n got={result}\n exp={expected}",
            case["key"]
        );
    }
}

// ---------------------------------------------------------------------------
// test_is_si_per_speed_supported
// ---------------------------------------------------------------------------
#[test]
fn test_is_si_per_speed_supported() {
    let per_speed = json!({
        "speed:400G-GAUI-4": {"main": {"lane0": "0x0"}},
        "speed:400GAUI-8": {"post1": {"lane0": "0x0"}}
    });
    assert!(is_si_per_speed_supported(&per_speed));

    let flat = json!({
        "main": {"lane0": "0x0"},
        "post1": {"lane0": "0x0"}
    });
    assert!(!is_si_per_speed_supported(&flat));
}

// ---------------------------------------------------------------------------
// test_get_speed_lane_count_and_subport
// ---------------------------------------------------------------------------
#[test]
fn test_get_speed_lane_count_and_subport() {
    struct Case {
        found: bool,
        data: Vec<(&'static str, &'static str)>,
        expected: (i64, u32, i64),
    }
    let cases = vec![
        Case {
            found: true,
            data: vec![("speed", "400000"), ("lanes", "1,2,3,4,5,6,7,8"), ("mtu", "9100")],
            expected: (400000, 8, 0),
        },
        Case {
            found: true,
            data: vec![("speed", "25000"), ("lanes", "1"), ("mtu", "9100"), ("subport", "1")],
            expected: (25000, 1, 1),
        },
        Case {
            found: true,
            data: vec![("lanes", "1,2,3,4,5,6,7,8"), ("mtu", "9100")],
            expected: (0, 0, 0),
        },
        Case {
            found: true,
            data: vec![("speed", "400000"), ("mtu", "9100")],
            expected: (0, 0, 0),
        },
        Case { found: false, data: vec![], expected: (0, 0, 0) },
    ];
    for c in cases {
        let tbl = MockTable::new();
        if c.found {
            let fvs: Vec<(String, String)> =
                c.data.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            tbl.set("Ethernet0", &fvs).unwrap();
        }
        let got = get_speed_lane_count_and_subport("Ethernet0", &tbl);
        assert_eq!(got, c.expected);
    }
}

// ---------------------------------------------------------------------------
// test_get_media_settings_key
// ---------------------------------------------------------------------------
#[test]
fn test_get_media_settings_key() {
    // Non-CMIS: good specification_compliance.
    let xcvr = json!({
        "0": {
            "manufacturer": "Molex",
            "model": "1064141421",
            "cable_type": "Length Cable Assembly(m)",
            "cable_length": "255",
            "specification_compliance": "{'10/40G Ethernet Compliance Code': '10GBase-SR'}",
            "type_abbrv_name": "QSFP+"
        }
    });
    let key = get_media_settings_key(0, &xcvr, 100000, 2, false, None, true);
    assert_eq!(key.vendor_key, "MOLEX-1064141421");
    assert_eq!(key.media_key, "QSFP+-10GBase-SR-255M");
    assert_eq!(key.lane_speed_key.as_deref(), Some("speed:50G"));
    assert_eq!(key.medium_lane_speed_key, "COPPER50");

    // Non-CMIS: bad specification_compliance.
    let mut xcvr_bad = xcvr.clone();
    xcvr_bad["0"]["specification_compliance"] = json!("N/A");
    let key = get_media_settings_key(0, &xcvr_bad, 100000, 2, false, None, true);
    assert_eq!(key.vendor_key, "MOLEX-1064141421");
    assert_eq!(key.media_key, "QSFP+-*");
    assert_eq!(key.lane_speed_key.as_deref(), Some("speed:50G"));
    assert_eq!(key.medium_lane_speed_key, "COPPER50");

    // Non-CMIS QSFP28 with extended specification compliance + float cable length.
    let xcvr_qsfp28 = json!({
        "0": {
            "type_abbrv_name": "QSFP28",
            "manufacturer": "AVAGO",
            "model": "XXX-YYY-ZZZ",
            "cable_type": "Length Cable Assembly(m)",
            "cable_length": 50.0,
            "specification_compliance": "{'10/40G Ethernet Compliance Code': 'Unknown', 'Extended Specification Compliance': '100GBASE-SR4 or 25GBASE-SR'}"
        }
    });
    let key = get_media_settings_key(0, &xcvr_qsfp28, 100000, 4, false, None, true);
    assert_eq!(key.vendor_key, "AVAGO-XXX-YYY-ZZZ");
    assert_eq!(key.media_key, "QSFP28-100GBASE-SR4 or 25GBASE-SR-50.0M");
    assert_eq!(key.lane_speed_key.as_deref(), Some("speed:25G"));
    assert_eq!(key.medium_lane_speed_key, "COPPER25");

    // CMIS: host electrical interface resolved from the application advertisement.
    let xcvr_cmis = json!({
        "0": {
            "manufacturer": "Molex",
            "model": "1064141421",
            "cable_type": "Length Cable Assembly(m)",
            "cable_length": "255",
            "specification_compliance": "sm_media_interface",
            "type_abbrv_name": "QSFP-DD"
        }
    });
    let api = MockCmisApi::new();
    api.set_application_advertisement(json!({
        "1": {"host_electrical_interface_id": "400G CR8", "host_lane_count": 8},
        "2": {"host_electrical_interface_id": "200GBASE-CR4 (Clause 136)", "host_lane_count": 4},
        "3": {"host_electrical_interface_id": "100GBASE-CR2 (Clause 136)", "host_lane_count": 2},
        "4": {"host_electrical_interface_id": "100GBASE-CR4 (Clause 92)", "host_lane_count": 4},
        "5": {"host_electrical_interface_id": "50GBASE-CR (Clause 126)", "host_lane_count": 1},
        "6": {"host_electrical_interface_id": "40GBASE-CR4 (Clause 85)", "host_lane_count": 4},
        "7": {"host_electrical_interface_id": "25GBASE-CR CA-N (Clause 110)", "host_lane_count": 1},
        "8": {"host_electrical_interface_id": "1000BASE -CX(Clause 39)", "host_lane_count": 1}
    }));
    let key = get_media_settings_key(0, &xcvr_cmis, 100000, 2, true, Some(&api as &dyn CmisApi), true);
    assert_eq!(key.vendor_key, "MOLEX-1064141421");
    assert_eq!(key.media_key, "QSFP-DD-sm_media_interface");
    assert_eq!(key.lane_speed_key.as_deref(), Some("speed:100GBASE-CR2"));
    assert_eq!(key.medium_lane_speed_key, "COPPER50");
}

// ---------------------------------------------------------------------------
// test_media_settings_parser_base_get_lane_values_str
// ---------------------------------------------------------------------------
#[test]
fn test_media_settings_parser_base_get_lane_values_str() {
    let lane_dict = json!({"lane0": "1", "lane1": "2", "lane2": "3", "lane3": "4"});
    assert_eq!(MediaSettingsParserBase::get_lane_values_str(&lane_dict, 4, 0), "1,2,3,4");
    assert_eq!(MediaSettingsParserBase::get_lane_values_str(&lane_dict, 2, 2), "3,4");
    assert_eq!(MediaSettingsParserBase::get_lane_values_str(&lane_dict, 2, 0), "1,2");
    assert_eq!(MediaSettingsParserBase::get_lane_values_str(&lane_dict, 2, 3), "1,2");

    let small = json!({"lane0": "1", "lane1": "2"});
    assert_eq!(MediaSettingsParserBase::get_lane_values_str(&small, 2, 2), "1,2");

    let unordered = json!({"lane0": "a", "lane2": "c", "lane1": "b", "lane3": "d"});
    assert_eq!(MediaSettingsParserBase::get_lane_values_str(&unordered, 2, 2), "c,d");

    let numeric = json!({"lane0": 1, "lane1": 2, "lane2": 3, "lane3": 4});
    assert_eq!(MediaSettingsParserBase::get_lane_values_str(&numeric, 2, 2), "3,4");
}

// ---------------------------------------------------------------------------
// test_media_settings_to_db_value
// ---------------------------------------------------------------------------
#[test]
fn test_media_settings_to_db_value() {
    let r = MediaSettingsParserBase::to_db_value(
        &json!({"main": {"lane0": "0x11", "lane1": "0x12", "lane2": "0x13", "lane3": "0x14"}}),
        2,
        2,
        None,
    );
    assert_eq!(r, vec![("main".to_string(), "0x13,0x14".to_string())]);

    let r = MediaSettingsParserBase::to_db_value(
        &json!({"main": {"lane0": "0x11", "lane1": "0x12"}, "los_thresh": "7"}),
        2,
        0,
        None,
    );
    assert_eq!(
        r,
        vec![
            ("main".to_string(), "0x11,0x12".to_string()),
            ("los_thresh".to_string(), "7".to_string())
        ]
    );

    let r = MediaSettingsParserBase::to_db_value(&json!({}), 2, 2, None);
    assert_eq!(r, Vec::<(String, String)>::new());

    let r = MediaSettingsParserBase::to_db_value(
        &json!({
            "gb_line_main": {"lane0": "0x10", "lane1": "0x11", "lane2": "0x12", "lane3": "0x13",
                             "lane4": "0x14", "lane5": "0x15", "lane6": "0x16", "lane7": "0x17"},
            "gb_system_main": {"lane0": "0x20", "lane1": "0x21", "lane2": "0x22", "lane3": "0x23"}
        }),
        4,
        0,
        Some(8),
    );
    assert_eq!(
        r,
        vec![
            ("gb_line_main".to_string(), "0x10,0x11,0x12,0x13,0x14,0x15,0x16,0x17".to_string()),
            ("gb_system_main".to_string(), "0x20,0x21,0x22,0x23".to_string()),
        ]
    );
}

// ---------------------------------------------------------------------------
// test_custom_media_settings_to_db_value
// ---------------------------------------------------------------------------
#[test]
fn test_custom_media_settings_to_db_value() {
    let r = CustomMediaSettingsParser::to_db_value(
        &json!({
            "CUSTOM:XYZ": {"lane0": 10, "lane1": 11, "lane2": 12, "lane3": 13},
            "CUSTOM:ABC": {"lane0": 1, "lane1": 2, "lane2": 3, "lane3": 4},
            "main": {"lane0": "0x11", "lane1": "0x12", "lane2": "0x13", "lane3": "0x14"}
        }),
        2,
        2,
    );
    assert_eq!(
        r.as_deref(),
        Some(r#"{"attributes":[{"XYZ":{"value":[12,13]}},{"ABC":{"value":[3,4]}}]}"#)
    );

    let r = CustomMediaSettingsParser::to_db_value(
        &json!({
            "CUSTOM:XYZ": {"lane0": "ADAPTIVE", "lane1": "ADAPTIVE", "lane2": "ADAPTIVE", "lane3": "ADAPTIVE"},
            "CUSTOM:ABC": {"lane0": 1, "lane1": 2, "lane2": 3, "lane3": 4}
        }),
        2,
        2,
    );
    assert_eq!(
        r.as_deref(),
        Some(r#"{"attributes":[{"XYZ":{"value":["ADAPTIVE","ADAPTIVE"]}},{"ABC":{"value":[3,4]}}]}"#)
    );

    let r = CustomMediaSettingsParser::to_db_value(
        &json!({"main": {"lane0": "0x11", "lane1": "0x12", "lane2": "0x13", "lane3": "0x14"}}),
        2,
        2,
    );
    assert_eq!(r, None);
}

// ---------------------------------------------------------------------------
// test_custom_media_settings_get_lane_values
// ---------------------------------------------------------------------------
#[test]
fn test_custom_media_settings_get_lane_values() {
    let lane_dict = json!({"lane0": 1, "lane1": 2, "lane2": 3, "lane3": 4});
    assert_eq!(
        CustomMediaSettingsParser::get_lane_values(&lane_dict, 2, 2),
        vec![json!(3), json!(4)]
    );
    assert_eq!(
        CustomMediaSettingsParser::get_lane_values(&lane_dict, 2, 3),
        vec![json!(1), json!(2)]
    );
}

// ---------------------------------------------------------------------------
// test_custom_media_settings_is_port_selected
// ---------------------------------------------------------------------------
#[test]
fn test_custom_media_settings_is_port_selected() {
    assert!(CustomMediaSettingsParser::is_port_selected(&json!("1, 3-4, 8"), 8));
    assert!(CustomMediaSettingsParser::is_port_selected(&json!("1,3-4,8"), 4));
    assert!(CustomMediaSettingsParser::is_port_selected(&json!("01"), 1));
    assert!(CustomMediaSettingsParser::is_port_selected(&json!("1 - 3"), 2));
    assert!(!CustomMediaSettingsParser::is_port_selected(&json!("1,3-4,8"), 2));
    assert!(!CustomMediaSettingsParser::is_port_selected(&json!("   "), 1));
    assert!(!CustomMediaSettingsParser::is_port_selected(&json!("1,,3"), 2));
    assert!(!CustomMediaSettingsParser::is_port_selected(&json!("1-a"), 1));
    assert!(!CustomMediaSettingsParser::is_port_selected(&json!("1-2-3"), 2));
    assert!(!CustomMediaSettingsParser::is_port_selected(&json!("a"), 2));
    // Non-string selectors are rejected (Rust JSON keys are always strings, so this
    // exercises the type guard directly — the omitted Python non-string-selector
    // fixture would resolve to `{}` for the same reason).
    assert!(!CustomMediaSettingsParser::is_port_selected(&json!(123), 1));
}

// ---------------------------------------------------------------------------
// test_get_custom_media_settings_value
// ---------------------------------------------------------------------------
#[test]
fn test_get_custom_media_settings_value() {
    let f = fixtures();
    let key = json!({
        "vendor_key": "UNKOWN",
        "media_key": "QSFP-DD-active_cable_media_interface",
        "lane_speed_key": "speed:100GAUI-2",
        "medium_lane_speed_key": "UNKNOWN"
    });
    let key = key_from_json(&key);

    let g = &f["custom_attrs"];
    assert_eq!(
        get_custom_media_settings_value(g, 8, &key),
        json!({
            "CUSTOM:XYZ": f["custom_serdes_attrs_xyz_10"],
            "CUSTOM:ABC": f["custom_serdes_attrs_abc_mode"]
        })
    );
    assert_eq!(
        get_custom_media_settings_value(g, 7, &key),
        json!({"CUSTOM:XYZ": f["custom_serdes_attrs_xyz_20"]})
    );

    let mut key_no_match = key.clone();
    key_no_match.media_key = "UNMATCHED_MEDIA".to_string();
    assert_eq!(
        get_custom_media_settings_value(g, 8, &key_no_match),
        json!({"CUSTOM:XYZ": f["custom_serdes_attrs_xyz_20"]})
    );

    assert_eq!(
        get_custom_media_settings_value(&f["custom_attrs_no_space"], 4, &key),
        json!({"CUSTOM:XYZ": f["custom_serdes_attrs_xyz_10"]})
    );

    assert_eq!(
        get_custom_media_settings_value(&f["custom_attrs_empty_explicit_then_default"], 8, &key),
        json!({"CUSTOM:XYZ": f["custom_serdes_attrs_xyz_20"]})
    );
}

// ---------------------------------------------------------------------------
// test_custom_media_settings_mixed_with_port_and_global
// ---------------------------------------------------------------------------
#[test]
fn test_custom_media_settings_mixed_with_port_and_global() {
    let f = fixtures();
    let g = &f["custom_attrs_with_port_and_global"];
    let key = key_from_json(&json!({
        "vendor_key": "UNKOWN",
        "media_key": "QSFP-DD-active_cable_media_interface",
        "lane_speed_key": "speed:100GAUI-2",
        "medium_lane_speed_key": "UNKNOWN"
    }));

    assert_eq!(
        get_media_settings_value(g, 7, &key),
        json!({
            "pre1": {"lane0": "0x00000002", "lane1": "0x00000002"},
            "main": {"lane0": "0x00000020", "lane1": "0x00000020"},
            "post1": {"lane0": "0x00000006", "lane1": "0x00000006"},
            "regn_bfm1n": {"lane0": "0x000000aa", "lane1": "0x000000aa"}
        })
    );
    assert_eq!(
        get_custom_media_settings_value(g, 7, &key),
        json!({"CUSTOM:XYZ": f["custom_serdes_attrs_xyz_20"]})
    );
}

// ---------------------------------------------------------------------------
// test_custom_media_settings_medium_lane_key
// ---------------------------------------------------------------------------
#[test]
fn test_custom_media_settings_medium_lane_key() {
    let f = fixtures();
    let key = key_from_json(&json!({
        "vendor_key": "UNKOWN",
        "media_key": "UNMATCHED_MEDIA",
        "lane_speed_key": "speed:100GAUI-2",
        "medium_lane_speed_key": "COPPER50"
    }));
    assert_eq!(
        get_custom_media_settings_value(&f["custom_attrs_medium_lane"], 7, &key),
        json!({"CUSTOM:XYZ": f["custom_serdes_attrs_xyz_20"]})
    );
}

// ---------------------------------------------------------------------------
// test_custom_media_settings_parser_empty_or_invalid_settings
// ---------------------------------------------------------------------------
#[test]
fn test_custom_media_settings_parser_empty_or_invalid_settings() {
    let key = key_from_json(&json!({
        "vendor_key": "UNKOWN",
        "media_key": "UNMATCHED_MEDIA",
        "lane_speed_key": "speed:100GAUI-2",
        "medium_lane_speed_key": "COPPER50"
    }));
    let parser = CustomMediaSettingsParser::new();
    for settings in [json!({}), json!([]), json!(null)] {
        let (result, default) = parser.parse(&settings, 7, &key);
        assert_eq!(result, json!({}));
        assert_eq!(default, json!({}));
    }
}

// ---------------------------------------------------------------------------
// Load tests
// ---------------------------------------------------------------------------
#[test]
fn test_load_media_settings_missing_file() {
    assert_eq!(load_media_settings("/invalid/path", "/invalid/path"), json!({}));
}

#[test]
fn test_load_media_settings_file_from_platform_folder() {
    let g = load_media_settings(TESTDATA_DIR, "/invalid/path");
    assert!(media_settings_present(&g));
}

#[test]
fn test_load_media_settings_file_from_hwsku_folder() {
    let g = load_media_settings("/invalid/path", TESTDATA_DIR);
    assert!(media_settings_present(&g));
}

// ---------------------------------------------------------------------------
// notify_media_setting — shared harness + ports of the Python notify tests.
// ---------------------------------------------------------------------------

/// Reproduces `_check_notify_media_setting`: run notify against mock APPL/STATE tables
/// and assert what was published for `Ethernet0`.
#[allow(clippy::too_many_arguments)]
fn run_notify(
    g_dict: &Value,
    transceiver_dict: &Value,
    key: &MediaSettingsKey,
    port_speed: i64,
    lane_count: u32,
    subport: i64,
    index: i32,
    present: bool,
    gearbox: HashMap<String, u32>,
) -> (MockTable, MockTable) {
    let app = MockTable::new();
    let state = MockTable::new();
    let mut pm = PortMapping::new();
    pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", index, 0, PortEventType::PortAdd));
    let tables = MediaNotifyTables { app_port_tbl: &app, state_port_tbl: &state };
    notify_media_setting(
        "Ethernet0",
        transceiver_dict,
        g_dict,
        &pm,
        true,
        true,
        port_speed,
        lane_count,
        subport,
        &gearbox,
        &tables,
        &|_| present,
        &|_, _| key.clone(),
    );
    (app, state)
}

fn molex_xcvr(index: i32) -> Value {
    json!({
        index.to_string(): {
            "manufacturer": "Molex",
            "model": "1064141421",
            "cable_type": "Length Cable Assembly(m)",
            "cable_length": "255",
            "specification_compliance": "{'10/40G Ethernet Compliance Code': '10GBase-SR'}",
            "type_abbrv_name": "QSFP+"
        }
    })
}

fn assert_row_eq(app: &MockTable, expected: &Value) {
    let row = app.row("Ethernet0").expect("expected a published row");
    let mut want = BTreeMap::new();
    for (k, v) in expected.as_object().unwrap() {
        want.insert(k.clone(), v.as_str().unwrap().to_string());
    }
    assert_eq!(row, want);
}

#[test]
fn test_notify_media_setting() {
    let f = fixtures();
    let g = &f["optic_copper_si"];
    let mk = |media: &str, lsk: Option<&str>| MediaSettingsKey {
        vendor_key: "INNOLIGHT-X-DDDDD-NNN".to_string(),
        media_key: media.to_string(),
        lane_speed_key: lsk.map(|s| s.to_string()),
        medium_lane_speed_key: "UNKNOWN".to_string(),
    };

    // 400G optical (lane speed 50G).
    let (app, state) = run_notify(g, &molex_xcvr(1),
        &mk("QSFP-DD-sm_media_interface", Some("speed:400GAUI-8")), 400000, 8, 0, 1, true, HashMap::new());
    assert_row_eq(&app, &f["example4_expected_db"]);
    assert_eq!(state.field("Ethernet0", NPU_SI_SETTINGS_SYNC_STATUS_KEY).as_deref(), Some(NPU_SI_SETTINGS_NOTIFIED_VALUE));

    // 100G optical (lane speed 25G) via regex lane speed pattern.
    let (app, _) = run_notify(g, &molex_xcvr(1),
        &mk("QSFP28-100GBASE-SR4", Some("speed:25G")), 100000, 4, 0, 1, true, HashMap::new());
    assert_row_eq(&app, &f["example3_expected_db_4_lanes"]);

    // 100G copper (lane speed 25G).
    let (app, _) = run_notify(g, &molex_xcvr(1),
        &mk("QSFP28-100GBASE-CR4, 25GBASE-CR CA-25G-L or 50GBASE-CR2 with RS-1.0M", Some("speed:25G")),
        100000, 4, 0, 1, true, HashMap::new());
    assert_row_eq(&app, &f["example4_expected_db_4_lanes"]);

    // Lane speed None -> no match, no publish.
    let (app, _) = run_notify(g, &molex_xcvr(1),
        &mk("QSFP28-100GBASE-CR4", None), 100000, 4, 0, 1, true, HashMap::new());
    assert!(!app.contains("Ethernet0"));

    // 800G copper (lane speed 100G) -> Default fallback.
    let (app, _) = run_notify(g, &molex_xcvr(1),
        &mk("QSFP-DD-passive_copper_media_interface", Some("speed:800G-ETC-CR8")), 800000, 8, 0, 1, true, HashMap::new());
    assert_row_eq(&app, &f["example5_expected_db"]);

    // Lane speed matching under 'Default' vendor/media for a 400G transceiver on port 41.
    let key = MediaSettingsKey {
        vendor_key: "Molex".to_string(),
        media_key: "QSFP-DD-passive_copper_media_interface".to_string(),
        lane_speed_key: Some("speed:400GAUI-8".to_string()),
        medium_lane_speed_key: "UNKNOWN".to_string(),
    };
    let (app, _) = run_notify(g, &molex_xcvr(41), &key, 400000, 8, 0, 41, true, HashMap::new());
    assert_row_eq(&app, &f["example3_expected_db"]);

    // Empty transceiver dict -> nothing published.
    let (app, _) = run_notify(g, &json!({}),
        &mk("QSFP-DD-sm_media_interface", Some("speed:400GAUI-8")), 400000, 8, 0, 1, true, HashMap::new());
    assert!(!app.contains("Ethernet0"));

    // SFP not present -> nothing published.
    let (app, _) = run_notify(g, &molex_xcvr(1),
        &mk("QSFP-DD-sm_media_interface", Some("speed:400GAUI-8")), 400000, 8, 0, 1, false, HashMap::new());
    assert!(!app.contains("Ethernet0"));
}

#[test]
fn test_notify_media_setting_with_comma() {
    let f = fixtures();
    let g = &f["with_comma"];
    let key = MediaSettingsKey {
        vendor_key: "MOLEX-1064141421".to_string(),
        media_key: "QSFP+-10GBase-SR-255M".to_string(),
        lane_speed_key: Some("speed:100GBASE-CR2".to_string()),
        medium_lane_speed_key: "UNKNOWN".to_string(),
    };
    let (app, _) = run_notify(g, &molex_xcvr(1), &key, 100000, 2, 0, 1, true, HashMap::new());
    assert_row_eq(&app, &json!({"preemphasis": "0x164509,0x164509"}));

    let (app, _) = run_notify(g, &molex_xcvr(6), &key, 100000, 2, 0, 6, true, HashMap::new());
    assert_row_eq(&app, &json!({"preemphasis": "0x124A08,0x124A08"}));
}

#[test]
fn test_notify_media_setting_custom_only() {
    // Custom-only settings: media dict empty, custom dict present. The daemon publishes
    // just the custom_serdes_attrs field. Reproduced by a g_dict that yields no media
    // match but a CUSTOM match for port 1 (subport 1, lane_count 2 -> lanes 0..1).
    let g = json!({
        "CUSTOM_MEDIA_SETTINGS": {
            "1": {
                "QSFP-DD-active_cable_media_interface": {
                    "speed:100GAUI-2": {
                        "CUSTOM:XYZ": {"lane0": 10, "lane1": 11, "lane2": 12, "lane3": 13}
                    }
                }
            }
        }
    });
    let key = MediaSettingsKey {
        vendor_key: "MOLEX-1064141421".to_string(),
        media_key: "QSFP-DD-active_cable_media_interface".to_string(),
        lane_speed_key: Some("speed:100GAUI-2".to_string()),
        medium_lane_speed_key: "UNKNOWN".to_string(),
    };
    let (app, _) = run_notify(&g, &molex_xcvr(1), &key, 100000, 2, 1, 1, true, HashMap::new());
    let row = app.row("Ethernet0").expect("expected a published row");
    let mut want = BTreeMap::new();
    want.insert(
        CustomMediaSettingsParser::CUSTOM_SERDES_ATTRS_KEY_IN_DB.to_string(),
        r#"{"attributes":[{"XYZ":{"value":[10,11]}}]}"#.to_string(),
    );
    assert_eq!(row, want);
}

#[test]
fn test_notify_media_setting_empty_serialized_payload() {
    // Custom dict present but with no CUSTOM: attributes -> serialized payload is None,
    // media dict empty -> nothing is published at all.
    let g = json!({
        "CUSTOM_MEDIA_SETTINGS": {
            "1": {
                "QSFP-DD-active_cable_media_interface": {
                    "speed:100GAUI-2": {
                        "NOT_CUSTOM": {"lane0": 10, "lane1": 11, "lane2": 12, "lane3": 13}
                    }
                }
            }
        }
    });
    let key = MediaSettingsKey {
        vendor_key: "MOLEX-1064141421".to_string(),
        media_key: "QSFP-DD-active_cable_media_interface".to_string(),
        lane_speed_key: Some("speed:100GAUI-2".to_string()),
        medium_lane_speed_key: "UNKNOWN".to_string(),
    };
    let (app, state) = run_notify(&g, &molex_xcvr(1), &key, 100000, 2, 1, 1, true, HashMap::new());
    assert!(!app.contains("Ethernet0"));
    assert!(!state.contains("Ethernet0"));
}

#[test]
fn test_notify_media_setting_mixed_settings() {
    let f = fixtures();
    let g = &f["custom_attrs_with_port_and_global"];
    let key = MediaSettingsKey {
        vendor_key: "UNKOWN".to_string(),
        media_key: "QSFP-DD-active_cable_media_interface".to_string(),
        lane_speed_key: Some("speed:100GAUI-2".to_string()),
        medium_lane_speed_key: "UNKNOWN".to_string(),
    };
    let (app, _) = run_notify(g, &molex_xcvr(7), &key, 100000, 2, 1, 7, true, HashMap::new());
    let row = app.row("Ethernet0").expect("expected a published row");
    let mut want = BTreeMap::new();
    want.insert("pre1".to_string(), "0x00000002,0x00000002".to_string());
    want.insert("main".to_string(), "0x00000020,0x00000020".to_string());
    want.insert("post1".to_string(), "0x00000006,0x00000006".to_string());
    want.insert("regn_bfm1n".to_string(), "0x000000aa,0x000000aa".to_string());
    want.insert(
        CustomMediaSettingsParser::CUSTOM_SERDES_ATTRS_KEY_IN_DB.to_string(),
        r#"{"attributes":[{"XYZ":{"value":[20,21]}}]}"#.to_string(),
    );
    assert_eq!(row, want);
}

#[test]
fn test_notify_media_setting_with_gearbox() {
    let g = gearbox_settings();
    let test_vendor = json!({
        "1": {
            "manufacturer": "TestVendor",
            "model": "TestModel",
            "cable_type": "Length Cable Assembly(m)",
            "cable_length": "1",
            "specification_compliance": "passive_copper_media_interface",
            "type_abbrv_name": "QSFP-DD"
        }
    });
    let scenarios = [
        ("COPPER50", "speed:50G", 8u32, 4u32),
        ("OPTICAL50", "speed:50G", 8, 4),
        ("COPPER25", "speed:25G", 4, 4),
        ("OPTICAL25", "speed:25G", 4, 4),
    ];
    for (medium, lsk, gb_line, gb_system) in scenarios {
        let key = MediaSettingsKey {
            vendor_key: "TEST-VENDOR".to_string(),
            media_key: "TEST-MEDIA".to_string(),
            lane_speed_key: Some(lsk.to_string()),
            medium_lane_speed_key: medium.to_string(),
        };
        let mut gearbox = HashMap::new();
        gearbox.insert("Ethernet0".to_string(), gb_line);
        let (app, _) = run_notify(&g, &test_vendor, &key, 400000, 4, 0, 1, true, gearbox);
        let row = app.row("Ethernet0").expect("gearbox row expected");
        assert!(!row.is_empty());
        for (field, value) in &row {
            let lanes = value.split(',').count() as u32;
            if field.contains("gb_line") {
                assert_eq!(lanes, gb_line, "field {field} expected {gb_line} lanes");
            } else if field.contains("gb_system") {
                assert_eq!(lanes, gb_system, "field {field} expected {gb_system} lanes");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// end-to-end parse + notify from the shipped fixture.
// Exercises get_media_settings_key -> get_media_settings_value -> notify against
// the real media_settings.json, confirming the NPU_SI lifecycle stamp.
// ---------------------------------------------------------------------------
#[test]
fn media_settings_parse_and_notify_from_fixture() {
    let g = load_media_settings(TESTDATA_DIR, "/invalid/path");
    assert!(media_settings_present(&g));

    // Port 1 is covered by GLOBAL_MEDIA_SETTINGS '1-32'; a QSFP28 CR4 1M cable resolves
    // its preemphasis via the media key (note: the '+' variants are regex quantifiers,
    // so only the QSFP28 literal key matches by media_key).
    let key = MediaSettingsKey {
        vendor_key: "NOMATCH-X".to_string(),
        media_key: "QSFP28-40GBASE-CR4-1M".to_string(),
        lane_speed_key: None,
        medium_lane_speed_key: "NOMATCH".to_string(),
    };
    let resolved = get_media_settings_value(&g, 1, &key);
    assert!(is_nonempty_obj(&resolved), "expected a traditional media match for CR4-1M");
    assert!(resolved.get("preemphasis").is_some());

    let xcvr = json!({"1": {"manufacturer": "Molex", "model": "ABC"}});
    let (app, state) = run_notify(&g, &xcvr, &key, 40000, 4, 0, 1, true, HashMap::new());
    assert!(app.contains("Ethernet0"));
    assert_eq!(
        state.field("Ethernet0", NPU_SI_SETTINGS_SYNC_STATUS_KEY).as_deref(),
        Some(NPU_SI_SETTINGS_NOTIFIED_VALUE)
    );
}
