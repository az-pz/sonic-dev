//! DOM poll tasks — port of `dom/dom_mgr.py`.
//!
//! `DomInfoUpdateTask` polls DOM sensors / HW status / flags / VDM / PM / firmware
//! every ~60 s and publishes `TRANSCEIVER_DOM_SENSOR`, `TRANSCEIVER_STATUS`, etc.
//! `DomThermalInfoUpdateTask` runs a faster module-temperature poll into
//! `TRANSCEIVER_DOM_TEMPERATURE`. Generic over the HAL + STATE_DB seams so a poll
//! pass can be unit-tested with mocks.
//!
//! M2 scope: the DOM-**sensor** poll pass (`TRANSCEIVER_DOM_SENSOR`) plus the
//! polling cadence, the CONFIG_DB `dom_polling` gate, the CMIS-init gate and the
//! link-change scheduler. Firmware / DOM flags / HW status / VDM / PM are later
//! milestones. The deployed binary is still the bootstrap `crate::daemon`; this
//! task is exercised by unit tests (and is the formal home of the poll logic).

#![allow(dead_code, unused_variables)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::dom::utilities::dom_sensor::db_utils::DomDbUtils;
use crate::hal::Hal;
use crate::statedb::{DbError, StateDb, TableApi};
use crate::xcvrd_utilities::common::{get_cmis_state_from_state_db, wrapper_get_presence};
use crate::xcvrd_utilities::port_event_helper::{PortMapping, CFG_PORT_TABLE_NAME};
use crate::xcvrd_utilities::sfp_status_helper::detect_port_in_error_status;
use crate::xcvrd_utilities::xcvr_table_helper::{
    TRANSCEIVER_DOM_SENSOR_TABLE, TRANSCEIVER_STATUS_SW_TABLE,
};

/// Default DOM poll cadence (Python `DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS` = 60 s).
pub const DOM_INFO_UPDATE_PERIOD_SECS: u64 = 60;

/// Seconds after a link change before the DB diagnostics are refreshed
/// (`DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE`).
pub const DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE_SECS: u64 = 1;

/// `dom_polling` value that disables DOM monitoring for a port.
const DOM_POLLING_DISABLED: &str = "disabled";
/// `dom_polling` default (enabled).
const DOM_POLLING_ENABLED: &str = "enabled";

/// `DomInfoUpdateTask` (`dom/dom_mgr.py:141`): the periodic DOM/status poller.
pub struct DomInfoUpdateTask<H: Hal, D: StateDb> {
    hal: H,
    db: D,
    port_mapping: PortMapping,
    dom_update_interval: u64,
    skip_cmis_mgr: bool,
    stop_event: Arc<AtomicBool>,
    /// physical port -> deadline after which its DB diagnostics are refreshed
    /// following a link change (`link_change_affected_ports`).
    link_change_affected_ports: BTreeMap<usize, Instant>,
}

impl<H: Hal, D: StateDb> DomInfoUpdateTask<H, D> {
    pub fn new(
        hal: H,
        db: D,
        port_mapping: PortMapping,
        stop_event: Arc<AtomicBool>,
        skip_cmis_mgr: bool,
        dom_update_interval: Option<u64>,
    ) -> Self {
        // Python: negative interval -> keep the default; otherwise use the value.
        let interval = match dom_update_interval {
            Some(v) => v,
            None => DOM_INFO_UPDATE_PERIOD_SECS,
        };
        Self {
            hal,
            db,
            port_mapping,
            dom_update_interval: interval,
            skip_cmis_mgr,
            stop_event,
            link_change_affected_ports: BTreeMap::new(),
        }
    }

    /// Thread body: run `task_worker` on the configured cadence until
    /// `stop_event` is set. The first pass is delayed by one interval to let
    /// xcvrd seed the ports (mirrors the Python `next_periodic_db_update_time`). [M2]
    pub fn run(mut self) {
        while !self.stop_event.load(Ordering::Relaxed) {
            // Wait one interval (interruptibly) before each periodic pass.
            if !self.sleep_interruptibly(self.dom_update_interval) {
                break;
            }
            if let Err(e) = self.task_worker() {
                eprintln!("DomInfoUpdateTask: task_worker error: {e}");
            }
        }
    }

