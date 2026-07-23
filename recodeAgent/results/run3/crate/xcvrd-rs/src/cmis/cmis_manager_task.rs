//! CMIS datapath state machine — port of `cmis/cmis_manager_task.py`.
//!
//! The full Python task is the heaviest module (1177 L). The `xcvrd-tests` oracle
//! only requires `TRANSCEIVER_STATUS_SW.cmis_state == "READY"` for a present
//! module (analysis §1.6 / §3.7), and the emulator brings the datapath up itself,
//! so M3 can ship a REDUCED driver: `process_single_lport` short-circuits to
//! `READY` for non-CMIS / flat-memory / no-api modules (Python `:1247`). Grow the
//! full state machine only if a later gate demands it. The reduced driver is
//! implemented below (M3); the full datapath SM (`process_cmis_state_machine`)
//! remains a later-milestone stub.

#![allow(dead_code, unused_variables)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::hal::Hal;
use crate::statedb::{DbError, Row, StateDb, TableApi};
use crate::xcvrd_utilities::common::{wrapper_get_presence, CmisState};
use crate::xcvrd_utilities::port_event_helper::PortMapping;
use crate::xcvrd_utilities::xcvr_table_helper::{TRANSCEIVER_INFO_TABLE, TRANSCEIVER_STATUS_SW_TABLE};

/// Steady-state cadence of the reduced CMIS pass (Python runs a tight loop; a
/// short sleep keeps the supervisor happy without busy-spinning).
const CMIS_POLL_SECS: u64 = 1;

/// `CmisManagerTask.CMIS_MAX_HOST_LANES` (`cmis/cmis_manager_task.py`): a CMIS
/// module has up to 8 host lanes, one `active_apsel_hostlane{n}` field each.
pub const CMIS_MAX_HOST_LANES: usize = 8;

/// `CmisManagerTask` (`cmis/cmis_manager_task.py:41`): per-lport CMIS bring-up.
pub struct CmisManagerTask<H: Hal, D: StateDb> {
    hal: H,
    db: D,
    port_mapping: PortMapping,
    skip_cmis_mgr: bool,
    stop_event: Arc<AtomicBool>,
}

impl<H: Hal, D: StateDb> CmisManagerTask<H, D> {
    pub fn new(
        hal: H,
        db: D,
        port_mapping: PortMapping,
        stop_event: Arc<AtomicBool>,
        skip_cmis_mgr: bool,
    ) -> Self {
        Self { hal, db, port_mapping, skip_cmis_mgr, stop_event }
    }

    /// Thread body (`cmis/cmis_manager_task.py:1348`): skip when `--skip_cmis_mgr`,
    /// otherwise seed every lport to `UNKNOWN` and loop `task_worker` until the
    /// stop flag is set. [M3]
    pub fn run(mut self) {
        if self.skip_cmis_mgr {
            return;
        }
        let lports = self.port_mapping.logical_port_list.clone();
        for lport in &lports {
            let _ = self.update_port_transceiver_status_table_sw_cmis_state(lport, CmisState::Unknown);
        }
        while !self.stop_event.load(Ordering::Relaxed) {
            if let Err(e) = self.task_worker() {
                eprintln!("CmisManagerTask: task_worker error: {e}");
            }
            std::thread::sleep(Duration::from_secs(CMIS_POLL_SECS));
        }
    }

    /// One pass over all lports (`cmis/cmis_manager_task.py:1324`). [M3]
    pub fn task_worker(&mut self) -> Result<(), DbError> {
        let lports = self.port_mapping.logical_port_list.clone();
        for lport in &lports {
            if self.stop_event.load(Ordering::Relaxed) {
                break;
            }
            if let Err(e) = self.process_single_lport(lport) {
                // A per-port CMIS error must never take down the task.
                eprintln!("CmisManagerTask: {lport}: {e}");
            }
        }
        Ok(())
    }

    /// `update_port_transceiver_status_table_sw_cmis_state` (`:85`): write
    /// `TRANSCEIVER_STATUS_SW.cmis_state`. The real STATE_DB `Table.set` merges
    /// fields (per-field HSET), so we read-modify-write to preserve the SW
    /// `status`/`error` the `SfpStateUpdateTask` owns. [M3]
    pub fn update_port_transceiver_status_table_sw_cmis_state(
        &self,
        lport: &str,
        state: CmisState,
    ) -> Result<(), DbError> {
        let status_table = self.db.table(TRANSCEIVER_STATUS_SW_TABLE)?;
        let mut row: Row = status_table.get(lport)?.unwrap_or_default();
        row.insert("cmis_state".to_string(), state.as_str().to_string());
        status_table.set(lport, &row)
    }

