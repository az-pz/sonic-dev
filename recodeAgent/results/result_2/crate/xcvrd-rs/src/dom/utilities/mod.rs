//! `dom/utilities/` package — the STATE_DB posters built on the common
//! `post_diagnostic_values_to_db` pattern (validate → read dict → beautify →
//! append `last_update_time` → `table.set`), with flag-metadata siblings.

pub mod db;
pub mod dom_sensor;
pub mod status;
pub mod vdm;
