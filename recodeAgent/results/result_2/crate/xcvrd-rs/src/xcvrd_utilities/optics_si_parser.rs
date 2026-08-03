//! Port of `xcvrd_utilities/optics_si_parser.py` — stage per-vendor MODULE optics-SI
//! from `optics_si_settings.json` into the page-10h Staged Control Set (flipping
//! ExplicitControl) during CMIS AP_CONF (M11).

use crate::error::Result;

/// `fetch_optics_si_setting` — resolve the optics-SI entry for a module/vendor.
///
/// TODO(Translator): port the optics_si_settings.json lookup + vendor-key matching.
pub fn fetch_optics_si_setting(
    _physical_port: usize,
    _lane_speed: u32,
    _vendor_key: &str,
) -> Result<serde_json::Value> {
    todo!("optics_si_parser.py:fetch_optics_si_setting")
}

/// `get_module_vendor_key` — `(vendor_name, vendor_pn)` key for a module.
///
/// TODO(Translator): port `get_module_vendor_key`.
pub fn get_module_vendor_key(_physical_port: usize) -> Option<String> {
    todo!("optics_si_parser.py:get_module_vendor_key")
}

/// `optics_si_present` — whether any optics-SI settings were loaded.
pub fn optics_si_present() -> bool {
    todo!("optics_si_parser.py:optics_si_present")
}
