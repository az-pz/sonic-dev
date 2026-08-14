//! `cmis/cmis_manager_task.py` → `CmisManagerTask`, the CMIS datapath bring-up state
//! machine (analysis §1.3, §3.2).
//!
//! State (read from `TRANSCEIVER_STATUS_SW.cmis_state`):
//! `INSERTED → DP_PRE_INIT_CHECK → DP_DEINIT → AP_CONFIGURED → DP_INIT → DP_TXON →
//! DP_ACTIVATION → READY`, plus `FAILED`/`REMOVED`. Per-state `handle_cmis_*_state`
//! handlers drive the CMIS page-10h control bytes through the [`CmisApi`] seam
//! (analysis §3.4).
//!
//! This translation runs against injected mock HAL/DB
//! seams so unit tests can drive it deterministically. The **deployed** datapath
//! bring-up lives in [`crate::daemon`] (single-threaded, over the real bridge); both
//! reproduce the same Python semantics + STATE_DB contract.
#![allow(dead_code, unused_variables, unused_imports)]

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cmis::cmis_api::{json_as_u32, CmisApi};
use crate::db::Table;
use crate::dom::utilities::db::utils::py_str;
use crate::hal::{Chassis, Sfp};
use crate::xcvrd_utilities::common::{
    self, get_cmis_application_desired, StateDbHget, CMIS_STATE_AP_CONF, CMIS_STATE_DP_ACTIVATE,
    CMIS_STATE_DP_DEINIT, CMIS_STATE_DP_INIT, CMIS_STATE_DP_PRE_INIT_CHECK, CMIS_STATE_DP_TXON,
    CMIS_STATE_FAILED, CMIS_STATE_INSERTED, CMIS_STATE_READY, CMIS_STATE_REMOVED,
    CMIS_STATE_UNKNOWN, CMIS_TERMINAL_STATES,
};
use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType, PortMapping};

// --- constants (cmis_manager_task.py:41) ------------------------------------------
pub const CMIS_MAX_RETRIES: u32 = 3;
pub const CMIS_MAX_HOST_LANES: u32 = 8;
pub const CMIS_EXPIRATION_BUFFER_MS: u64 = 2;

/// Minimum wall-clock time (ms) each *datapath bring-up* `cmis_state`
/// (`DP_PRE_INIT_CHECK` → `DP_ACTIVATION`) is HELD before the machine advances — a
/// **testbed-pacing adaptation** (see [`crate::daemon`] for the deployed rationale). The
/// synchronous xcvr emulator flips `DP{n}State` in the same register-write call, so the
/// machine would free-run INSERTED→READY between two STATE_DB polls; the dwell holds each
/// intermediate state long enough to be observed. Unit tests set this to zero.
pub const CMIS_INTER_STATE_DWELL_MS: u64 = 1000;

/// Factory that turns a HAL [`Sfp`] into a decode-capable [`CmisApi`] (production =
/// `BridgeCmisApi`; unit tests inject a `MockCmisApi`). `None` mirrors Python
/// `sfp.get_xcvr_api()` returning `None` (no CMIS api for this port).
pub type CmisApiFactory = Box<dyn Fn(Box<dyn Sfp>) -> Option<Box<dyn CmisApi>>>;

/// Per-logical-port bring-up state (the Python `port_dict[lport]` sub-dict).
#[derive(Debug, Clone, Default)]
pub struct PortInfo {
    pub asic_id: i32,
    pub index: Option<i64>,
    pub speed: Option<i64>,
    pub speed_str: Option<String>,
    pub lanes: Option<String>,
    pub subport: Option<i64>,
    pub admin_status: Option<String>,
    /// STATE_DB `PORT_TABLE|<lport>.host_tx_ready` — the host/ASIC's "I am driving a valid
    /// Tx electrical signal" signal (`cmis_manager_task.py` `host_tx_ready`). `None` until
    /// reconciled via [`CmisManagerTask::get_host_tx_status`] (Python default `'false'`).
    pub host_tx_ready: Option<String>,
    pub host_lane_count: Option<u32>,
    pub appl: Option<u32>,
    pub host_lanes_mask: u32,
    pub media_lanes_mask: u32,
    pub max_host_lanes_mask: u32,
    pub max_media_lanes_mask: u32,
    pub media_lane_count: u32,
    pub media_lane_assignment_options: u32,
    pub forced_tx_disabled: bool,
    pub txoff_duration: f64,
    pub cmis_retries: u32,
    pub cmis_expired: Option<Instant>,
    /// Inter-state pacing (testbed adaptation): the instant until which the CURRENT
    /// datapath bring-up `cmis_state` must be held before advancing. `None` = not dwelling.
    pub dwell_until: Option<Instant>,
    /// The datapath state the active [`Self::dwell_until`] was armed for.
    pub dwelled_state: Option<String>,
    /// Coherent (ZR) user-requested Tx output power (dBm). `None` models the Python
    /// `'tx_power' not in port_dict` — populated once, lazily, from CONFIG_DB.
    pub tx_power: Option<f64>,
    /// Coherent (ZR) user-requested laser frequency (GHz). `None` models the Python
    /// `'laser_freq' not in port_dict` — populated once, lazily, from CONFIG_DB.
    pub laser_freq: Option<i64>,
}

/// One sibling logical-port config sharing a physical port (`get_sibling_port_configs`).
#[derive(Debug, Clone, PartialEq)]
struct SiblingPortConfig {
    lport: String,
    subport: i64,
    speed: u32,
    host_lane_count: u32,
}

