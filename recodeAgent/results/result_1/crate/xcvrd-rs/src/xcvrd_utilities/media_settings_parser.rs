//! Media SI settings — port of `xcvrd_utilities/media_settings_parser.py`.
//!
//! Parses `media_settings.json` -> APPL_DB PORT SI. Out of the `xcvrd-tests`
//! oracle scope (README §5); stubbed first, lowest priority. Present so the
//! package layout mirrors Python and later milestones can grow it.

#![allow(dead_code, unused_variables)]

/// `notify_media_setting` (`media_settings_parser.py:554`): compute + publish SI.
pub fn notify_media_setting(logical_port_name: &str) {
    // TODO(translator): out-of-oracle-scope; implement only if a gate needs it.
    todo!("late: notify_media_setting")
}

/// `load_media_settings` (`media_settings_parser.py:296`).
pub fn load_media_settings() {
    todo!("late: load_media_settings")
}