    /// `process_single_lport` (`:1247`), reduced (analysis §3.7): the emulator
    /// brings the datapath up itself and CMIS decode stays in Python, so the only
    /// oracle requirement is `cmis_state == "READY"` for a present module. The
    /// Python short-circuits (no-api / flat-memory / non-CMIS) all resolve to
    /// `READY`; an absent module resolves to `REMOVED` (Python `:1274`). A present
    /// module also gets its active-apsel projection published (all `'N/A'` — the
    /// emulated datapath is not activated here). [M3/M6]
    pub fn process_single_lport(&mut self, lport: &str) -> Result<(), DbError> {
        let phys = match self
            .port_mapping
            .logical_port_name_to_physical_port_list(lport)
            .and_then(|l| l.first().copied())
        {
            Some(p) => p,
            None => return Ok(()),
        };
        let sfp = match self.hal.sfp(phys) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        // Double-check HW presence before moving forward (Python `:1272`).
        if wrapper_get_presence(&sfp) {
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CmisState::Ready)?;
            // Reduced bring-up: the emulated datapath is not activated in this pass,
            // so every host lane's active-apsel is 'N/A' (host_lanes_mask == 0).
            // This field is owned by the CMIS manager, NOT the identity publish.
            self.post_port_active_apsel_to_db(lport, 0, &BTreeMap::new(), true)
        } else {
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CmisState::Removed)
        }
    }

    /// `post_port_active_apsel_to_db` (`cmis/cmis_manager_task.py:751-770`): publish
    /// the per-host-lane active application-select into `TRANSCEIVER_INFO`. A lane
    /// that is masked out (`host_lanes_mask` bit clear) — or every lane when the
    /// datapath has not activated (`reset_apsel`) — is written `'N/A'`; an active
    /// lane gets its numeric `ActiveAppSelLane{n}`. Keeping this in the CMIS manager
    /// (not `post_port_sfp_info_to_db`) is why a module with no active datapath (the
    /// emulated 40G-LR4 at capture) reports `'N/A'` per lane — what the golden pins.
    ///
    /// Read-modify-write so the identity fields the info publish owns survive (the
    /// real `Table.set` merges per-field; the mock replaces — same guard as
    /// `update_port_transceiver_status_table_sw_cmis_state`). [M6]
    pub fn post_port_active_apsel_to_db(
        &self,
        lport: &str,
        host_lanes_mask: u32,
        act_apsel: &BTreeMap<String, i64>,
        reset_apsel: bool,
    ) -> Result<(), DbError> {
        let intf_tbl = self.db.table(TRANSCEIVER_INFO_TABLE)?;
        // No TRANSCEIVER_INFO row yet -> nothing to update (Python logs + returns).
        let mut row: Row = match intf_tbl.get(lport)? {
            Some(r) => r,
            None => return Ok(()),
        };
        for lane in 0..CMIS_MAX_HOST_LANES {
            let field = format!("active_apsel_hostlane{}", lane + 1);
            let masked_out = (host_lanes_mask & (1 << lane)) == 0;
            let value = if masked_out || reset_apsel {
                "N/A".to_string()
            } else {
                act_apsel
                    .get(&format!("ActiveAppSelLane{}", lane + 1))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "N/A".to_string())
            };
            row.insert(field, value);
        }
        // host_lane_count / media_lane_count follow the active application; with no
        // active datapath they are 'N/A' (Python reset_apsel branch, :780-782).
        if reset_apsel || host_lanes_mask == 0 {
            row.insert("host_lane_count".to_string(), "N/A".to_string());
            row.insert("media_lane_count".to_string(), "N/A".to_string());
        }
        intf_tbl.set(lport, &row)
    }

    /// `process_cmis_state_machine` (`:1061`): the full datapath SM (grow later).
    pub fn process_cmis_state_machine(&mut self, lport: &str) {
        todo!("later: CmisManagerTask::process_cmis_state_machine")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockHal, MockSfp, MockStateDb};
    use crate::statedb::{Row, StateDb, TableApi};
    use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType};
    use serde_json::json;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn mapping_with(port: &str, phys: usize) -> PortMapping {
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new(port, phys, 0, PortChangeEventType::Add));
        pm
    }

    fn new_task(hal: MockHal, db: MockStateDb, pm: PortMapping) -> CmisManagerTask<MockHal, MockStateDb> {
        CmisManagerTask::new(hal, db, pm, Arc::new(AtomicBool::new(false)), false)
    }

    /// <- test_CmisManagerTask_update_port_transceiver_status_table_sw_cmis_state:
    /// writing cmis_state records it and preserves the SW status/error fields
    /// (real Table.set merges).
    #[test]
    fn update_cmis_state_writes_and_merges() {
        let db = MockStateDb::new();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        sw.set("Ethernet0", &row(&[("status", "1"), ("error", "N/A")])).unwrap();

        let task = new_task(MockHal::with_ports(1), db.clone(), mapping_with("Ethernet0", 0));
        task.update_port_transceiver_status_table_sw_cmis_state("Ethernet0", CmisState::Inserted)
            .unwrap();

        let r = sw.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.get("cmis_state").map(String::as_str), Some("INSERTED"));
        // status / error preserved (merge, not replace).
        assert_eq!(r.get("status").map(String::as_str), Some("1"));
        assert_eq!(r.get("error").map(String::as_str), Some("N/A"));
    }

    /// <- reduced process_single_lport (from test_CmisManagerTask_process_single_lport_*):
    /// a present module drives cmis_state to READY; an absent one to REMOVED.
    #[test]
    fn process_single_lport_present_ready_absent_removed() {
        let db = MockStateDb::new();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();

        // Present CMIS module -> READY.
        let hal = MockHal::new(vec![MockSfp::present(json!({"cmis_rev": "5.0"}))]);
        let mut task = new_task(hal, db.clone(), mapping_with("Ethernet0", 0));
        task.process_single_lport("Ethernet0").unwrap();
        assert_eq!(sw.hget("Ethernet0", "cmis_state").unwrap().as_deref(), Some("READY"));

        // Absent module -> REMOVED.
        let hal = MockHal::new(vec![MockSfp::default()]);
        let mut task = new_task(hal, db.clone(), mapping_with("Ethernet4", 0));
        task.process_single_lport("Ethernet4").unwrap();
        assert_eq!(sw.hget("Ethernet4", "cmis_state").unwrap().as_deref(), Some("REMOVED"));
    }

    /// task_worker drives every configured present lport to READY in one pass.
    #[test]
    fn task_worker_one_pass_sets_ready() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![MockSfp::present(json!({"cmis_rev": "5.0"}))]);
        let mut task = new_task(hal, db.clone(), mapping_with("Ethernet0", 0));
        task.task_worker().unwrap();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "cmis_state").unwrap().as_deref(), Some("READY"));
    }

    /// `--skip_cmis_mgr` short-circuits run() (no state written, returns promptly).
    #[test]
    fn run_skips_when_skip_cmis_mgr() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![MockSfp::present(json!({"cmis_rev": "5.0"}))]);
        let task = CmisManagerTask::new(
            hal,
            db.clone(),
            mapping_with("Ethernet0", 0),
            Arc::new(AtomicBool::new(false)),
            true, // skip_cmis_mgr
        );
        task.run();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert!(sw.get("Ethernet0").unwrap().is_none());
    }

    /// run() seeds UNKNOWN then reaches READY, and returns once the stop flag is set.
    #[test]
    fn run_seeds_then_ready_then_stops() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![MockSfp::present(json!({"cmis_rev": "5.0"}))]);
        let stop = Arc::new(AtomicBool::new(false));
        let task = CmisManagerTask::new(
            hal,
            db.clone(),
            mapping_with("Ethernet0", 0),
            stop.clone(),
            false,
        );
        let stop2 = stop.clone();
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            stop2.store(true, Ordering::Relaxed);
        });
        task.run();
        flipper.join().unwrap();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "cmis_state").unwrap().as_deref(), Some("READY"));
    }

    /// <- post_port_active_apsel_to_db (no active datapath): every host lane 'N/A',
    /// host/media_lane_count 'N/A', and the identity fields the info publish owns
    /// are preserved (read-modify-write merge).
    #[test]
    fn active_apsel_no_datapath_writes_all_na() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        // Seed identity incl. the raw numeric active_apsel a naive info publish would
        // have leaked, so we prove the CMIS manager overwrites them.
        intf.set(
            "Ethernet100",
            &row(&[
                ("manufacturer", "xcvr-emu"),
                ("cmis_rev", "5.2"),
                ("active_apsel_hostlane1", "1"),
                ("active_apsel_hostlane8", "0"),
            ]),
        )
        .unwrap();

        let task = new_task(MockHal::with_ports(0), db.clone(), mapping_with("Ethernet100", 25));
        task.post_port_active_apsel_to_db("Ethernet100", 0, &BTreeMap::new(), true)
            .unwrap();

        let r = intf.get("Ethernet100").unwrap().unwrap();
        for lane in 1..=8 {
            assert_eq!(
                r.get(&format!("active_apsel_hostlane{lane}")).map(String::as_str),
                Some("N/A"),
                "lane {lane} not N/A"
            );
        }
        assert_eq!(r.get("host_lane_count").map(String::as_str), Some("N/A"));
        assert_eq!(r.get("media_lane_count").map(String::as_str), Some("N/A"));
        // Identity preserved.
        assert_eq!(r.get("manufacturer").map(String::as_str), Some("xcvr-emu"));
        assert_eq!(r.get("cmis_rev").map(String::as_str), Some("5.2"));
    }

    /// <- post_port_active_apsel_to_db (active datapath): lanes in host_lanes_mask
    /// get their numeric ActiveAppSelLane{n}; masked-out lanes stay 'N/A'.
    #[test]
    fn active_apsel_active_lanes_write_numeric() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        intf.set("Ethernet0", &row(&[("manufacturer", "m")])).unwrap();

        let mut apsel = BTreeMap::new();
        apsel.insert("ActiveAppSelLane1".to_string(), 1i64);
        apsel.insert("ActiveAppSelLane2".to_string(), 1i64);
        // host lanes 1 and 2 active (mask 0b11), rest masked out.
        let task = new_task(MockHal::with_ports(0), db.clone(), mapping_with("Ethernet0", 0));
        task.post_port_active_apsel_to_db("Ethernet0", 0b11, &apsel, false).unwrap();

        let r = intf.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.get("active_apsel_hostlane1").map(String::as_str), Some("1"));
        assert_eq!(r.get("active_apsel_hostlane2").map(String::as_str), Some("1"));
        for lane in 3..=8 {
            assert_eq!(
                r.get(&format!("active_apsel_hostlane{lane}")).map(String::as_str),
                Some("N/A")
            );
        }
    }

    /// No TRANSCEIVER_INFO row yet -> post_port_active_apsel_to_db is a no-op.
    #[test]
    fn active_apsel_no_info_row_is_noop() {
        let db = MockStateDb::new();
        let task = new_task(MockHal::with_ports(0), db.clone(), mapping_with("Ethernet0", 0));
        task.post_port_active_apsel_to_db("Ethernet0", 0, &BTreeMap::new(), true).unwrap();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        assert!(intf.get("Ethernet0").unwrap().is_none());
    }

    /// <- process_single_lport (present): drives cmis_state READY AND overwrites any
    /// leaked numeric active_apsel with 'N/A' (no active datapath).
    #[test]
    fn process_single_lport_publishes_na_active_apsel() {
        let db = MockStateDb::new();
        let intf = db.table(TRANSCEIVER_INFO_TABLE).unwrap();
        intf.set(
            "Ethernet0",
            &row(&[("manufacturer", "xcvr-emu"), ("active_apsel_hostlane1", "1")]),
        )
        .unwrap();

        let hal = MockHal::new(vec![MockSfp::present(json!({"cmis_rev": "5.0"}))]);
        let mut task = new_task(hal, db.clone(), mapping_with("Ethernet0", 0));
        task.process_single_lport("Ethernet0").unwrap();

        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        assert_eq!(sw.hget("Ethernet0", "cmis_state").unwrap().as_deref(), Some("READY"));
        let r = intf.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.get("active_apsel_hostlane1").map(String::as_str), Some("N/A"));
        assert_eq!(r.get("manufacturer").map(String::as_str), Some("xcvr-emu"));
    }
}
