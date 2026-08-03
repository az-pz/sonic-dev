//! `xcvrd_utilities/` package (analysis §3.2) — shared helpers: CMIS-state
//! constants + DB/wrapper helpers (`common`), the STATE_DB table-name registry
//! (`xcvr_table_helper`), logical↔physical port mapping + change observer
//! (`port_event_helper`), SFP error-bitmap decode (`sfp_status_helper`), the
//! presence/flat-memory/lpmode SFP helper (`utils`), and the media/optics-SI
//! parsers.

pub mod common;
pub mod media_settings_parser;
pub mod optics_si_parser;
pub mod port_event_helper;
pub mod sfp_status_helper;
pub mod utils;
pub mod xcvr_table_helper;
