//! Port of `xcvrd_utilities/media_settings_parser.py` — resolve NPU/ASIC-side media
//! SerDes settings from `media_settings.json` and publish them to APPL_DB
//! `PORT_TABLE`, seeding STATE_DB `PORT_TABLE.NPU_SI_SETTINGS_SYNC_STATUS`
//! DEFAULT→NOTIFIED as the idempotency guard (M11).

use crate::error::Result;

/// `notify_media_setting` — push serdes SI for a port to APPL_DB PORT_TABLE (once
/// per port).
///
/// TODO(Translator): port the media_settings.json lookup (global/port/custom
/// parsers), lane-value serialization, and the DEFAULT→NOTIFIED guard.
pub fn notify_media_setting(_logical_port_name: &str, _physical_port: usize) -> Result<()> {
    todo!("media_settings_parser.py:notify_media_setting")
}

/// `MediaSettingsParserBase` variants (global / port / custom).
///
/// TODO(Translator): port `GlobalMediaSettingsParser` / `PortMediaSettingsParser` /
/// `CustomMediaSettingsParser` (`parse`, `get_media_settings`, `to_db_value`).
pub struct MediaSettingsParser;
