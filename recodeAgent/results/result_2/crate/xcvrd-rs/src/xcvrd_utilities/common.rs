//! Port of `xcvrd_utilities/common.py` — CMIS state constants, the SW-status
//! table writer, CMIS-state read-back, DB deletion helper, and the platform
//! wrapper helpers.

use std::collections::BTreeMap;

use crate::db::DbTable;
use crate::error::Result;
use crate::xcvrd_utilities::port_event_helper::PortMapping;

// --- CMIS states (STATE_DB `TRANSCEIVER_STATUS_SW.cmis_state` contract) ---------
pub const CMIS_STATE_UNKNOWN: &str = "UNKNOWN";
pub const CMIS_STATE_INSERTED: &str = "INSERTED";
pub const CMIS_STATE_DP_PRE_INIT_CHECK: &str = "DP_PRE_INIT_CHECK";
pub const CMIS_STATE_DP_DEINIT: &str = "DP_DEINIT";
pub const CMIS_STATE_AP_CONF: &str = "AP_CONFIGURED";
pub const CMIS_STATE_DP_ACTIVATE: &str = "DP_ACTIVATION";
pub const CMIS_STATE_DP_INIT: &str = "DP_INIT";
pub const CMIS_STATE_DP_TXON: &str = "DP_TXON";
pub const CMIS_STATE_READY: &str = "READY";
pub const CMIS_STATE_REMOVED: &str = "REMOVED";
pub const CMIS_STATE_FAILED: &str = "FAILED";

pub const NOT_IMPLEMENTED_ERROR: i32 = 3;

/// The ordered CMIS datapath bring-up states (`common.py:23`). `as_str` is the
/// STATE_DB contract string; the transition LOGIC lives in the CMIS manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmisState {
    Unknown,
    Inserted,
    DpPreInitCheck,
    DpDeinit,
    ApConfigured,
    DpInit,
    DpTxOn,
    DpActivation,
    Ready,
    Removed,
    Failed,
}

impl CmisState {
    /// The `cmis_state` string written to `TRANSCEIVER_STATUS_SW`.
    pub fn as_str(&self) -> &'static str {
        match self {
            CmisState::Unknown => CMIS_STATE_UNKNOWN,
            CmisState::Inserted => CMIS_STATE_INSERTED,
            CmisState::DpPreInitCheck => CMIS_STATE_DP_PRE_INIT_CHECK,
            CmisState::DpDeinit => CMIS_STATE_DP_DEINIT,
            CmisState::ApConfigured => CMIS_STATE_AP_CONF,
            CmisState::DpInit => CMIS_STATE_DP_INIT,
            CmisState::DpTxOn => CMIS_STATE_DP_TXON,
            CmisState::DpActivation => CMIS_STATE_DP_ACTIVATE,
            CmisState::Ready => CMIS_STATE_READY,
            CmisState::Removed => CMIS_STATE_REMOVED,
            CmisState::Failed => CMIS_STATE_FAILED,
        }
    }

    /// `CMIS_TERMINAL_STATES = {FAILED, READY, REMOVED}`.
    pub fn is_terminal(&self) -> bool {
        matches!(self, CmisState::Failed | CmisState::Ready | CmisState::Removed)
    }

    /// Parse the STATE_DB `cmis_state` string back into the enum (inverse of
    /// [`CmisState::as_str`]); an unrecognized/absent value is `Unknown`.
    pub fn from_db_str(s: &str) -> CmisState {
        match s {
            CMIS_STATE_INSERTED => CmisState::Inserted,
            CMIS_STATE_DP_PRE_INIT_CHECK => CmisState::DpPreInitCheck,
            CMIS_STATE_DP_DEINIT => CmisState::DpDeinit,
            CMIS_STATE_AP_CONF => CmisState::ApConfigured,
            CMIS_STATE_DP_INIT => CmisState::DpInit,
            CMIS_STATE_DP_TXON => CmisState::DpTxOn,
            CMIS_STATE_DP_ACTIVATE => CmisState::DpActivation,
            CMIS_STATE_READY => CmisState::Ready,
            CMIS_STATE_REMOVED => CmisState::Removed,
            CMIS_STATE_FAILED => CmisState::Failed,
            _ => CmisState::Unknown,
        }
    }
}

/// `update_port_transceiver_status_table_sw` - write `status` + `error` to
/// `TRANSCEIVER_STATUS_SW` on an SFP change event (`common.py:110`). The real DB
/// `set` merges, so this never clobbers `cmis_state`.
pub fn update_port_transceiver_status_table_sw(
    logical_port_name: &str,
    status_sw_tbl: &dyn DbTable,
    status: &str,
    error_descriptions: &str,
) {
    status_sw_tbl.set(
        logical_port_name,
        &[
            ("status".to_string(), status.to_string()),
            ("error".to_string(), error_descriptions.to_string()),
        ],
    );
}

/// `get_cmis_state_from_state_db` - read `cmis_state` back (`UNKNOWN` if absent)
/// (`common.py:259`).
pub fn get_cmis_state_from_state_db(lport: &str, status_sw_tbl: &dyn DbTable) -> String {
    status_sw_tbl
        .hget(lport, "cmis_state")
        .unwrap_or_else(|| CMIS_STATE_UNKNOWN.to_string())
}

/// `get_physical_port_name` (`common.py:268`) - the STATE_DB row key for a
/// (logical, physical) pair; ganged members get a suffix.
pub fn get_physical_port_name(logical_port: &str, physical_port: usize, ganged: bool) -> String {
    if ganged {
        format!("{logical_port}:{physical_port} (ganged)")
    } else {
        logical_port.to_string()
    }
}