    /// Sleep up to `secs`, waking early (returning `false`) if the stop flag is
    /// set. Returns `true` if the full interval elapsed without a stop.
    fn sleep_interruptibly(&self, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if self.stop_event.load(Ordering::Relaxed) {
                return false;
            }
            thread::sleep(Duration::from_millis(200));
        }
        !self.stop_event.load(Ordering::Relaxed)
    }

    /// One DOM poll pass over every physical port: for each port not disabled,
    /// not in a blocking-error state and present, publish `TRANSCEIVER_DOM_SENSOR`
    /// (M2). The first logical port of each breakout group represents the group. [M2]
    pub fn task_worker(&mut self) -> Result<(), DbError> {
        let dom_tbl = self.db.table(TRANSCEIVER_DOM_SENSOR_TABLE)?;
        let status_sw_tbl = self.db.table(TRANSCEIVER_STATUS_SW_TABLE)?;

        let groups: Vec<(usize, Vec<String>)> = self
            .port_mapping
            .physical_to_logical
            .iter()
            .map(|(phys, logical)| (*phys, logical.clone()))
            .collect();

        for (physical_port, logical_ports) in groups {
            if self.stop_event.load(Ordering::Relaxed) {
                break;
            }
            // First logical port corresponds to the first subport of the group.
            let logical_port_name = match logical_ports.first() {
                Some(l) => l.clone(),
                None => continue,
            };

            if self.is_port_dom_monitoring_disabled(&logical_port_name) {
                continue;
            }

            // Skip ports whose EEPROM read is blocked by an error.
            if detect_port_in_error_status(&logical_port_name, &status_sw_tbl) {
                continue;
            }

            let sfp = match self.hal.sfp(physical_port) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !wrapper_get_presence(&sfp) {
                continue;
            }

            // M3+: firmware / DOM flags / HW status / VDM / PM also post here.
            if let Err(e) =
                DomDbUtils::post_port_dom_sensor_info_to_db(&logical_port_name, &sfp, &dom_tbl)
            {
                eprintln!("DomInfoUpdateTask: dom sensor post {logical_port_name}: {e}");
            }
        }
        Ok(())
    }

    /// `get_dom_polling_from_config_db` (`dom_mgr.py:76`): read `dom_polling` from
    /// the CONFIG_DB `PORT` row of the group's first subport; `enabled` unless the
    /// row explicitly says `disabled`. (Modular simplification: the CONFIG_DB PORT
    /// table is read through the same `StateDb` seam; single-ASIC testbed.)
    pub fn get_dom_polling_from_config_db(&self, lport: &str) -> String {
        let default = DOM_POLLING_ENABLED.to_string();

        let pport = match self
            .port_mapping
            .get_logical_to_physical(lport)
            .and_then(|l| l.first().copied())
        {
            Some(p) => p,
            None => return default,
        };

        let logical_port_list = match self.port_mapping.get_physical_to_logical(pport) {
            Some(l) if !l.is_empty() => l,
            _ => return default,
        };
        // First logical port corresponds to the first subport.
        let first_logical_port = &logical_port_list[0];

        let port_tbl = match self.db.table(CFG_PORT_TABLE_NAME) {
            Ok(t) => t,
            Err(_) => return default,
        };
        match port_tbl.get(first_logical_port) {
            Ok(Some(info)) => info.get("dom_polling").cloned().unwrap_or(default),
            _ => default,
        }
    }

    /// `is_port_dom_monitoring_disabled`: disabled via CONFIG_DB, or blocked while
    /// CMIS is bringing the port up.
    pub fn is_port_dom_monitoring_disabled(&self, logical_port_name: &str) -> bool {
        self.get_dom_polling_from_config_db(logical_port_name) == DOM_POLLING_DISABLED
            || self.is_port_in_cmis_initialization_process(logical_port_name)
    }

    /// `is_port_in_cmis_initialization_process`: skip DOM while CMIS is bringing up
    /// the port (any non-terminal `cmis_state`). Always false when the CMIS manager
    /// is disabled for the platform. [M2/M3]
    pub fn is_port_in_cmis_initialization_process(&self, logical_port_name: &str) -> bool {
        if self.skip_cmis_mgr {
            return false;
        }
        let status_sw_tbl = match self.db.table(TRANSCEIVER_STATUS_SW_TABLE) {
            Ok(t) => t,
            Err(_) => return false,
        };
        let cmis_state = get_cmis_state_from_state_db(logical_port_name, &status_sw_tbl)
            .unwrap_or(crate::xcvrd_utilities::common::CmisState::Unknown);
        !cmis_state.is_terminal()
    }

    /// `check_port_update` (`dom_mgr.py:267`): process the pending link-change set,
    /// refreshing DB diagnostics for ports whose scheduled time has elapsed. (The
    /// runtime PORT subscription / `PortChangeObserver` polling is M5; here we
    /// drive the link-change schedule directly.) `now` is injectable for tests.
    pub fn check_port_update(&mut self, now: Instant) -> Result<(), DbError> {
        let due: Vec<usize> = self
            .link_change_affected_ports
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(port, _)| *port)
            .collect();

        for port in due {
            if self.stop_event.load(Ordering::Relaxed) {
                break;
            }
            self.update_port_db_diagnostics_on_link_change(port)?;
            self.link_change_affected_ports.remove(&port);
        }
        Ok(())
    }

    /// Schedule `physical_port`'s diagnostics to be refreshed
    /// `DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE` seconds from now (the effect of a
    /// PORT_SET/link-change event; the M5 observer will feed these).
    pub fn schedule_link_change(&mut self, physical_port: usize) {
        let deadline =
            Instant::now() + Duration::from_secs(DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE_SECS);
        self.link_change_affected_ports.insert(physical_port, deadline);
    }

    /// `update_port_db_diagnostics_on_link_change` (`dom_mgr.py:442`): refresh the
    /// DOM diagnostics of `physical_port`'s first logical port (M2: DOM sensor).
    fn update_port_db_diagnostics_on_link_change(&mut self, physical_port: usize) -> Result<(), DbError> {
        if self.stop_event.load(Ordering::Relaxed) {
            return Ok(());
        }
        let logical_port_list = match self.port_mapping.get_physical_to_logical(physical_port) {
            Some(l) if !l.is_empty() => l,
            _ => return Ok(()),
        };
        let first_logical_port = logical_port_list[0].clone();

        if self.is_port_dom_monitoring_disabled(&first_logical_port) {
            return Ok(());
        }
        let sfp = match self.hal.sfp(physical_port) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        if !wrapper_get_presence(&sfp) {
            return Ok(());
        }
        let dom_tbl = self.db.table(TRANSCEIVER_DOM_SENSOR_TABLE)?;
        DomDbUtils::post_port_dom_sensor_info_to_db(&first_logical_port, &sfp, &dom_tbl)?;
        Ok(())
    }
}

