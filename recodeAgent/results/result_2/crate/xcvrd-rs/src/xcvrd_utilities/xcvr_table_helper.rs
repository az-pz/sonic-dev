//! Port of `xcvrd_utilities/xcvr_table_helper.py` — the central STATE_DB table
//! registry. Table-name constants are the authoritative STATE_DB contract (§1.4)
//! and are reproduced verbatim; `XcvrTableHelper` is the per-(asic, table) handle
//! factory the daemon/posters obtain tables from.

// --- STATE_DB table names (contract data; verbatim from the Python module) -----
pub const TRANSCEIVER_INFO_TABLE: &str = "TRANSCEIVER_INFO";
pub const TRANSCEIVER_FIRMWARE_INFO_TABLE: &str = "TRANSCEIVER_FIRMWARE_INFO";
pub const TRANSCEIVER_DOM_SENSOR_TABLE: &str = "TRANSCEIVER_DOM_SENSOR";
pub const TRANSCEIVER_DOM_FLAG_TABLE: &str = "TRANSCEIVER_DOM_FLAG";
pub const TRANSCEIVER_DOM_FLAG_CHANGE_COUNT_TABLE: &str = "TRANSCEIVER_DOM_FLAG_CHANGE_COUNT";
pub const TRANSCEIVER_DOM_FLAG_SET_TIME_TABLE: &str = "TRANSCEIVER_DOM_FLAG_SET_TIME";
pub const TRANSCEIVER_DOM_FLAG_CLEAR_TIME_TABLE: &str = "TRANSCEIVER_DOM_FLAG_CLEAR_TIME";
pub const TRANSCEIVER_DOM_THRESHOLD_TABLE: &str = "TRANSCEIVER_DOM_THRESHOLD";
pub const TRANSCEIVER_DOM_TEMPERATURE_TABLE: &str = "TRANSCEIVER_DOM_TEMPERATURE";
pub const TRANSCEIVER_STATUS_TABLE: &str = "TRANSCEIVER_STATUS";
pub const TRANSCEIVER_STATUS_FLAG_TABLE: &str = "TRANSCEIVER_STATUS_FLAG";
pub const TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT_TABLE: &str = "TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT";
pub const TRANSCEIVER_STATUS_FLAG_SET_TIME_TABLE: &str = "TRANSCEIVER_STATUS_FLAG_SET_TIME";
pub const TRANSCEIVER_STATUS_FLAG_CLEAR_TIME_TABLE: &str = "TRANSCEIVER_STATUS_FLAG_CLEAR_TIME";
pub const TRANSCEIVER_STATUS_SW_TABLE: &str = "TRANSCEIVER_STATUS_SW";
pub const TRANSCEIVER_VDM_REAL_VALUE_TABLE: &str = "TRANSCEIVER_VDM_REAL_VALUE";
pub const TRANSCEIVER_PM_TABLE: &str = "TRANSCEIVER_PM";

pub const NPU_SI_SETTINGS_SYNC_STATUS_KEY: &str = "NPU_SI_SETTINGS_SYNC_STATUS";
pub const NPU_SI_SETTINGS_DEFAULT_VALUE: &str = "NPU_SI_SETTINGS_DEFAULT";
pub const NPU_SI_SETTINGS_NOTIFIED_VALUE: &str = "NPU_SI_SETTINGS_NOTIFIED";

/// The four VDM threshold/flag fan-out types (`['halarm','lalarm','hwarn','lwarn']`).
/// The VDM tables are `TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_{THRESHOLD,FLAG}`
/// (+ `_CHANGE_COUNT/_SET_TIME/_CLEAR_TIME`).
pub const VDM_THRESHOLD_TYPES: [&str; 4] = ["halarm", "lalarm", "hwarn", "lwarn"];

/// `TRANSCEIVER_VDM_{TYPE}_THRESHOLD` — per-type VDM threshold value table name
/// (`xcvr_table_helper.py:get_vdm_threshold_tbl`, type upper-cased).
pub fn vdm_threshold_table_name(threshold_type: &str) -> String {
    format!("TRANSCEIVER_VDM_{}_THRESHOLD", threshold_type.to_uppercase())
}

/// `TRANSCEIVER_VDM_{TYPE}_FLAG` — per-type VDM flag value table name.
pub fn vdm_flag_table_name(threshold_type: &str) -> String {
    format!("TRANSCEIVER_VDM_{}_FLAG", threshold_type.to_uppercase())
}

/// `TRANSCEIVER_VDM_{TYPE}_FLAG_CHANGE_COUNT` — per-type VDM flag change-count table.
pub fn vdm_flag_change_count_table_name(threshold_type: &str) -> String {
    format!(
        "TRANSCEIVER_VDM_{}_FLAG_CHANGE_COUNT",
        threshold_type.to_uppercase()
    )
}

/// `TRANSCEIVER_VDM_{TYPE}_FLAG_SET_TIME` — per-type VDM flag last-set-time table.
pub fn vdm_flag_set_time_table_name(threshold_type: &str) -> String {
    format!(
        "TRANSCEIVER_VDM_{}_FLAG_SET_TIME",
        threshold_type.to_uppercase()
    )
}

/// `TRANSCEIVER_VDM_{TYPE}_FLAG_CLEAR_TIME` — per-type VDM flag last-clear-time table.
pub fn vdm_flag_clear_time_table_name(threshold_type: &str) -> String {
    format!(
        "TRANSCEIVER_VDM_{}_FLAG_CLEAR_TIME",
        threshold_type.to_uppercase()
    )
}

use std::sync::{Arc, Mutex};

use crate::db::{DbTable, RealDbTable};
use crate::error::Result;
use swss_common::DbConnector;

/// Per-(asic, table) STATE_DB/APPL_DB handle factory (`XcvrTableHelper`). Built
/// against the [`DbTable`] seam so production uses `swss-common` and tests use
/// `MockDbTable`.
///
/// TODO(Translator): build a table per name (single-ASIC: `asic_id=0`) and expose
/// the `get_*_tbl(asic_id)` accessors + `VDM_THRESHOLD_TYPES` fan-out.
pub struct XcvrTableHelper {
    // TODO(Translator): map<table-name, Box<dyn DbTable>> per asic_id.
}

impl XcvrTableHelper {
    pub fn new() -> Result<Self> {
        todo!("open a DbTable per STATE_DB table name (xcvr_table_helper.py:__init__)")
    }

    pub fn get_intf_tbl(&self, _asic_id: usize) -> &dyn DbTable {
        todo!("TRANSCEIVER_INFO table accessor")
    }

    pub fn get_dom_tbl(&self, _asic_id: usize) -> &dyn DbTable {
        todo!("TRANSCEIVER_DOM_SENSOR table accessor")
    }

    pub fn get_status_sw_tbl(&self, _asic_id: usize) -> &dyn DbTable {
        todo!("TRANSCEIVER_STATUS_SW table accessor")
    }

    /// `is_npu_si_settings_update_required` — DEFAULT/absent ⇒ update needed.
    pub fn is_npu_si_settings_update_required(&self, _lport: &str) -> bool {
        todo!("xcvr_table_helper.py:is_npu_si_settings_update_required")
    }
}

/// Convenience: open one real STATE_DB table by name over a shared connection
/// (skeleton passthrough).
pub fn open_table(conn: Arc<Mutex<DbConnector>>, name: &str) -> Result<RealDbTable> {
    crate::db::open_state_table(conn, name)
}
