//! xcvrd daemon core — port of `xcvrd.py` (the `DaemonXcvrd` orchestrator +
//! `post_port_sfp_info_to_db`).
//!
//! `sfp_state_update` holds the presence/hot-plug event loop (`SfpStateUpdateTask`).
//!
//! NOTE: the deployed M0/M1 binary still runs the bootstrap `crate::daemon::run`
//! (untouched, keeps the deploy-smoke + M1 gate green). `Daemon` here is the
//! FUTURE multi-thread orchestrator the Translator grows into `main`/`daemon.rs`
//! as the milestones add tasks. Everything below is a stub.

#![allow(dead_code, unused_variables)]

pub mod sfp_state_update;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::dom::dom_mgr::DomInfoUpdateTask;
use crate::hal::{Hal, SfpApi};
use crate::statedb::{DbError, Row, StateDb, TableApi};
use crate::xcvrd_utilities::common::{
    del_port_sfp_dom_info_from_db, is_cmis_manager_owned_field, pybool, stringify_field,
    wrapper_get_presence,
};
use crate::xcvrd_utilities::port_event_helper::{get_port_mapping, PortMapping};
use crate::xcvrd_utilities::xcvr_table_helper::TRANSCEIVER_INFO_TABLE;
use sfp_state_update::SfpStateUpdateTask;

/// Result of `post_port_sfp_info_to_db` (Python return codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostSfpInfoResult {
    /// Row written (or skipped because absent) — Python `None`.
    Ok,
    /// `PHYSICAL_PORT_NOT_EXIST`: no physical port for the logical name.
    PhysicalPortNotExist,
    /// `SFP_EEPROM_NOT_READY`: identity unreadable, retry later.
    EepromNotReady,
}

/// `post_port_sfp_info_to_db` (`xcvrd.py:178`): publish one present port's
/// identity to `TRANSCEIVER_INFO`. CMIS branch (has `cmis_rev`) writes every
/// field as `str(value)` + `is_replaceable`; else the fixed SFF field list.
/// (Bootstrap `daemon.rs::sync_port` already reproduces the CMIS branch.) [M1]
pub fn post_port_sfp_info_to_db<S: SfpApi, T: TableApi>(
    logical_port_name: &str,
    port_mapping: &PortMapping,
    intf_tbl: &T,
    sfp: &S,
) -> Result<PostSfpInfoResult, DbError> {
    let physical_port_list = match port_mapping.logical_port_name_to_physical_port_list(logical_port_name) {
        Some(l) if !l.is_empty() => l,
        _ => return Ok(PostSfpInfoResult::PhysicalPortNotExist),
    };

    // Single-ASIC / non-ganged testbed: the caller has already resolved the one
    // `Sfp` for this logical port. Mirror the Python per-physical-port loop's
    // early-outs (absent -> skip; identity unreadable -> retry later).
    let _ = physical_port_list;
    if !wrapper_get_presence(sfp) {
        // Transceiver not present -> nothing published (Python `continue`).
        return Ok(PostSfpInfoResult::Ok);
    }

    let info = match sfp.get_transceiver_info() {
        Ok(v) => v,
        // `_wrapper_get_transceiver_info` -> None (NotImplemented/failure) means the
        // EEPROM isn't ready yet; the caller retries.
        Err(_) => return Ok(PostSfpInfoResult::EepromNotReady),
    };
    // The real `get_transceiver_info()` returns `None` (-> JSON null) — or an empty
    // dict — while the EEPROM is not ready yet (upstream `cmis.py` returns `None` if
    // *any* field read yields `None`). Treat that as SFP_EEPROM_NOT_READY so the
    // caller retries, exactly like the Python `SFP_EEPROM_NOT_READY` path.
    if info.as_object().map_or(true, |o| o.is_empty()) {
        return Ok(PostSfpInfoResult::EepromNotReady);
    }
    let is_replaceable = sfp.is_replaceable().unwrap_or(false);
    let row = build_info_row(&info, is_replaceable);
    intf_tbl.set(logical_port_name, &row)?;
    Ok(PostSfpInfoResult::Ok)
}