/// `CmisManagerTask` — runs the per-port datapath bring-up machine over injected DB/HAL
/// seams. The table handles are the STATE_DB/CONFIG_DB tables the Python task uses
/// (`get_status_sw_tbl` / `get_intf_tbl` / `get_cfg_port_tbl` / `get_state_port_tbl`),
/// passed directly here so the task stays self-contained.
pub struct CmisManagerTask {
    port_mapping: PortMapping,
    chassis: Box<dyn Chassis>,
    status_sw_tbl: Rc<dyn Table>,
    intf_tbl: Rc<dyn Table>,
    cfg_port_tbl: Rc<dyn Table>,
    state_port_tbl: Rc<dyn Table>,
    port_dict: HashMap<String, PortInfo>,
    decomm_pending_dict: HashMap<i64, String>,
    /// Cache of per-lport gearbox line-lane counts (`_gearbox_lanes_dict`), refreshed once
    /// per `task_worker` sweep. When a port has a gearbox entry the line-lane count wins
    /// over the CONFIG_DB `lanes` count for CMIS host-lane sizing.
    gearbox_lanes_dict: HashMap<String, u32>,
    api_factory: CmisApiFactory,
    inter_state_dwell: Duration,
    skip_cmis_mgr: bool,
    /// STATE_DB namespaces this task serves (`[""]` on the single-ASIC target). The
    /// per-namespace fast-reboot verdict is cached at construction (Python
    /// `initialize_fast_reboot_status`), never re-read while the daemon runs.
    namespaces: Vec<String>,
    /// `initialize_fast_reboot_status` result: namespace → `is_fast_reboot_enabled`, read
    /// **once** at start-up (the fast-reboot flag is set before xcvrd (re)starts, so caching
    /// matches the reference and the `test_fast_reboot_dp_skip` contract).
    fast_reboot_status: HashMap<String, bool>,
    /// Platform is multi-ASIC (`multi_asic.is_multi_asic()`); drives
    /// `get_namespace_from_asic_id`. `false` on the deployed single-ASIC target.
    multi_asic: bool,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CmisManagerTask {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        port_mapping: PortMapping,
        chassis: Box<dyn Chassis>,
        status_sw_tbl: Rc<dyn Table>,
        intf_tbl: Rc<dyn Table>,
        cfg_port_tbl: Rc<dyn Table>,
        state_port_tbl: Rc<dyn Table>,
        api_factory: CmisApiFactory,
    ) -> Self {
        // Seed `port_dict` from the port map (Python `__init__` @56-61): every known logical
        // port starts with its `asic_id` (+ physical `index`) so `get_asic_id` /
        // `is_fast_reboot_enabled_for_lport` resolve before the first PORT SET event.
        let mut port_dict: HashMap<String, PortInfo> = HashMap::new();
        for lport in &port_mapping.logical_port_list {
            if let Some(asic_id) = port_mapping.get_asic_id_for_logical_port(lport) {
                let mut pi = PortInfo { asic_id, ..Default::default() };
                if let Some(pports) = port_mapping.get_logical_to_physical(lport) {
                    pi.index = pports.first().map(|&p| p as i64);
                }
                port_dict.insert(lport.clone(), pi);
            }
        }
        CmisManagerTask {
            port_mapping,
            chassis,
            status_sw_tbl,
            intf_tbl,
            cfg_port_tbl,
            state_port_tbl,
            port_dict,
            decomm_pending_dict: HashMap::new(),
            gearbox_lanes_dict: HashMap::new(),
            api_factory,
            inter_state_dwell: Duration::from_millis(CMIS_INTER_STATE_DWELL_MS),
            skip_cmis_mgr: false,
            namespaces: vec![String::new()],
            fast_reboot_status: HashMap::new(),
            multi_asic: false,
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Configure the STATE_DB namespaces + multi-ASIC flag this task serves (Python
    /// `__init__` `self.namespaces = namespaces`). Left at `[""]`/single-ASIC by default,
    /// which is the deployed KVM target; unit tests set a specific namespace to exercise the
    /// per-namespace fast-reboot map.
    pub fn set_namespaces(&mut self, namespaces: Vec<String>, multi_asic: bool) {
        self.namespaces = namespaces;
        self.multi_asic = multi_asic;
    }

    /// `initialize_fast_reboot_status` (cmis_manager_task.py:73): cache, per namespace,
    /// whether fast reboot is enabled — read **once** at start-up from the injected STATE_DB
    /// seam. The flag is set before xcvrd (re)starts, so caching mirrors the reference and
    /// the `test_fast_reboot_dp_skip` contract (a running daemon never re-reads it).
    pub fn initialize_fast_reboot_status(&mut self, db: &dyn StateDbHget) {
        let enabled = common::is_fast_reboot_enabled(db);
        self.fast_reboot_status = self.namespaces.iter().map(|ns| (ns.clone(), enabled)).collect();
    }

    /// `get_asic_id(lport)` (cmis_manager_task.py:88): the port's cached `asic_id`, or `-1`
    /// when the port is unknown (`port_dict.get(lport, {}).get("asic_id", -1)`).
    fn get_asic_id(&self, lport: &str) -> i32 {
        self.port_dict.get(lport).map(|p| p.asic_id).unwrap_or(-1)
    }

    /// `is_fast_reboot_enabled_for_lport(lport)` (cmis_manager_task.py:91): map the port to
    /// its ASIC's namespace and look up the cached fast-reboot verdict. An unknown port
    /// (`asic_id < 0`) resolves to the default namespace `""`.
    pub fn is_fast_reboot_enabled_for_lport(&self, lport: &str) -> bool {
        let asic_id = self.get_asic_id(lport);
        let namespace = if asic_id >= 0 {
            common::get_namespace_from_asic_id(asic_id, self.multi_asic)
        } else {
            String::new()
        };
        *self.fast_reboot_status.get(&namespace).unwrap_or(&false)
    }

    /// `update_port_transceiver_status_table_sw_cmis_state` → `cmis_state` projection.
    /// Uses `hset` (field merge) so it never clobbers the `status`/`error` fields the
    /// DOM/status tasks share on the same `TRANSCEIVER_STATUS_SW|<lport>` row.
    pub fn update_port_transceiver_status_table_sw_cmis_state(&self, lport: &str, cmis_state: &str) {
        let _ = self.status_sw_tbl.hset(lport, "cmis_state", cmis_state);
    }

    fn get_cmis_state(&self, lport: &str) -> String {
        common::get_cmis_state_from_state_db(lport, &*self.status_sw_tbl)
    }

    /// `on_port_update_event` — soak a CONFIG/APPL/STATE PORT `SET`/`DEL` into `port_dict`.
    pub fn on_port_update_event(&mut self, event: &PortChangeEvent) {
        if !matches!(event.event_type, PortEventType::PortSet | PortEventType::PortDel) {
            return;
        }
        let lport = event.port_name.clone();
        if lport == "PortInitDone" || lport == "PortConfigDone" {
            return;
        }
        if !lport.starts_with("Ethernet") {
            return;
        }
        let pport = event.port_index as i64;

        match event.event_type {
            PortEventType::PortSet => {
                let asic_id = event.asic_id;
                let entry = self
                    .port_dict
                    .entry(lport.clone())
                    .or_insert_with(|| PortInfo { asic_id, ..Default::default() });
                entry.index = Some(pport);
                let d: HashMap<&str, &str> = event
                    .port_dict
                    .as_deref()
                    .map(|kv| kv.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect())
                    .unwrap_or_default();
                if let Some(s) = d.get("speed") {
                    if *s != "N/A" {
                        entry.speed_str = Some(s.to_string());
                        if let Ok(n) = s.parse::<i64>() {
                            entry.speed = Some(n);
                        }
                    }
                }
                if let Some(l) = d.get("lanes") {
                    entry.lanes = Some(l.to_string());
                }
                if let Some(a) = d.get("admin_status") {
                    entry.admin_status = Some(a.to_string());
                }
                if let Some(h) = d.get("host_tx_ready") {
                    entry.host_tx_ready = Some(h.to_string());
                }
                if let Some(sp) = d.get("subport") {
                    if let Ok(n) = sp.parse::<i64>() {
                        entry.subport = Some(n);
                    }
                }
                self.force_cmis_reinit(&lport, 0);
            }
            PortEventType::PortDel => {
                // A STATE_DB TRANSCEIVER_INFO DEL (transceiver plug-out) stamps REMOVED and
                // PRESERVES the port entry; a CONFIG_DB PORT DEL (logical de-provision) pops
                // the port and skips the REMOVED stamp (the SfpState task deletes the row).
                let is_config_port_del = event.db_name.as_deref() == Some("CONFIG_DB")
                    && event.table_name.as_deref() == Some("PORT");
                if self.port_dict.contains_key(&lport) && !is_config_port_del {
                    self.update_port_transceiver_status_table_sw_cmis_state(&lport, CMIS_STATE_REMOVED);
                }
                if is_config_port_del {
                    self.clear_decomm_pending(&lport);
                    self.port_dict.remove(&lport);
                }
            }
            _ => {}
        }
    }

    /// `force_cmis_reinit` — restart the machine at `INSERTED` and clear the timer/dwell.
    fn force_cmis_reinit(&mut self, lport: &str, retries: u32) {
        self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_INSERTED);
        if let Some(p) = self.port_dict.get_mut(lport) {
            p.cmis_retries = retries;
            p.cmis_expired = None;
            p.dwell_until = None;
            p.dwelled_state = None;
        }
    }

    /// `get_host_lane_count(lport, port_config_lanes)` — prefer the gearbox line-lane count
    /// (refreshed each sweep into `gearbox_lanes_dict`); fall back to the comma-separated
    /// CONFIG_DB `lanes` count when the port has no gearbox entry.
    pub fn get_host_lane_count(&self, lport: &str, port_config_lanes: &str) -> u32 {
        let gearbox = self.gearbox_lanes_dict.get(lport).copied().unwrap_or(0);
        if gearbox > 0 {
            return gearbox;
        }
        port_config_lanes.split(',').count() as u32
    }

    /// `set_gearbox_lanes_dict` — install the per-sweep gearbox line-lane cache
    /// (`_gearbox_lanes_dict`, refreshed once per `task_worker` iteration).
    pub fn set_gearbox_lanes_dict(&mut self, dict: HashMap<String, u32>) {
        self.gearbox_lanes_dict = dict;
    }

    /// `get_host_tx_status(lport)` — read STATE_DB `PORT_TABLE|<lport>.host_tx_ready`
    /// (Python default `'false'` when the field is absent).
    pub fn get_host_tx_status(&self, lport: &str) -> String {
        self.state_port_tbl
            .hget(lport, "host_tx_ready")
            .ok()
            .flatten()
            .unwrap_or_else(|| "false".to_string())
    }

    /// `get_cmis_max_host_lanes_mask` — `0x0f` for `QSFP+C`, else `0xff`.
    pub fn get_cmis_max_host_lanes_mask(&self, api: &dyn CmisApi) -> u32 {
        if api.get_module_type_abbreviation().as_deref() == Some("QSFP+C") {
            0x0f
        } else {
            0xff
        }
    }

    /// `get_cmis_host_lanes_mask(api, appl, host_lane_count, subport)`.
    pub fn get_cmis_host_lanes_mask(
        &self,
        api: &dyn CmisApi,
        appl: Option<u32>,
        host_lane_count: u32,
        subport: i64,
    ) -> u32 {
        let Some(appl) = appl else {
            return 0;
        };
        if appl < 1 || host_lane_count == 0 || subport < 0 {
            return 0;
        }
        let hlao = api.get_host_lane_assignment_option(appl) as u64;
        let start = host_lane_count as u64 * (if subport == 0 { 0 } else { (subport - 1) as u64 });
        let bit = 1u64.checked_shl(start as u32).unwrap_or(0);
        if hlao & bit != 0 {
            let width = ((1u64 << host_lane_count) - 1) << start;
            return width as u32;
        }
        0
    }

    /// `get_cmis_media_lanes_mask(api, appl, lport, subport)`.
    pub fn get_cmis_media_lanes_mask(&self, appl: u32, lport: &str, subport: i64) -> u32 {
        let (media_lane_count, media_lane_assignment_option) = {
            let p = &self.port_dict[lport];
            (p.media_lane_count, p.media_lane_assignment_options)
        };
        if appl < 1 || media_lane_count == 0 || subport < 0 {
            return 0;
        }
        let start = media_lane_count as u64 * (if subport == 0 { 0 } else { (subport - 1) as u64 });
        let bit = 1u64.checked_shl(start as u32).unwrap_or(0);
        if (media_lane_assignment_option as u64) & bit != 0 {
            let width = ((1u64 << media_lane_count) - 1) << start;
            return width as u32;
        }
        0
    }

    /// `get_sibling_port_configs` — CONFIG_DB PORT rows for every logical port sharing
    /// this lport's physical port (`index`). One hash read per key; skips rows without a
    /// usable `index`/`speed`/`lanes`.
    fn get_sibling_port_configs(&self, lport: &str) -> Vec<SiblingPortConfig> {
        let mut siblings = Vec::new();
        let Some(pport) = self.port_dict.get(lport).and_then(|p| p.index) else {
            return siblings;
        };
        for sibling_lport in self.cfg_port_tbl.get_keys().unwrap_or_default() {
            let Ok(Some(row)) = self.cfg_port_tbl.get(&sibling_lport) else {
                continue;
            };
            let map: HashMap<String, String> = row.into_iter().collect();
            let Some(sib_pport) = map.get("index").and_then(|s| s.parse::<i64>().ok()) else {
                continue;
            };
            if sib_pport != pport {
                continue;
            }
            let sib_speed = map.get("speed").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            let sib_subport = map.get("subport").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            let sib_lanes = map.get("lanes").cloned().unwrap_or_default();
            if sib_speed == 0 || sib_lanes.is_empty() {
                continue;
            }
            let host_lane_count = self.get_host_lane_count(&sibling_lport, &sib_lanes);
            siblings.push(SiblingPortConfig {
                lport: sibling_lport.clone(),
                subport: sib_subport,
                speed: sib_speed,
                host_lane_count,
            });
        }
        siblings
    }

    /// `get_desired_app_map` — per-lane desired app code across the physical port
    /// (sibling logical ports sharing this SFP).
    fn get_desired_app_map(&self, api: &dyn CmisApi, lport: &str) -> Vec<u32> {
        let mut desired_map = vec![0u32; CMIS_MAX_HOST_LANES as usize];
        for sibling in self.get_sibling_port_configs(lport) {
            let Some(sibling_appl) =
                get_cmis_application_desired(api, sibling.host_lane_count, sibling.speed)
            else {
                continue;
            };
            let sibling_mask = self.get_cmis_host_lanes_mask(
                api,
                Some(sibling_appl),
                sibling.host_lane_count,
                sibling.subport,
            );
            for lane in 0..CMIS_MAX_HOST_LANES {
                if (1u32 << lane) & sibling_mask != 0 {
                    desired_map[lane as usize] = sibling_appl;
                }
            }
        }
        desired_map
    }

    /// `is_decommission_required` — a currently-active lane needs a different app code.
    pub fn is_decommission_required(&self, api: &dyn CmisApi, lport: &str) -> bool {
        let desired_map = self.get_desired_app_map(api, lport);
        let active_apsel = match api.get_active_apsel_hostlane() {
            Ok(v) => v,
            Err(_) => return true,
        };
        let mut current_map = [0u32; CMIS_MAX_HOST_LANES as usize];
        for lane in 0..CMIS_MAX_HOST_LANES {
            let key = format!("ActiveAppSelLane{}", lane + 1);
            match active_apsel.get(key.as_str()) {
                None => return true,
                Some(v) => match json_as_u32(v) {
                    Some(n) => current_map[lane as usize] = n,
                    None => return true,
                },
            }
        }
        for lane in 0..CMIS_MAX_HOST_LANES as usize {
            if current_map[lane] != 0 && current_map[lane] != desired_map[lane] {
                return true;
            }
        }
        false
    }

    fn clear_decomm_pending(&mut self, lport: &str) {
        if let Some(idx) = self.port_dict.get(lport).and_then(|p| p.index) {
            self.decomm_pending_dict.remove(&idx);
        }
    }

    fn set_decomm_pending(&mut self, lport: &str) {
        let Some(idx) = self.port_dict.get(lport).and_then(|p| p.index) else {
            return;
        };
        self.decomm_pending_dict.entry(idx).or_insert_with(|| lport.to_string());
    }

    fn is_decomm_lead_lport(&self, lport: &str) -> bool {
        let Some(idx) = self.port_dict.get(lport).and_then(|p| p.index) else {
            return false;
        };
        self.decomm_pending_dict.get(&idx).map(|s| s == lport).unwrap_or(false)
    }

    fn is_decomm_pending(&self, lport: &str) -> bool {
        let Some(idx) = self.port_dict.get(lport).and_then(|p| p.index) else {
            return false;
        };
        self.decomm_pending_dict.contains_key(&idx)
    }

    fn is_decomm_failed(&self, lport: &str) -> bool {
        let Some(idx) = self.port_dict.get(lport).and_then(|p| p.index) else {
            return false;
        };
        let Some(lead) = self.decomm_pending_dict.get(&idx).cloned() else {
            return false;
        };
        self.get_cmis_state(&lead) == CMIS_STATE_FAILED
    }

    /// `is_cmis_application_update_required(api, app_new, host_lanes_mask)`.
    pub fn is_cmis_application_update_required(
        &self,
        api: &dyn CmisApi,
        app_new: u32,
        host_lanes_mask: u32,
    ) -> bool {
        if api.is_flat_memory() || app_new == 0 || host_lanes_mask == 0 {
            return false;
        }
        let mut app_old = 0u32;
        for lane in 0..CMIS_MAX_HOST_LANES {
            if (1u32 << lane) & host_lanes_mask == 0 {
                continue;
            }
            if app_old == 0 {
                app_old = api.get_application(lane);
            } else if app_old != api.get_application(lane) {
                return true;
            }
        }
        if app_old == app_new {
            let mut skip = true;
            let dp_state = api.get_datapath_state();
            let conf_state = api.get_config_datapath_hostlane_status();
            for lane in 0..CMIS_MAX_HOST_LANES {
                if (1u32 << lane) & host_lanes_mask == 0 {
                    continue;
                }
                let dp_key = format!("DP{}State", lane + 1);
                if dp_state.get(dp_key.as_str()).and_then(|v| v.as_str()) != Some("DataPathActivated") {
                    skip = false;
                    break;
                }
                let cfg_key = format!("ConfigStatusLane{}", lane + 1);
                if conf_state.get(cfg_key.as_str()).and_then(|v| v.as_str()) != Some("ConfigSuccess") {
                    skip = false;
                    break;
                }
            }
            return !skip;
        }
        true
    }

    fn check_module_state(&self, api: &dyn CmisApi, states: &[&str]) -> bool {
        states.contains(&api.get_module_state().as_str())
    }

    fn check_config_error(&self, api: &dyn CmisApi, host_lanes_mask: u32, states: &[&str]) -> bool {
        let cerr = api.get_config_datapath_hostlane_status();
        for lane in 0..CMIS_MAX_HOST_LANES {
            if (1u32 << lane) & host_lanes_mask == 0 {
                continue;
            }
            let key = format!("ConfigStatusLane{}", lane + 1);
            match cerr.get(key.as_str()).and_then(|v| v.as_str()) {
                Some(s) if states.contains(&s) => {}
                _ => return false,
            }
        }
        true
    }

    fn check_datapath_init_pending(&self, api: &dyn CmisApi, host_lanes_mask: u32) -> bool {
        let d = api.get_dpinit_pending();
        for lane in 0..CMIS_MAX_HOST_LANES {
            if (1u32 << lane) & host_lanes_mask == 0 {
                continue;
            }
            let key = format!("DPInitPending{}", lane + 1);
            if !d.get(key.as_str()).and_then(|v| v.as_bool()).unwrap_or(false) {
                return false;
            }
        }
        true
    }

    fn check_datapath_state(&self, api: &dyn CmisApi, host_lanes_mask: u32, states: &[&str]) -> bool {
        let dp = api.get_datapath_state();
        for lane in 0..CMIS_MAX_HOST_LANES {
            if (1u32 << lane) & host_lanes_mask == 0 {
                continue;
            }
            let key = format!("DP{}State", lane + 1);
            match dp.get(key.as_str()).and_then(|v| v.as_str()) {
                Some(s) if states.contains(&s) => {}
                _ => return false,
            }
        }
        true
    }

    fn get_port_admin_status(&self, lport: &str) -> String {
        self.cfg_port_tbl
            .hget(lport, "admin_status")
            .ok()
            .flatten()
            .unwrap_or_else(|| "down".to_string())
    }

    /// `get_configured_laser_freq_from_db` — the user's `laser_freq` (GHz) from CONFIG_DB's
    /// PORT table (`0` when unset), int-parsed like the Python `int(laser_freq) if found`.
    pub fn get_configured_laser_freq_from_db(&self, lport: &str) -> i64 {
        self.cfg_port_tbl
            .hget(lport, "laser_freq")
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0)
    }

    /// `get_configured_tx_power_from_db` — the user's `tx_power` (dBm) from CONFIG_DB's PORT
    /// table (`0` when unset), float-parsed like the Python `float(power) if found`.
    pub fn get_configured_tx_power_from_db(&self, lport: &str) -> f64 {
        self.cfg_port_tbl
            .hget(lport, "tx_power")
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// `configure_tx_output_power` — clamp-log the request against the module's supported
    /// range, then provision it. Returns the `set_tx_power` result.
    pub fn configure_tx_output_power(&self, api: &dyn CmisApi, lport: &str, tx_power: f64) -> bool {
        let (min_p, max_p) = api.get_supported_power_config();
        if tx_power < min_p {
            eprintln!("{lport} configured tx power {tx_power} < minimum power {min_p} supported");
        }
        if tx_power > max_p {
            eprintln!("{lport} configured tx power {tx_power} > maximum power {max_p} supported");
        }
        api.set_tx_power(tx_power)
    }

    /// `validate_frequency_and_grid` — is `freq` (GHz) within the module's supported range and
    /// on the requested `grid` (75/100 GHz)? Mirrors the Python bounds + channel-alignment.
    pub fn validate_frequency_and_grid(
        &self,
        api: &dyn CmisApi,
        lport: &str,
        freq: i64,
        grid: u32,
    ) -> bool {
        let (supported_grid, _, _, lowf, highf) = api.get_supported_freq_config();
        if freq < lowf {
            eprintln!("{lport} configured freq:{freq} GHz is lower than the supported freq:{lowf} GHz");
            return false;
        }
        if freq > highf {
            eprintln!("{lport} configured freq:{freq} GHz is higher than the supported freq:{highf} GHz");
            return false;
        }
        if grid == 75 {
            if (supported_grid >> 7) & 0x1 != 1 {
                eprintln!("{lport} configured freq:{freq}GHz supported grid:{supported_grid} 75GHz is not supported");
                return false;
            }
            let chan = ((freq - 193100) as f64 / 25.0).round() as i64;
            if chan % 3 != 0 {
                eprintln!("{lport} configured freq:{freq}GHz is NOT in 75GHz grid");
                return false;
            }
        } else if grid == 100 {
            if (supported_grid >> 5) & 0x1 != 1 {
                eprintln!("{lport} configured freq:{freq}GHz 100GHz is not supported");
                return false;
            }
        } else {
            eprintln!("{lport} configured freq:{freq}GHz {grid}GHz is not supported");
            return false;
        }
        true
    }

    /// `configure_laser_frequency` — warn if a tuning is already in progress, then provision
    /// the frequency. Returns the `set_laser_freq` result.
    pub fn configure_laser_frequency(
        &self,
        api: &dyn CmisApi,
        lport: &str,
        freq: i64,
        grid: u32,
    ) -> bool {
        if api.get_tuning_in_progress() {
            eprintln!("{lport} Tuning in progress, subport selection may fail!");
        }
        api.set_laser_freq(freq, grid)
    }

    /// `post_port_active_apsel_to_db` → `active_apsel_hostlaneN`/`host_lane_count`/
    /// `media_lane_count` on `TRANSCEIVER_INFO`.
    pub fn post_port_active_apsel_to_db(
        &self,
        api: &dyn CmisApi,
        lport: &str,
        host_lanes_mask: u32,
        reset_apsel: bool,
    ) {
        let mut act_apsel = Value::Null;
        let mut appl_advt = Value::Null;
        if !reset_apsel {
            match api.get_active_apsel_hostlane() {
                Ok(v) => act_apsel = v,
                Err(_) => return,
            }
            appl_advt = api.get_application_advertisement();
        }

        let mut tuple_list: Vec<(String, String)> = Vec::new();
        let mut last_act_key: Option<String> = None;
        for lane in 0..CMIS_MAX_HOST_LANES {
            let field = format!("active_apsel_hostlane{}", lane + 1);
            if (1u32 << lane) & host_lanes_mask == 0 || reset_apsel {
                tuple_list.push((field, "N/A".to_string()));
                continue;
            }
            let key = format!("ActiveAppSelLane{}", lane + 1);
            let v = act_apsel
                .get(key.as_str())
                .cloned()
                .unwrap_or_else(|| Value::String("N/A".to_string()));
            let s = py_str(&v);
            last_act_key = Some(s.clone());
            tuple_list.push((field, s));
        }

        if !reset_apsel {
            let appl_advt_act = last_act_key.as_ref().and_then(|k| appl_advt.get(k.as_str()));
            let host_lane_count = appl_advt_act
                .and_then(|a| a.get("host_lane_count"))
                .map(py_str)
                .unwrap_or_else(|| "N/A".to_string());
            let media_lane_count = appl_advt_act
                .and_then(|a| a.get("media_lane_count"))
                .map(py_str)
                .unwrap_or_else(|| "N/A".to_string());
            tuple_list.push(("host_lane_count".to_string(), host_lane_count));
            tuple_list.push(("media_lane_count".to_string(), media_lane_count));
        } else {
            tuple_list.push(("host_lane_count".to_string(), "N/A".to_string()));
            tuple_list.push(("media_lane_count".to_string(), "N/A".to_string()));
        }

        // The reference only writes when TRANSCEIVER_INFO already has a row for the port.
        if !matches!(self.intf_tbl.get(lport), Ok(Some(_))) {
            return;
        }
        let _ = self.intf_tbl.set(lport, &tuple_list);
    }

    fn update_cmis_state_expiration_time(&mut self, lport: &str, duration_secs: f64) {
        let when = Instant::now()
            + Duration::from_secs_f64(duration_secs.max(0.0))
            + Duration::from_millis(CMIS_EXPIRATION_BUFFER_MS);
        if let Some(p) = self.port_dict.get_mut(lport) {
            p.cmis_expired = Some(when);
        }
    }

    /// `is_timer_expired(expired_time, current_time)`.
    pub fn is_timer_expired(&self, expired: Option<Instant>, current: Option<Instant>) -> bool {
        let Some(exp) = expired else {
            return false;
        };
        let now = current.unwrap_or_else(Instant::now);
        exp <= now
    }

    fn is_dwelled_state(state: &str) -> bool {
        matches!(
            state,
            CMIS_STATE_DP_PRE_INIT_CHECK
                | CMIS_STATE_DP_DEINIT
                | CMIS_STATE_AP_CONF
                | CMIS_STATE_DP_INIT
                | CMIS_STATE_DP_TXON
                | CMIS_STATE_DP_ACTIVATE
        )
    }

    /// Inter-state pacing gate (testbed adaptation). Returns `true` when the machine should
    /// HOLD (return without advancing) because the current datapath state has not yet been
    /// published for its full [`Self::inter_state_dwell`]. A zero dwell (unit tests) never
    /// holds, preserving one transition per pass.
    fn inter_state_dwell_gate(&mut self, lport: &str, state: &str) -> bool {
        if !Self::is_dwelled_state(state) {
            if let Some(p) = self.port_dict.get_mut(lport) {
                p.dwell_until = None;
                p.dwelled_state = None;
            }
            return false;
        }
        let dwell = self.inter_state_dwell;
        let now = Instant::now();
        if let Some(p) = self.port_dict.get_mut(lport) {
            if p.dwelled_state.as_deref() != Some(state) {
                p.dwelled_state = Some(state.to_string());
                p.dwell_until = Some(now + dwell);
            }
            match p.dwell_until {
                Some(until) => now < until,
                None => false,
            }
        } else {
            false
        }
    }

    fn arm_inter_state_dwell_on_entry(&mut self, lport: &str, state: &str) {
        if !Self::is_dwelled_state(state) {
            return;
        }
        let dwell = self.inter_state_dwell;
        let now = Instant::now();
        if let Some(p) = self.port_dict.get_mut(lport) {
            if p.dwelled_state.as_deref() != Some(state) {
                p.dwelled_state = Some(state.to_string());
                p.dwell_until = Some(now + dwell);
            }
        }
    }

    /// `handle_cmis_inserted_state` — app-select, lane masks, decommission, DEINIT gating.
    fn handle_cmis_inserted_state(&mut self, lport: &str, api: &dyn CmisApi) -> bool {
        let (host_lane_count, speed, subport) = {
            let p = &self.port_dict[lport];
            (p.host_lane_count.unwrap_or(0), p.speed.unwrap_or(0) as u32, p.subport.unwrap_or(0))
        };

        let Some(appl) = get_cmis_application_desired(api, host_lane_count, speed) else {
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_FAILED);
            return false;
        };
        self.port_dict.get_mut(lport).unwrap().appl = Some(appl);

        let max_host_lanes_mask = self.get_cmis_max_host_lanes_mask(api);
        let host_lanes_mask = self.get_cmis_host_lanes_mask(api, Some(appl), host_lane_count, subport);
        {
            let p = self.port_dict.get_mut(lport).unwrap();
            p.max_host_lanes_mask = max_host_lanes_mask;
            p.host_lanes_mask = host_lanes_mask;
        }
        if host_lanes_mask == 0 {
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_FAILED);
            return false;
        }

        let media_lane_count = api.get_media_lane_count(appl);
        let media_lane_assignment_options = api.get_media_lane_assignment_option(appl);
        {
            let p = self.port_dict.get_mut(lport).unwrap();
            p.media_lane_count = media_lane_count;
            p.media_lane_assignment_options = media_lane_assignment_options;
            p.max_media_lanes_mask = max_host_lanes_mask;
        }
        let media_lanes_mask = self.get_cmis_media_lanes_mask(appl, lport, subport);
        self.port_dict.get_mut(lport).unwrap().media_lanes_mask = media_lanes_mask;
        if media_lanes_mask == 0 {
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_FAILED);
            return false;
        }