/// `DomThermalInfoUpdateTask` (`dom/dom_mgr.py:526`): faster temperature poll.
pub struct DomThermalInfoUpdateTask<H: Hal, D: StateDb> {
    hal: H,
    db: D,
    port_mapping: PortMapping,
    poll_interval: u64,
    stop_event: Arc<AtomicBool>,
}

impl<H: Hal, D: StateDb> DomThermalInfoUpdateTask<H, D> {
    pub fn new(
        hal: H,
        db: D,
        port_mapping: PortMapping,
        stop_event: Arc<AtomicBool>,
        poll_interval: u64,
    ) -> Self {
        Self { hal, db, port_mapping, poll_interval, stop_event }
    }

    pub fn run(self) {
        todo!("opt: DomThermalInfoUpdateTask::run")
    }

    pub fn task_worker(&mut self) {
        todo!("opt: DomThermalInfoUpdateTask::task_worker")
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

    fn dom_sfp() -> MockSfp {
        let mut sfp = MockSfp::default();
        sfp.presence = true;
        sfp.dom_real_value = Some(json!({"temperature": 30.5, "voltage": 3.3}));
        sfp
    }

    fn add(pm: &mut PortMapping, name: &str, phys: usize) {
        pm.handle_port_change_event(&PortChangeEvent::new(name, phys, 0, PortChangeEventType::Add));
    }

    fn task(
        hal: MockHal,
        db: MockStateDb,
        pm: PortMapping,
        skip_cmis_mgr: bool,
    ) -> DomInfoUpdateTask<MockHal, MockStateDb> {
        DomInfoUpdateTask::new(hal, db, pm, Arc::new(AtomicBool::new(false)), skip_cmis_mgr, Some(0))
    }

    /// <- test_DomInfoUpdateTask_task_worker (modular refactor: one poll pass over
    /// a multi-port MockHal): present, dom-enabled, non-error ports get
    /// TRANSCEIVER_DOM_SENSOR with temperature + voltage; disabled/absent skipped.
    #[test]
    fn task_worker_one_poll_pass() {
        let db = MockStateDb::new();
        // Ethernet0 -> phys0 present; Ethernet4 -> phys1 absent; Ethernet8 -> phys2 present but disabled.
        let hal = MockHal::new(vec![dom_sfp(), MockSfp::default(), dom_sfp()]);
        let mut pm = PortMapping::new();
        add(&mut pm, "Ethernet0", 0);
        add(&mut pm, "Ethernet4", 1);
        add(&mut pm, "Ethernet8", 2);
        // Disable DOM polling for Ethernet8 via CONFIG_DB PORT.
        db.table(CFG_PORT_TABLE_NAME)
            .unwrap()
            .set("Ethernet8", &row(&[("dom_polling", "disabled")]))
            .unwrap();

        let mut t = task(hal, db.clone(), pm, true);
        t.task_worker().unwrap();

        let dom = db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap();
        let r = dom.get("Ethernet0").unwrap().unwrap();
        assert_eq!(r.get("temperature").map(String::as_str), Some("30.5"));
        assert_eq!(r.get("voltage").map(String::as_str), Some("3.3"));
        assert!(r.contains_key("last_update_time"));
        // Absent port -> nothing.
        assert!(dom.get("Ethernet4").unwrap().is_none());
        // dom_polling disabled -> skipped.
        assert!(dom.get("Ethernet8").unwrap().is_none());
    }

    /// <- new concurrency test (distinct per-port DOM, no cross-talk): a single
    /// poll pass over many ports writes each port its own `TRANSCEIVER_DOM_SENSOR`
    /// row reflecting that port's own sensor values.
    #[test]
    fn task_worker_distinct_dom_per_port() {
        let db = MockStateDb::new();
        // Three present modules, each reporting a distinct temperature/voltage.
        let mut sfps = Vec::new();
        for (t, v) in [(30.5, 3.30), (41.0, 3.25), (55.5, 3.10)] {
            let mut sfp = MockSfp::default();
            sfp.presence = true;
            sfp.dom_real_value = Some(json!({"temperature": t, "voltage": v}));
            sfps.push(sfp);
        }
        let hal = MockHal::new(sfps);
        let mut pm = PortMapping::new();
        add(&mut pm, "Ethernet0", 0);
        add(&mut pm, "Ethernet4", 1);
        add(&mut pm, "Ethernet8", 2);

        let mut t = task(hal, db.clone(), pm, true);
        t.task_worker().unwrap();

        let dom = db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap();
        for (lp, temp, volt) in [
            ("Ethernet0", "30.5", "3.3"),
            ("Ethernet4", "41.0", "3.25"),
            ("Ethernet8", "55.5", "3.1"),
        ] {
            let r = dom.get(lp).unwrap().unwrap();
            assert_eq!(r.get("temperature").map(String::as_str), Some(temp), "{lp} temp");
            assert_eq!(r.get("voltage").map(String::as_str), Some(volt), "{lp} volt");
        }
    }

    /// A port whose STATUS_SW.error is a blocking error is skipped by the poll.
    #[test]
    fn task_worker_skips_blocking_error_port() {
        use crate::xcvrd_utilities::sfp_status_helper::SFP_ERROR_DESCRIPTION_BLOCKING;
        let db = MockStateDb::new();
        db.table(TRANSCEIVER_STATUS_SW_TABLE)
            .unwrap()
            .set("Ethernet0", &row(&[("error", SFP_ERROR_DESCRIPTION_BLOCKING)]))
            .unwrap();
        let hal = MockHal::new(vec![dom_sfp()]);
        let mut pm = PortMapping::new();
        add(&mut pm, "Ethernet0", 0);

        let mut t = task(hal, db.clone(), pm, true);
        t.task_worker().unwrap();
        assert!(db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
    }

    /// <- test_DomInfoUpdateTask_get_dom_polling_from_config_db: the group's first
    /// (natsorted) subport drives the whole group; unknown ports default enabled.
    #[test]
    fn get_dom_polling_from_config_db_group_semantics() {
        let db = MockStateDb::new();
        let cfg = db.table(CFG_PORT_TABLE_NAME).unwrap();
        cfg.set("Ethernet0", &row(&[("dom_polling", "disabled")])).unwrap();
        for p in ["Ethernet4", "Ethernet8", "Ethernet12", "Ethernet16"] {
            cfg.set(p, &row(&[("dom_polling", "enabled")])).unwrap();
        }

        let mut pm = PortMapping::new();
        // Ethernet4/12/8/0 all share physical port 1 (breakout group); Ethernet16 -> 2.
        add(&mut pm, "Ethernet4", 1);
        add(&mut pm, "Ethernet12", 1);
        add(&mut pm, "Ethernet8", 1);
        add(&mut pm, "Ethernet0", 1);
        add(&mut pm, "Ethernet16", 2);

        let t = task(MockHal::with_ports(3), db, pm, true);
        // First subport of group 1 is natsorted Ethernet0 -> disabled for all.
        for p in ["Ethernet0", "Ethernet4", "Ethernet8", "Ethernet12"] {
            assert_eq!(t.get_dom_polling_from_config_db(p), "disabled", "{p}");
        }
        // Group 2 first subport Ethernet16 -> enabled.
        assert_eq!(t.get_dom_polling_from_config_db("Ethernet16"), "enabled");
        // Unknown logical port -> default enabled.
        assert_eq!(t.get_dom_polling_from_config_db("Ethernet20"), "enabled");
    }

    /// CMIS-init gate: with the CMIS manager active, a non-terminal cmis_state
    /// blocks DOM; a terminal one (READY) allows it; skip_cmis_mgr short-circuits.
    #[test]
    fn cmis_initialization_gate() {
        let db = MockStateDb::new();
        let sw = db.table(TRANSCEIVER_STATUS_SW_TABLE).unwrap();
        let mut pm = PortMapping::new();
        add(&mut pm, "Ethernet0", 0);

        // CMIS manager active, INSERTED (non-terminal) -> in init -> disabled.
        sw.set("Ethernet0", &row(&[("cmis_state", "INSERTED")])).unwrap();
        let t = task(MockHal::with_ports(1), db.clone(), pm.clone(), false);
        assert!(t.is_port_in_cmis_initialization_process("Ethernet0"));
        assert!(t.is_port_dom_monitoring_disabled("Ethernet0"));

        // READY (terminal) -> not in init.
        sw.set("Ethernet0", &row(&[("cmis_state", "READY")])).unwrap();
        let t = task(MockHal::with_ports(1), db.clone(), pm.clone(), false);
        assert!(!t.is_port_in_cmis_initialization_process("Ethernet0"));

        // skip_cmis_mgr -> always false regardless of state.
        sw.set("Ethernet0", &row(&[("cmis_state", "INSERTED")])).unwrap();
        let t = task(MockHal::with_ports(1), db, pm, true);
        assert!(!t.is_port_in_cmis_initialization_process("Ethernet0"));
    }

    /// <- test_DomInfoUpdateTask_check_port_update: due (past) ports are refreshed
    /// and dropped from the schedule; future ports are kept and not refreshed.
    #[test]
    fn check_port_update_drains_due_ports() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![dom_sfp(), dom_sfp()]);
        let mut pm = PortMapping::new();
        add(&mut pm, "Ethernet0", 0);
        add(&mut pm, "Ethernet4", 1);
        let mut t = task(hal, db.clone(), pm, true);

        let base = Instant::now();
        t.link_change_affected_ports.insert(0, base); // due (<= base)
        t.link_change_affected_ports.insert(1, base + Duration::from_secs(60)); // future

        t.check_port_update(base).unwrap();

        let dom = db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap();
        // Due port refreshed + removed from the schedule.
        assert!(dom.get("Ethernet0").unwrap().is_some());
        assert!(!t.link_change_affected_ports.contains_key(&0));
        // Future port untouched + still scheduled.
        assert!(dom.get("Ethernet4").unwrap().is_none());
        assert!(t.link_change_affected_ports.contains_key(&1));
    }