/// Build the `TRANSCEIVER_INFO` row from an identity dict, mirroring the two
/// branches of the Python `post_port_sfp_info_to_db`: a CMIS module (`cmis_rev`
/// present) publishes every field via `str(value)` plus `is_replaceable`; an SFF
/// module publishes the fixed field list (missing optional fields -> `N/A`).
///
/// `active_apsel_hostlaneN`, `host_lane_count`, and `media_lane_count` are
/// deliberately dropped from the CMIS branch: those fields are owned by the CMIS
/// manager's `post_port_active_apsel_to_db` (`cmis/cmis_manager_task.py:751-782`),
/// which writes `'N/A'` for every host lane that is masked out / until the datapath
/// activates — not the raw numeric values (`get_active_apsel_hostlane()`,
/// `host_lane_count`, `media_lane_count`) the emulated identity dict carries. (Real
/// upstream `get_transceiver_info` never embeds these; the emulator does, so the
/// port must filter them to keep the manager's `'N/A'` authoritative.)
fn build_info_row(info: &serde_json::Value, is_replaceable: bool) -> Row {
    let mut row = Row::new();
    let obj = match info.as_object() {
        Some(o) => o,
        None => return row,
    };

    if obj.contains_key("cmis_rev") {
        for (field, v) in obj {
            // CMIS-manager-owned fields (post_port_active_apsel_to_db): the emulated
            // identity dict carries their raw numeric values, but TRANSCEIVER_INFO
            // must keep the manager's authoritative 'N/A' until the datapath activates.
            // Shared skip-set (see is_cmis_manager_owned_field) so this path and the
            // daemon's sync_port can never drift on which fields the manager owns.
            if is_cmis_manager_owned_field(field) {
                continue;
            }
            if let Some(s) = stringify_field(v) {
                row.insert(field.clone(), s);
            }
        }
        row.insert("is_replaceable".to_string(), pybool(is_replaceable).to_string());
    } else {
        let field = |k: &str| -> String {
            obj.get(k).and_then(|v| stringify_field(v)).unwrap_or_else(|| "N/A".to_string())
        };
        for k in [
            "type",
            "vendor_rev",
            "serial",
            "manufacturer",
            "model",
            "vendor_oui",
            "vendor_date",
            "connector",
            "encoding",
            "ext_identifier",
            "ext_rateselect_compliance",
            "cable_type",
            "cable_length",
            "specification_compliance",
            "nominal_bit_rate",
        ] {
            row.insert(k.to_string(), field(k));
        }
        row.insert("application_advertisement".to_string(), field("application_advertisement"));
        row.insert("is_replaceable".to_string(), pybool(is_replaceable).to_string());
        row.insert("dom_capability".to_string(), field("dom_capability"));
    }
    row
}

/// `DaemonXcvrd` (`xcvrd.py:877`): the top-level orchestrator that builds the
/// port mapping, seeds STATE_DB, and spawns the task threads.
///
/// For M1 the only task is `SfpStateUpdateTask` (DOM/CMIS arrive in later
/// milestones), so `run` drives it directly and blocks on the shared
/// `Arc<AtomicBool>` stop flag. The deployed binary is still the bootstrap
/// `crate::daemon::run`; this orchestrator is exercised by unit tests.
pub struct Daemon<H: Hal, D: StateDb> {
    hal: H,
    db: D,
    port_mapping: PortMapping,
    stop_event: Arc<AtomicBool>,
    skip_cmis_mgr: bool,
    enable_sff_mgr: bool,
}

impl<H: Hal, D: StateDb> Daemon<H, D> {
    pub fn new(hal: H, db: D, skip_cmis_mgr: bool, enable_sff_mgr: bool) -> Self {
        Self {
            hal,
            db,
            port_mapping: PortMapping::new(),
            stop_event: Arc::new(AtomicBool::new(false)),
            skip_cmis_mgr,
            enable_sff_mgr,
        }
    }