        if self.is_decommission_required(api, lport) {
            self.set_decomm_pending(lport);
        }
        if self.is_decomm_lead_lport(lport) {
            {
                let p = self.port_dict.get_mut(lport).unwrap();
                p.appl = Some(0);
                p.host_lanes_mask = p.max_host_lanes_mask;
                p.media_lanes_mask = p.max_media_lanes_mask;
            }
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_DP_DEINIT);
            return false;
        } else if self.is_decomm_pending(lport) {
            if self.is_decomm_failed(lport) {
                self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_FAILED);
            }
            return false;
        }

        // Precondition gate: a port that is admin-down OR whose host has not asserted a good
        // Tx signal (`host_tx_ready != 'true'`) is torn down (datapath deinit + Tx-off) and
        // short-circuited to a forced-Tx-disabled terminal READY, never driven out of low power.
        //
        // Fast-reboot exception (cmis_manager_task.py:943): if fast reboot is enabled AND the
        // datapath is still ACTIVATED, SKIP the deinit/Tx-off so the live datapath survives the
        // xcvrd re-init (test_fast_reboot_dp_skip). Otherwise deinit as usual.
        let admin_status =
            self.port_dict[lport].admin_status.clone().unwrap_or_else(|| "down".to_string());
        let host_tx_ready =
            self.port_dict[lport].host_tx_ready.clone().unwrap_or_else(|| "false".to_string());
        if host_tx_ready != "true" || admin_status != "up" {
            let is_fast_reboot = self.is_fast_reboot_enabled_for_lport(lport);
            if is_fast_reboot && self.check_datapath_state(api, host_lanes_mask, &["DataPathActivated"])
            {
                // Skip datapath re-init in fast-reboot — preserve the activated datapath.
            } else {
                api.set_datapath_deinit(host_lanes_mask);
                api.tx_disable_channel(media_lanes_mask, true);
                let txoff_duration = api.get_datapath_tx_turnoff_duration() / 1000.0;
                {
                    let p = self.port_dict.get_mut(lport).unwrap();
                    p.forced_tx_disabled = true;
                    p.txoff_duration = txoff_duration;
                }
                self.update_cmis_state_expiration_time(lport, txoff_duration);
                self.post_port_active_apsel_to_db(api, lport, host_lanes_mask, true);
            }
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_READY);
            return false;
        }

        let forced = self.port_dict[lport].forced_tx_disabled;
        if forced {
            let txoff_duration = self.port_dict[lport].txoff_duration;
            self.update_cmis_state_expiration_time(lport, txoff_duration);
        }
        self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_DP_PRE_INIT_CHECK);
        true
    }

    /// `handle_cmis_dp_pre_init_check_state` — Tx-off confirm + reconfig gate.
    fn handle_cmis_dp_pre_init_check_state(&mut self, lport: &str, api: &dyn CmisApi) -> bool {
        let (host_lanes_mask, appl, expired, retries, forced) = {
            let p = &self.port_dict[lport];
            (p.host_lanes_mask, p.appl.unwrap_or(0), p.cmis_expired, p.cmis_retries, p.forced_tx_disabled)
        };

        if forced {
            if !self.check_datapath_state(
                api,
                host_lanes_mask,
                &["DataPathDeactivated", "DataPathInitialized"],
            ) {
                if self.is_timer_expired(expired, None) {
                    self.force_cmis_reinit(lport, retries + 1);
                }
                return false;
            }
            self.port_dict.get_mut(lport).unwrap().forced_tx_disabled = false;
        }

        // Configure the target output power on a coherent (ZR) module before the app-update
        // gate, skipping a redundant write of the already-configured value (Python @996).
        if api.is_coherent_module() {
            let tx_power = self.port_dict[lport].tx_power.unwrap_or(0.0);
            if tx_power != 0.0 && tx_power != api.get_tx_config_power()
                && !self.configure_tx_output_power(api, lport, tx_power)
            {
                eprintln!("{lport} failed to configure Tx power = {tx_power}");
            }
        }

        let mut need_update = self.is_cmis_application_update_required(api, appl, host_lanes_mask);

        // On a coherent module a new laser frequency forces a datapath re-init; an invalid
        // request is cleared so it is not retried (Python @1008-1018).
        if api.is_coherent_module() {
            let freq = self.port_dict[lport].laser_freq.unwrap_or(0);
            if freq != 0 && freq != api.get_laser_config_freq() {
                if self.validate_frequency_and_grid(api, lport, freq, 75) {
                    need_update = true;
                } else {
                    self.port_dict.get_mut(lport).unwrap().laser_freq = Some(0);
                }
            }
        }

        if !need_update {
            self.post_port_active_apsel_to_db(api, lport, host_lanes_mask, false);
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_READY);
            return false;
        }
        self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_DP_DEINIT);
        true
    }

    /// `handle_cmis_dp_deinit_state` — deinit + Tx-off + high-power, arm DpDeinit timer.
    /// In `ModuleLowPwr` the module has no provisioned datapath, so deinit/disable the FULL
    /// max lane set (not just the app's masks).
    fn handle_cmis_dp_deinit_state(&mut self, lport: &str, api: &dyn CmisApi) -> bool {
        let (retries, host_lanes_mask, media_lanes_mask, max_host, max_media) = {
            let p = &self.port_dict[lport];
            (
                p.cmis_retries,
                p.host_lanes_mask,
                p.media_lanes_mask,
                p.max_host_lanes_mask,
                p.max_media_lanes_mask,
            )
        };
        let mut deinit_host_lanes_mask = host_lanes_mask;
        let mut disable_media_lanes_mask = media_lanes_mask;
        if self.check_module_state(api, &["ModuleLowPwr"]) {
            deinit_host_lanes_mask = max_host;
            disable_media_lanes_mask = max_media;
        }

        api.set_datapath_deinit(deinit_host_lanes_mask);
        if !api.tx_disable_channel(disable_media_lanes_mask, true) {
            self.port_dict.get_mut(lport).unwrap().cmis_retries = retries + 1;
            return false;
        }

        api.set_lpmode(false, false);
        self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_AP_CONF);
        let dp_deinit = api.get_datapath_deinit_duration() / 1000.0;
        let pwr_up = api.get_module_pwr_up_duration() / 1000.0;
        self.update_cmis_state_expiration_time(lport, pwr_up.max(dp_deinit));
        true
    }

    /// `process_cmis_state_machine` — one transition per pass for a single lport.
    pub fn process_cmis_state_machine(&mut self, lport: &str, api: &dyn CmisApi) {
        let state = self.get_cmis_state(lport);

        if self.inter_state_dwell_gate(lport, &state) {
            return;
        }

        let (expired, retries, host_lanes_mask, appl) = {
            let p = &self.port_dict[lport];
            (p.cmis_expired, p.cmis_retries, p.host_lanes_mask, p.appl.unwrap_or(0))
        };

        if state != CMIS_STATE_INSERTED
            && !self.is_decomm_lead_lport(lport)
            && (host_lanes_mask == 0 || appl < 1)
        {
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_FAILED);
            return;
        }

        if retries > CMIS_MAX_RETRIES {
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_FAILED);
            return;
        }

        if state == CMIS_STATE_INSERTED {
            if !self.handle_cmis_inserted_state(lport, api) {
                return;
            }
        }

        if state == CMIS_STATE_DP_PRE_INIT_CHECK {
            if !self.handle_cmis_dp_pre_init_check_state(lport, api) {
                return;
            }
        } else if state == CMIS_STATE_DP_DEINIT {
            if !self.handle_cmis_dp_deinit_state(lport, api) {
                return;
            }
        } else if state == CMIS_STATE_AP_CONF {
            let ec = 0u32;
            if !self.check_module_state(api, &["ModuleReady"]) {
                if self.is_timer_expired(expired, None) {
                    self.force_cmis_reinit(lport, retries + 1);
                }
                return;
            }
            if !self.check_datapath_state(api, host_lanes_mask, &["DataPathDeactivated"]) {
                if self.is_timer_expired(expired, None) {
                    self.force_cmis_reinit(lport, retries + 1);
                }
                return;
            }
            // On a coherent (ZR) module, tune the laser while the datapath is Deactivated —
            // but not during a decommission handshake (Python @1142-1151).
            if !self.is_decomm_pending(lport) && api.is_coherent_module() {
                let freq = self.port_dict[lport].laser_freq.unwrap_or(0);
                if freq != 0 && !self.configure_laser_frequency(api, lport, freq, 75) {
                    eprintln!("{lport} failed to configure laser frequency {freq} GHz");
                }
            }
            api.set_application(host_lanes_mask, appl, ec);
            if !api.scs_apply_datapath_init(host_lanes_mask) {
                self.force_cmis_reinit(lport, retries + 1);
                return;
            }
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_DP_INIT);
        } else if state == CMIS_STATE_DP_INIT {
            if !self.check_config_error(api, host_lanes_mask, &["ConfigSuccess"]) {
                if self.is_timer_expired(expired, None) {
                    // Decommission failed: clear the pending status before the retry so the
                    // physical port isn't stuck in the decommission handshake (Python @1190).
                    if self.is_decomm_pending(lport) {
                        self.clear_decomm_pending(lport);
                    }
                    self.force_cmis_reinit(lport, retries + 1);
                }
                return;
            }
            if self.is_decomm_pending(lport) {
                self.clear_decomm_pending(lport);
                self.force_cmis_reinit(lport, 0);
                return;
            }
            let major_rev = api
                .get_cmis_rev()
                .split('.')
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            if major_rev >= 5 && !self.check_datapath_init_pending(api, host_lanes_mask) {
                self.force_cmis_reinit(lport, retries + 1);
                return;
            }
            let admin_status = self.port_dict[lport].admin_status.as_deref().unwrap_or("down");
            let host_tx_ready = self.port_dict[lport].host_tx_ready.as_deref().unwrap_or("false");
            if admin_status != "up" || host_tx_ready != "true" {
                return;
            }
            api.set_datapath_init(host_lanes_mask);
            let dp_init = api.get_datapath_init_duration() / 1000.0;
            self.update_cmis_state_expiration_time(lport, dp_init);
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_DP_TXON);
        } else if state == CMIS_STATE_DP_TXON {
            if !self.check_datapath_state(api, host_lanes_mask, &["DataPathInitialized"]) {
                if self.is_timer_expired(expired, None) {
                    self.force_cmis_reinit(lport, retries + 1);
                }
                return;
            }
            let media_lanes_mask = self.port_dict[lport].media_lanes_mask;
            api.tx_disable_channel(media_lanes_mask, false);
            let dp_init = api.get_datapath_init_duration() / 1000.0;
            let dp_txon = api.get_datapath_tx_turnon_duration() / 1000.0;
            self.update_cmis_state_expiration_time(lport, dp_init.max(dp_txon));
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_DP_ACTIVATE);
        } else if state == CMIS_STATE_DP_ACTIVATE {
            if !self.check_datapath_state(api, host_lanes_mask, &["DataPathActivated"]) {
                if self.is_timer_expired(expired, None) {
                    self.force_cmis_reinit(lport, retries + 1);
                }
                return;
            }
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_READY);
            self.post_port_active_apsel_to_db(api, lport, host_lanes_mask, false);
        }

        let entered_state = self.get_cmis_state(lport);
        self.arm_inter_state_dwell_on_entry(lport, &entered_state);
    }

    /// `process_single_lport` — drive one logical port's CMIS state-machine advance.
    pub fn process_single_lport(&mut self, lport: &str) {
        if !self.port_dict.contains_key(lport) {
            return;
        }
        let state = self.get_cmis_state(lport);
        if CMIS_TERMINAL_STATES.contains(&state.as_str()) || state == CMIS_STATE_UNKNOWN {
            if state != CMIS_STATE_READY {
                if let Some(p) = self.port_dict.get_mut(lport) {
                    p.appl = Some(0);
                    p.host_lanes_mask = 0;
                }
            }
            return;
        }

        if self.port_dict[lport].host_tx_ready.is_none() {
            let v = self.get_host_tx_status(lport);
            self.port_dict.get_mut(lport).unwrap().host_tx_ready = Some(v);
        }

        if self.port_dict[lport].admin_status.is_none() {
            let v = self.get_port_admin_status(lport);
            self.port_dict.get_mut(lport).unwrap().admin_status = Some(v);
        }

        let (pport, speed, lanes, subport) = {
            let p = &self.port_dict[lport];
            (
                p.index.unwrap_or(-1),
                p.speed_str.as_deref().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                p.lanes.clone().unwrap_or_default().trim().to_string(),
                p.subport.unwrap_or(0),
            )
        };
        if pport < 0 || speed == 0 || lanes.is_empty() || subport < 0 {
            return;
        }
        let host_lane_count = self.get_host_lane_count(lport, &lanes);

        let sfp = match self.chassis.sfp(pport as usize) {
            Ok(s) => s,
            Err(_) => {
                self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_REMOVED);
                return;
            }
        };
        match sfp.get_presence() {
            Ok(true) => {}
            _ => {
                self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_REMOVED);
                return;
            }
        }

        let api_box = match (self.api_factory)(sfp) {
            Some(a) => a,
            None => {
                self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_READY);
                return;
            }
        };
        let api: &dyn CmisApi = api_box.as_ref();

        if api.is_flat_memory() {
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_READY);
            return;
        }
        let is_cmis = common::is_cmis_api(api.get_module_type_abbreviation().as_deref());
        if !is_cmis {
            self.update_port_transceiver_status_table_sw_cmis_state(lport, CMIS_STATE_READY);
            return;
        }

        // Populate the coherent (ZR) tuning targets once, lazily, from CONFIG_DB — the
        // Python `'tx_power'/'laser_freq' not in port_dict` guard (@1321-1325).
        if api.is_coherent_module() {
            if self.port_dict[lport].tx_power.is_none() {
                let v = self.get_configured_tx_power_from_db(lport);
                self.port_dict.get_mut(lport).unwrap().tx_power = Some(v);
            }
            if self.port_dict[lport].laser_freq.is_none() {
                let v = self.get_configured_laser_freq_from_db(lport);
                self.port_dict.get_mut(lport).unwrap().laser_freq = Some(v);
            }
        }

        {
            let p = self.port_dict.get_mut(lport).unwrap();
            p.speed = Some(speed);
            p.subport = Some(subport);
            p.host_lane_count = Some(host_lane_count);
        }

        self.process_cmis_state_machine(lport, api);
    }

    /// One sweep over all logical ports — mirrors a single `task_worker` loop body.
    pub fn process_ports_once(&mut self) {
        let lports: Vec<String> = self.port_dict.keys().cloned().collect();
        for lport in lports {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            self.process_single_lport(&lport);
        }
    }

    /// `task_worker` — sweep every port until stopped (single-threaded; production paces
    /// this with the port-update observer). Terminates promptly when `stop` is set.
    pub fn task_worker(&mut self) {
        while !self.stop.load(Ordering::Relaxed) {
            self.process_ports_once();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// `Task.run()` — start the task's liveness thread (the analogue of the Python
    /// `threading.Thread(target=task_worker).start()`). The mock DB/HAL seams are `!Send`,
    /// so the port state machine is driven synchronously (`process_ports_once`) rather than
    /// inside this thread; the thread models the task lifecycle so `join()` terminates
    /// cleanly. The deployed bring-up loop lives in [`crate::daemon`].
    pub fn run(&mut self) {
        if self.skip_cmis_mgr || self.handle.is_some() {
            return;
        }
        self.stop.store(false, Ordering::Relaxed);
        let stop = self.stop.clone();
        self.handle = Some(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }));
    }

    /// True while the liveness thread is running (i.e. after `run`, before `join`).
    pub fn is_running(&self) -> bool {
        self.handle.is_some()
    }

    /// `Task.join()` — signal stop and join the liveness thread.
    pub fn join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmis::cmis_api::MockCmisApi;
    use crate::mock::{MockChassis, MockSfp, MockTable};
    use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType, PortMapping};
    use serde_json::json;

    /// A 400G/8-lane single-app advertisement (app 1) the app-select matches for a
    /// `host_lane_count=8`, `speed=400000` port.
    fn advert_400g_8lane() -> Value {
        json!({
            "1": {
                "host_electrical_interface_id": "400GAUI-8 C2M (Annex 120E)",
                "host_lane_count": 8,
                "media_lane_count": 8,
                "host_lane_assignment_options": 1,
                "media_lane_assignment_options": 1
            }
        })
    }

    fn dp_all(state: &str) -> Value {
        let mut m = serde_json::Map::new();
        for n in 1..=8 {
            m.insert(format!("DP{n}State"), json!(state));
        }
        Value::Object(m)
    }
    fn cfg_all(state: &str) -> Value {
        let mut m = serde_json::Map::new();
        for n in 1..=8 {
            m.insert(format!("ConfigStatusLane{n}"), json!(state));
        }
        Value::Object(m)
    }
    fn dpinit_all(v: bool) -> Value {
        let mut m = serde_json::Map::new();
        for n in 1..=8 {
            m.insert(format!("DPInitPending{n}"), json!(v));
        }
        Value::Object(m)
    }
    fn apsel_all(v: u64) -> Value {
        let mut m = serde_json::Map::new();
        for n in 1..=8 {
            m.insert(format!("ActiveAppSelLane{n}"), json!(v));
        }
        Value::Object(m)
    }

    struct Env {
        status_sw: MockTable,
        intf: MockTable,
        cfg: MockTable,
        state: MockTable,
    }

    fn build_task(api: MockCmisApi, present: bool) -> (CmisManagerTask, MockCmisApi, Env) {
        let status_sw = MockTable::new();
        let intf = MockTable::new();
        let cfg = MockTable::new();
        let state = MockTable::new();
        let chassis = MockChassis::with_sfps(vec![if present {
            MockSfp::present()
        } else {
            MockSfp::absent()
        }]);
        let api_for_factory = api.clone();
        let factory: CmisApiFactory =
            Box::new(move |_sfp| Some(Box::new(api_for_factory.clone()) as Box<dyn CmisApi>));
        let mut task = CmisManagerTask::new(
            PortMapping::new(),
            Box::new(chassis),
            Rc::new(status_sw.clone()),
            Rc::new(intf.clone()),
            Rc::new(cfg.clone()),
            Rc::new(state.clone()),
            factory,
        );
        // Unit tests drive one transition per pass — disable the production pacing dwell.
        task.inter_state_dwell = Duration::ZERO;
        (task, api, Env { status_sw, intf, cfg, state })
    }

    fn set_event(lport: &str, pport: i32, fields: &[(&str, &str)]) -> PortChangeEvent {
        let mut ev = PortChangeEvent::new(lport, pport, 0, PortEventType::PortSet);
        ev.port_dict = Some(fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        ev
    }

    // A `Box<dyn CmisApi>` view over a clone (borrow-checker helper for the handler tests).
    fn boxed(api: &MockCmisApi) -> Box<dyn CmisApi> {
        Box::new(api.clone())
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_update_port_transceiver_status_table_sw_cmis_state
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_update_port_transceiver_status_table_sw_cmis_state() {
        let (task, _api, env) = build_task(MockCmisApi::new(), true);
        // A prior status/error row must survive the cmis_state field-merge.
        env.status_sw.hset("Ethernet0", "status", "1").unwrap();
        env.status_sw.hset("Ethernet0", "error", "N/A").unwrap();
        task.update_port_transceiver_status_table_sw_cmis_state("Ethernet0", CMIS_STATE_INSERTED);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("INSERTED"));
        assert_eq!(env.status_sw.field("Ethernet0", "status").as_deref(), Some("1"));
        assert_eq!(env.status_sw.field("Ethernet0", "error").as_deref(), Some("N/A"));
        task.update_port_transceiver_status_table_sw_cmis_state("Ethernet0", CMIS_STATE_READY);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("READY"));
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_handle_port_change_event (on_port_update_event SET/DEL)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_handle_port_change_event() {
        let (mut task, _api, env) = build_task(MockCmisApi::new(), true);

        // SET soaks the datapath config into port_dict and force-reinits to INSERTED.
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "0,1,2,3,4,5,6,7"), ("admin_status", "up"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);
        assert!(task.port_dict.contains_key("Ethernet0"));
        let p = &task.port_dict["Ethernet0"];
        assert_eq!(p.speed, Some(400000));
        assert_eq!(p.admin_status.as_deref(), Some("up"));
        assert_eq!(p.index, Some(0));
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("INSERTED"));

        // A non-Ethernet key (PortConfigDone) is ignored.
        let mut done = PortChangeEvent::new("PortConfigDone", 0, 0, PortEventType::PortSet);
        done.port_dict = Some(vec![]);
        task.on_port_update_event(&done);
        assert_eq!(task.port_dict.len(), 1);

        // A transceiver plug-out (non-CONFIG_DB DEL) stamps REMOVED but keeps the entry.
        let mut del = PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortDel);
        del.db_name = Some("STATE_DB".to_string());
        del.table_name = Some("TRANSCEIVER_INFO".to_string());
        task.on_port_update_event(&del);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("REMOVED"));
        assert!(task.port_dict.contains_key("Ethernet0"));

        // A CONFIG_DB PORT DEL de-provisions the port (pop it, no REMOVED stamp needed).
        let mut cfg_del = PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortDel);
        cfg_del.db_name = Some("CONFIG_DB".to_string());
        cfg_del.table_name = Some("PORT".to_string());
        task.on_port_update_event(&cfg_del);
        assert!(!task.port_dict.contains_key("Ethernet0"));
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_dp_deinit_low_pwr_deinits_and_disables_all_lanes
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_dp_deinit_low_pwr_deinits_and_disables_all_lanes() {
        let api = MockCmisApi::new();
        api.set_module_state("ModuleLowPwr");
        let (mut task, api, _env) = build_task(api, true);
        task.port_dict.insert(
            "Ethernet0".to_string(),
            PortInfo {
                index: Some(0),
                appl: Some(1),
                host_lanes_mask: 0x0f, // only lanes 0-3 provisioned...
                media_lanes_mask: 0x0f,
                max_host_lanes_mask: 0xff, // ...but low-power deinits the FULL max set.
                max_media_lanes_mask: 0xff,
                ..Default::default()
            },
        );
        assert!(task.handle_cmis_dp_deinit_state("Ethernet0", &*boxed(&api)));
        // ModuleLowPwr → the FULL 0xff lane set is deinit'd + disabled, not the app masks.
        assert_eq!(api.last_deinit_mask(), 0xff);
        assert_eq!(api.last_tx_disable_mask(), 0xff);
        assert!(api.call_count("set_lpmode") >= 1);
    }

    /// A map-backed `StateDbHget` double so `initialize_fast_reboot_status` can be driven
    /// with a canned `FAST_RESTART_ENABLE_TABLE|system.enable` (the Rust analogue of the
    /// Python `@patch('...common.is_fast_reboot_enabled', MagicMock(return_value=...))`).
    #[derive(Default)]
    struct MockRebootDb {
        fields: std::collections::HashMap<(String, String), String>,
    }
    impl MockRebootDb {
        fn with_fast_reboot(enabled: bool) -> Self {
            let mut db = MockRebootDb::default();
            if enabled {
                db.fields.insert(
                    ("FAST_RESTART_ENABLE_TABLE|system".to_string(), "enable".to_string()),
                    "true".to_string(),
                );
            }
            db
        }
    }
    impl StateDbHget for MockRebootDb {
        fn get_field(&self, key: &str, field: &str) -> Option<String> {
            self.fields.get(&(key.to_string(), field.to_string())).cloned()
        }
    }

    /// Build a task whose port map + namespaces come from the caller (for the
    /// per-namespace fast-reboot tests). Mirrors `build_task` but seeds a specific mapping.
    fn build_task_with_mapping(
        api: MockCmisApi,
        present: bool,
        port_mapping: PortMapping,
    ) -> (CmisManagerTask, MockCmisApi, Env) {
        let status_sw = MockTable::new();
        let intf = MockTable::new();
        let cfg = MockTable::new();
        let state = MockTable::new();
        let chassis = MockChassis::with_sfps(vec![if present {
            MockSfp::present()
        } else {
            MockSfp::absent()
        }]);
        let api_for_factory = api.clone();
        let factory: CmisApiFactory =
            Box::new(move |_sfp| Some(Box::new(api_for_factory.clone()) as Box<dyn CmisApi>));
        let mut task = CmisManagerTask::new(
            port_mapping,
            Box::new(chassis),
            Rc::new(status_sw.clone()),
            Rc::new(intf.clone()),
            Rc::new(cfg.clone()),
            Rc::new(state.clone()),
            factory,
        );
        task.inter_state_dwell = Duration::ZERO;
        (task, api, Env { status_sw, intf, cfg, state })
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_is_fast_reboot_enabled_for_lport (+ default namespace) live in
    // the #[ignore] stub section below, next to the ported #[ignore] placeholder
    // they replace. The task_worker fast-reboot tests are here (they use build_task).
    // ---------------------------------------------------------------------------------

    /// Drive a QSFP-DD port whose host has NOT asserted Tx (`host_tx_ready='false'`) with
    /// fast reboot enabled but the datapath still DEACTIVATED: because the datapath is not
    /// activated, the fast-reboot skip does NOT apply — the port is torn down (one
    /// `set_datapath_deinit` + one `tx_disable_channel`) and short-circuited to READY. Rust
    /// counterpart of `tests/test_xcvrd.py::test_CmisManagerTask_task_worker_fastboot`.
    #[test]
    fn test_CmisManagerTask_task_worker_fastboot() {
        let api = MockCmisApi::new();
        api.set_module_type_abbreviation(Some("QSFP-DD"));
        api.set_module_state("ModuleReady");
        api.set_cmis_rev("5.0");
        api.set_application_advertisement(advert_400g_8lane());
        api.set_active_apsel(apsel_all(0));
        api.set_application_by_lane(0);
        api.set_datapath_state_value(dp_all("DataPathDeactivated")); // not activated → no skip
        api.set_config_status(cfg_all("ConfigSuccess"));
        api.set_dpinit_pending(dpinit_all(true));
        api.set_durations_ms(10.0, 10.0, 10.0, 10.0, 10.0, 10.0);

        let (mut task, api, env) = build_task(api, true);
        env.intf.hset("Ethernet0", "present", "1").unwrap();
        task.initialize_fast_reboot_status(&MockRebootDb::with_fast_reboot(true));

        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "0,1,2,3,4,5,6,7"), ("admin_status", "up"), ("host_tx_ready", "false"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("INSERTED"));

        // INSERTED → deinit gate (host_tx_ready != 'true'); fast reboot on but datapath NOT
        // activated → the normal deinit path runs, then READY.
        task.process_single_lport("Ethernet0");
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("READY"));
        assert_eq!(api.call_count("set_datapath_deinit"), 1);
        assert_eq!(api.call_count("tx_disable_channel"), 1);
    }

    /// NEW (bridge/mock seams): the fast-reboot datapath-skip itself. With fast reboot
    /// enabled AND the datapath still ACTIVATED, re-initialising the port (host_tx_ready
    /// drops) must SKIP the DataPathDeinit so the live datapath survives — no
    /// `set_datapath_deinit`/`tx_disable_channel`, straight to READY. The control (fast
    /// reboot disabled) DOES deinit, proving the skip is a real fast-reboot behaviour.
    #[test]
    fn fast_reboot_skips_dp_deinit() {
        let build = |fast_reboot: bool| {
            let api = MockCmisApi::new();
            api.set_module_type_abbreviation(Some("QSFP-DD"));
            api.set_module_state("ModuleReady");
            api.set_cmis_rev("5.0");
            api.set_application_advertisement(advert_400g_8lane());
            api.set_active_apsel(apsel_all(0));
            api.set_application_by_lane(0);
            api.set_datapath_state_value(dp_all("DataPathActivated")); // live datapath
            api.set_config_status(cfg_all("ConfigSuccess"));
            api.set_dpinit_pending(dpinit_all(true));
            api.set_durations_ms(10.0, 10.0, 10.0, 10.0, 10.0, 10.0);
            let (mut task, api, env) = build_task(api, true);
            env.intf.hset("Ethernet0", "present", "1").unwrap();
            task.initialize_fast_reboot_status(&MockRebootDb::with_fast_reboot(fast_reboot));
            let ev = set_event(
                "Ethernet0",
                0,
                &[("speed", "400000"), ("lanes", "0,1,2,3,4,5,6,7"), ("admin_status", "up"), ("host_tx_ready", "false"), ("subport", "0")],
            );
            task.on_port_update_event(&ev);
            task.process_single_lport("Ethernet0");
            (api, env)
        };

        // Fast reboot ON + datapath activated → SKIP the deinit.
        let (api, env) = build(true);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("READY"));
        assert_eq!(api.call_count("set_datapath_deinit"), 0, "fast reboot must skip DataPathDeinit");
        assert_eq!(api.call_count("tx_disable_channel"), 0, "fast reboot must not force Tx off");

        // Control: fast reboot OFF → the same admin state DOES deinit the datapath.
        let (api, env) = build(false);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("READY"));
        assert_eq!(api.call_count("set_datapath_deinit"), 1, "normal reboot must deinit the datapath");
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_test_is_timer_expired
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_test_is_timer_expired() {
        let (task, _api, _env) = build_task(MockCmisApi::new(), true);
        let now = Instant::now();
        // No timer armed → never expired.
        assert!(!task.is_timer_expired(None, Some(now)));
        // Future deadline → not expired.
        assert!(!task.is_timer_expired(Some(now + Duration::from_secs(5)), Some(now)));
        // Past deadline → expired.
        assert!(task.is_timer_expired(Some(now - Duration::from_secs(1)), Some(now)));
        // Exactly now → expired (<=).
        assert!(task.is_timer_expired(Some(now), Some(now)));
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_task_run_stop — thread lifecycle (start + stop).
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_task_run_stop() {
        let (mut task, _api, _env) = build_task(MockCmisApi::new(), true);
        assert!(!task.is_running());
        task.run();
        assert!(task.is_running());
        task.join();
        assert!(!task.is_running());
        assert!(task.stop.load(Ordering::Relaxed));
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_task_worker — a sweep advances a port; stop terminates the loop.
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_task_worker() {
        // A flat-memory module short-circuits straight to READY on one sweep.
        let api = MockCmisApi::new();
        api.set_flat_memory(true);
        let (mut task, _api, env) = build_task(api, true);
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "0,1,2,3,4,5,6,7"), ("admin_status", "up"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);
        task.process_ports_once();
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("READY"));

        // task_worker must return promptly once stop is set (no hang).
        task.stop.store(true, Ordering::Relaxed);
        task.task_worker();
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_task_run_with_exception (new) — an error path in a sweep does
    // not corrupt the task; run/join still work. (Rust analogue of the Python test that a
    // task-worker exception is caught and the loop keeps going.)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_task_run_with_exception() {
        // A port whose SFP is absent gets stamped REMOVED (an error path) without panicking;
        // the sweep continues to the next port.
        let (mut task, _api, env) = build_task(MockCmisApi::new(), false);
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "0,1,2,3,4,5,6,7"), ("admin_status", "up"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);
        task.process_ports_once();
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("REMOVED"));
        // Run/stop still clean after an error-path sweep.
        task.run();
        task.join();
        assert!(!task.is_running());
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_get_xcvr_api_exception (new) — no CMIS api for a present module
    // (`factory -> None`, the Python `sfp.get_xcvr_api()` returning None) short-circuits
    // the port to READY without touching the datapath.
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_get_xcvr_api_exception() {
        let status_sw = MockTable::new();
        let intf = MockTable::new();
        let cfg = MockTable::new();
        let state = MockTable::new();
        let chassis = MockChassis::with_sfps(vec![MockSfp::present()]);
        let factory: CmisApiFactory = Box::new(|_sfp| None);
        let mut task = CmisManagerTask::new(
            PortMapping::new(),
            Box::new(chassis),
            Rc::new(status_sw.clone()),
            Rc::new(intf.clone()),
            Rc::new(cfg.clone()),
            Rc::new(state.clone()),
            factory,
        );
        task.inter_state_dwell = Duration::ZERO;
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "0,1,2,3,4,5,6,7"), ("admin_status", "up"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);
        task.process_ports_once();
        assert_eq!(status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("READY"));
    }

    // ---------------------------------------------------------------------------------
    // cmis_state_machine_progression_scripted_sfp (new) — drive INSERTED→…→READY, updating
    // the scripted module's datapath state at each phase, and assert every transition is
    // published to TRANSCEIVER_STATUS_SW.cmis_state in order.
    // ---------------------------------------------------------------------------------
    #[test]
    fn cmis_state_machine_progression_scripted_sfp() {
        let api = MockCmisApi::new();
        api.set_module_type_abbreviation(Some("QSFP-DD"));
        api.set_module_state("ModuleReady");
        api.set_cmis_rev("5.0");
        api.set_application_advertisement(advert_400g_8lane());
        api.set_active_apsel(apsel_all(0)); // fresh module → no active app, no decommission
        api.set_application_by_lane(0); // active app 0 → application update required
        api.set_datapath_state_value(dp_all("DataPathDeactivated"));
        api.set_config_status(cfg_all("ConfigSuccess"));
        api.set_dpinit_pending(dpinit_all(true));
        // Short durations so no timer expiry interferes with the scripted advance.
        api.set_durations_ms(10.0, 10.0, 10.0, 10.0, 10.0, 10.0);

        let (mut task, api, env) = build_task(api, true);
        // Seed TRANSCEIVER_INFO so post_port_active_apsel_to_db writes on READY.
        env.intf.hset("Ethernet0", "present", "1").unwrap();
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "0,1,2,3,4,5,6,7"), ("admin_status", "up"), ("host_tx_ready", "true"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some("INSERTED"));

        let record = |env: &Env, seq: &mut Vec<String>| {
            if let Some(s) = env.status_sw.field("Ethernet0", "cmis_state") {
                if seq.last() != Some(&s) {
                    seq.push(s);
                }
            }
        };
        let mut seq: Vec<String> = vec![];
        record(&env, &mut seq);

        // INSERTED → DP_PRE_INIT_CHECK
        task.process_single_lport("Ethernet0");
        record(&env, &mut seq);
        // DP_PRE_INIT_CHECK → DP_DEINIT (application update required)
        task.process_single_lport("Ethernet0");
        record(&env, &mut seq);
        // DP_DEINIT → AP_CONFIGURED
        task.process_single_lport("Ethernet0");
        record(&env, &mut seq);
        // AP_CONFIGURED → DP_INIT (ModuleReady + DataPathDeactivated)
        task.process_single_lport("Ethernet0");
        record(&env, &mut seq);
        // DP_INIT → DP_TXON (ConfigSuccess + DPInitPending)
        task.process_single_lport("Ethernet0");
        record(&env, &mut seq);
        // Module finishes datapath init → DP_TXON checks DataPathInitialized.
        api.set_datapath_state_value(dp_all("DataPathInitialized"));
        // DP_TXON → DP_ACTIVATION
        task.process_single_lport("Ethernet0");
        record(&env, &mut seq);
        // Module activates the datapath and reflects the applied app in ActiveAppSelLane.
        api.set_datapath_state_value(dp_all("DataPathActivated"));
        api.set_active_apsel(apsel_all(1));
        // DP_ACTIVATION → READY
        task.process_single_lport("Ethernet0");
        record(&env, &mut seq);

        assert_eq!(
            seq,
            vec![
                "INSERTED",
                "DP_PRE_INIT_CHECK",
                "DP_DEINIT",
                "AP_CONFIGURED",
                "DP_INIT",
                "DP_TXON",
                "DP_ACTIVATION",
                "READY",
            ]
        );
        // On READY the applied active apsel + lane counts are posted to TRANSCEIVER_INFO.
        assert_eq!(env.intf.field("Ethernet0", "active_apsel_hostlane1").as_deref(), Some("1"));
        assert_eq!(env.intf.field("Ethernet0", "host_lane_count").as_deref(), Some("8"));
        assert_eq!(env.intf.field("Ethernet0", "media_lane_count").as_deref(), Some("8"));
        assert!(api.call_count("set_application") >= 1);
        assert!(api.call_count("scs_apply_datapath_init") >= 1);
    }

    /// Script a fresh-module (low-power, datapath-deactivated, no active app) and step
    /// `process_single_lport` through the whole CMIS bring-up, updating the module's
    /// datapath state at each phase exactly like `cmis_state_machine_progression_scripted_sfp`.
    /// Returns the ordered list of DISTINCT `cmis_state` values published, starting from the
    /// state already latched when the bring-up begins.
    fn drive_full_bringup(
        task: &mut CmisManagerTask,
        api: &MockCmisApi,
        env: &Env,
        lport: &str,
    ) -> Vec<String> {
        // Fresh module: deactivated datapath, no active app (apsel 0), applied app 0 → an
        // application update is required, so the machine runs the full DP_DEINIT→…→READY path.
        api.set_module_state("ModuleReady");
        api.set_datapath_state_value(dp_all("DataPathDeactivated"));
        api.set_config_status(cfg_all("ConfigSuccess"));
        api.set_dpinit_pending(dpinit_all(true));
        api.set_active_apsel(apsel_all(0));
        api.set_application_by_lane(0);

        let mut seq: Vec<String> = vec![];
        let record = |env: &Env, seq: &mut Vec<String>| {
            if let Some(s) = env.status_sw.field(lport, "cmis_state") {
                if seq.last() != Some(&s) {
                    seq.push(s);
                }
            }
        };
        record(env, &mut seq);
        // INSERTED → DP_PRE_INIT_CHECK → DP_DEINIT → AP_CONFIGURED → DP_INIT → DP_TXON
        for _ in 0..5 {
            task.process_single_lport(lport);
            record(env, &mut seq);
        }
        // Module finishes datapath init → DP_TXON observes DataPathInitialized.
        api.set_datapath_state_value(dp_all("DataPathInitialized"));
        task.process_single_lport(lport); // DP_TXON → DP_ACTIVATION
        record(env, &mut seq);
        // Module activates the datapath and reflects the applied app.
        api.set_datapath_state_value(dp_all("DataPathActivated"));
        api.set_active_apsel(apsel_all(1));
        task.process_single_lport(lport); // DP_ACTIVATION → READY
        record(env, &mut seq);
        seq
    }

    // ---------------------------------------------------------------------------------
    // cmis_replug_after_ready_reruns_full_progression (new) — a transceiver plug-out on a
    // port already at terminal READY must stamp cmis_state=REMOVED (not leave the stale
    // READY), and the subsequent re-plug must re-run the WHOLE datapath machine rather than
    // short-circuit straight to READY off the stale terminal state. This is the trait-seam
    // regression for test_cmis_state_progression re-plug: the deployed daemon fuses
    // SfpStateUpdateTask + CmisManagerTask, so its removal handler must perform the same
    // on_port_update_event PORT_DEL → REMOVED stamp this seam does here.
    // ---------------------------------------------------------------------------------
    #[test]
    fn cmis_replug_after_ready_reruns_full_progression() {
        let api = MockCmisApi::new();
        api.set_module_type_abbreviation(Some("QSFP-DD"));
        api.set_cmis_rev("5.0");
        api.set_application_advertisement(advert_400g_8lane());
        // Short durations so no timer expiry interferes with the scripted advance.
        api.set_durations_ms(10.0, 10.0, 10.0, 10.0, 10.0, 10.0);

        let (mut task, api, env) = build_task(api, true);
        // Seed TRANSCEIVER_INFO so post_port_active_apsel_to_db writes on READY.
        env.intf.hset("Ethernet0", "present", "1").unwrap();
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "0,1,2,3,4,5,6,7"), ("admin_status", "up"), ("host_tx_ready", "true"), ("subport", "0")],
        );

        // --- first bring-up drives all the way to READY --------------------------------
        task.on_port_update_event(&ev);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_INSERTED));
        let first = drive_full_bringup(&mut task, &api, &env, "Ethernet0");
        assert_eq!(first.last().map(String::as_str), Some(CMIS_STATE_READY), "first bring-up must reach READY (first={first:?})");

        // --- transceiver plug-out: STATE_DB TRANSCEIVER_INFO DEL clears READY → REMOVED ---
        let mut del = PortChangeEvent::new("Ethernet0", 0, 0, PortEventType::PortDel);
        del.db_name = Some("STATE_DB".to_string());
        del.table_name = Some("TRANSCEIVER_INFO".to_string());
        task.on_port_update_event(&del);
        assert_eq!(
            env.status_sw.field("Ethernet0", "cmis_state").as_deref(),
            Some(CMIS_STATE_REMOVED),
            "plug-out must clear the terminal READY to REMOVED, not leave it stale"
        );

        // --- re-plug: PORT_SET force-reinits the machine back to INSERTED ---------------
        task.on_port_update_event(&ev);
        assert_eq!(
            env.status_sw.field("Ethernet0", "cmis_state").as_deref(),
            Some(CMIS_STATE_INSERTED),
            "re-plug must restart the machine at INSERTED"
        );

        // --- the re-plug bring-up must traverse the WHOLE machine again, not jump to READY ---
        let second = drive_full_bringup(&mut task, &api, &env, "Ethernet0");
        assert_eq!(second.last().map(String::as_str), Some(CMIS_STATE_READY), "re-plug bring-up must reach READY (second={second:?})");

        let intermediate: std::collections::BTreeSet<&str> = second
            .iter()
            .map(String::as_str)
            .filter(|s| {
                matches!(
                    *s,
                    CMIS_STATE_INSERTED
                        | CMIS_STATE_DP_PRE_INIT_CHECK
                        | CMIS_STATE_DP_DEINIT
                        | CMIS_STATE_AP_CONF
                        | CMIS_STATE_DP_INIT
                        | CMIS_STATE_DP_TXON
                        | CMIS_STATE_DP_ACTIVATE
                )
            })
            .collect();
        assert!(
            intermediate.len() >= 4,
            "re-plug published too few intermediate cmis_states {intermediate:?} (second={second:?}); \
             the machine short-circuited instead of traversing the datapath bring-up"
        );
        // A LATE datapath state must be reached before READY (mirrors the reference assertion).
        assert!(
            intermediate.contains(CMIS_STATE_DP_TXON) || intermediate.contains(CMIS_STATE_DP_ACTIVATE),
            "re-plug never reached a late datapath state before READY (intermediates {intermediate:?})"
        );
    }

    // =================================================================================
    // app-select / lane-count / host_tx_ready
    // =================================================================================

    /// Insert a bare `port_dict` entry with a physical-port `index` (the Python tests'
    /// `task.port_dict['Ethernet0'] = {'index': N, 'asic_id': 0}`).
    fn put_port(task: &mut CmisManagerTask, lport: &str, index: i64) {
        task.port_dict
            .insert(lport.to_string(), PortInfo { index: Some(index), ..Default::default() });
    }

    /// Build an `ActiveAppSelLaneN` object from a per-lane app-code array.
    fn apsel_from(v: &[u32; 8]) -> Value {
        let mut m = serde_json::Map::new();
        for (i, a) in v.iter().enumerate() {
            m.insert(format!("ActiveAppSelLane{}", i + 1), json!(a));
        }
        Value::Object(m)
    }

    /// Seed CONFIG_DB PORT rows `(lport, index, speed, lanes, subport)` for sibling tests.
    fn seed_siblings(cfg: &MockTable, specs: &[(&str, i64, u32, &str, i64)]) {
        for (lport, index, speed, lanes, subport) in specs {
            cfg.hset(lport, "index", &index.to_string()).unwrap();
            cfg.hset(lport, "speed", &speed.to_string()).unwrap();
            cfg.hset(lport, "lanes", lanes).unwrap();
            cfg.hset(lport, "subport", &subport.to_string()).unwrap();
        }
    }

    /// A 3-app advertisement whose apps have distinct (host_lane_count, speed) signatures so
    /// real app-select resolves each sibling deterministically: app1=4-lane/100G,
    /// app2=8-lane/400G, app3=4-lane/200G. All lanes assignable (`hlao=0xff`).
    fn advert_apps_123() -> Value {
        json!({
            "1": {"host_electrical_interface_id":"100GAUI-4 C2M (Annex 135E)","host_lane_count":4,"media_lane_count":4,"host_lane_assignment_options":255,"media_lane_assignment_options":255},
            "2": {"host_electrical_interface_id":"400GAUI-8 C2M (Annex 120E)","host_lane_count":8,"media_lane_count":8,"host_lane_assignment_options":255,"media_lane_assignment_options":255},
            "3": {"host_electrical_interface_id":"200GAUI-4 C2M (Annex 120F)","host_lane_count":4,"media_lane_count":4,"host_lane_assignment_options":255,"media_lane_assignment_options":255}
        })
    }

    /// The exact advertisement from `test_CmisManagerTask_get_cmis_host_lanes_mask`
    /// (apps 1/2/3 with `hlao` 1/17/255).
    fn advert_host_lanes_mask() -> Value {
        json!({
            "1": {"host_electrical_interface_id":"400GAUI-8 C2M (Annex 120E)","module_media_interface_id":"400GBASE-DR4 (Cl 124)","media_lane_count":4,"host_lane_count":8,"host_lane_assignment_options":1},
            "2": {"host_electrical_interface_id":"CAUI-4 C2M (Annex 83E)","module_media_interface_id":"Active Cable assembly","media_lane_count":4,"host_lane_count":4,"host_lane_assignment_options":17},
            "3": {"host_electrical_interface_id":"50GAUI-1 C2M","module_media_interface_id":"50GBASE-SR","media_lane_count":1,"host_lane_count":1,"host_lane_assignment_options":255}
        })
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_get_host_lane_count (gearbox line-lanes vs port-config lanes)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_get_host_lane_count() {
        let (mut task, _api, _env) = build_task(MockCmisApi::new(), true);
        let cases: [(&[(&str, u32)], &str, &str, u32); 6] = [
            (&[("Ethernet0", 2)], "Ethernet0", "25,26,27,28", 2),
            (&[("Ethernet0", 4)], "Ethernet0", "29,30", 4),
            (&[("Ethernet4", 2)], "Ethernet0", "33,34,35,36", 4),
            (&[], "Ethernet0", "37,38", 2),
            (&[("Ethernet0", 2), ("Ethernet4", 4)], "Ethernet0", "25,26,27,28", 2),
            (&[("Ethernet4", 4)], "Ethernet8", "41,42,43", 3),
        ];
        for (dict, lport, lanes, expected) in cases {
            let map: HashMap<String, u32> = dict.iter().map(|(k, v)| (k.to_string(), *v)).collect();
            task.set_gearbox_lanes_dict(map);
            assert_eq!(task.get_host_lane_count(lport, lanes), expected, "lport={lport}");
        }
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_get_cmis_host_lanes_mask (real app-select + start-bit tiling)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_get_cmis_host_lanes_mask() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(advert_host_lanes_mask());
        let (task, api, _env) = build_task(api, true);
        let cases = [
            (8u32, 400000u32, 0i64, 0xFFu32),
            (4, 100000, 1, 0xF),
            (4, 100000, 2, 0xF0),
            (4, 100000, 0, 0xF),
            (4, 100000, 9, 0x0),
            (1, 50000, 2, 0x2),
            (1, 200000, 2, 0x0),
        ];
        for (hlc, speed, subport, expected) in cases {
            let appl = get_cmis_application_desired(&api, hlc, speed);
            assert_eq!(
                task.get_cmis_host_lanes_mask(&api, appl, hlc, subport),
                expected,
                "hlc={hlc} speed={speed} subport={subport}"
            );
        }
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_is_cmis_application_update_required
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_is_cmis_application_update_required() {
        let default_dp = dp_all("DataPathActivated");
        let default_cfg = cfg_all("ConfigSuccess");
        let mut cfg_l8 = cfg_all("ConfigSuccess");
        cfg_l8["ConfigStatusLane8"] = json!("ConfigUndefined");

        // (app_new, host_lanes_mask, lane_appl_code, dp_state, config_status, expected).
        // NOTE: the Python `(-1, ...)` row exercises `app_new <= 0 → False`; the Rust signature
        // takes an unsigned app code, so `app_new == 0` is the only reachable non-positive case
        // and is used here as the faithful analogue.
        let cases: Vec<(u32, u32, Vec<(u32, u32)>, Value, Value, bool)> = vec![
            (1, 0x0F, vec![(0, 1), (1, 1), (2, 1), (3, 1)], default_dp.clone(), default_cfg.clone(), false),
            (1, 0x0F, vec![(0, 1), (1, 1), (2, 1), (3, 0)], default_dp.clone(), default_cfg.clone(), true),
            (1, 0xF0, vec![(4, 1), (5, 1), (6, 1), (7, 1)], default_dp.clone(), default_cfg.clone(), false),
            (1, 0xF0, vec![(4, 1), (5, 1), (6, 1), (7, 1)], default_dp.clone(), cfg_l8.clone(), true),
            (1, 0xF0, vec![(4, 1), (5, 7), (6, 1), (7, 1)], default_dp.clone(), default_cfg.clone(), true),
            (4, 0xF0, vec![(4, 1), (5, 7), (6, 1), (7, 1)], default_dp.clone(), default_cfg.clone(), true),
            (3, 0xC0, vec![(7, 3), (8, 3)], default_dp.clone(), default_cfg.clone(), false),
            (1, 0x0F, vec![], default_dp.clone(), default_cfg.clone(), true),
            (0, 0x0F, vec![], default_dp.clone(), default_cfg.clone(), false),
        ];
        for (app_new, mask, lane_appl, dp, cfg, expected) in cases {
            let api = MockCmisApi::new();
            api.set_flat_memory(false);
            api.set_datapath_state_value(dp);
            api.set_config_status(cfg);
            for (lane, appl) in &lane_appl {
                api.set_application_for_lane(*lane, *appl);
            }
            let (task, api, _env) = build_task(api, true);
            assert_eq!(
                task.is_cmis_application_update_required(&api, app_new, mask),
                expected,
                "app_new={app_new} mask={mask:#x}"
            );
        }
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_get_sibling_port_configs (+ no-cfg + gearbox variants)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_get_sibling_port_configs() {
        let (mut task, _api, env) = build_task(MockCmisApi::new(), true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::new());
        seed_siblings(
            &env.cfg,
            &[
                ("Ethernet0", 1, 400000, "1,2,3,4", 1),
                ("Ethernet4", 1, 100000, "5", 5),
                ("Ethernet8", 2, 100000, "9", 1), // different pport
            ],
        );
        // rows with missing / invalid fields (all skipped)
        env.cfg.hset("Ethernet12", "speed", "100000").unwrap();
        env.cfg.hset("Ethernet12", "lanes", "1").unwrap(); // missing index
        env.cfg.hset("Ethernet16", "lanes", "1,2,3,4").unwrap();
        env.cfg.hset("Ethernet16", "index", "1").unwrap(); // missing speed
        env.cfg.hset("Ethernet20", "speed", "100000").unwrap();
        env.cfg.hset("Ethernet20", "subport", "1").unwrap();
        env.cfg.hset("Ethernet20", "index", "1").unwrap(); // missing lanes
        env.cfg.hset("Ethernet24", "speed", "foo").unwrap();
        env.cfg.hset("Ethernet24", "lanes", "1").unwrap();
        env.cfg.hset("Ethernet24", "subport", "1").unwrap();
        env.cfg.hset("Ethernet24", "index", "1").unwrap(); // invalid speed
        env.cfg.hset("Ethernet28", "speed", "100000").unwrap();
        env.cfg.hset("Ethernet28", "lanes", "1").unwrap();
        env.cfg.hset("Ethernet28", "subport", "1").unwrap();
        env.cfg.hset("Ethernet28", "index", "bar").unwrap(); // invalid index

        assert_eq!(
            task.get_sibling_port_configs("Ethernet0"),
            vec![
                SiblingPortConfig { lport: "Ethernet0".into(), subport: 1, speed: 400000, host_lane_count: 4 },
                SiblingPortConfig { lport: "Ethernet4".into(), subport: 5, speed: 100000, host_lane_count: 1 },
            ]
        );
    }

    #[test]
    fn test_CmisManagerTask_get_sibling_port_configs_no_cfg_port_tbl() {
        // Rust has no None-table; an empty CONFIG_DB PORT table is the faithful analogue of
        // the Python `cfg_port_tbl is None` short-circuit — both yield no siblings.
        let (mut task, _api, _env) = build_task(MockCmisApi::new(), true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::new());
        assert!(task.get_sibling_port_configs("Ethernet0").is_empty());
    }

    #[test]
    fn test_CmisManagerTask_get_sibling_port_configs_uses_gearbox_lane_count() {
        let (mut task, _api, env) = build_task(MockCmisApi::new(), true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::from([("Ethernet0".to_string(), 2u32)]));
        seed_siblings(&env.cfg, &[("Ethernet0", 1, 200000, "1,2,3,4", 1)]);
        assert_eq!(
            task.get_sibling_port_configs("Ethernet0"),
            vec![SiblingPortConfig { lport: "Ethernet0".into(), subport: 1, speed: 200000, host_lane_count: 2 }]
        );
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_get_desired_app_map (+ mixed / no-match / gearbox variants)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_get_desired_app_map() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(json!({
            "3": {"host_electrical_interface_id":"400GAUI-8 C2M (Annex 120E)","host_lane_count":8,"media_lane_count":8,"host_lane_assignment_options":255}
        }));
        let (mut task, api, env) = build_task(api, true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::new());
        seed_siblings(&env.cfg, &[("Ethernet0", 1, 400000, "1,2,3,4,5,6,7,8", 0)]);
        env.cfg.hset("Ethernet8", "speed", "100000").unwrap();
        env.cfg.hset("Ethernet8", "lanes", "9").unwrap();
        env.cfg.hset("Ethernet8", "subport", "1").unwrap();
        env.cfg.hset("Ethernet8", "index", "2").unwrap(); // different pport
        env.cfg.hset("Ethernet16", "lanes", "1,2,3,4").unwrap();
        env.cfg.hset("Ethernet16", "index", "1").unwrap(); // missing speed
        assert_eq!(task.get_desired_app_map(&api, "Ethernet0"), vec![3, 3, 3, 3, 3, 3, 3, 3]);
    }

    #[test]
    fn test_CmisManagerTask_get_desired_app_map_mixed_mode() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(json!({
            "3": {"host_electrical_interface_id":"400GAUI-4 C2M (Annex 120E)","host_lane_count":4,"media_lane_count":4,"host_lane_assignment_options":255},
            "1": {"host_electrical_interface_id":"100GAUI-1 C2M","host_lane_count":1,"media_lane_count":1,"host_lane_assignment_options":255}
        }));
        let (mut task, api, env) = build_task(api, true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::new());
        seed_siblings(
            &env.cfg,
            &[
                ("Ethernet0", 1, 400000, "1,2,3,4", 1),
                ("Ethernet4", 1, 100000, "5", 5),
                ("Ethernet5", 1, 100000, "6", 6),
                ("Ethernet6", 1, 100000, "7", 7),
                ("Ethernet7", 1, 100000, "8", 8),
            ],
        );
        assert_eq!(task.get_desired_app_map(&api, "Ethernet0"), vec![3, 3, 3, 3, 1, 1, 1, 1]);
    }

    #[test]
    fn test_CmisManagerTask_get_desired_app_map_no_matching_app() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(advert_400g_8lane());
        let (mut task, api, env) = build_task(api, true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::new());
        seed_siblings(&env.cfg, &[("Ethernet0", 1, 999999, "1,2,3,4", 0)]);
        assert_eq!(task.get_desired_app_map(&api, "Ethernet0"), vec![0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_CmisManagerTask_get_desired_app_map_uses_gearbox_lane_count() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(json!({
            "2": {"host_electrical_interface_id":"200GAUI-2 C2M (Annex 120F)","host_lane_count":2,"media_lane_count":2,"host_lane_assignment_options":255}
        }));
        let (mut task, api, env) = build_task(api, true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::from([("Ethernet0".to_string(), 2u32)]));
        seed_siblings(&env.cfg, &[("Ethernet0", 1, 200000, "1,2,3,4", 1)]);
        assert_eq!(task.get_desired_app_map(&api, "Ethernet0"), vec![2, 2, 0, 0, 0, 0, 0, 0]);
    }

    /// NEW: two 4-lane 200G breakout siblings on one physical port must tile the full 8-lane
    /// host map with the SAME app code (table-driven app-select across siblings).
    #[test]
    fn desired_app_map_table_driven() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(json!({
            "3": {"host_electrical_interface_id":"200GAUI-4 C2M (Annex 120F)","host_lane_count":4,"media_lane_count":4,"host_lane_assignment_options":255}
        }));
        let (mut task, api, env) = build_task(api, true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::new());
        seed_siblings(
            &env.cfg,
            &[("Ethernet0", 1, 200000, "1,2,3,4", 1), ("Ethernet4", 1, 200000, "5,6,7,8", 2)],
        );
        assert_eq!(task.get_desired_app_map(&api, "Ethernet0"), vec![3, 3, 3, 3, 3, 3, 3, 3]);
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_is_decommission_required (+ active-not-staged / invalid / missing)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_is_decommission_required() {
        struct Case {
            siblings: Vec<(&'static str, i64, u32, &'static str, i64)>,
            active: [u32; 8],
            expected: bool,
        }
        let cases = vec![
            // lanes 0-3 active app1 but desired app2 for all → decommission
            Case { siblings: vec![("Ethernet0", 1, 400000, "1,2,3,4,5,6,7,8", 0)], active: [1, 1, 1, 1, 2, 2, 2, 2], expected: true },
            // lanes 0-3 active app1, desired 0 (no siblings) → decommission
            Case { siblings: vec![], active: [1, 1, 1, 1, 0, 0, 0, 0], expected: true },
            // all lanes unused, desired has an app on 0-3 → no decommission
            Case { siblings: vec![("Ethernet0", 1, 100000, "1,2,3,4", 1)], active: [0, 0, 0, 0, 0, 0, 0, 0], expected: false },
            // mixed: adding new DPs on unused lanes → no decommission
            Case { siblings: vec![("Ethernet0", 1, 200000, "1,2,3,4", 1), ("Ethernet4", 1, 100000, "5,6,7,8", 2)], active: [3, 3, 3, 3, 0, 0, 0, 0], expected: false },
            // mixed steady state: all lanes match → no decommission
            Case { siblings: vec![("Ethernet0", 1, 200000, "1,2,3,4", 1), ("Ethernet4", 1, 100000, "5,6,7,8", 2)], active: [3, 3, 3, 3, 1, 1, 1, 1], expected: false },
        ];
        for (i, c) in cases.into_iter().enumerate() {
            let api = MockCmisApi::new();
            api.set_application_advertisement(advert_apps_123());
            api.set_active_apsel(apsel_from(&c.active));
            let (mut task, api, env) = build_task(api, true);
            put_port(&mut task, "Ethernet0", 1);
            task.set_gearbox_lanes_dict(HashMap::new());
            seed_siblings(&env.cfg, &c.siblings);
            assert_eq!(task.is_decommission_required(&api, "Ethernet0"), c.expected, "case {i}");
        }
    }

    #[test]
    fn test_CmisManagerTask_is_decommission_required_uses_active_appsel_not_staged() {
        // is_decommission_required compares the ACTIVE apsel (get_active_apsel_hostlane), not
        // the staged get_application: old active [1;8] vs desired [3,3,3,3,1,1,1,1] → decommission.
        let api = MockCmisApi::new();
        api.set_application_advertisement(advert_apps_123());
        api.set_active_apsel(apsel_from(&[1, 1, 1, 1, 1, 1, 1, 1]));
        for (lane, appl) in [(0, 3), (1, 3), (2, 3), (3, 3), (4, 1), (5, 1), (6, 1), (7, 1)] {
            api.set_application_for_lane(lane, appl); // staged map already matches → must be ignored
        }
        let (mut task, api, env) = build_task(api, true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::new());
        seed_siblings(
            &env.cfg,
            &[("Ethernet0", 1, 200000, "1,2,3,4", 1), ("Ethernet4", 1, 100000, "5,6,7,8", 2)],
        );
        assert!(task.is_decommission_required(&api, "Ethernet0"));
    }

    #[test]
    fn test_CmisManagerTask_is_decommission_required_invalid_active_appsel() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(advert_apps_123());
        api.set_active_apsel(json!({
            "ActiveAppSelLane1":"N/A","ActiveAppSelLane2":1,"ActiveAppSelLane3":1,"ActiveAppSelLane4":1,
            "ActiveAppSelLane5":2,"ActiveAppSelLane6":2,"ActiveAppSelLane7":2,"ActiveAppSelLane8":2
        }));
        let (mut task, api, env) = build_task(api, true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::new());
        seed_siblings(&env.cfg, &[("Ethernet0", 1, 400000, "1,2,3,4,5,6,7,8", 0)]);
        assert!(task.is_decommission_required(&api, "Ethernet0"));
    }

    #[test]
    fn test_CmisManagerTask_is_decommission_required_missing_active_appsel_lane() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(advert_apps_123());
        // Missing ActiveAppSelLane8 → fail-safe decommission.
        api.set_active_apsel(json!({
            "ActiveAppSelLane1":1,"ActiveAppSelLane2":1,"ActiveAppSelLane3":1,"ActiveAppSelLane4":1,
            "ActiveAppSelLane5":2,"ActiveAppSelLane6":2,"ActiveAppSelLane7":2
        }));
        let (mut task, api, env) = build_task(api, true);
        put_port(&mut task, "Ethernet0", 1);
        task.set_gearbox_lanes_dict(HashMap::new());
        seed_siblings(&env.cfg, &[("Ethernet0", 1, 400000, "1,2,3,4,5,6,7,8", 0)]);
        assert!(task.is_decommission_required(&api, "Ethernet0"));
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_process_single_lport_invalid_host_lanes_mask
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_process_single_lport_invalid_host_lanes_mask() {
        // Advertisement whose app has NO assignable host lanes (host_lane_assignment_options=0)
        // → get_cmis_host_lanes_mask returns 0 → the port latches FAILED.
        let api = MockCmisApi::new();
        api.set_module_type_abbreviation(Some("QSFP-DD"));
        api.set_module_state("ModuleReady");
        api.set_cmis_rev("5.0");
        api.set_application_advertisement(json!({
            "1": {"host_electrical_interface_id":"400GAUI-8 C2M (Annex 120E)","host_lane_count":8,"media_lane_count":8,"host_lane_assignment_options":0}
        }));
        api.set_datapath_state_value(dp_all("DataPathDeactivated"));
        let (mut task, _api, env) = build_task(api, true);
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "1,2,3,4,5,6,7,8"), ("admin_status", "up"), ("host_tx_ready", "true"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);
        task.process_single_lport("Ethernet0");
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_FAILED));
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_post_port_active_apsel_to_db (+ error cases)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_post_port_active_apsel_to_db() {
        let api = MockCmisApi::new();
        // Static advert carrying BOTH apps — the write keys off the LAST active apsel value.
        api.set_application_advertisement(json!({
            "1": {"media_lane_count":4,"host_lane_count":8},
            "2": {"media_lane_count":1,"host_lane_count":2}
        }));
        api.push_active_apsel_result(Ok(apsel_all(1)));
        api.push_active_apsel_result(Ok(apsel_all(2)));
        let (task, api, env) = build_task(api, true);
        // The reference only writes an EXISTING TRANSCEIVER_INFO row — pre-seed each row with a
        // field the write overwrites so the resulting row is exactly the posted fields.
        for lport in ["Ethernet0", "Ethernet8", "Ethernet16", "Ethernet32"] {
            env.intf.hset(lport, "active_apsel_hostlane1", "seed").unwrap();
        }

        // partial lanes update (mask 0xc → lanes 2,3)
        task.post_port_active_apsel_to_db(&api, "Ethernet0", 0xc, false);
        let e0 = env.intf.row("Ethernet0").unwrap();
        assert_eq!(e0.get("active_apsel_hostlane3").map(String::as_str), Some("1"));
        assert_eq!(e0.get("active_apsel_hostlane4").map(String::as_str), Some("1"));
        for f in ["active_apsel_hostlane1", "active_apsel_hostlane2", "active_apsel_hostlane5", "active_apsel_hostlane6", "active_apsel_hostlane7", "active_apsel_hostlane8"] {
            assert_eq!(e0.get(f).map(String::as_str), Some("N/A"), "{f}");
        }
        assert_eq!(e0.get("host_lane_count").map(String::as_str), Some("8"));
        assert_eq!(e0.get("media_lane_count").map(String::as_str), Some("4"));

        // full lanes update (mask 0xff, apsel = 2)
        task.post_port_active_apsel_to_db(&api, "Ethernet8", 0xff, false);
        let e8 = env.intf.row("Ethernet8").unwrap();
        for n in 1..=8 {
            assert_eq!(e8.get(&format!("active_apsel_hostlane{n}")).map(String::as_str), Some("2"));
        }
        assert_eq!(e8.get("host_lane_count").map(String::as_str), Some("2"));
        assert_eq!(e8.get("media_lane_count").map(String::as_str), Some("1"));

        // reset partial → all 'N/A'
        task.post_port_active_apsel_to_db(&api, "Ethernet16", 0xc, true);
        let e16 = env.intf.row("Ethernet16").unwrap();
        for n in 1..=8 {
            assert_eq!(e16.get(&format!("active_apsel_hostlane{n}")).map(String::as_str), Some("N/A"));
        }
        assert_eq!(e16.get("host_lane_count").map(String::as_str), Some("N/A"));
        assert_eq!(e16.get("media_lane_count").map(String::as_str), Some("N/A"));

        // reset full → all 'N/A'
        task.post_port_active_apsel_to_db(&api, "Ethernet32", 0xff, true);
        let e32 = env.intf.row("Ethernet32").unwrap();
        for n in 1..=8 {
            assert_eq!(e32.get(&format!("active_apsel_hostlane{n}")).map(String::as_str), Some("N/A"));
        }
        assert_eq!(e32.get("host_lane_count").map(String::as_str), Some("N/A"));
    }

    #[test]
    fn test_CmisManagerTask_post_port_active_apsel_to_db_error_cases() {
        // No TRANSCEIVER_INFO row exists → nothing is written (both the Python "table is None"
        // and "lport not in table" cases collapse to this in the seam).
        let api = MockCmisApi::new();
        api.set_application_advertisement(advert_400g_8lane());
        api.set_active_apsel(apsel_all(1));
        let (task, api, env) = build_task(api, true);
        task.post_port_active_apsel_to_db(&api, "Ethernet0", 0xff, false);
        assert_eq!(env.intf.set_count(), 0);
        assert!(!env.intf.contains("Ethernet0"));
    }

    // ---------------------------------------------------------------------------------
    // gearbox integration (end-to-end + caching)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_gearbox_integration_end_to_end() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(json!({
            "1": {"host_electrical_interface_id":"100GAUI-2 C2M (Annex 135G)","host_lane_count":2,"media_lane_count":1,"host_lane_assignment_options":85},
            "2": {"host_electrical_interface_id":"CAUI-4 C2M (Annex 83E)","host_lane_count":4,"media_lane_count":4,"host_lane_assignment_options":17}
        }));
        let (mut task, api, _env) = build_task(api, true);
        task.set_gearbox_lanes_dict(HashMap::from([("Ethernet0".to_string(), 2u32)]));
        // gearbox line-lanes (2) win over the port-config lanes (4)
        assert_eq!(task.get_host_lane_count("Ethernet0", "25,26,27,28"), 2);
        // and drive app-select to the 2-lane app (1), not the 4-lane app (2)
        assert_eq!(get_cmis_application_desired(&api, 2, 100000), Some(1));
    }

    #[test]
    fn test_CmisManagerTask_gearbox_caching_integration() {
        let (mut task, _api, _env) = build_task(MockCmisApi::new(), true);
        task.set_gearbox_lanes_dict(HashMap::from([
            ("Ethernet0".to_string(), 2u32),
            ("Ethernet4".to_string(), 4u32),
        ]));
        assert_eq!(task.get_host_lane_count("Ethernet0", "25,26,27,28"), 2);
        assert_eq!(task.get_host_lane_count("Ethernet4", "29,30"), 4);
        assert_eq!(task.get_host_lane_count("Ethernet8", "33,34,35"), 3);
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_task_worker_host_tx_ready_false_to_true (adapted: scripted DP state)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_task_worker_host_tx_ready_false_to_true() {
        let api = MockCmisApi::new();
        api.set_module_type_abbreviation(Some("QSFP-DD"));
        api.set_module_state("ModuleReady");
        api.set_cmis_rev("5.0");
        api.set_application_advertisement(advert_400g_8lane());
        api.set_datapath_state_value(dp_all("DataPathDeactivated"));
        api.set_config_status(cfg_all("ConfigSuccess"));
        api.set_dpinit_pending(dpinit_all(true));
        api.set_active_apsel(apsel_all(0));
        api.set_application_by_lane(0);
        api.set_durations_ms(60000.0, 600000.0, 5000.0, 500.0, 70000.0, 70000.0);
        let (mut task, api, env) = build_task(api, true);
        env.state.hset("Ethernet0", "host_tx_ready", "false").unwrap();
        env.intf.hset("Ethernet0", "present", "1").unwrap();
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "1,2,3,4,5,6,7,8"), ("admin_status", "up"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_INSERTED));

        // --- host_tx_ready=false: INSERTED short-circuits to a forced-Tx-disabled READY -----
        task.process_single_lport("Ethernet0");
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_READY));
        assert!(task.port_dict["Ethernet0"].forced_tx_disabled);
        assert_eq!(api.call_count("set_datapath_deinit"), 1);
        assert_eq!(api.call_count("tx_disable_channel"), 1);

        // --- host asserts host_tx_ready=true → reinit restarts the machine ------------------
        task.port_dict.get_mut("Ethernet0").unwrap().host_tx_ready = Some("true".to_string());
        task.force_cmis_reinit("Ethernet0", 0);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_INSERTED));
        task.process_single_lport("Ethernet0"); // INSERTED → DP_PRE_INIT_CHECK
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_DP_PRE_INIT_CHECK));

        // --- failure scenario: datapath still Activated on first attempt → retry, stays PRE_INIT
        api.set_datapath_state_value(dp_all("DataPathActivated"));
        task.port_dict.get_mut("Ethernet0").unwrap().cmis_expired = Some(Instant::now() - Duration::from_secs(1));
        task.process_single_lport("Ethernet0"); // PRE_INIT (forced, not deactivated) → reinit(1) → INSERTED
        task.process_single_lport("Ethernet0"); // INSERTED → DP_PRE_INIT_CHECK
        assert_eq!(task.port_dict["Ethernet0"].cmis_retries, 1);
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_DP_PRE_INIT_CHECK));

        // --- second attempt: datapath deactivated → clears forced, advances to DP_DEINIT -----
        api.set_datapath_state_value(dp_all("DataPathDeactivated"));
        task.process_single_lport("Ethernet0");
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_DP_DEINIT));
        assert!(!task.port_dict["Ethernet0"].forced_tx_disabled);
        assert_eq!(task.port_dict["Ethernet0"].cmis_retries, 1);
    }

    /// NEW: the per-port host_tx_ready reconcile (get_host_tx_status) gates bring-up — a
    /// STATE_DB 'false' read forces the Tx-disabled READY terminal; re-reading 'true' after a
    /// reinit lets the machine advance past INSERTED.
    #[test]
    fn host_tx_ready_false_to_true_triggers_reinit() {
        let api = MockCmisApi::new();
        api.set_module_type_abbreviation(Some("QSFP-DD"));
        api.set_module_state("ModuleReady");
        api.set_cmis_rev("5.0");
        api.set_application_advertisement(advert_400g_8lane());
        api.set_datapath_state_value(dp_all("DataPathDeactivated"));
        api.set_config_status(cfg_all("ConfigSuccess"));
        api.set_dpinit_pending(dpinit_all(true));
        api.set_active_apsel(apsel_all(0));
        api.set_application_by_lane(0);
        api.set_durations_ms(10.0, 10.0, 10.0, 10.0, 10.0, 10.0);
        let (mut task, api, env) = build_task(api, true);
        env.intf.hset("Ethernet0", "present", "1").unwrap();
        env.state.hset("Ethernet0", "host_tx_ready", "false").unwrap();
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "1,2,3,4,5,6,7,8"), ("admin_status", "up"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);

        // host_tx_ready reconciles to 'false' → forced-Tx-disabled READY.
        task.process_single_lport("Ethernet0");
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_READY));
        assert!(task.port_dict["Ethernet0"].forced_tx_disabled);
        assert_eq!(task.port_dict["Ethernet0"].host_tx_ready.as_deref(), Some("false"));

        // Host asserts host_tx_ready → a fresh reconcile (host_tx_ready cleared) + reinit advances.
        env.state.hset("Ethernet0", "host_tx_ready", "true").unwrap();
        task.port_dict.get_mut("Ethernet0").unwrap().host_tx_ready = None;
        task.force_cmis_reinit("Ethernet0", 0);
        task.process_single_lport("Ethernet0");
        assert_eq!(task.port_dict["Ethernet0"].host_tx_ready.as_deref(), Some("true"));
        assert_eq!(env.status_sw.field("Ethernet0", "cmis_state").as_deref(), Some(CMIS_STATE_DP_PRE_INIT_CHECK));
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_task_worker_decommission (adapted: drive is_decommission_required
    // via the active-apsel result queue since Rust can't monkeypatch the method)
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_task_worker_decommission() {
        let api = MockCmisApi::new();
        api.set_module_type_abbreviation(Some("QSFP-DD"));
        api.set_module_state("ModuleReady");
        api.set_cmis_rev("5.0");
        // 2-lane 100G app (matches the reconfigured port speed/lanes).
        api.set_application_advertisement(json!({
            "1": {"host_electrical_interface_id":"100GAUI-2 C2M (Annex 135G)","host_lane_count":2,"media_lane_count":1,"host_lane_assignment_options":255,"media_lane_assignment_options":255}
        }));
        api.set_datapath_state_value(dp_all("DataPathDeactivated"));
        api.set_config_status(cfg_all("ConfigRejected"));
        api.set_dpinit_pending(dpinit_all(true));
        api.set_application_by_lane(1);
        api.set_durations_ms(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        // First decommission check sees the OLD active app (1) → decommission required; every
        // subsequent check sees the decommissioned lanes (app 0) → not required (mirrors the
        // Python `is_decommission_required=[True] + [False]*20` side_effect).
        api.set_active_apsel(apsel_all(0));
        api.push_active_apsel_result(Ok(apsel_all(1)));

        let (mut task, _api, env) = build_task(api, true);
        env.state.hset("Ethernet0", "host_tx_ready", "true").unwrap();
        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "100000"), ("lanes", "1,2"), ("subport", "1"), ("admin_status", "up")],
        );
        task.on_port_update_event(&ev);

        // Sweep until the retries are exhausted (ConfigRejected never converges) → FAILED.
        // Force the CMIS timer expired each pass (Python mocks is_timer_expired=True).
        for _ in 0..80 {
            if let Some(p) = task.port_dict.get_mut("Ethernet0") {
                p.cmis_expired = Some(Instant::now() - Duration::from_secs(1));
            }
            task.process_single_lport("Ethernet0");
            if env.status_sw.field("Ethernet0", "cmis_state").as_deref() == Some(CMIS_STATE_FAILED) {
                break;
            }
        }

        assert!(!task.is_decomm_pending("Ethernet0"));
        assert!(!task.is_decomm_lead_lport("Ethernet0"));
        assert!(!task.is_decomm_failed("Ethernet0"));
        let st = env.status_sw.field("Ethernet0", "cmis_state").unwrap();
        assert!(st == CMIS_STATE_AP_CONF || st == CMIS_STATE_FAILED, "unexpected end state {st}");
    }

    // ---- ported #[ignore] stubs for unported behaviour (coherent/gearbox/tx-power/freq) ----

    #[test]
    fn test_CmisManagerTask_is_fast_reboot_enabled_for_lport() {
        // Ethernet0 → asic 1 (multi-ASIC), fast reboot enabled for that namespace.
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 0, 1, PortEventType::PortAdd));
        let (mut task, _api, _env) = build_task_with_mapping(MockCmisApi::new(), true, pm);
        task.set_namespaces(vec!["asic1".to_string()], true);
        task.initialize_fast_reboot_status(&MockRebootDb::with_fast_reboot(true));

        assert!(task.is_fast_reboot_enabled_for_lport("Ethernet0"));
        // The port's asic_id was seeded from the port map at construction.
        assert_eq!(task.get_asic_id("Ethernet0"), 1);
    }

    #[test]
    fn test_CmisManagerTask_is_fast_reboot_enabled_for_lport_default_namespace() {
        // No ports mapped: an unknown lport resolves asic_id -1 → default namespace "".
        let (mut task, _api, _env) = build_task(MockCmisApi::new(), true);
        task.set_namespaces(vec![String::new()], false);
        task.initialize_fast_reboot_status(&MockRebootDb::with_fast_reboot(false));

        assert!(!task.is_fast_reboot_enabled_for_lport("Ethernet999"));
        assert_eq!(task.get_asic_id("Ethernet999"), -1);
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_get_configured_freq — the user's laser_freq from CONFIG_DB PORT.
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_get_configured_freq() {
        let (task, _api, env) = build_task(MockCmisApi::new(), true);
        env.cfg.hset("Ethernet0", "laser_freq", "193100").unwrap();
        assert_eq!(task.get_configured_laser_freq_from_db("Ethernet0"), 193100);
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_get_configured_tx_power_from_db — the user's tx_power (dBm).
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_get_configured_tx_power_from_db() {
        let (task, _api, env) = build_task(MockCmisApi::new(), true);
        env.cfg.hset("Ethernet0", "tx_power", "-10").unwrap();
        assert_eq!(task.get_configured_tx_power_from_db("Ethernet0"), -10.0);
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_validate_frequency_and_grid — bounds + 75/100 GHz alignment
    // against a 75GHz-only module (supported_grid=0x80, range 191300..196100 GHz).
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_validate_frequency_and_grid() {
        let api = MockCmisApi::new();
        api.set_supported_freq_config(0x80, 0, 0, 191300, 196100);
        let (task, api, _env) = build_task(api, true);
        // (freq, grid, expected) — mirrors the Python parametrization.
        let cases: &[(i64, u32, bool)] = &[
            (193100, 75, true),
            (193100, 100, false),
            (193125, 75, false),
            (193100, 25, false),
            (191295, 75, false),
            (196105, 75, false),
        ];
        for &(freq, grid, expected) in cases {
            assert_eq!(
                task.validate_frequency_and_grid(&api, "Ethernet0", freq, grid),
                expected,
                "freq={freq} grid={grid}"
            );
        }
    }

    // ---------------------------------------------------------------------------------
    // test_CmisManagerTask_process_single_lport_tx_power_config_failure — a coherent module
    // at DP_PRE_INIT_CHECK whose Tx-power provisioning FAILS still advances the datapath
    // machine (the error is logged, not fatal): the port must reach DP_DEINIT.
    // ---------------------------------------------------------------------------------
    #[test]
    fn test_CmisManagerTask_process_single_lport_tx_power_config_failure() {
        let api = MockCmisApi::new();
        api.set_module_type_abbreviation(Some("QSFP-DD"));
        api.set_cmis_rev("5.0");
        api.set_coherent_module(true);
        api.set_module_state("ModuleReady");
        api.set_datapath_state_value(dp_all("DataPathDeactivated"));
        // Configured tx power (-10) differs from the module's current (-5) → a write is issued.
        api.set_supported_power_config(-20.0, 0.0);
        api.set_tx_config_power(-5.0);
        api.set_set_tx_power_result(false); // set_tx_power fails
        api.set_laser_config_freq(193100); // matches configured freq → no laser re-tune
        api.set_application_by_lane(0); // active app 0 vs appl 1 → update required
        let (mut task, api, env) = build_task(api, true);

        let ev = set_event(
            "Ethernet0",
            0,
            &[("speed", "400000"), ("lanes", "1,2,3,4,5,6,7,8"), ("subport", "0")],
        );
        task.on_port_update_event(&ev);
        task.update_port_transceiver_status_table_sw_cmis_state(
            "Ethernet0",
            CMIS_STATE_DP_PRE_INIT_CHECK,
        );
        {
            let p = task.port_dict.get_mut("Ethernet0").unwrap();
            p.host_tx_ready = Some("true".to_string());
            p.admin_status = Some("up".to_string());
            p.appl = Some(1);
            p.host_lanes_mask = 0xff;
            p.media_lanes_mask = 0xff;
            p.tx_power = Some(-10.0);
            p.laser_freq = Some(193100);
        }

        task.process_single_lport("Ethernet0");

        // Tx-power write was attempted and, despite failing, the machine advanced to DP_DEINIT.
        assert!(api.call_count("set_tx_power") >= 1);
        assert_eq!(
            env.status_sw.field("Ethernet0", "cmis_state").as_deref(),
            Some(CMIS_STATE_DP_DEINIT)
        );
    }

    // ---------------------------------------------------------------------------------
    // NEW: laser_freq_grid_validation_cases — a dual-grid module (75GHz+100GHz supported,
    // supported_grid=0xA0) accepts a 100GHz-grid frequency and on-grid 75GHz boundary
    // frequencies, but rejects an off-75GHz-grid frequency. Complements the 75GHz-only
    // translated case above by exercising the 100GHz acceptance branch + range boundaries.
    // ---------------------------------------------------------------------------------
    #[test]
    fn laser_freq_grid_validation_cases() {
        let api = MockCmisApi::new();
        // 0xA0 = 1010_0000 → bit7 (75GHz) and bit5 (100GHz) both set.
        api.set_supported_freq_config(0xA0, 0, 0, 191300, 196100);
        let (task, api, _env) = build_task(api, true);
        let cases: &[(i64, u32, bool)] = &[
            (193100, 100, true),  // 100GHz grid supported, no channel-alignment check
            (193100, 75, true),   // channel 0 → aligned
            (191300, 75, true),   // low boundary, channel -72 (÷3) → aligned
            (196100, 75, true),   // high boundary, channel 120 (÷3) → aligned
            (193150, 75, false),  // channel 2 → NOT on 75GHz grid
            (191275, 75, false),  // below the supported low freq
            (196125, 75, false),  // above the supported high freq
        ];
        for &(freq, grid, expected) in cases {
            assert_eq!(
                task.validate_frequency_and_grid(&api, "Ethernet0", freq, grid),
                expected,
                "freq={freq} grid={grid}"
            );
        }
    }
}
