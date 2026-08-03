//! M6 golden-conformance projection test (Part B).
//!
//! The e2e `test_golden` (`../xcvrd-tests/tests/test_golden.py`) asserts the
//! STATE_DB projection `{TRANSCEIVER_INFO, TRANSCEIVER_STATUS_SW,
//! TRANSCEIVER_DOM_THRESHOLD}` the daemon produces for `Ethernet100` matches the
//! committed `golden/Ethernet100.json` byte-for-byte (minus the volatile
//! `last_update_time`). This unit test reproduces that assertion against the
//! MOCK HAL/DB seams: a `MockSfp` is programmed with the emulator identity + DOM
//! thresholds exactly as the bridge/`sonic_platform` deliver them (NUL-padded CMIS
//! strings, a nested `application_advertisement` dict, a trailing-space
//! `vendor_date`, and `str()`-ed threshold values incl. `-inf`), the daemon's
//! projection functions run, and the resulting `MockTable` rows are compared to an
//! embedded copy of the golden. It fails loudly if any field rendering regresses.
//!
//! Test-only (`#[cfg(test)]`); mirrors the golden field list rather than reading
//! the immutable `xcvrd-tests` file (the crate builds standalone in the container).

#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::cmis::cmis_manager_task::CmisManagerTask;
use crate::dom::utilities::dom_sensor::db_utils::DomDbUtils;
use crate::mock::{MockHal, MockSfp, MockStateDb};
use crate::statedb::{Row, StateDb, TableApi};
use crate::xcvrd::post_port_sfp_info_to_db;
use crate::xcvrd_utilities::common::{update_port_transceiver_status_table_sw, CmisState, NO_ERROR};
use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType, PortMapping};
use crate::xcvrd_utilities::xcvr_table_helper::{
    TRANSCEIVER_DOM_THRESHOLD_TABLE, TRANSCEIVER_INFO_TABLE, TRANSCEIVER_STATUS_SW_TABLE,
};

/// The golden port + its emulator index (`lib/emu.py`: `Ethernet{i*4}` <-> `i`).
const PORT: &str = "Ethernet100";
const PHYS: usize = 25;

/// A verbatim copy of `xcvrd-tests/golden/Ethernet100.json` (the conformance
/// oracle). `last_update_time` is not part of the golden (dropped as volatile).
const GOLDEN: &str = r#"{
  "TRANSCEIVER_DOM_THRESHOLD": {
    "lasertemphighalarm": "0.0",
    "lasertemphighwarning": "0.0",
    "lasertemplowalarm": "0.0",
    "lasertemplowwarning": "0.0",
    "rxpowerhighalarm": "-inf",
    "rxpowerhighwarning": "-inf",
    "rxpowerlowalarm": "-inf",
    "rxpowerlowwarning": "-inf",
    "temphighalarm": "0.0",
    "temphighwarning": "0.0",
    "templowalarm": "0.0",
    "templowwarning": "0.0",
    "txbiashighalarm": "0.0",
    "txbiashighwarning": "0.0",
    "txbiaslowalarm": "0.0",
    "txbiaslowwarning": "0.0",
    "txpowerhighalarm": "-inf",
    "txpowerhighwarning": "-inf",
    "txpowerlowalarm": "-inf",
    "txpowerlowwarning": "-inf",
    "vcchighalarm": "0.0",
    "vcchighwarning": "0.0",
    "vcclowalarm": "0.0",
    "vcclowwarning": "0.0"
  },
  "TRANSCEIVER_INFO": {
    "active_apsel_hostlane1": "N/A",
    "active_apsel_hostlane2": "N/A",
    "active_apsel_hostlane3": "N/A",
    "active_apsel_hostlane4": "N/A",
    "active_apsel_hostlane5": "N/A",
    "active_apsel_hostlane6": "N/A",
    "active_apsel_hostlane7": "N/A",
    "active_apsel_hostlane8": "N/A",
    "application_advertisement": "{1: {'host_electrical_interface_id': 'XLAUI C2M (Annex 83B)', 'module_media_interface_id': '40GBASE-LR4 (Cl 87)', 'media_lane_count': 4, 'host_lane_count': 4, 'host_lane_assignment_options': 1, 'media_lane_assignment_options': 1}}",
    "cable_length": "100.0",
    "cable_type": "Length Cable Assembly(m)",
    "cmis_rev": "5.2",
    "connector": "MPO 1x16",
    "encoding": "N/A",
    "ext_identifier": "Power Class 8 (10.0W Max)",
    "ext_rateselect_compliance": "N/A",
    "hardware_rev": "0.0",
    "host_lane_count": "N/A",
    "is_replaceable": "True",
    "manufacturer": "xcvr-emu",
    "media_interface_technology": "850 nm VCSEL",
    "media_lane_count": "N/A",
    "model": "EMU-40G-LR4",
    "nominal_bit_rate": "N/A",
    "serial": "0123456789",
    "specification_compliance": "sm_media_interface",
    "type": "QSFP-DD Double Density 8X Pluggable Transceiver",
    "type_abbrv_name": "QSFP-DD",
    "vdm_supported": "False",
    "vendor_date": "2024-12-14 ",
    "vendor_oui": "01-02-03",
    "vendor_rev": "01"
  },
  "TRANSCEIVER_STATUS_SW": {
    "cmis_state": "READY",
    "error": "N/A",
    "status": "1"
  }
}"#;