    /// Shared stop flag (`SIGINT`/`SIGTERM` -> `stop_event.set()` in Python).
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop_event.clone()
    }

    /// `init`: build the port mapping from CONFIG_DB and clean stale
    /// `TRANSCEIVER_INFO` rows for absent transceivers. [M1]
    pub fn init(&mut self) -> Result<(), DbError> {
        self.port_mapping = get_port_mapping(&self.db)?;
        let pm = self.port_mapping.clone();
        self.remove_stale_transceiver_info(&pm)?;
        Ok(())
    }

    /// `initialize_sfp_obj_dict` (`xcvrd.py:962`): the physical ports for which a
    /// HAL SFP handle can be obtained. The Python dict of `Sfp` objects is
    /// replaced by the `Hal` seam (handles are created on demand). [M1]
    pub fn initialize_sfp_obj_dict(&self, port_mapping: &PortMapping) -> Vec<usize> {
        let mut result = Vec::new();
        for &phys in port_mapping.physical_to_logical.keys() {
            if self.hal.sfp(phys).is_ok() {
                result.push(phys);
            }
        }
        result
    }

    /// `remove_stale_transceiver_info` (`xcvrd.py:986`): drop `TRANSCEIVER_INFO`
    /// rows for ports whose transceiver is currently absent. [M1]
    pub fn remove_stale_transceiver_info(&self, port_mapping: &PortMapping) -> Result<(), DbError> {
        let intf_tbl = self.db.table(TRANSCEIVER_INFO_TABLE)?;
        for lport in &port_mapping.logical_port_list {
            if intf_tbl.get(lport)?.is_none() {
                continue;
            }
            let pport = match port_mapping.get_logical_to_physical(lport).and_then(|l| l.first().copied()) {
                Some(p) => p,
                None => continue,
            };
            let present = match self.hal.sfp(pport) {
                Ok(sfp) => wrapper_get_presence(&sfp),
                Err(_) => false,
            };
            if !present {
                del_port_sfp_dom_info_from_db(lport, &[&intf_tbl])?;
            }
        }
        Ok(())
    }

    /// `run`: seed STATE_DB then run the SFP state-update loop until the stop flag
    /// is set. [M1]
    pub fn run(mut self) -> Result<(), DbError> {
        self.init()?;
        let sfp_error_event = Arc::new(AtomicBool::new(false));
        let Daemon { hal, db, port_mapping, stop_event, .. } = self;
        let mut task = SfpStateUpdateTask::new(hal, db, port_mapping, stop_event, sfp_error_event);
        task.run();
        Ok(())
    }

    /// `run` (`xcvrd.py:1142`) — the multi-thread form: seed STATE_DB, then spawn
    /// the `SfpStateUpdateTask` (presence/hot-plug) and `DomInfoUpdateTask` (DOM
    /// poll) on their own `std::thread`s, each iterating all ports independently and
    /// sharing one HAL (`Arc<H>`, the single Python `platform_chassis`) and one
    /// thread-safe STATE_DB (`D: Clone`), then block until the stop flag is set.
    /// This is the M5 concurrency shape; requires `Send + Sync` seams (the real
    /// `PlatformHal`/`SwssStateDb` satisfy this — see the static asserts in
    /// `hal.rs`/`statedb.rs`). [M5]
    pub fn run_threaded(mut self) -> Result<(), DbError>
    where
        H: Send + Sync + 'static,
        D: Clone + Send + 'static,
    {
        self.init()?;
        let Daemon { hal, db, port_mapping, stop_event, skip_cmis_mgr, .. } = self;
        let hal = Arc::new(hal);
        let sfp_error_event = Arc::new(AtomicBool::new(false));

        let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

        // SfpStateUpdateTask: presence/hot-plug event loop across all ports.
        {
            let hal = hal.clone();
            let db = db.clone();
            let port_mapping = port_mapping.clone();
            let stop_event = stop_event.clone();
            let sfp_error_event = sfp_error_event.clone();
            handles.push(std::thread::spawn(move || {
                let mut task =
                    SfpStateUpdateTask::new(hal, db, port_mapping, stop_event, sfp_error_event);
                task.run();
            }));
        }

        // DomInfoUpdateTask: periodic DOM poll across all ports.
        {
            let hal = hal.clone();
            let db = db.clone();
            let port_mapping = port_mapping.clone();
            let stop_event = stop_event.clone();
            handles.push(std::thread::spawn(move || {
                let task =
                    DomInfoUpdateTask::new(hal, db, port_mapping, stop_event, skip_cmis_mgr, None);
                task.run();
            }));
        }

        for handle in handles {
            let _ = handle.join();
        }
        Ok(())
    }

    /// `deinit`: teardown on shutdown (DOM/status tables cleared in later
    /// milestones; nothing to do for M1).
    pub fn deinit(&mut self) -> Result<(), DbError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockHal, MockSfp, MockStateDb};
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn mapping_with(port: &str, phys: usize) -> PortMapping {
        use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType};
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new(port, phys, 0, PortChangeEventType::Add));
        pm
    }

    fn cmis_info() -> serde_json::Value {
        json!({"cmis_rev": "5.0", "model": "EMU-40G-LR4\u{0}\u{0}", "host_lane_count": 8})
    }

    /// <- test_post_port_sfp_info_to_db: present CMIS module publishes every field
    /// (NUL-trimmed) + is_replaceable.
    #[test]
    fn post_info_present_writes_cmis_fields() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        let pm = mapping_with("Ethernet0", 0);
        let mut sfp = MockSfp::present(cmis_info());
        sfp.replaceable = true;

        let rc = post_port_sfp_info_to_db("Ethernet0", &pm, &intf, &sfp).unwrap();
        assert_eq!(rc, PostSfpInfoResult::Ok);
        let r = intf.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.get("cmis_rev").map(String::as_str), Some("5.0"));
        assert_eq!(r.get("model").map(String::as_str), Some("EMU-40G-LR4")); // NUL trimmed
        // host_lane_count is owned by the CMIS manager (post_port_active_apsel_to_db);
        // the identity publish must NOT leak the raw numeric value it carries.
        assert!(!r.contains_key("host_lane_count"), "host_lane_count leaked into TRANSCEIVER_INFO");
        assert_eq!(r.get("is_replaceable").map(String::as_str), Some("True"));
    }

    /// <- test_post_port_sfp_info_to_db_with_sfp_not_present: absent -> no row.
    #[test]
    fn post_info_absent_writes_nothing() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        let pm = mapping_with("Ethernet0", 0);
        let sfp = MockSfp::default(); // presence = false

        let rc = post_port_sfp_info_to_db("Ethernet0", &pm, &intf, &sfp).unwrap();
        assert_eq!(rc, PostSfpInfoResult::Ok);
        assert!(intf.get("Ethernet0").unwrap().is_none());
    }

    /// No physical port for the logical name -> PHYSICAL_PORT_NOT_EXIST.
    #[test]
    fn post_info_unknown_logical_port() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        let pm = PortMapping::new(); // empty
        let sfp = MockSfp::present(cmis_info());
        let rc = post_port_sfp_info_to_db("EthernetX", &pm, &intf, &sfp).unwrap();
        assert_eq!(rc, PostSfpInfoResult::PhysicalPortNotExist);
    }

    /// Present but identity unreadable (`get_transceiver_info` -> NotImplemented)
    /// -> SFP_EEPROM_NOT_READY.
    #[test]
    fn post_info_eeprom_not_ready() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        let pm = mapping_with("Ethernet0", 0);
        let mut sfp = MockSfp::default();
        sfp.presence = true;
        sfp.info = None; // get_transceiver_info -> Err(NotImplemented)
        let rc = post_port_sfp_info_to_db("Ethernet0", &pm, &intf, &sfp).unwrap();
        assert_eq!(rc, PostSfpInfoResult::EepromNotReady);
        assert!(intf.get("Ethernet0").unwrap().is_none());
    }

    /// Present but identity is JSON null (`get_transceiver_info` -> None, EEPROM not
    /// ready) or an empty dict -> SFP_EEPROM_NOT_READY (nothing published).
    #[test]
    fn post_info_null_or_empty_eeprom_not_ready() {
        for empty in [serde_json::Value::Null, json!({})] {
            let db = MockStateDb::new();
            let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
            let pm = mapping_with("Ethernet0", 0);
            let mut sfp = MockSfp::default();
            sfp.presence = true;
            sfp.info = Some(empty);
            let rc = post_port_sfp_info_to_db("Ethernet0", &pm, &intf, &sfp).unwrap();
            assert_eq!(rc, PostSfpInfoResult::EepromNotReady);
            assert!(intf.get("Ethernet0").unwrap().is_none());
        }
    }

    /// <- M6 golden: the CMIS identity publish must NOT leak the raw numeric
    /// CMIS-manager-owned fields — active_apsel_hostlaneN, host_lane_count, and
    /// media_lane_count (all published as 'N/A' by post_port_active_apsel_to_db).
    #[test]
    fn post_info_drops_cmis_manager_owned_fields() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        let pm = mapping_with("Ethernet0", 0);
        let sfp = MockSfp::present(json!({
            "cmis_rev": "5.2", "manufacturer": "xcvr-emu",
            "active_apsel_hostlane1": 1, "active_apsel_hostlane4": 1,
            "active_apsel_hostlane8": 0,
            "host_lane_count": 4, "media_lane_count": 4
        }));
        post_port_sfp_info_to_db("Ethernet0", &pm, &intf, &sfp).unwrap();
        let r = intf.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.get("manufacturer").map(String::as_str), Some("xcvr-emu"));
        assert_eq!(r.get("cmis_rev").map(String::as_str), Some("5.2"));
        for lane in [1, 4, 8] {
            assert!(
                !r.contains_key(&format!("active_apsel_hostlane{lane}")),
                "active_apsel_hostlane{lane} leaked into TRANSCEIVER_INFO"
            );
        }
        for field in ["host_lane_count", "media_lane_count"] {
            assert!(
                !r.contains_key(field),
                "{field} leaked into TRANSCEIVER_INFO (owned by the CMIS manager)"
            );
        }
    }

    /// SFF module (no `cmis_rev`) publishes the fixed field list with `N/A` for
    /// absent optionals.
    #[test]
    fn post_info_sff_fixed_fields() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        let pm = mapping_with("Ethernet0", 0);
        let sfp = MockSfp::present(json!({
            "type": "SFP", "vendor_rev": "A", "serial": "S1", "manufacturer": "M",
            "model": "MOD", "vendor_oui": "OUI", "vendor_date": "2020", "connector": "LC",
            "encoding": "64B66B", "ext_identifier": "SFP+", "ext_rateselect_compliance": "N/A",
            "cable_type": "N/A", "cable_length": 5, "specification_compliance": "N/A",
            "nominal_bit_rate": 10300
        }));
        let rc = post_port_sfp_info_to_db("Ethernet0", &pm, &intf, &sfp).unwrap();
        assert_eq!(rc, PostSfpInfoResult::Ok);
        let r = intf.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.get("type").map(String::as_str), Some("SFP"));
        assert_eq!(r.get("cable_length").map(String::as_str), Some("5"));
        assert_eq!(r.get("application_advertisement").map(String::as_str), Some("N/A"));
        assert_eq!(r.get("dom_capability").map(String::as_str), Some("N/A"));
        assert!(!r.contains_key("cmis_rev"));
    }

    /// <- test_initialize_sfp_obj_dict (retargeted onto Hal::num_sfps/sfp): only
    /// physical ports with a valid HAL handle are returned.
    #[test]
    fn initialize_sfp_obj_dict_uses_hal() {
        let hal = MockHal::with_ports(3); // valid indices 0,1,2
        let db = MockStateDb::new();
        let daemon = Daemon::new(hal, db, true, false);

        let mut pm = mapping_with("Ethernet0", 0);
        use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType};
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet4", 1, 0, PortChangeEventType::Add));
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet8", 5, 0, PortChangeEventType::Add));

        // phys 0 and 1 resolve; phys 5 has no HAL handle (only 3 ports) -> excluded.
        let dict = daemon.initialize_sfp_obj_dict(&pm);
        assert_eq!(dict, vec![0, 1]);
    }

    /// <- test_remove_stale_transceiver_info: an existing INFO row is dropped only
    /// when the transceiver is absent.
    #[test]
    fn remove_stale_only_absent_ports() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        intf.set("Ethernet0", &row(&[("model", "present")])).unwrap();
        intf.set("Ethernet4", &row(&[("model", "absent")])).unwrap();

        // phys 0 present, phys 1 absent.
        let hal = MockHal::new(vec![MockSfp::present(cmis_info()), MockSfp::default()]);
        let daemon = Daemon::new(hal, db.clone(), true, false);

        let mut pm = mapping_with("Ethernet0", 0);
        use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType};
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet4", 1, 0, PortChangeEventType::Add));

        daemon.remove_stale_transceiver_info(&pm).unwrap();
        assert!(intf.get("Ethernet0").unwrap().is_some()); // present -> kept
        assert!(intf.get("Ethernet4").unwrap().is_none()); // absent -> removed
    }

    /// <- test_DaemonXcvrd_run (retargeted onto std::thread + AtomicBool): run
    /// seeds INFO/STATUS_SW then returns once the stop flag is set.
    #[test]
    fn daemon_run_seeds_then_stops_on_flag() {
        let db = MockStateDb::new();
        db.table("PORT").unwrap().set("Ethernet0", &row(&[("index", "0")])).unwrap();
        let hal = MockHal::new(vec![MockSfp::present(cmis_info())]);
        let daemon = Daemon::new(hal, db.clone(), true, false);

        // Pre-set stop so the event loop exits immediately after the initial seed.
        daemon.stop_flag().store(true, Ordering::Relaxed);
        daemon.run().unwrap();

        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        assert!(intf.get("Ethernet0").unwrap().is_some());
        let sw = db.table("TRANSCEIVER_STATUS_SW").unwrap();
        assert_eq!(sw.hget("Ethernet0", "status").unwrap().as_deref(), Some("1"));
    }
}
