//! VDM DB posters — port of `dom/utilities/vdm/db_utils.py` (`VDMDBUtils`).
//! Posts `TRANSCEIVER_VDM_*` real values / thresholds / flags. Stubs only.

#![allow(dead_code, unused_variables)]

use crate::hal::SfpApi;
use crate::statedb::{DbError, TableApi};

/// `VDMDBUtils` (`vdm/db_utils.py:8`).
pub struct VdmDbUtils;

impl VdmDbUtils {
    /// `post_port_vdm_real_values_from_dict_to_db` -> `TRANSCEIVER_VDM_REAL_VALUE`.
    pub fn post_port_vdm_real_values_from_dict_to_db<T: TableApi>(
        logical_port_name: &str,
        real_value_tbl: &T,
    ) -> Result<(), DbError> {
        todo!("later: VdmDbUtils::post_port_vdm_real_values_from_dict_to_db")
    }

    /// `post_port_vdm_thresholds_to_db` -> `TRANSCEIVER_VDM_*_THRESHOLD`.
    pub fn post_port_vdm_thresholds_to_db<S: SfpApi, T: TableApi>(
        logical_port_name: &str,
        sfp: &S,
        threshold_tbl: &T,
    ) -> Result<(), DbError> {
        todo!("later: VdmDbUtils::post_port_vdm_thresholds_to_db")
    }

    /// `post_port_vdm_flags_to_db` (+ flag metadata).
    pub fn post_port_vdm_flags_to_db<S: SfpApi, T: TableApi>(
        logical_port_name: &str,
        sfp: &S,
        flag_tbl: &T,
    ) -> Result<(), DbError> {
        todo!("later: VdmDbUtils::post_port_vdm_flags_to_db")
    }
}