/// `get_transceiver_info()` as the bridge/`sonic_platform` deliver it for the
/// golden module — i.e. BEFORE the daemon's `str()` rendering. CMIS strings are
/// NUL-padded (`model`), `vendor_date` carries a trailing space, `cable_length`
/// is a float, `vdm_supported` a bool, and `application_advertisement` is the
/// nested app dict (int selector key, decode-order inner fields). `is_replaceable`
/// is NOT here (the daemon appends it). `active_apsel_hostlaneN`, `host_lane_count`,
/// and `media_lane_count` arrive as raw NUMERIC values (the emulated
/// `get_transceiver_info` embeds them) — the daemon must NOT leak these into
/// TRANSCEIVER_INFO; the CMIS manager owns them as 'N/A' until the datapath
/// activates. This is what a real `MockSfp` mirrors.
fn emulator_identity() -> Value {
    json!({
        "active_apsel_hostlane1": 1,
        "active_apsel_hostlane2": 1,
        "active_apsel_hostlane3": 1,
        "active_apsel_hostlane4": 1,
        "active_apsel_hostlane5": 0,
        "active_apsel_hostlane6": 0,
        "active_apsel_hostlane7": 0,
        "active_apsel_hostlane8": 0,
        "application_advertisement": {
            "1": {
                "host_electrical_interface_id": "XLAUI C2M (Annex 83B)",
                "module_media_interface_id": "40GBASE-LR4 (Cl 87)",
                "media_lane_count": 4,
                "host_lane_count": 4,
                "host_lane_assignment_options": 1,
                "media_lane_assignment_options": 1
            }
        },
        "cable_length": 100.0,
        "cable_type": "Length Cable Assembly(m)",
        "cmis_rev": "5.2",
        "connector": "MPO 1x16",
        "encoding": "N/A",
        "ext_identifier": "Power Class 8 (10.0W Max)",
        "ext_rateselect_compliance": "N/A",
        "hardware_rev": "0.0",
        "host_lane_count": 4,
        "manufacturer": "xcvr-emu",
        "media_interface_technology": "850 nm VCSEL",
        "media_lane_count": 4,
        "model": "EMU-40G-LR4\u{0}\u{0}\u{0}\u{0}\u{0}",
        "nominal_bit_rate": "N/A",
        "serial": "0123456789",
        "specification_compliance": "sm_media_interface",
        "type": "QSFP-DD Double Density 8X Pluggable Transceiver",
        "type_abbrv_name": "QSFP-DD",
        "vdm_supported": false,
        "vendor_date": "2024-12-14 ",
        "vendor_oui": "01-02-03",
        "vendor_rev": "01"
    })
}

