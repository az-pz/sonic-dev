//! STATE_DB table registry — port of `xcvrd_utilities/xcvr_table_helper.py`.
//!
//! The Python `XcvrTableHelper` opens one `swsscommon.Table` per `TRANSCEIVER_*`
//! table per ASIC. Here the table names are exact constants and the helper is a
//! thin generic over a `StateDb` seam: each getter maps a name to a `Table`
//! handle. Single-ASIC on the testbed, so `asic_id` is elided.

#![allow(dead_code, unused_variables)]

use crate::statedb::{DbError, StateDb};

// --- Table name constants (xcvr_table_helper.py:11-47) ---------------------
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

/// VDM threshold sub-types (`halarm`, `lalarm`, `hwarn`, `lwarn`).
pub const VDM_THRESHOLD_TYPES: [&str; 4] = ["halarm", "lalarm", "hwarn", "lwarn"];

/// Thin table registry over a `StateDb` seam (the `XcvrTableHelper` role).
pub struct XcvrTableHelper<D: StateDb> {
    db: D,
}

impl<D: StateDb> XcvrTableHelper<D> {
    pub fn new(db: D) -> Self {
        Self { db }
    }

    fn tbl(&self, name: &str) -> Result<D::Table, DbError> {
        self.db.table(name)
    }

    pub fn get_intf_tbl(&self) -> Result<D::Table, DbError> {
        self.tbl(TRANSCEIVER_INFO_TABLE)
    }
    pub fn get_status_sw_tbl(&self) -> Result<D::Table, DbError> {
        self.tbl(TRANSCEIVER_STATUS_SW_TABLE)
    }
    pub fn get_dom_tbl(&self) -> Result<D::Table, DbError> {
        self.tbl(TRANSCEIVER_DOM_SENSOR_TABLE)
    }
    pub fn get_dom_threshold_tbl(&self) -> Result<D::Table, DbError> {
        self.tbl(TRANSCEIVER_DOM_THRESHOLD_TABLE)
    }
    pub fn get_dom_temperature_tbl(&self) -> Result<D::Table, DbError> {
        self.tbl(TRANSCEIVER_DOM_TEMPERATURE_TABLE)
    }
    pub fn get_status_tbl(&self) -> Result<D::Table, DbError> {
        self.tbl(TRANSCEIVER_STATUS_TABLE)
    }
    pub fn get_pm_tbl(&self) -> Result<D::Table, DbError> {
        self.tbl(TRANSCEIVER_PM_TABLE)
    }
    pub fn get_firmware_info_tbl(&self) -> Result<D::Table, DbError> {
        self.tbl(TRANSCEIVER_FIRMWARE_INFO_TABLE)
    }
    // TODO(translator): flag/vdm table getters as later milestones need them.
}