    /// A stop event set before `check_port_update` halts processing (no refresh).
    #[test]
    fn check_port_update_honors_stop_event() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![dom_sfp()]);
        let mut pm = PortMapping::new();
        add(&mut pm, "Ethernet0", 0);
        let stop = Arc::new(AtomicBool::new(true));
        let mut t = DomInfoUpdateTask::new(hal, db.clone(), pm, stop, true, Some(0));

        let base = Instant::now();
        t.link_change_affected_ports.insert(0, base);
        t.check_port_update(base).unwrap();
        // Stop set -> no diagnostics written, port left in the schedule.
        assert!(db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
        assert!(t.link_change_affected_ports.contains_key(&0));
    }

    /// <- test_DomInfoUpdateTask_task_run_stop: run returns promptly when the stop
    /// flag is already set (no poll pass performed).
    #[test]
    fn run_stops_when_flag_set() {
        let db = MockStateDb::new();
        let hal = MockHal::new(vec![dom_sfp()]);
        let mut pm = PortMapping::new();
        add(&mut pm, "Ethernet0", 0);
        let stop = Arc::new(AtomicBool::new(true));
        let t = DomInfoUpdateTask::new(hal, db.clone(), pm, stop, true, Some(0));
        t.run();
        // No pass ran, so nothing was published.
        assert!(db.table(TRANSCEIVER_DOM_SENSOR_TABLE).unwrap().get("Ethernet0").unwrap().is_none());
    }
}
