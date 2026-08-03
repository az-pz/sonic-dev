//! VDM HAL readers — port of `dom/utilities/vdm/utils.py` (`VDMUtils`).
//! VDM real values, thresholds, flags, and the freeze/unfreeze stats context.
//! Out of the near-term oracle scope; stubs only.

#![allow(dead_code, unused_variables)]

use serde_json::Value;

use crate::hal::SfpApi;

/// `VDMUtils` (`vdm/utils.py:8`).
pub struct VdmUtils;

impl VdmUtils {
    /// `is_transceiver_vdm_supported`.
    pub fn is_transceiver_vdm_supported<S: SfpApi>(sfp: &S) -> bool {
        todo!("later: VdmUtils::is_transceiver_vdm_supported")
    }

    /// `get_vdm_real_values_basic`.
    pub fn get_vdm_real_values_basic<S: SfpApi>(sfp: &S) -> Value {
        todo!("later: VdmUtils::get_vdm_real_values_basic")
    }

    /// `get_vdm_thresholds`.
    pub fn get_vdm_thresholds<S: SfpApi>(sfp: &S) -> Value {
        todo!("later: VdmUtils::get_vdm_thresholds")
    }

    /// `get_vdm_flags`.
    pub fn get_vdm_flags<S: SfpApi>(sfp: &S) -> Value {
        todo!("later: VdmUtils::get_vdm_flags")
    }
}
