//! Port of `xcvrd.dom.utilities.vdm`.
pub mod utils;
pub mod db_utils;

/// `xcvr_table_helper.VDM_THRESHOLD_TYPES` — the four VDM threshold/flag categories
/// as the lower-case key tokens that appear inside the raw HAL dict keys (e.g.
/// `laser_temperature_media_halarm1`). The STATE_DB table names use the upper-cased
/// form (`TRANSCEIVER_VDM_HALARM_THRESHOLD`, …). The order is significant: the
/// value-write loop posts each type's row in this order and stops at the first empty
/// category (faithful to `_post_port_vdm_thresholds_or_flags_to_db`).
pub const VDM_THRESHOLD_TYPES: [&str; 4] = ["halarm", "lalarm", "hwarn", "lwarn"];
