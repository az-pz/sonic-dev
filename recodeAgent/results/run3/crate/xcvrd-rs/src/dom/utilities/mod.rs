//! DOM utilities — port of `dom/utilities/`.
//!
//! - `db`         <- dom/utilities/db/utils.py (generic DB writer + flag engine)
//! - `dom_sensor` <- dom/utilities/dom_sensor/{utils.py,db_utils.py}
//! - `status`     <- dom/utilities/status/{utils.py,db_utils.py}
//! - `vdm`        <- dom/utilities/vdm/{utils.py,db_utils.py}

pub mod db;
pub mod dom_sensor;
pub mod status;
pub mod vdm;
