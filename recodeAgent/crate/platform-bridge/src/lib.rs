//! platform-bridge — PyO3 wrappers around the Python `sonic_platform` plugin.
//!
//! Placeholder for the harness spine proof. In the bridge-implementation phase
//! this exposes Chassis/Sfp (get_presence, get_transceiver_info,
//! get_transceiver_dom_real_value, get_transceiver_status, get_change_event, ...)
//! as typed Rust structs backed by PyO3 calls into the deployed sonic_platform.

/// Trivial marker so the crate compiles standalone.
pub fn bridge_version() -> &'static str {
    "0.1.0-skeleton"
}