/// `get_physical_port_name_dict` (`common.py:275`) - map each physical port of a
/// logical port to its STATE_DB row key.
pub fn get_physical_port_name_dict(
    logical_port_name: &str,
    port_mapping: &PortMapping,
) -> BTreeMap<usize, String> {
    let mut out = BTreeMap::new();
    let physical_port_list = match port_mapping.logical_port_name_to_physical_port_list(logical_port_name) {
        Some(list) => list,
        None => return out,
    };
    let ganged = physical_port_list.len() > 1;
    let mut ganged_member_num = 1;
    for physical_port in physical_port_list {
        let name = get_physical_port_name(logical_port_name, ganged_member_num, ganged);
        ganged_member_num += 1;
        out.insert(physical_port, name);
    }
    out
}

/// `del_port_sfp_dom_info_from_db` (`common.py:335`) - delete a port's row from each
/// table in the provided set (used on remove / logical-port teardown).
pub fn del_port_sfp_dom_info_from_db(
    logical_port_name: &str,
    port_mapping: &PortMapping,
    tbl_to_del_list: &[&dyn DbTable],
) {
    let names = get_physical_port_name_dict(logical_port_name, port_mapping);
    for physical_port_name in names.values() {
        for tbl in tbl_to_del_list {
            tbl.del(physical_port_name);
        }
    }
}

/// `is_fast_reboot_enabled` — read `FAST_RESTART_ENABLE_TABLE|system.enable`
/// directly via the DB seam (no `sonic-db-cli` subprocess, analysis §1.6).
pub fn is_fast_reboot_enabled(_state_db: &dyn DbTable) -> bool {
    todo!("common.py:is_fast_reboot_enabled -> STATE_DB read")
}

/// `is_syncd_warm_restore_complete` — restore_count>0 or system enable==true.
pub fn is_syncd_warm_restore_complete(_state_db: &dyn DbTable) -> bool {
    todo!("common.py:is_syncd_warm_restore_complete")
}

/// `get_cmis_application_desired` — match host-lane-count + speed against the
/// module application advertisement (returns the app code).
pub fn get_cmis_application_desired(
    _appl_advert: &serde_json::Value,
    _host_lane_count: u32,
    _speed: u32,
) -> Option<u32> {
    todo!("common.py:get_cmis_application_desired")
}

/// `get_interface_speed` — parse the host-electrical-interface id → bps.
pub fn get_interface_speed(_ifname: &str) -> u32 {
    todo!("common.py:get_interface_speed")
}

/// `_wrapper_get_presence` HAL passthrough (the trait already gives presence; kept
/// for name traceability).
pub fn wrapper_get_presence(sfp: &dyn crate::hal::SfpHandle) -> Result<bool> {
    sfp.get_presence()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockDbTable;
    use crate::xcvrd_utilities::port_event_helper::{
        PortChangeEvent, PortChangeEventType,
    };
    use crate::xcvrd_utilities::sfp_status_helper::{SFP_STATUS_INSERTED, SFP_STATUS_REMOVED};

    // Port of common.py:update_port_transceiver_status_table_sw semantics: writes
    // status + error, and (real-DB merge) preserves a previously-set cmis_state.
    #[test]
    fn test_update_port_transceiver_status_table_sw() {
        let tbl = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        update_port_transceiver_status_table_sw("Ethernet0", &tbl, SFP_STATUS_INSERTED, "N/A");
        assert_eq!(tbl.hget("Ethernet0", "status").as_deref(), Some("1"));
        assert_eq!(tbl.hget("Ethernet0", "error").as_deref(), Some("N/A"));

        update_port_transceiver_status_table_sw("Ethernet0", &tbl, SFP_STATUS_REMOVED, "N/A");
        assert_eq!(tbl.hget("Ethernet0", "status").as_deref(), Some("0"));
    }

    // Port of common.py:get_cmis_state_from_state_db: UNKNOWN when absent, else the
    // stored value.
    #[test]
    fn test_get_cmis_state_from_state_db() {
        let tbl = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        assert_eq!(get_cmis_state_from_state_db("Ethernet0", &tbl), "UNKNOWN");
        tbl.hset("Ethernet0", "cmis_state", "READY");
        assert_eq!(get_cmis_state_from_state_db("Ethernet0", &tbl), "READY");
    }

    #[test]
    fn test_get_physical_port_name() {
        assert_eq!(get_physical_port_name("Ethernet0", 1, false), "Ethernet0");
        assert_eq!(
            get_physical_port_name("Ethernet0", 2, true),
            "Ethernet0:2 (ganged)"
        );
    }

    // Port of common.py:del_port_sfp_dom_info_from_db: deletes the port's row from
    // every supplied table.
    #[test]
    fn test_del_port_sfp_dom_info_from_db() {
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet0",
            0,
            0,
            PortChangeEventType::PortAdd,
        ));
        let intf = MockDbTable::new("TRANSCEIVER_INFO");
        let dom = MockDbTable::new("TRANSCEIVER_DOM_SENSOR");
        intf.set("Ethernet0", &[("manufacturer".to_string(), "xcvr-emu".to_string())]);
        dom.set("Ethernet0", &[("temperature".to_string(), "22.0".to_string())]);

        del_port_sfp_dom_info_from_db("Ethernet0", &pm, &[&intf, &dom]);
        assert!(intf.get("Ethernet0").is_none());
        assert!(dom.get("Ethernet0").is_none());
    }
}