/// `get_transceiver_threshold_info()` as the pyo3 stringifying reader
/// (`hal::read_thresholds_stringified`) yields it: every value already `str()`-ed
/// Python-side (so the -inf power thresholds survive) — `"0.0"` / `"-inf"`.
fn emulator_thresholds() -> Value {
    json!({
        "temphighalarm": "0.0", "temphighwarning": "0.0",
        "templowalarm": "0.0", "templowwarning": "0.0",
        "vcchighalarm": "0.0", "vcchighwarning": "0.0",
        "vcclowalarm": "0.0", "vcclowwarning": "0.0",
        "rxpowerhighalarm": "-inf", "rxpowerhighwarning": "-inf",
        "rxpowerlowalarm": "-inf", "rxpowerlowwarning": "-inf",
        "txbiashighalarm": "0.0", "txbiashighwarning": "0.0",
        "txbiaslowalarm": "0.0", "txbiaslowwarning": "0.0",
        "txpowerhighalarm": "-inf", "txpowerhighwarning": "-inf",
        "txpowerlowalarm": "-inf", "txpowerlowwarning": "-inf",
        "lasertemphighalarm": "0.0", "lasertemphighwarning": "0.0",
        "lasertemplowalarm": "0.0", "lasertemplowwarning": "0.0"
    })
}

fn mapping() -> PortMapping {
    let mut pm = PortMapping::new();
    pm.handle_port_change_event(&PortChangeEvent::new(PORT, PHYS, 0, PortChangeEventType::Add));
    pm
}

/// A row's golden projection: drop the volatile `last_update_time` (matches
/// `lib/golden.py` VOLATILE_KEYS).
fn projected(row: &Row) -> BTreeMap<String, String> {
    let mut m = row.clone();
    m.remove("last_update_time");
    m
}

/// The golden table as a `{field: value}` map.
fn golden_table(golden: &Value, table: &str) -> BTreeMap<String, String> {
    golden[table]
        .as_object()
        .expect("golden table object")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().expect("golden value string").to_string()))
        .collect()
}

