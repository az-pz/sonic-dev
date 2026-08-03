//! DOM subsystem — port of the Python `dom/` subpackage.
//!
//! - `dom_mgr`   <- dom/dom_mgr.py (DomInfoUpdateTask + DomThermalInfoUpdateTask)
//! - `utilities` <- dom/utilities/** (DB writer engine + DOM/status/VDM readers)

pub mod dom_mgr;
pub mod utilities;
