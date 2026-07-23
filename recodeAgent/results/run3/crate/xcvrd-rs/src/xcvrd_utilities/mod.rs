//! `xcvrd_utilities` — shared helpers, mirroring the Python subpackage of the
//! same name (`source/xcvrd/xcvrd_utilities/`).
//!
//! Each submodule maps 1:1 to a Python file:
//! - `common`               <- common.py (CMIS state, SW-status + HAL wrappers)
//! - `port_event_helper`    <- port_event_helper.py (PortMapping, get_port_mapping)
//! - `sfp_status_helper`    <- sfp_status_helper.py (error bitmasks + descriptions)
//! - `xcvr_table_helper`    <- xcvr_table_helper.py (TRANSCEIVER_* table names)
//! - `media_settings_parser`<- media_settings_parser.py (out of oracle scope)
//! - `optics_si_parser`     <- optics_si_parser.py (out of oracle scope)
//! - `utils`                <- utils.py (presence / flat-memory / lpmode helpers)

pub mod common;
pub mod media_settings_parser;
pub mod optics_si_parser;
pub mod port_event_helper;
pub mod sfp_status_helper;
pub mod utils;
pub mod xcvr_table_helper;