/// The full projection reproduces `golden/Ethernet100.json` field-for-field
/// (minus `last_update_time`): identity rendering (NUL-strip, trailing space,
/// Python-repr application_advertisement, str(bool)/str(float)), SW status/error +
/// cmis_state READY, and the DOM thresholds incl. `-inf`.
#[test]
fn golden_projection_matches_reference() {
    let golden: Value = serde_json::from_str(GOLDEN).expect("parse embedded golden");

    let db = MockStateDb::new();
    let mut sfp = MockSfp::present(emulator_identity());
    sfp.replaceable = true;
    sfp.threshold_info = Some(emulator_thresholds());
    let pm = mapping();

    // TRANSCEIVER_INFO — post_port_sfp_info_to_db (CMIS branch: every field via
    // str(value) + is_replaceable, EXCEPT active_apsel_hostlaneN which it must not
    // leak). The CMIS manager then owns the active-apsel projection.
    let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
    post_port_sfp_info_to_db(PORT, &pm, &intf, &sfp).unwrap();
    // The daemon must not publish the raw numeric active_apsel / lane counts carried
    // in the identity dict — those TRANSCEIVER_INFO fields are owned by the CMIS
    // manager (it publishes 'N/A' until the datapath activates).
    assert!(
        intf.hget(PORT, "active_apsel_hostlane1").unwrap().is_none(),
        "post_port_sfp_info_to_db leaked active_apsel into TRANSCEIVER_INFO"
    );
    for field in ["host_lane_count", "media_lane_count"] {
        assert!(
            intf.hget(PORT, field).unwrap().is_none(),
            "post_port_sfp_info_to_db leaked {field} into TRANSCEIVER_INFO"
        );
    }

    // TRANSCEIVER_STATUS_SW — status=1/error=N/A (SfpStateUpdateTask) + cmis_state
    // READY (CmisManagerTask, read-modify-write merge).
    let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
    update_port_transceiver_status_table_sw(PORT, &sw, "1", NO_ERROR).unwrap();
    let cmis = CmisManagerTask::new(
        MockHal::with_ports(0),
        db.clone(),
        pm.clone(),
        Arc::new(AtomicBool::new(false)),
        false,
    );
    cmis.update_port_transceiver_status_table_sw_cmis_state(PORT, CmisState::Ready)
        .unwrap();
    // TRANSCEIVER_INFO.active_apsel_hostlaneN — owned by the CMIS manager: the
    // emulated 40G-LR4 has no active datapath at capture (host_lanes_mask=0), so
    // every host lane is 'N/A' (post_port_active_apsel_to_db, reset path).
    cmis.post_port_active_apsel_to_db(PORT, 0, &BTreeMap::new(), true)
        .unwrap();

    // TRANSCEIVER_DOM_THRESHOLD — post_port_dom_thresholds_to_db (present module).
    let thr = db.table(TRANSCEIVER_DOM_THRESHOLD_TABLE).unwrap();
    assert!(DomDbUtils::post_port_dom_thresholds_to_db(PORT, &sfp, &thr).unwrap());

    // Compare each table's projection to the golden.
    assert_eq!(
        projected(&intf.get(PORT).unwrap().unwrap()),
        golden_table(&golden, "TRANSCEIVER_INFO"),
        "TRANSCEIVER_INFO projection diverged from golden"
    );
    assert_eq!(
        projected(&sw.get(PORT).unwrap().unwrap()),
        golden_table(&golden, "TRANSCEIVER_STATUS_SW"),
        "TRANSCEIVER_STATUS_SW projection diverged from golden"
    );
    assert_eq!(
        projected(&thr.get(PORT).unwrap().unwrap()),
        golden_table(&golden, "TRANSCEIVER_DOM_THRESHOLD"),
        "TRANSCEIVER_DOM_THRESHOLD projection diverged from golden"
    );
}

/// Guard the specific renderings the golden pins so a regression names the field:
/// the NUL-padded `model` is trimmed, `vendor_date` keeps its trailing space, and
/// `application_advertisement` is a Python `str(dict)` repr (not JSON).
#[test]
fn golden_info_field_renderings() {
    let db = MockStateDb::new();
    let sfp = MockSfp::present(emulator_identity());
    let pm = mapping();
    let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
    post_port_sfp_info_to_db(PORT, &pm, &intf, &sfp).unwrap();
    let row = intf.get(PORT).unwrap().unwrap();

    assert_eq!(row.get("model").map(String::as_str), Some("EMU-40G-LR4")); // NUL padding stripped
    assert_eq!(row.get("vendor_date").map(String::as_str), Some("2024-12-14 ")); // trailing space kept
    assert_eq!(row.get("cable_length").map(String::as_str), Some("100.0")); // str(float)
    assert_eq!(row.get("vdm_supported").map(String::as_str), Some("False")); // str(bool)
    assert_eq!(row.get("is_replaceable").map(String::as_str), Some("True")); // appended by daemon
    assert_eq!(
        row.get("application_advertisement").map(String::as_str),
        Some("{1: {'host_electrical_interface_id': 'XLAUI C2M (Annex 83B)', \
'module_media_interface_id': '40GBASE-LR4 (Cl 87)', 'media_lane_count': 4, \
'host_lane_count': 4, 'host_lane_assignment_options': 1, \
'media_lane_assignment_options': 1}}")
    );
}
