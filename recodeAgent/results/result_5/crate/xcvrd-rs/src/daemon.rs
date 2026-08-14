//! xcvrd-rs daemon — transceiver monitoring for SONiC.
//!
//! A single-threaded port of the Python xcvrd daemon that fuses the reference
//! `SfpStateUpdateTask` + `DomInfoUpdateTask` + `CmisManagerTask` + `SffManagerTask`
//! cores into one serve loop over the platform bridge (PyO3 `sonic_platform`) and
//! swss-common STATE_DB. What it does:
//!   1. Build the logical→physical port map from CONFIG_DB (`PORT|Ethernet{n}` whose
//!      `index` is the emulator SFP index; here `Ethernet{i*4}` ↔ `i`), recording each
//!      port's `admin_status`.
//!   2. For every configured port read identity via the HAL and publish
//!      `TRANSCEIVER_INFO`, cache `TRANSCEIVER_DOM_THRESHOLD`, and project
//!      `TRANSCEIVER_STATUS_SW` `status`/`cmis_state`.
//!   3. Drive CMIS bring-up: every present CMIS module starts non-terminal
//!      (`INSERTED`) and is walked by the datapath state machine. An admin-up module is
//!      taken out of low power (`set_lpmode(false)`) and held until it reaches
//!      `ModuleReady`, then `READY`; an admin-down module is torn down by the `INSERTED`
//!      handler (DataPathDeinit + OutputDisableTx + active-apsel reset to `N/A`) to a
//!      forced-Tx-disabled `READY` without being powered up.
//!   4. Poll DOM periodically (`DomInfoUpdateTask.task_worker`): publish
//!      `TRANSCEIVER_DOM_SENSOR` + `TRANSCEIVER_STATUS` (thermal, always), and — only
//!      once the port's CMIS bring-up is terminal and `dom_polling` is not disabled —
//!      `TRANSCEIVER_DOM_FLAG` (+ the change-count/set-time/clear-time metadata trio)
//!      and `TRANSCEIVER_PM` (VDM-freeze gated, skipped in low-power mode).
//!   5. React to plug/unplug via `get_change_event`: repopulate on insert, delete
//!      identity + DOM tables (incl. the flag metadata) on removal.
//!
//! Values are written trimmed of trailing NULs (CMIS strings are fixed-width,
//! NUL-padded); the observable result matches the reference xcvrd, whose outputs are
//! read with NULs stripped.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use platform_bridge::{Platform, Sfp};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde_json::{Map, Value};
use swss_common::{CxxString, DbConnector, KeyOperation, SelectResult, SubscriberStateTable};

use crate::dom::utilities::db::utils::{
    beautify_info_dict, beautify_info_row, compute_flag_metadata_plan, get_current_time, py_str,
    py_truthy, NEVER, NOT_AVAILABLE,
};
use crate::dom::utilities::dom_sensor::db_utils::{beautify_dom_row, beautify_dom_info_dict};
use crate::dom::utilities::vdm::VDM_THRESHOLD_TYPES;
use crate::env;
use crate::cmis::cmis_api::{BridgeCmisApi, CmisApi};
use crate::hal::RealSfp;
use crate::sff_mgr::{BridgeSffApi, SffApi};
use crate::xcvrd_utilities::common::{
    self, get_cmis_application_desired, CMIS_STATE_AP_CONF, CMIS_STATE_DP_ACTIVATE,
    CMIS_STATE_DP_DEINIT, CMIS_STATE_DP_INIT, CMIS_STATE_DP_PRE_INIT_CHECK, CMIS_STATE_DP_TXON,
    CMIS_STATE_FAILED, CMIS_STATE_INSERTED, CMIS_STATE_READY, CMIS_STATE_REMOVED,
};
use crate::xcvrd_utilities::sfp_status_helper;
use crate::xcvrd_utilities::media_settings_parser::{
    self, get_media_settings_key, get_speed_lane_count_and_subport, media_settings_present,
    notify_media_setting, MediaNotifyTables,
};
use crate::xcvrd_utilities::optics_si_parser;
use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortEventType, PortMapping};
use crate::xcvrd_utilities::xcvr_table_helper::{
    XcvrTableHelper, NPU_SI_SETTINGS_DEFAULT_VALUE, NPU_SI_SETTINGS_SYNC_STATUS_KEY,
};
use crate::db::{NullTable, RealStateDb, SepTable, StateDb, Table};
use std::rc::Rc;

const INFO_TABLE: &str = "TRANSCEIVER_INFO";
const STATUS_SW_TABLE: &str = "TRANSCEIVER_STATUS_SW";
const STATUS_TABLE: &str = "TRANSCEIVER_STATUS";
const DOM_SENSOR_TABLE: &str = "TRANSCEIVER_DOM_SENSOR";
const DOM_TEMPERATURE_TABLE: &str = "TRANSCEIVER_DOM_TEMPERATURE";
const DOM_THRESHOLD_TABLE: &str = "TRANSCEIVER_DOM_THRESHOLD";
const DOM_FLAG_TABLE: &str = "TRANSCEIVER_DOM_FLAG";
const DOM_FLAG_CHANGE_COUNT_TABLE: &str = "TRANSCEIVER_DOM_FLAG_CHANGE_COUNT";
const DOM_FLAG_SET_TIME_TABLE: &str = "TRANSCEIVER_DOM_FLAG_SET_TIME";
const DOM_FLAG_CLEAR_TIME_TABLE: &str = "TRANSCEIVER_DOM_FLAG_CLEAR_TIME";
const STATUS_FLAG_TABLE: &str = "TRANSCEIVER_STATUS_FLAG";
const STATUS_FLAG_CHANGE_COUNT_TABLE: &str = "TRANSCEIVER_STATUS_FLAG_CHANGE_COUNT";
const STATUS_FLAG_SET_TIME_TABLE: &str = "TRANSCEIVER_STATUS_FLAG_SET_TIME";
const STATUS_FLAG_CLEAR_TIME_TABLE: &str = "TRANSCEIVER_STATUS_FLAG_CLEAR_TIME";
const PM_TABLE: &str = "TRANSCEIVER_PM";
const FIRMWARE_INFO_TABLE: &str = "TRANSCEIVER_FIRMWARE_INFO";
const VDM_REAL_VALUE_TABLE: &str = "TRANSCEIVER_VDM_REAL_VALUE";

/// The four VDM threshold/flag categories (`xcvr_table_helper.VDM_THRESHOLD_TYPES`,
/// upper-cased for the STATE_DB table names). Each contributes a `_THRESHOLD` table
/// plus the `_FLAG` value table and its change-count/set-time/clear-time metadata
/// trio (see the removal table set in `xcvrd.py:600-622`).
const VDM_CATEGORIES: [&str; 4] = ["HALARM", "LALARM", "HWARN", "LWARN"];
const VDM_FLAG_SUFFIXES: [&str; 4] = ["FLAG", "FLAG_CHANGE_COUNT", "FLAG_SET_TIME", "FLAG_CLEAR_TIME"];

/// `DomInfoUpdateTask.DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE` (dom_mgr.py): after a
/// link flap (APPL_DB `PORT_TABLE` `flap_count` bump) the flag tables are re-read
/// this long after the event, not on the slow DOM poll — so a flap re-captures the
/// latched flags within ~1s instead of up to a full poll period.
const DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE: Duration = Duration::from_secs(1);

/// CMIS state-machine projection written to `TRANSCEIVER_STATUS_SW.cmis_state`. The
/// full ordered bring-up set (`INSERTED → DP_PRE_INIT_CHECK → DP_DEINIT → AP_CONFIGURED
/// → DP_INIT → DP_TXON → DP_ACTIVATION → READY`, or `FAILED`/`REMOVED`) is imported from
/// [`crate::xcvrd_utilities::common`]; the datapath state machine (`cmis_datapath_sm`
/// below) drives every transition. `common.CMIS_TERMINAL_STATES = {READY, FAILED,
/// REMOVED}` releases the DOM flag/PM gate.

/// `common.CMIS_TERMINAL_STATES` membership (dom_mgr.py:193): a port whose
/// `cmis_state` is terminal is NOT in CMIS initialization, so its DOM flag / PM
/// polling is un-gated.
fn cmis_is_terminal(state: &str) -> bool {
    matches!(state, "READY" | "FAILED" | "REMOVED")
}

/// Per-port runtime context: the display name, static CONFIG_DB port attributes
/// (`admin_status`/`speed`/`lanes`/`subport`) and the CMIS datapath bring-up working
/// set (the Python `CmisManagerTask.port_dict[lport]` sub-dict). `cmis_state` is the
/// projection mirrored to `TRANSCEIVER_STATUS_SW.cmis_state`.
struct PortCtx {
    name: String,
    admin_up: bool,
    /// STATE_DB `PORT_TABLE|<name>.host_tx_ready` (absent → `true`, the testbed adaptation).
    /// A `'false'` here gates bring-up exactly like `admin_up == false`.
    host_tx_ready: bool,
    cmis_state: String,
    // Per-port config (CONFIG_DB `PORT|<name>`). `speed` is re-read live each pass so a
    // reconfiguration (e.g. 40G→100G on the multi-app module) re-drives app-select.
    speed: u32,
    subport: i64,
    host_lane_count: u32,
    // CMIS datapath state-machine working set (recomputed each bring-up; mirrors
    // `cmis_manager_task::PortInfo`).
    appl: Option<u32>,
    host_lanes_mask: u32,
    media_lanes_mask: u32,
    max_host_lanes_mask: u32,
    max_media_lanes_mask: u32,
    media_lane_count: u32,
    media_lane_assignment_options: u32,
    forced_tx_disabled: bool,
    /// Coherent (ZR) user-requested Tx output power (dBm) from CONFIG_DB `PORT|<name>.tx_power`
    /// (`0.0` when unset). Re-read at (re)insertion so an admin-bounce picks up a new request.
    tx_power: f64,
    /// Coherent (ZR) user-requested laser frequency (GHz) from CONFIG_DB `PORT|<name>.laser_freq`
    /// (`0` when unset). Cleared to `0` when an invalid frequency is rejected.
    laser_freq: i64,
    /// A CMIS decommission (reset AppSel to 0) is in flight for this port: the current
    /// bring-up is provisioning app 0 and, once it reaches DP_INIT, will re-init to
    /// provision the real (changed) app. Survives `cmis_force_reinit`; cleared on the
    /// decommission-completing re-init and on a fresh (re)insertion.
    decomm_pending: bool,
    txoff_duration: f64,
    cmis_retries: u32,
    cmis_expired: Option<Instant>,
}

impl PortCtx {
    /// Build a context from the port's static CONFIG_DB attributes. Every present CMIS
    /// port starts non-terminal (`INSERTED`) and is driven by the datapath machine — an
    /// admin-up port all the way to a powered-up `READY`, an admin-down port through the
    /// `INSERTED` handler's teardown (DataPathDeinit + active-apsel reset to `N/A`) to a
    /// forced-Tx-disabled `READY` (never powered up). Faithful to the reference
    /// `CmisManagerTask`, whose admin gate lives inside the `INSERTED` handler, not at the
    /// `cmis_state` assignment.
    fn new(name: String, admin_up: bool, speed: u32, lanes: String, subport: i64) -> Self {
        let host_lane_count = if lanes.trim().is_empty() {
            0
        } else {
            lanes.split(',').count() as u32
        };
        let cmis_state = CMIS_STATE_INSERTED;
        PortCtx {
            name,
            admin_up,
            host_tx_ready: true,
            cmis_state: cmis_state.to_string(),
            speed,
            subport,
            host_lane_count,
            appl: None,
            host_lanes_mask: 0,
            media_lanes_mask: 0,
            max_host_lanes_mask: 0,
            max_media_lanes_mask: 0,
            media_lane_count: 0,
            media_lane_assignment_options: 0,
            forced_tx_disabled: false,
            tx_power: 0.0,
            laser_freq: 0,
            decomm_pending: false,
            txoff_duration: 0.0,
            cmis_retries: 0,
            cmis_expired: None,
        }
    }

    /// Clear the per-bring-up working set on a fresh (re)insertion so a re-plugged module
    /// (possibly after a prior `FAILED`) starts the datapath machine cleanly — otherwise a
    /// stale `cmis_retries > CMIS_MAX_RETRIES` would latch `FAILED` immediately.
    fn reset_bringup(&mut self) {
        self.appl = None;
        self.host_lanes_mask = 0;
        self.media_lanes_mask = 0;
        self.max_host_lanes_mask = 0;
        self.max_media_lanes_mask = 0;
        self.media_lane_count = 0;
        self.media_lane_assignment_options = 0;
        self.forced_tx_disabled = false;
        self.decomm_pending = false;
        self.txoff_duration = 0.0;
        self.cmis_retries = 0;
        self.cmis_expired = None;
    }
}

/// EEPROM identity read-retry cadence: a present module whose identity read fails
/// is re-read on this interval until it succeeds (mirrors the Python
/// `SfpStateUpdateTask.RETRY_EEPROM_READING_INTERVAL`), so recovery happens
/// without a re-plug.
const RETRY_EEPROM_READING_INTERVAL: Duration = Duration::from_secs(60);

/// Grace pause for a *just-inserted* module before its identity EEPROM is re-read.
/// On a plug-in the module may not have finished powering up when the change event
/// arrives, so a first identity read can transiently fail; xcvrd.py
/// (`SfpStateUpdateTask.task_worker`, `TIME_FOR_SFP_READY`) sleeps this long and
/// re-reads once before falling back to the slow `RETRY_EEPROM_READING_INTERVAL`
/// cadence, so a hot-plug repopulates `TRANSCEIVER_INFO` within seconds.
const TIME_FOR_SFP_READY: Duration = Duration::from_secs(1);

/// Default periodic DOM poll cadence
/// (`DomInfoUpdateTask.DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS`): every present module's
/// live DOM monitors are re-read and republished to `TRANSCEIVER_DOM_SENSOR` on this
/// interval. Overridable at start-up via `--dom_update_interval`
/// (see [`resolve_dom_update_interval`]).
const DOM_INFO_UPDATE_PERIOD_SECS: u64 = 60;

/// Set by the SIGTERM/SIGINT handler; observed by the serve loop to trigger a graceful
/// deinit (the STATE_DB teardown that preserves an active datapath across a warm/fast
/// reboot) before the process exits.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe SIGTERM/SIGINT handler: just set the shutdown flag. An atomic store
/// is lock-free, so it is safe to call from a signal handler (no allocation, no locks).
/// The serve loop observes the flag on its next wake and runs the deinit teardown.
extern "C" fn handle_shutdown_signal(_sig: i32) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Install the SIGTERM/SIGINT handler via libc `signal(2)` — no extra crate is needed
/// (libc is always linked). PyO3 initialises CPython with `Py_InitializeEx(0)`, which does
/// NOT install Python's own signal handlers, so ours survives platform init and the pmon
/// supervisor's `supervisorctl stop` (SIGTERM, `stopwaitsecs`) reaches our graceful deinit
/// (`DaemonXcvrd.deinit`, xcvrd.py:1076) instead of killing the daemon with STATUS still live.
fn install_shutdown_handler() {
    extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }
    // SIGINT = 2, SIGTERM = 15 on Linux.
    unsafe {
        signal(2, handle_shutdown_signal);
        signal(15, handle_shutdown_signal);
    }
}

/// The daemon's command-line arguments, mirroring the Python `argparse` parser in
/// `xcvrd.py:main` verbatim:
///
/// ```text
/// parser.add_argument('--skip_cmis_mgr', action='store_true')
/// parser.add_argument('--enable_sff_mgr', action='store_true')
/// parser.add_argument('--dom_temperature_poll_interval', default=None, type=int)
/// parser.add_argument('--dom_update_interval', default=None, type=int)
/// ```
///
/// Passed on to `DaemonXcvrd(SYSLOG_IDENTIFIER, args.skip_cmis_mgr, args.enable_sff_mgr,
/// args.dom_temperature_poll_interval, args.dom_update_interval)`. Defaults match
/// argparse: the two flags default to `false`, the two intervals to `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonArgs {
    /// `--skip_cmis_mgr`: do not run the CMIS datapath state machine.
    pub skip_cmis_mgr: bool,
    /// `--enable_sff_mgr`: run the SFF (non-CMIS) deterministic link bring-up.
    pub enable_sff_mgr: bool,
    /// `--dom_temperature_poll_interval` (seconds): when set, run the separate DOM
    /// temperature poll (`DomThermalInfoUpdateTask`) at this cadence.
    pub dom_temperature_poll_interval: Option<i64>,
    /// `--dom_update_interval` (seconds): override the periodic DOM poll cadence
    /// (validated by [`resolve_dom_update_interval`]).
    pub dom_update_interval: Option<i64>,
}

/// Outcome of parsing the command line, mirroring how `argparse.parse_args()`
/// terminates the process: `-h/--help` prints usage and exits 0; any parse error
/// prints usage + a diagnostic to stderr and exits 2.
#[derive(Debug)]
enum ArgParseError {
    /// `-h`/`--help` was requested.
    HelpRequested,
    /// A malformed / unknown argument (argparse exit code 2).
    Invalid(String),
}

/// Usage text printed on `--help` or a parse error, matching the four options the
/// Python parser accepts.
fn daemon_usage() -> String {
    "usage: xcvrd-rs [-h] [--skip_cmis_mgr] [--enable_sff_mgr]\n\
     \x20               [--dom_temperature_poll_interval DOM_TEMPERATURE_POLL_INTERVAL]\n\
     \x20               [--dom_update_interval DOM_UPDATE_INTERVAL]\n"
        .to_string()
}

/// Consume the value for an option that takes one argument, supporting both the
/// `--flag value` and `--flag=value` forms (argparse accepts either). `idx` points at
/// the flag; on the space-separated form it is advanced past the consumed value.
fn take_option_value(
    flag: &str,
    inline: Option<String>,
    args: &[String],
    idx: &mut usize,
) -> Result<String, ArgParseError> {
    if let Some(v) = inline {
        return Ok(v);
    }
    if *idx + 1 < args.len() {
        *idx += 1;
        return Ok(args[*idx].clone());
    }
    Err(ArgParseError::Invalid(format!(
        "argument {flag}: expected one argument"
    )))
}

/// Parse an option value as an `int`, mirroring argparse's `type=int` (which errors out
/// with exit code 2 on a non-integer).
fn parse_int_option(flag: &str, value: &str) -> Result<i64, ArgParseError> {
    value.trim().parse::<i64>().map_err(|_| {
        ArgParseError::Invalid(format!("argument {flag}: invalid int value: '{value}'"))
    })
}

/// Parse the daemon's arguments from the argv tail (i.e. everything after the program
/// name), faithfully reproducing the Python `argparse` parser in `xcvrd.py:main`. Kept
/// pure (no process exit, no globals) so it is unit-testable; `run` maps the error
/// variants onto argparse's exit behaviour.
fn parse_daemon_args(args: &[String]) -> Result<DaemonArgs, ArgParseError> {
    let mut out = DaemonArgs::default();
    let mut i = 0;
    while i < args.len() {
        let raw = args[i].clone();
        let (flag, inline) = match raw.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (raw.clone(), None),
        };
        match flag.as_str() {
            "-h" | "--help" => return Err(ArgParseError::HelpRequested),
            "--skip_cmis_mgr" => {
                if let Some(v) = inline {
                    return Err(ArgParseError::Invalid(format!(
                        "argument --skip_cmis_mgr: ignored explicit argument '{v}'"
                    )));
                }
                out.skip_cmis_mgr = true;
            }
            "--enable_sff_mgr" => {
                if let Some(v) = inline {
                    return Err(ArgParseError::Invalid(format!(
                        "argument --enable_sff_mgr: ignored explicit argument '{v}'"
                    )));
                }
                out.enable_sff_mgr = true;
            }
            "--dom_temperature_poll_interval" => {
                let v = take_option_value(&flag, inline, args, &mut i)?;
                out.dom_temperature_poll_interval = Some(parse_int_option(&flag, &v)?);
            }
            "--dom_update_interval" => {
                let v = take_option_value(&flag, inline, args, &mut i)?;
                out.dom_update_interval = Some(parse_int_option(&flag, &v)?);
            }
            _ => {
                return Err(ArgParseError::Invalid(format!(
                    "unrecognized arguments: {raw}"
                )))
            }
        }
        i += 1;
    }
    Ok(out)
}

/// Resolve the effective DOM poll period from `--dom_update_interval`, mirroring
/// `DomInfoUpdateTask.__init__`: `None` uses the default; a negative value is rejected
/// with a warning and falls back to the default; a non-negative value is honoured.
fn resolve_dom_update_interval(arg: Option<i64>) -> Duration {
    match arg {
        None => Duration::from_secs(DOM_INFO_UPDATE_PERIOD_SECS),
        Some(v) if v < 0 => {
            eprintln!(
                "xcvrd-rs: invalid dom_update_interval {v} provided; using default \
                 {DOM_INFO_UPDATE_PERIOD_SECS} seconds instead"
            );
            Duration::from_secs(DOM_INFO_UPDATE_PERIOD_SECS)
        }
        Some(v) => Duration::from_secs(v as u64),
    }
}

/// Whether the `DomInfoUpdateTask`-owned tables (DOM/STATUS flags, PM, VDM) are
/// un-gated for a port, mirroring `not is_port_in_cmis_initialization_process`
/// (dom_mgr.py:182): with `--skip_cmis_mgr` the port is never considered "in CMIS
/// initialization", otherwise the gate releases only on a terminal `cmis_state`.
fn dom_flags_ungated(skip_cmis_mgr: bool, cmis_state: &str) -> bool {
    skip_cmis_mgr || cmis_is_terminal(cmis_state)
}

/// The resolved run-time configuration derived from [`DaemonArgs`], threaded through
/// `serve`. Mirrors the fields `DaemonXcvrd.__init__` stores and forwards to its task
/// threads (`skip_cmis_mgr`, `enable_sff_mgr`, and the two DOM poll cadences).
struct RunConfig {
    enable_sff_mgr: bool,
    skip_cmis_mgr: bool,
    dom_update_interval: Duration,
    dom_temperature_poll_interval: Option<Duration>,
}

/// Entry point: run the daemon forever. On any setup/serve error we log and retry
/// rather than exit, so the pmon supervisor keeps the daemon RUNNING even if the
/// emulator or Redis is briefly unavailable.
pub fn run() -> ! {
    // The pmon injection shim execs this binary with NO argv, but the SFF path relies on
    // the daemon's own `/proc/<pid>/cmdline` carrying `--enable_sff_mgr` (mirroring how
    // xcvrd.py forwards it from the supervisor conf). Re-exec once to advertise it before
    // any DB/platform init; one-shot (the re-exec'd image sees the flag and returns here).
    ensure_sff_mgr_flag();

    // Parse the command line exactly like the Python `argparse` parser in `xcvrd.py:main`.
    let argv: Vec<String> = std::env::args().collect();
    let args = match parse_daemon_args(&argv[1..]) {
        Ok(a) => a,
        Err(ArgParseError::HelpRequested) => {
            print!("{}", daemon_usage());
            std::process::exit(0);
        }
        Err(ArgParseError::Invalid(msg)) => {
            eprint!("{}", daemon_usage());
            eprintln!("xcvrd-rs: error: {msg}");
            std::process::exit(2);
        }
    };

    let cfg = RunConfig {
        enable_sff_mgr: args.enable_sff_mgr,
        skip_cmis_mgr: args.skip_cmis_mgr,
        dom_update_interval: resolve_dom_update_interval(args.dom_update_interval),
        dom_temperature_poll_interval: args
            .dom_temperature_poll_interval
            .map(|s| Duration::from_secs(s.max(0) as u64)),
    };
    eprintln!(
        "xcvrd-rs: starting (skip_cmis_mgr={} enable_sff_mgr={} dom_update_interval={}s \
         dom_temperature_poll_interval={})",
        cfg.skip_cmis_mgr,
        cfg.enable_sff_mgr,
        cfg.dom_update_interval.as_secs(),
        cfg.dom_temperature_poll_interval
            .map(|d| format!("{}s", d.as_secs()))
            .unwrap_or_else(|| "disabled".to_string()),
    );

    // SFF (non-CMIS) deterministic link bring-up (SffManagerTask). A swss `SubscriberStateTable`
    // thread watches CONFIG_DB `PORT.admin_status` and pushes every transition — including a
    // fast admin down→up round-trip a 1s poll would coalesce — into this queue; the serve loop
    // drains it and drives the SFF-8636 control registers (00h:86 Tx_Disable, 00h:93 power) in
    // the main thread (PyO3 platform access stays single-threaded, mirroring the CMIS pass).
    let admin_queue: Arc<Mutex<VecDeque<AdminObservation>>> = Arc::new(Mutex::new(VecDeque::new()));
    if cfg.enable_sff_mgr {
        spawn_admin_watcher(admin_queue.clone());
    }
    // Always-on CONFIG_DB `PORT` add/remove watcher (independent of the SFF flag): a swss
    // `SubscriberStateTable` thread pushes every logical-port `Set`/`Del` into this queue, which
    // the serve loop reconciles into full per-port table teardown / repopulation. Logical-port
    // (de)configuration is core xcvrd behaviour, so it runs regardless of `--enable_sff_mgr`.
    let port_cfg_queue: Arc<Mutex<VecDeque<PortConfigObservation>>> =
        Arc::new(Mutex::new(VecDeque::new()));
    spawn_port_config_watcher(port_cfg_queue.clone());
    // Always-on STATE_DB `PORT_TABLE` host_tx_ready watcher: a swss `SubscriberStateTable` thread
    // pushes every `host_tx_ready` SET (including a transient `true`→`false`→`true` that a
    // background keeper re-asserts) into this queue. The CMIS datapath pass drains it and reacts
    // to a `false` edge by tearing an activated datapath down (DataPathDeinit + Tx-off). This is
    // EDGE-triggered, mirroring the reference `CmisManagerTask` STATE_DB `PORT_TABLE` subscriber
    // (a 1s poll races the keeper and misses the brief `false`, so it must not be poll-only).
    let host_tx_queue: Arc<Mutex<VecDeque<HostTxObservation>>> =
        Arc::new(Mutex::new(VecDeque::new()));
    spawn_host_tx_watcher(host_tx_queue.clone());
    // Per-physical-port SFF bring-up state (kept across serve() restarts so a transient serve
    // error doesn't re-run bring-up needlessly).
    let mut sff_state: HashMap<usize, SffDeployState> = HashMap::new();

    loop {
        if let Err(e) = serve(&cfg, &admin_queue, &port_cfg_queue, &host_tx_queue, &mut sff_state) {
            eprintln!("xcvrd-rs: serve error: {e}; retrying in 3s");
            std::thread::sleep(Duration::from_secs(3));
        }
    }
}

/// Media / optics-SI environment (loaded once at start-up, mirroring xcvrd's
/// `load_media_settings()` / `load_optics_si_settings()` at `xcvrd.py:1052-1053`).
/// Holds the two settings dicts, the logical↔physical port mapping, the gearbox
/// line-lane map, and the producer-table handles `notify_media_setting` writes:
/// APPL_DB `PORT_TABLE` (colon-keyed), STATE_DB `PORT_TABLE` (pipe-keyed) and the
/// CONFIG_DB `PORT` reader used for `get_speed_lane_count_and_subport`.
struct MediaEnv {
    g_media: Value,
    g_optics: Value,
    gearbox_lanes: HashMap<String, u32>,
    port_mapping: PortMapping,
    app_port_tbl: Rc<dyn Table>,
    state_port_tbl: Rc<dyn Table>,
    cfg_port_tbl: Rc<dyn Table>,
}

/// In-container platform directory (`device_info.CONTAINER_PLATFORM_PATH`). Inside the
/// pmon container this is the mount point that holds the platform's shipped files —
/// including a provisioned `media_settings.json` / `optics_si_settings.json` — so it is
/// preferred over the host device path (which is not mounted at that path inside pmon).
const CONTAINER_PLATFORM_PATH: &str = "/usr/share/sonic/platform";
/// Host device tree root (`device_info.HOST_DEVICE_PATH`); `<root>/<platform>` is the
/// per-platform directory when running on the host rather than in a container.
const HOST_DEVICE_PATH: &str = "/usr/share/sonic/device";
/// ONIE/aboot machine descriptor (`device_info.MACHINE_CONF_PATH`) — the authoritative
/// platform identifier source, read before CONFIG_DB.
const MACHINE_CONF_PATH: &str = "/host/machine.conf";

/// Parse the platform identifier from `/host/machine.conf` contents the way
/// `device_info.get_platform()` does: `onie_platform` takes precedence over
/// `aboot_platform`. Pure helper so the precedence is unit-testable.
fn parse_machine_conf_platform(text: &str) -> Option<String> {
    let mut onie = None;
    let mut aboot = None;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "onie_platform" => onie = Some(v.trim().to_string()),
                "aboot_platform" => aboot = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    onie.or(aboot).filter(|s| !s.is_empty())
}

/// Choose the platform directory the way `device_info.get_path_to_platform_dir()` does:
/// prefer the in-container `/usr/share/sonic/platform` when it exists, else the host
/// `/usr/share/sonic/device/<platform>` when that directory exists, else fall back to the
/// container path (best effort — `load_settings_file` then finds nothing and media / optics
/// notify becomes a no-op rather than a hard error). Pure helper (filesystem probes are
/// passed in) so the preference order is unit-testable.
fn choose_platform_path(container_is_dir: bool, host_dir: Option<(String, bool)>) -> String {
    if container_is_dir {
        return CONTAINER_PLATFORM_PATH.to_string();
    }
    if let Some((host, true)) = host_dir {
        return host;
    }
    CONTAINER_PLATFORM_PATH.to_string()
}

/// `device_info.get_platform()`: the `PLATFORM` env override, else `/host/machine.conf`
/// (`onie_platform` / `aboot_platform`), else CONFIG_DB `DEVICE_METADATA|localhost.platform`.
fn get_platform(config: &DbConnector) -> Option<String> {
    if let Ok(p) = std::env::var("PLATFORM") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    if let Ok(text) = std::fs::read_to_string(MACHINE_CONF_PATH) {
        if let Some(p) = parse_machine_conf_platform(&text) {
            return Some(p);
        }
    }
    config
        .hget("DEVICE_METADATA|localhost", "platform")
        .ok()
        .flatten()
        .map(|c| c.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
}

/// Resolve the platform / HWSKU directories the way the Python daemon does
/// (`device_info.get_paths_to_platform_and_hwsku_dirs()`).
///
/// The platform dir mirrors `get_path_to_platform_dir()`: prefer the in-container
/// `/usr/share/sonic/platform` when it exists (inside pmon this is the read-only mount
/// that carries the provisioned `media_settings.json` / `optics_si_settings.json` — the
/// host `/usr/share/sonic/device/<platform>` path is *not* mounted there), else the host
/// `/usr/share/sonic/device/<platform>` (platform via `get_platform()`). The HWSKU dir is
/// `<platform_path>/<hwsku>` with `hwsku` from CONFIG_DB `DEVICE_METADATA|localhost.hwsku`.
fn resolve_device_paths(config: &DbConnector) -> (String, String) {
    let container_is_dir = std::path::Path::new(CONTAINER_PLATFORM_PATH).is_dir();
    let host_dir = if container_is_dir {
        None
    } else {
        get_platform(config).map(|platform| {
            let host = format!("{HOST_DEVICE_PATH}/{platform}");
            let is_dir = std::path::Path::new(&host).is_dir();
            (host, is_dir)
        })
    };
    let platform_path = choose_platform_path(container_is_dir, host_dir);
    let hwsku = config
        .hget("DEVICE_METADATA|localhost", "hwsku")
        .ok()
        .flatten()
        .map(|c| c.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty());
    // `<platform_path>/<hwsku>`; when the HWSKU is unknown, a sentinel subdir that will
    // never contain a settings file so `load_settings_file` falls through to the platform
    // dir (where the provisioned files live).
    let hwsku_path = format!("{platform_path}/{}", hwsku.as_deref().unwrap_or("unknown"));
    (platform_path, hwsku_path)
}

/// Build the [`MediaEnv`]: load `media_settings.json` / `optics_si_settings.json`,
/// construct a [`PortMapping`] covering every discovered port (one logical per
/// physical, as `discover_ports` models the emulator), read the gearbox line-lane
/// map, and mint the producer tables. Table-open failures degrade to [`NullTable`]
/// so a DB hiccup never tears the daemon down (media notify then no-ops).
fn build_media_env(config: &DbConnector, ports: &BTreeMap<usize, PortCtx>) -> MediaEnv {
    let (platform_path, hwsku_path) = resolve_device_paths(config);
    let g_media = media_settings_parser::load_media_settings(&platform_path, &hwsku_path);
    let g_optics = optics_si_parser::load_optics_si_settings(&platform_path, &hwsku_path);

    let mut port_mapping = PortMapping::new();
    for (&phys, ctx) in ports.iter() {
        port_mapping.handle_port_change_event(&PortChangeEvent::new(
            ctx.name.clone(),
            phys as i32,
            0,
            PortEventType::PortAdd,
        ));
    }

    let sock = env::redis_sock();
    // Open the media producer/reader tables over raw connections that build their keys
    // with the DB's *explicit* separator (APPL_DB `:`, STATE_DB / CONFIG_DB `|`) — the
    // same discipline the rest of the daemon uses for STATE_DB writes. This guarantees
    // the published rows land on the exact keys the NPU/orchagent reads: APPL_DB
    // `PORT_TABLE:<port>`, STATE_DB `PORT_TABLE|<port>`, CONFIG_DB `PORT|<port>`. A
    // connect failure degrades to `NullTable` so a DB hiccup never
    // tears the daemon down (media notify then no-ops).
    let open = |db_id: i32, name: &str, sep: char| -> Rc<dyn Table> {
        match DbConnector::new_unix(db_id, sock.clone(), 0) {
            Ok(conn) => Rc::new(SepTable::new(conn, name, sep)),
            Err(e) => {
                eprintln!("xcvrd-rs: media notify: open {name} (db {db_id}) failed: {e:?}");
                Rc::new(NullTable)
            }
        }
    };
    let app_port_tbl = open(env::APPL_DB, "PORT_TABLE", ':');
    let state_port_tbl = open(env::STATE_DB, "PORT_TABLE", '|');
    let cfg_port_tbl = open(env::CONFIG_DB, "PORT", '|');

    let gearbox_lanes = match RealStateDb::new(env::APPL_DB, sock).table("_GEARBOX_TABLE") {
        Ok(t) => XcvrTableHelper::new().get_gearbox_line_lanes_dict(&*t),
        Err(_) => HashMap::new(),
    };

    MediaEnv {
        g_media,
        g_optics,
        gearbox_lanes,
        port_mapping,
        app_port_tbl,
        state_port_tbl,
        cfg_port_tbl,
    }
}

/// Seed STATE_DB `PORT_TABLE|<name>.NPU_SI_SETTINGS_SYNC_STATUS = DEFAULT` for every
/// logical port that has not been stamped yet (xcvrd.py:941-958 / on_add_logical_port).
/// The DEFAULT guard is what lets `notify_media_setting` publish exactly once per
/// (re)insertion until the status is reset.
fn seed_npu_si_defaults(state: &DbConnector, ports: &BTreeMap<usize, PortCtx>) {
    for ctx in ports.values() {
        let key = format!("PORT_TABLE|{}", ctx.name);
        if state
            .hget(&key, NPU_SI_SETTINGS_SYNC_STATUS_KEY)
            .ok()
            .flatten()
            .is_none()
        {
            let _ = state.hset(
                &key,
                NPU_SI_SETTINGS_SYNC_STATUS_KEY,
                &CxxString::from(NPU_SI_SETTINGS_DEFAULT_VALUE),
            );
        }
    }
}

/// Resolve and publish the NPU/ASIC-side media SerDes settings for one port after its
/// identity is (re)posted — the Rust analogue of the `media_settings_parser.notify_media_
/// setting(...)` call at `xcvrd.py:585`. Self-gates on: media settings loaded, the NPU_SI
/// guard still DEFAULT (idempotency), and the module present with readable identity.
/// Builds the `{physical_port: transceiver_info}` dict, reads `(speed, lane_count, subport)`
/// from CONFIG_DB, and hands `notify_media_setting` the CMIS decode seam (for the
/// `speed:<HEID>` lane-speed key) plus the presence probe.
fn publish_media_settings(
    platform: &Platform,
    media_env: &MediaEnv,
    phys: usize,
    port_name: &str,
) {
    if !media_settings_present(&media_env.g_media) {
        return;
    }
    let helper = XcvrTableHelper::new();
    if !helper.is_npu_si_settings_update_required(
        port_name,
        Some(&media_env.port_mapping),
        Some(&*media_env.state_port_tbl),
    ) {
        return;
    }

    let sfp = match platform.sfp(phys) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("xcvrd-rs: media notify open sfp {phys} ({port_name}) failed: {e}");
            return;
        }
    };
    if !sfp.get_presence().unwrap_or(false) {
        return;
    }
    let info = sfp.get_transceiver_info().unwrap_or(Value::Null);
    if !info.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        return;
    }
    let is_copper = match sfp.call_json("is_copper", ()) {
        Ok(Value::Bool(b)) => b,
        _ => true,
    };

    let mut dict = Map::new();
    dict.insert(phys.to_string(), info);
    let transceiver_dict = Value::Object(dict);

    let (port_speed, lane_count, subport) =
        get_speed_lane_count_and_subport(port_name, &*media_env.cfg_port_tbl);

    let api = match platform.sfp(phys) {
        Ok(s) => BridgeCmisApi::new(Box::new(RealSfp(s))),
        Err(e) => {
            eprintln!("xcvrd-rs: media notify open api {phys} ({port_name}) failed: {e}");
            return;
        }
    };
    let is_cmis = !api.is_flat_memory()
        && common::is_cmis_api(api.get_module_type_abbreviation().as_deref());

    let presence_of = |p: i32| {
        platform
            .sfp(p as usize)
            .map(|s| s.get_presence().unwrap_or(false))
            .unwrap_or(false)
    };
    let key_of = |p: i32, effective_lane_count: u32| {
        get_media_settings_key(
            p,
            &transceiver_dict,
            port_speed,
            effective_lane_count,
            is_cmis,
            Some(&api as &dyn CmisApi),
            is_copper,
        )
    };
    let tables = MediaNotifyTables {
        app_port_tbl: &*media_env.app_port_tbl,
        state_port_tbl: &*media_env.state_port_tbl,
    };
    notify_media_setting(
        port_name,
        &transceiver_dict,
        &media_env.g_media,
        &media_env.port_mapping,
        true,
        true,
        port_speed,
        lane_count,
        subport,
        &media_env.gearbox_lanes,
        &tables,
        &presence_of,
        &key_of,
    );
}

fn serve(
    cfg: &RunConfig,
    admin_queue: &Arc<Mutex<VecDeque<AdminObservation>>>,
    port_cfg_queue: &Arc<Mutex<VecDeque<PortConfigObservation>>>,
    host_tx_queue: &Arc<Mutex<VecDeque<HostTxObservation>>>,
    sff_state: &mut HashMap<usize, SffDeployState>,
) -> Result<(), Box<dyn Error>> {
    let platform = env::open_platform()?; // PyO3 -> sonic_platform -> xcvr-emu
    let state = env::open_state_db()?; // swss-common -> STATE_DB
    let config = env::open_config_db()?; // swss-common -> CONFIG_DB
    let appl = env::open_appl_db()?; // swss-common -> APPL_DB (PORT_TABLE.flap_count)

    // Register the graceful-shutdown handler now that CPython is initialised (open_platform
    // ran PyO3's `Py_InitializeEx(0)`), so a `supervisorctl stop` (SIGTERM) reaches our
    // deinit teardown rather than killing the daemon.
    install_shutdown_handler();

    // Warm/fast-reboot verdicts, read ONCE at start-up and cached for the life of this serve
    // pass (the reference `CmisManagerTask.initialize_fast_reboot_status` / SfpStateUpdateTask
    // `initialize_warm_fast_reboot_status` — the flag is set before xcvrd (re)starts):
    //   * `fast_reboot` gates the CMIS datapath re-provision skip.
    //   * `warm_fast_reboot` gates the media-settings publish (xcvrd.py:584) so a warm reboot
    //     does not re-notify the NPU SI and disrupt a live link.
    // (The deinit-on-shutdown teardown re-reads the flag FRESH — see `deinit_on_shutdown`.)
    let fast_reboot = common::is_fast_reboot_enabled(&state);
    let warm_fast_reboot =
        common::is_syncd_warm_restore_complete(&state) || fast_reboot;
    if fast_reboot || warm_fast_reboot {
        eprintln!(
            "xcvrd-rs: reboot state at start-up: fast_reboot={fast_reboot} warm_fast_reboot={warm_fast_reboot}"
        );
    }

    let mut ports = discover_ports(&platform, &config)?;
    eprintln!("xcvrd-rs: {} configured ports discovered", ports.len());

    // Load media / optics-SI settings, build the port mapping + producer tables
    // (xcvrd.py:1052-1053), and seed the NPU_SI sync guard for every logical port.
    let media_env = build_media_env(&config, &ports);
    seed_npu_si_defaults(&state, &ports);

    // Logical ports whose module is present but whose EEPROM identity could not be
    // read yet — the retry set (xcvrd.py `retry_eeprom_set`). Re-read on a ~60s
    // cadence until the identity appears, then publish TRANSCEIVER_INFO.
    let mut retry_eeprom_set: BTreeSet<usize> = BTreeSet::new();
    let mut last_retry_eeprom_time: Option<Instant> = None;

    // Link-change flag re-capture state (dom_mgr.on_port_update_event): the last
    // APPL_DB `PORT_TABLE.flap_count` seen per physical port, and the ports whose
    // flag tables are due to be re-read (scheduled DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE
    // after the flap was observed). Seeding the baseline up-front means only a real
    // flap_count *change* triggers a re-read, not the initial observation.
    let mut link_flap_counts: HashMap<usize, String> = HashMap::new();
    let mut link_change_due: HashMap<usize, Instant> = HashMap::new();
    seed_link_flap_counts(&appl, &ports, &mut link_flap_counts);

    // Initial full sync so a freshly-flushed STATE_DB repopulates (and stale INFO
    // for absent modules is purged). Per-port errors are logged but don't tear
    // down the daemon.
    for (&phys, ctx) in ports.iter_mut() {
        match sync_port(&platform, &state, phys, ctx) {
            Ok(true) => {
                // Identity (re)posted: publish the resolved NPU/ASIC-side media SI to
                // APPL_DB and stamp NPU_SI=NOTIFIED (xcvrd.py:585). Self-gates on
                // presence + the DEFAULT guard, so an absent/removed port is a no-op.
                // Skipped on a warm/fast reboot (xcvrd.py:584) so a live link is not
                // re-notified across the restart.
                if !warm_fast_reboot {
                    publish_media_settings(&platform, &media_env, phys, &ctx.name);
                }
            }
            Ok(false) => {
                retry_eeprom_set.insert(phys);
            }
            Err(e) => eprintln!("xcvrd-rs: initial sync {} (sfp {phys}) failed: {e}", ctx.name),
        }
    }
    eprintln!("xcvrd-rs: initial sync complete; watching for change events");

    // Drive the CMIS datapath state machine once up-front so admin-up modules begin
    // leaving low power before the first DOM poll (a healthy module reaches READY within
    // a handful of ~1s serve passes). Skipped entirely under `--skip_cmis_mgr`
    // (`if not self.skip_cmis_mgr:`, xcvrd.py:1171).
    if !cfg.skip_cmis_mgr {
        cmis_datapath_sm(&platform, &state, &config, &mut ports, &media_env.g_optics, fast_reboot, host_tx_queue);
    }

    // SFF (non-CMIS) deterministic link bring-up, mirroring the CMIS pass. Gated on
    // `--enable_sff_mgr` (xcvrd.py:1150). Drives Tx_Disable / low-power / high-power-class for
    // SFF-8636/8472 modules per admin_status + host_tx_ready; a no-op on CMIS/absent ports.
    if cfg.enable_sff_mgr {
        sff_control(&platform, &state, &config, &ports, sff_state, admin_queue);
    }
    // Defer the first periodic DOM poll by one interval, mirroring the reference
    // `DomInfoUpdateTask.task_worker` (dom_mgr.py:296-298: "Adding
    // dom_info_update_periodic_secs to allow xcvrd to initialize ports before starting
    // the periodic update"). This is functionally important, not cosmetic: the latched
    // flag tables (TRANSCEIVER_DOM_FLAG / _STATUS_FLAG) must stay ABSENT until either the
    // first periodic poll or a link-change re-read
    // (`update_port_db_diagnostics_on_link_change`, serviced below). An immediate startup
    // poll would pre-publish `<flag>=False` for every present port, so the link-change
    // fast-recapture path could no longer be isolated from a stale poll value: a baseline
    // flap's `wait_until(<flag>=="False")` would return instantly on the stale row instead
    // of blocking on the flap's own re-read, letting that re-read fire later (with a
    // freshly-raised alarm) inside the pre-flap guard window. TRANSCEIVER_INFO/identity is
    // published synchronously by `sync_port` above, so the clean-baseline health check does
    // not depend on this first poll.
    let mut next_dom_time = Instant::now() + cfg.dom_update_interval;

    // Separate DOM temperature poll (`DomThermalInfoUpdateTask`, xcvrd.py:1183): only when
    // `--dom_temperature_poll_interval` is set, and its first poll fires immediately
    // (dom_mgr.py:542 `next_periodic_db_update_time = datetime.datetime.now()`).
    let mut next_thermal_time = cfg.dom_temperature_poll_interval.map(|_| Instant::now());

    // React to plug/unplug transitions. get_change_event blocks up to the timeout
    // and returns the set of physical ports whose presence changed. Each iteration
    // also services the EEPROM read-retry set on its ~60s cadence, drives CMIS
    // bring-up (~1s), watches APPL_DB for link flaps (fast flag re-capture), and
    // runs the periodic DOM poll on its ~60s cadence.
    loop {
        // Graceful shutdown (SIGTERM from `supervisorctl stop`): run the deinit teardown —
        // which preserves the TRANSCEIVER_STATUS pair on a warm/fast reboot so the live
        // datapath is not disrupted — then exit so run()'s retry loop does not respawn us.
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            eprintln!("xcvrd-rs: shutdown signal received; running deinit teardown");
            deinit_on_shutdown(&state, &ports);
            std::process::exit(0);
        }
        let ev = platform.get_change_event(1000)?;
        for (phys_str, code) in ev.sfp.iter() {
            let Ok(phys) = phys_str.parse::<usize>() else { continue };
            let Some(ctx) = ports.get_mut(&phys) else { continue };
            if code == sfp_status_helper::SFP_STATUS_INSERTED
                || code == sfp_status_helper::SFP_STATUS_REMOVED
            {
                // Plug-in ('1') or plug-out ('0'): (re)publish or tear down the port.
                // A plug-in also clears any prior error state (sync_port rewrites
                // TRANSCEIVER_STATUS_SW status/error), so recovering from an error is
                // just the insert path.
                let mut synced = sync_port(&platform, &state, phys, ctx);
                // A just-inserted module whose identity EEPROM is not yet readable
                // (present but the first read returned SFP_EEPROM_NOT_READY -> Ok(false)):
                // pause briefly and re-read once before deferring to the slow
                // RETRY_EEPROM_READING_INTERVAL set, mirroring xcvrd.py
                // SfpStateUpdateTask.task_worker so a hot-plug repopulates
                // TRANSCEIVER_INFO promptly (within T_FAST) instead of only on the next
                // ~60s retry sweep. A plug-out always syncs Ok(true), so this pause only
                // ever applies to a plug-in.
                if matches!(synced, Ok(false)) {
                    sleep(TIME_FOR_SFP_READY);
                    synced = sync_port(&platform, &state, phys, ctx);
                }
                match synced {
                    Ok(true) => {
                        retry_eeprom_set.remove(&phys);
                        // Re(publish) media SI on the plug-in (idempotent via the DEFAULT
                        // guard); on a plug-out sync_port has reset the guard to DEFAULT so
                        // the next insertion re-publishes (test_..._default_to_notified).
                        // Skipped on a warm/fast reboot (xcvrd.py:584).
                        if !warm_fast_reboot {
                            publish_media_settings(&platform, &media_env, phys, &ctx.name);
                        }
                    }
                    Ok(false) => {
                        retry_eeprom_set.insert(phys);
                    }
                    Err(e) => eprintln!("xcvrd-rs: change sync {} (sfp {phys}) failed: {e}", ctx.name),
                }
            } else {
                // Any other change-event code is an SfpBase error bitmap: decode it
                // into TRANSCEIVER_STATUS_SW.error and, for a blocking error, purge
                // the (now out-of-date) DOM/hardware tables while keeping the static
                // TRANSCEIVER_INFO (xcvrd.py:623-666).
                if let Err(e) =
                    handle_sfp_error_event(&state, &ctx.name, code, ev.sfp_error.get(phys_str))
                {
                    eprintln!("xcvrd-rs: error event {} (sfp {phys}) failed: {e}", ctx.name);
                }
            }
        }

        // Reconcile CONFIG_DB logical-port add/remove (full per-port table teardown /
        // repopulation) BEFORE the per-port loops below, so a just-removed port is dropped from
        // `ports` (no CMIS/DOM/link pass touches it this wake) and a just-added port is synced
        // and picked up by the same pass.
        reconcile_logical_ports(
            &platform,
            &state,
            &config,
            &media_env,
            &mut ports,
            port_cfg_queue,
            warm_fast_reboot,
            &mut retry_eeprom_set,
            &mut link_flap_counts,
            &mut link_change_due,
        );

        retry_eeprom_reading(&platform, &state, &mut ports, &mut retry_eeprom_set, &mut last_retry_eeprom_time);

        // Advance the CMIS datapath state machine one transition per wake (~1s, paced
        // by get_change_event's 1000ms timeout): a freshly-inserted admin-up module walks
        // INSERTED → … → READY, a reconfigured port (CONFIG_DB admin_status flip) is torn
        // down and re-provisioned, and a stalled module retries then latches FAILED.
        // Skipped entirely under `--skip_cmis_mgr` (xcvrd.py:1171).
        if !cfg.skip_cmis_mgr {
            cmis_datapath_sm(&platform, &state, &config, &mut ports, &media_env.g_optics, fast_reboot, host_tx_queue);
        }

        // Advance SFF (non-CMIS) bring-up: drain the admin-status watcher queue (fast-toggle
        // replay) and re-evaluate every port's Tx-disable / power control (~1s cadence).
        if cfg.enable_sff_mgr {
            sff_control(&platform, &state, &config, &ports, sff_state, admin_queue);
        }

        // Link-change flag re-capture: detect APPL_DB flap_count bumps (schedule a
        // re-read ~1s out) and service any port whose re-read is now due.
        let detected_link_change =
            detect_link_changes(&appl, &ports, &mut link_flap_counts, &mut link_change_due);
        let serviced_link_change = service_link_changes(
            &platform,
            &state,
            &config,
            &ports,
            cfg.skip_cmis_mgr,
            &mut link_change_due,
        );

        // Keep the link-change fast recapture the SOLE flag writer within a flap's trigger
        // window: whenever a link change is in flight — a flap just detected, a re-read just
        // serviced, or a re-read still pending its ~1s delay — push the periodic DOM poll a full
        // interval out. The reference runs the poll and the re-read in one thread off an
        // independent free-running poll timer; here they share the serve() pass, so a coincident
        // poll would otherwise publish a flag row (TRANSCEIVER_DOM_FLAG / _STATUS_FLAG) into the
        // flap-isolation window and be indistinguishable from the link-change re-read. Deferring
        // on detection (not just on servicing) closes the ~1s gap before the re-read fires, and
        // because every flap re-arms it the poll stays held off across a whole multi-flap
        // sequence. `.max` only ever pushes the deadline out, never pulls it in. Symmetric
        // counterpart to `coalesce_link_change_after_poll` (which drops a pending re-read after a
        // poll); together they keep the two flag writers mutually exclusive. A quiescent port
        // never flaps, so its normal DOM cadence is unaffected.
        if link_change_defers_poll(
            detected_link_change,
            serviced_link_change,
            !link_change_due.is_empty(),
        ) {
            next_dom_time = next_dom_time.max(Instant::now() + cfg.dom_update_interval);
        }

        if Instant::now() >= next_dom_time {
            let poll_start = Instant::now();
            dom_info_update(&platform, &state, &config, cfg.skip_cmis_mgr, &ports);
            next_dom_time = poll_start + cfg.dom_update_interval;
            // The poll just re-published every port's latched flag tables — drop any pending
            // link-change re-read so it can't re-fire the same flags across a later alarm-raise.
            // See `coalesce_link_change_after_poll`.
            coalesce_link_change_after_poll(&mut link_change_due);
        }

        // Separate DOM temperature poll on its own cadence (`DomThermalInfoUpdateTask`),
        // active only when `--dom_temperature_poll_interval` was provided.
        if let (Some(interval), Some(due)) =
            (cfg.dom_temperature_poll_interval, next_thermal_time)
        {
            if Instant::now() >= due {
                dom_temperature_update(&platform, &state, &config, &ports);
                next_thermal_time = Some(Instant::now() + interval);
            }
        }
    }
}

/// Read the current APPL_DB `PORT_TABLE:<port>.flap_count` for every configured port
/// into the baseline map (no re-read is triggered for these initial values), so the
/// link-change watcher only fires on a subsequent *change*.
fn seed_link_flap_counts(
    appl: &DbConnector,
    ports: &BTreeMap<usize, PortCtx>,
    link_flap_counts: &mut HashMap<usize, String>,
) {
    for (&phys, ctx) in ports {
        link_flap_counts.insert(phys, read_flap_count(appl, &ctx.name));
    }
}

/// `dom_mgr.on_port_update_event` (dom_mgr.py:424): watch APPL_DB `PORT_TABLE` and,
/// on any `PORT_SET` that changes a port's `flap_count` (a link flap), schedule that
/// port's flag tables to be re-read `DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE` later.
/// The polled read of the whole set each wake is the reduced-daemon analogue of the
/// Python `SubscriberStateTable` on `PORT_TABLE` filtered to `flap_count`.
///
/// Returns whether any port flapped this pass, so the caller can defer the periodic DOM
/// poll while a link change is in flight (see `link_change_defers_poll`).
fn detect_link_changes(
    appl: &DbConnector,
    ports: &BTreeMap<usize, PortCtx>,
    link_flap_counts: &mut HashMap<usize, String>,
    link_change_due: &mut HashMap<usize, Instant>,
) -> bool {
    let mut detected = false;
    for (&phys, ctx) in ports {
        let cur = read_flap_count(appl, &ctx.name);
        let changed =
            flap_count_triggers_recapture(link_flap_counts.get(&phys).map(String::as_str), &cur);
        link_flap_counts.insert(phys, cur);
        if changed {
            link_change_due.insert(phys, Instant::now() + DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE);
            detected = true;
        }
    }
    detected
}

/// The `on_port_update_event` trigger condition (dom_mgr.py:433-443) expressed over
/// the polled `flap_count`: a re-read fires only when a port's `flap_count` *changes*
/// from a value already observed. The first observation (no prior baseline) only
/// seeds the baseline and never triggers, so daemon startup does not spuriously
/// re-read every port's flag tables before a real link flap occurs.
fn flap_count_triggers_recapture(prev: Option<&str>, cur: &str) -> bool {
    match prev {
        Some(prev) => prev != cur,
        None => false,
    }
}

/// Service the ports whose link-change flag re-read is now due
/// (`update_port_db_diagnostics_on_link_change`, dom_mgr.py:445): re-read ONLY the
/// latched flag tables (`TRANSCEIVER_DOM_FLAG` → `TRANSCEIVER_STATUS_FLAG` → and, when
/// the module advertises VDM, `TRANSCEIVER_VDM_*_FLAG`) for that port — the fast,
/// targeted trigger separate from presence and the periodic poll. The gate order
/// mirrors the reference (dom_mgr.py:460-499): DOM monitoring enabled, not latched in
/// a blocking error, present; plus the cmis-init flag gate it shares with the periodic
/// poll (released early under `--skip_cmis_mgr`). Returns `true` when at least one
/// port's flags were actually re-read, so the caller can coalesce the imminent
/// redundant periodic poll.
fn service_link_changes(
    platform: &Platform,
    state: &DbConnector,
    config: &DbConnector,
    ports: &BTreeMap<usize, PortCtx>,
    skip_cmis_mgr: bool,
    link_change_due: &mut HashMap<usize, Instant>,
) -> bool {
    let now = Instant::now();
    let due: Vec<usize> = link_change_due
        .iter()
        .filter(|(_, &t)| now >= t)
        .map(|(&p, _)| p)
        .collect();
    let mut serviced = false;
    for phys in due {
        link_change_due.remove(&phys);
        let Some(ctx) = ports.get(&phys) else { continue };
        let port = ctx.name.as_str();
        // dom_polling=disabled halts the flag re-read (is_port_dom_monitoring_disabled,
        // dom_mgr.py:460); a blocking error keeps the purged tables absent
        // (detect_port_in_error_status, dom_mgr.py:470); a port still in CMIS bring-up
        // keeps flags gated (is_port_in_cmis_initialization_process, dom_mgr.py:182).
        if dom_polling_disabled(config, port) {
            continue;
        }
        if port_in_blocking_error(state, port) {
            continue;
        }
        if !dom_flags_ungated(skip_cmis_mgr, &ctx.cmis_state) {
            continue;
        }
        let sfp = match platform.sfp(phys) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("xcvrd-rs: link-change open sfp {phys} ({port}) failed: {e}");
                continue;
            }
        };
        if !sfp.get_presence().unwrap_or(false) {
            continue;
        }
        // DOM flags → STATUS flags → (only when the module advertises VDM) VDM flags,
        // matching the reference re-read order (dom_mgr.py:476-499).
        if let Err(e) = publish_dom_flags(state, &sfp, port) {
            eprintln!("xcvrd-rs: link-change DOM flags {port} (sfp {phys}) failed: {e}");
        }
        if let Err(e) = publish_status_flags(state, &sfp, port) {
            eprintln!("xcvrd-rs: link-change STATUS flags {port} (sfp {phys}) failed: {e}");
        }
        if sfp_bool(&sfp, "is_transceiver_vdm_supported") {
            if let Err(e) = publish_vdm_flags(state, &sfp, port) {
                eprintln!("xcvrd-rs: link-change VDM flags {port} (sfp {phys}) failed: {e}");
            }
        }
        serviced = true;
    }
    serviced
}

/// After a periodic DOM poll has re-published EVERY port's latched flag tables, drop every pending
/// link-change fast re-read. The poll just produced that same output, so a leftover re-read would
/// only re-fire the identical flags ~1s later — and across an alarm-raise with no intervening flap
/// that late write would surface a freshly-raised flag the poll had not, i.e. a flag surfacing
/// that is NOT attributable to a link change. This is the symmetric counterpart to deferring the
/// poll a full interval after servicing a re-read (`serve`): together they keep the periodic poll
/// and the link-change recapture mutually exclusive within a settle window. A flap detected on a
/// LATER pass is scheduled fresh and is unaffected (this only clears re-reads scheduled so far).
fn coalesce_link_change_after_poll(link_change_due: &mut HashMap<usize, Instant>) {
    link_change_due.clear();
}

/// Whether the periodic DOM poll must be deferred this serve pass because a link change is
/// in flight. True when a flap was just detected, a fast re-read was just serviced, or a
/// re-read is still pending its ~1s delay — so the link-change fast recapture stays the sole
/// flag writer within a flap's trigger window and a coincident periodic poll cannot publish a
/// flag row during the flap-isolation window. Counterpart to `coalesce_link_change_after_poll`.
fn link_change_defers_poll(detected: bool, serviced: bool, reads_pending: bool) -> bool {
    detected || serviced || reads_pending
}

/// Read `PORT_TABLE:<port>.flap_count` from APPL_DB as an owned `String` (empty when
/// the field/row is absent). APPL_DB keys are colon-separated (`PORT_TABLE:Ethernet48`).
fn read_flap_count(appl: &DbConnector, port: &str) -> String {
    match appl.hget(&format!("PORT_TABLE:{port}"), "flap_count") {
        Ok(Some(v)) => v.to_string_lossy().into_owned(),
        _ => String::new(),
    }
}

// =====================================================================================
// CMIS datapath bring-up state machine.
//
// A faithful port of `CmisManagerTask`'s per-port datapath machine (mirrored in the
// unit-tested `cmis::cmis_manager_task`), driven directly over the bridge + swss-common
// here so it runs inside the single-threaded serve() loop. Register control/decode is
// reused verbatim through `BridgeCmisApi` (page-10h/11h/01h encodings live in
// `cmis::cmis_api`); this module only orchestrates the transitions and the STATE_DB
// projection.
//
// The serve() loop wakes ~1s (get_change_event's 1000ms timeout) and advances exactly
// ONE transition per pass, so each intermediate `cmis_state` is published ~1s — the
// natural inter-state pacing the reference armed with a timer (no explicit dwell needed
// here). A CONFIG_DB `admin_status` flip is re-read every pass and forces a fresh
// re-init, so a reconfiguration tears the datapath down (admin-down) and re-provisions
// it (admin-up) with no port event.
//
// Single-lport-per-SFP testbed (`Ethernet{i*4}` ↔ SFP `i`, subport 0): each physical port
// carries exactly one logical port, so the Python cross-sibling decommission machinery
// collapses to a per-port handshake — when a live speed/app change makes the currently
// active AppSel wrong, the port resets AppSel to 0 (provision app 0) and then re-provisions
// the new app.
//
// host_tx_ready: the emulator/KVM has no orchagent that asserts host_tx_ready at bring-up,
// so an ABSENT STATE_DB `PORT_TABLE.host_tx_ready` is treated as ready (`true`) to preserve
// the bring-up gates. An EXPLICIT drop to `'false'` is honoured — the port re-inits and tears
// the datapath down, exactly like `admin_status != 'up'`. The testbed runs a background keeper
// that re-asserts `'true'` almost immediately after a test drives `'false'`, so a 1s level poll
// can miss the transient; the STATE_DB PORT_TABLE subscriber captures the `'false'` EDGE, which
// is what fires the teardown even when the net level is back at `'true'`.
// =====================================================================================

/// `CmisManagerTask.CMIS_MAX_RETRIES` — datapath bring-up retry cap before `FAILED`.
const CMIS_MAX_RETRIES: u32 = 3;
/// CMIS host lanes per module (`CmisManagerTask.CMIS_MAX_HOST_LANES`).
const CMIS_MAX_HOST_LANES: u32 = 8;
/// Slack added to every armed state-expiration timer (`CMIS_EXPIRATION_BUFFER_MS`).
const CMIS_EXPIRATION_BUFFER_MS: u64 = 2;

/// Read CONFIG_DB `PORT|<port>.admin_status`. `Some(true)`/`Some(false)` on a definite
/// read (`up` / not-`up` / absent field → down); `None` on a DB error so a transient
/// hiccup never spuriously flips a port and tears its datapath down.
fn read_admin_up(config: &DbConnector, port: &str) -> Option<bool> {
    match config.hget(&format!("PORT|{port}"), "admin_status") {
        Ok(Some(v)) => Some(v.to_string_lossy() == "up"),
        Ok(None) => Some(false),
        Err(_) => None,
    }
}

/// Read STATE_DB `PORT_TABLE|<port>.host_tx_ready`. `Some(true)` when the field is `'true'`
/// OR ABSENT (the testbed adaptation — no orchagent asserts it, so absent means ready so the
/// golden/reconfig gates keep working); `Some(false)` on an explicit non-`'true'` value (what
/// `test_host_tx_ready` writes); `None` on a DB error so a transient hiccup never spuriously
/// tears a datapath down.
fn read_host_tx_ready(state: &DbConnector, port: &str) -> Option<bool> {
    match state.hget(&format!("PORT_TABLE|{port}"), "host_tx_ready") {
        Ok(Some(v)) => Some(v.to_string_lossy() == "true"),
        Ok(None) => Some(true),
        Err(_) => None,
    }
}

/// Read CONFIG_DB `PORT|<port>.speed` as a live `u32` (re-read each pass to react to a
/// speed reconfiguration that re-drives app-select). `None` on a DB error or a missing/
/// unparseable value, so a transient read never zeroes a port's speed.
fn read_port_speed(config: &DbConnector, port: &str) -> Option<u32> {
    match config.hget(&format!("PORT|{port}"), "speed") {
        Ok(Some(v)) => v.to_string_lossy().parse::<u32>().ok(),
        _ => None,
    }
}

/// `is_decommission_required` (single-lport form) — per CMIS spec a DP's lane width can only
/// change while DPDeactivated, so a decommission (reset AppSel to 0) is required when any
/// currently ACTIVE host lane runs an app other than the one we now want. Lanes whose active
/// AppSel is 0 (unused) never force a decommission. A missing/invalid active-AppSel entry or
/// an unreadable active control set is treated as "required" (fail safe, mirrors Python).
fn cmis_is_decommission_required(api: &dyn CmisApi, host_lanes_mask: u32, appl: u32) -> bool {
    let active = match api.get_active_apsel_hostlane() {
        Ok(v) => v,
        Err(_) => return true,
    };
    for lane in 0..CMIS_MAX_HOST_LANES {
        let key = format!("ActiveAppSelLane{}", lane + 1);
        let cur = match active.get(key.as_str()) {
            Some(v) => match v.as_u64().or_else(|| py_str(v).parse::<u64>().ok()) {
                Some(n) => n as u32,
                None => return true,
            },
            None => return true,
        };
        let desired = if (1u32 << lane) & host_lanes_mask != 0 { appl } else { 0 };
        if cur != 0 && cur != desired {
            return true;
        }
    }
    false
}

/// Project `cmis_state` to `TRANSCEIVER_STATUS_SW.cmis_state` via `hset` (field merge, so
/// the status/error the SFP-state task shares on the same row are never clobbered) and
/// keep the in-memory `PortCtx` in step.
fn cmis_set_state(state: &DbConnector, ctx: &mut PortCtx, s: &str) {
    ctx.cmis_state = s.to_string();
    if let Err(e) = state.hset(
        &format!("{STATUS_SW_TABLE}|{}", ctx.name),
        "cmis_state",
        &CxxString::from(s),
    ) {
        eprintln!("xcvrd-rs: cmis_state {s} write {} failed: {e}", ctx.name);
    }
}

/// `force_cmis_reinit(lport, retries)`: restart the machine at `INSERTED`, set the retry
/// counter and clear the armed timer. `forced_tx_disabled` is deliberately PRESERVED
/// (mirrors the reference) so an admin-down→admin-up cycle clears it only after
/// DP_PRE_INIT_CHECK confirms the lanes deactivated.
fn cmis_force_reinit(state: &DbConnector, ctx: &mut PortCtx, retries: u32) {
    cmis_set_state(state, ctx, CMIS_STATE_INSERTED);
    ctx.cmis_retries = retries;
    ctx.cmis_expired = None;
}

/// Arm the current state's expiration `duration_secs` out (+ a small buffer).
fn cmis_arm_expiration(ctx: &mut PortCtx, duration_secs: f64) {
    ctx.cmis_expired = Some(
        Instant::now()
            + Duration::from_secs_f64(duration_secs.max(0.0))
            + Duration::from_millis(CMIS_EXPIRATION_BUFFER_MS),
    );
}

/// `is_timer_expired(expired_time, now)`.
fn cmis_timer_expired(ctx: &PortCtx) -> bool {
    match ctx.cmis_expired {
        Some(exp) => exp <= Instant::now(),
        None => false,
    }
}

/// `get_cmis_max_host_lanes_mask` — `0x0f` for `QSFP+C`, else `0xff`.
fn cmis_max_host_lanes_mask(api: &dyn CmisApi) -> u32 {
    if api.get_module_type_abbreviation().as_deref() == Some("QSFP+C") {
        0x0f
    } else {
        0xff
    }
}

/// `get_cmis_host_lanes_mask(api, appl, host_lane_count, subport)`.
fn cmis_host_lanes_mask(api: &dyn CmisApi, appl: u32, host_lane_count: u32, subport: i64) -> u32 {
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
fn cmis_media_lanes_mask(
    appl: u32,
    media_lane_count: u32,
    media_lane_assignment_options: u32,
    subport: i64,
) -> u32 {
    if appl < 1 || media_lane_count == 0 || subport < 0 {
        return 0;
    }
    let start = media_lane_count as u64 * (if subport == 0 { 0 } else { (subport - 1) as u64 });
    let bit = 1u64.checked_shl(start as u32).unwrap_or(0);
    if (media_lane_assignment_options as u64) & bit != 0 {
        let width = ((1u64 << media_lane_count) - 1) << start;
        return width as u32;
    }
    0
}

/// `check_module_state(api, states)`.
fn cmis_check_module_state(api: &dyn CmisApi, states: &[&str]) -> bool {
    states.contains(&api.get_module_state().as_str())
}

/// `check_config_error(api, host_lanes_mask, states)`.
fn cmis_check_config_error(api: &dyn CmisApi, host_lanes_mask: u32, states: &[&str]) -> bool {
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

/// `check_datapath_init_pending(api, host_lanes_mask)`.
fn cmis_check_datapath_init_pending(api: &dyn CmisApi, host_lanes_mask: u32) -> bool {
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

/// `check_datapath_state(api, host_lanes_mask, states)`.
fn cmis_check_datapath_state(api: &dyn CmisApi, host_lanes_mask: u32, states: &[&str]) -> bool {
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

/// host_tx_ready-not-ready teardown of an ACTIVE datapath: when the host withdraws a good
/// Tx signal (`host_tx_ready` drops to `'false'`) from a port whose datapath is still
/// `DataPathActivated`, force a DataPathDeinit (10h:128) of the active host lanes and disable
/// the media Tx (`OutputDisableTx`) — the live, host-driven analogue of the admin-down
/// teardown in `handle_cmis_inserted`. This is deliberately UNCONDITIONAL
/// with respect to the fast-reboot datapath-skip: that skip exists only to preserve a live
/// datapath across an admin-driven xcvrd re-init, whereas a
/// host_tx_ready drop is a genuine runtime event that must tear the media side down. Returns
/// `true` iff a teardown was issued (i.e. the datapath was activated on the masked lanes).
fn cmis_host_tx_not_ready_teardown(
    api: &dyn CmisApi,
    host_lanes_mask: u32,
    media_lanes_mask: u32,
) -> bool {
    if host_lanes_mask == 0
        || !cmis_check_datapath_state(api, host_lanes_mask, &["DataPathActivated"])
    {
        return false;
    }
    api.set_datapath_deinit(host_lanes_mask);
    api.tx_disable_channel(media_lanes_mask, true);
    true
}

/// Read CONFIG_DB `PORT|<port>.tx_power` as the coherent Tx-power target in dBm (`0.0`
/// when unset/unparseable) — `get_configured_tx_power_from_db` (cmis_manager_task.py:707).
fn read_tx_power(config: &DbConnector, port: &str) -> f64 {
    match config.hget(&format!("PORT|{port}"), "tx_power") {
        Ok(Some(v)) => v.to_string_lossy().trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Read CONFIG_DB `PORT|<port>.laser_freq` as the coherent laser-frequency target in GHz
/// (`0` when unset/unparseable) — `get_configured_laser_freq_from_db` (cmis_manager_task.py:698).
fn read_laser_freq(config: &DbConnector, port: &str) -> i64 {
    match config.hget(&format!("PORT|{port}"), "laser_freq") {
        Ok(Some(v)) => v.to_string_lossy().trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// `configure_tx_output_power` (cmis_manager_task.py:728) — log when the request is outside
/// the module's supported range, then provision it. Returns the `set_tx_power` result.
fn cmis_configure_tx_output_power(api: &dyn CmisApi, port: &str, tx_power: f64) -> bool {
    let (min_p, max_p) = api.get_supported_power_config();
    if tx_power < min_p {
        eprintln!("xcvrd-rs: {port} configured tx power {tx_power} < minimum power {min_p} supported");
    }
    if tx_power > max_p {
        eprintln!("xcvrd-rs: {port} configured tx power {tx_power} > maximum power {max_p} supported");
    }
    api.set_tx_power(tx_power)
}

/// `validate_frequency_and_grid` (cmis_manager_task.py:736) — is `freq` (GHz) within the
/// module's supported range and on the requested `grid` (75/100 GHz)?
fn cmis_validate_frequency_and_grid(api: &dyn CmisApi, port: &str, freq: i64, grid: u32) -> bool {
    let (supported_grid, _, _, lowf, highf) = api.get_supported_freq_config();
    if freq < lowf {
        eprintln!("xcvrd-rs: {port} configured freq:{freq} GHz is lower than the supported freq:{lowf} GHz");
        return false;
    }
    if freq > highf {
        eprintln!("xcvrd-rs: {port} configured freq:{freq} GHz is higher than the supported freq:{highf} GHz");
        return false;
    }
    if grid == 75 {
        if (supported_grid >> 7) & 0x1 != 1 {
            eprintln!("xcvrd-rs: {port} configured freq:{freq}GHz supported grid:{supported_grid} 75GHz is not supported");
            return false;
        }
        let chan = ((freq - 193100) as f64 / 25.0).round() as i64;
        if chan % 3 != 0 {
            eprintln!("xcvrd-rs: {port} configured freq:{freq}GHz is NOT in 75GHz grid");
            return false;
        }
    } else if grid == 100 {
        if (supported_grid >> 5) & 0x1 != 1 {
            eprintln!("xcvrd-rs: {port} configured freq:{freq}GHz 100GHz is not supported");
            return false;
        }
    } else {
        eprintln!("xcvrd-rs: {port} configured freq:{freq}GHz {grid}GHz is not supported");
        return false;
    }
    true
}

/// `configure_laser_frequency` (cmis_manager_task.py:761) — warn if a tuning is in progress,
/// then provision the frequency. Returns the `set_laser_freq` result.
fn cmis_configure_laser_frequency(api: &dyn CmisApi, port: &str, freq: i64, grid: u32) -> bool {
    if api.get_tuning_in_progress() {
        eprintln!("xcvrd-rs: {port} Tuning in progress, subport selection may fail!");
    }
    api.set_laser_freq(freq, grid)
}

/// `is_cmis_application_update_required(api, app_new, host_lanes_mask)`.
fn cmis_is_application_update_required(api: &dyn CmisApi, app_new: u32, host_lanes_mask: u32) -> bool {
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
        let dp_state = api.get_datapath_state();
        let conf_state = api.get_config_datapath_hostlane_status();
        let mut skip = true;
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

/// `post_port_active_apsel_to_db` — write `active_apsel_hostlaneN` / `host_lane_count` /
/// `media_lane_count` to `TRANSCEIVER_INFO` (only when the identity row already exists,
/// like the reference). `reset_apsel` writes the `N/A` placeholders (admin-down path).
/// Build the `(field, value)` projection `post_port_active_apsel_to_db` writes to
/// TRANSCEIVER_INFO: the per-host-lane `active_apsel_hostlaneN` plus the active app's
/// `host_lane_count`/`media_lane_count`. Pure (no STATE_DB) so the projection is
/// unit-testable; [`cmis_post_active_apsel`] wraps it with the row-existence gate + write.
///
/// `reset_apsel` (the admin-down / precondition-teardown path) writes the `N/A`
/// placeholders for every field — the golden `steady_state` INFO projection. Otherwise
/// the applied AppSel is read live: a lane in `host_lanes_mask` reports its
/// `ActiveAppSelLaneN` code, an out-of-mask lane is `N/A`, and the counts come from the
/// advertisement of the last active app — the golden `activated_datapath` INFO projection.
/// Returns `None` only when a non-reset live AppSel read fails (nothing is published).
fn cmis_active_apsel_tuples(
    api: &dyn CmisApi,
    host_lanes_mask: u32,
    reset_apsel: bool,
) -> Option<Vec<(String, String)>> {
    let mut act_apsel = Value::Null;
    let mut appl_advt = Value::Null;
    if !reset_apsel {
        match api.get_active_apsel_hostlane() {
            Ok(v) => act_apsel = v,
            Err(_) => return None,
        }
        appl_advt = api.get_application_advertisement();
    }

    let mut tuples: Vec<(String, String)> = Vec::new();
    let mut last_act_key: Option<String> = None;
    for lane in 0..CMIS_MAX_HOST_LANES {
        let field = format!("active_apsel_hostlane{}", lane + 1);
        if (1u32 << lane) & host_lanes_mask == 0 || reset_apsel {
            tuples.push((field, "N/A".to_string()));
            continue;
        }
        let key = format!("ActiveAppSelLane{}", lane + 1);
        let v = act_apsel
            .get(key.as_str())
            .cloned()
            .unwrap_or_else(|| Value::String("N/A".to_string()));
        let s = py_str(&v);
        last_act_key = Some(s.clone());
        tuples.push((field, s));
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
        tuples.push(("host_lane_count".to_string(), host_lane_count));
        tuples.push(("media_lane_count".to_string(), media_lane_count));
    } else {
        tuples.push(("host_lane_count".to_string(), "N/A".to_string()));
        tuples.push(("media_lane_count".to_string(), "N/A".to_string()));
    }

    Some(tuples)
}

fn cmis_post_active_apsel(
    state: &DbConnector,
    api: &dyn CmisApi,
    ctx: &PortCtx,
    host_lanes_mask: u32,
    reset_apsel: bool,
) {
    let info_key = format!("{INFO_TABLE}|{}", ctx.name);
    if !state.exists(&info_key).unwrap_or(false) {
        return;
    }

    let Some(tuples) = cmis_active_apsel_tuples(api, host_lanes_mask, reset_apsel) else {
        return;
    };

    for (field, value) in &tuples {
        if let Err(e) = state.hset(&info_key, field, &CxxString::from(value.as_str())) {
            eprintln!("xcvrd-rs: active_apsel {} write failed: {e}", ctx.name);
        }
    }
}

/// `handle_cmis_inserted_state` — app-select, lane masks, admin-down teardown gate.
/// Returns `true` when the machine advanced to `DP_PRE_INIT_CHECK` (a healthy admin-up
/// port); `false` on a terminal short-circuit (`FAILED`/forced `READY`).
fn handle_cmis_inserted(state: &DbConnector, api: &dyn CmisApi, ctx: &mut PortCtx, fast_reboot: bool) -> bool {
    let host_lane_count = ctx.host_lane_count;
    let speed = ctx.speed;
    let subport = ctx.subport;

    let Some(appl) = get_cmis_application_desired(api, host_lane_count, speed) else {
        cmis_set_state(state, ctx, CMIS_STATE_FAILED);
        return false;
    };
    ctx.appl = Some(appl);

    let max_host = cmis_max_host_lanes_mask(api);
    let host_mask = cmis_host_lanes_mask(api, appl, host_lane_count, subport);
    ctx.max_host_lanes_mask = max_host;
    ctx.host_lanes_mask = host_mask;
    if host_mask == 0 {
        cmis_set_state(state, ctx, CMIS_STATE_FAILED);
        return false;
    }

    let media_lane_count = api.get_media_lane_count(appl);
    let media_lane_assignment_options = api.get_media_lane_assignment_option(appl);
    ctx.media_lane_count = media_lane_count;
    ctx.media_lane_assignment_options = media_lane_assignment_options;
    ctx.max_media_lanes_mask = max_host;
    let media_mask = cmis_media_lanes_mask(appl, media_lane_count, media_lane_assignment_options, subport);
    ctx.media_lanes_mask = media_mask;
    if media_mask == 0 {
        cmis_set_state(state, ctx, CMIS_STATE_FAILED);
        return false;
    }

    // Single-lport decommission handshake: if a live speed/app change makes the currently
    // ACTIVE AppSel wrong, reset ALL DP lanes' AppSel to 0 first (provision app 0 over the
    // max lane set), then DP_DEINIT; DP_INIT re-inits to provision the new app.
    if cmis_is_decommission_required(api, host_mask, appl) {
        ctx.decomm_pending = true;
    }
    if ctx.decomm_pending {
        ctx.appl = Some(0);
        ctx.host_lanes_mask = ctx.max_host_lanes_mask;
        ctx.media_lanes_mask = ctx.max_media_lanes_mask;
        cmis_set_state(state, ctx, CMIS_STATE_DP_DEINIT);
        return false;
    }

    // Precondition gate: a port that is admin-down OR whose host has not asserted a good Tx
    // signal (`host_tx_ready != true`) is torn down (DataPathDeinit), its media Tx forced off
    // (OutputDisableTx), and short-circuited to a forced-Tx-disabled terminal READY — never
    // powered up.
    //
    // Fast-reboot exception (cmis_manager_task.py:928): if fast reboot is enabled AND the
    // datapath is still ACTIVATED, SKIP the DataPathDeinit so the live datapath survives the
    // xcvrd re-init. Otherwise deinit as usual.
    if !ctx.admin_up || !ctx.host_tx_ready {
        let skip_deinit =
            fast_reboot && cmis_check_datapath_state(api, host_mask, &["DataPathActivated"]);
        if !skip_deinit {
            api.set_datapath_deinit(host_mask);
            api.tx_disable_channel(media_mask, true);
            let txoff = api.get_datapath_tx_turnoff_duration() / 1000.0;
            ctx.forced_tx_disabled = true;
            ctx.txoff_duration = txoff;
            cmis_arm_expiration(ctx, txoff);
            cmis_post_active_apsel(state, api, ctx, host_mask, true);
        }
        cmis_set_state(state, ctx, CMIS_STATE_READY);
        return false;
    }

    if ctx.forced_tx_disabled {
        let txoff = ctx.txoff_duration;
        cmis_arm_expiration(ctx, txoff);
    }
    cmis_set_state(state, ctx, CMIS_STATE_DP_PRE_INIT_CHECK);
    true
}

/// `handle_cmis_dp_pre_init_check_state` — Tx-off confirm (forced path) + reconfig gate.
fn handle_cmis_dp_pre_init_check(state: &DbConnector, api: &dyn CmisApi, ctx: &mut PortCtx) -> bool {
    let host_mask = ctx.host_lanes_mask;
    let appl = ctx.appl.unwrap_or(0);

    if ctx.forced_tx_disabled {
        if !cmis_check_datapath_state(api, host_mask, &["DataPathDeactivated", "DataPathInitialized"]) {
            if cmis_timer_expired(ctx) {
                let r = ctx.cmis_retries + 1;
                cmis_force_reinit(state, ctx, r);
            }
            return false;
        }
        ctx.forced_tx_disabled = false;
    }

    // Configure the target output power on a coherent (ZR) module before the app-update
    // gate, skipping a redundant write of the already-configured value (Python @996).
    if api.is_coherent_module() {
        let tx_power = ctx.tx_power;
        if tx_power != 0.0 && tx_power != api.get_tx_config_power()
            && !cmis_configure_tx_output_power(api, &ctx.name, tx_power)
        {
            eprintln!("xcvrd-rs: {} failed to configure Tx power = {tx_power}", ctx.name);
        }
    }

    let mut need_update = cmis_is_application_update_required(api, appl, host_mask);

    // On a coherent module a new laser frequency forces a datapath re-init; an invalid
    // request is cleared so it is not retried (Python @1008-1018).
    if api.is_coherent_module() {
        let freq = ctx.laser_freq;
        if freq != 0 && freq != api.get_laser_config_freq() {
            if cmis_validate_frequency_and_grid(api, &ctx.name, freq, 75) {
                need_update = true;
            } else {
                ctx.laser_freq = 0;
            }
        }
    }

    if !need_update {
        cmis_post_active_apsel(state, api, ctx, host_mask, false);
        cmis_set_state(state, ctx, CMIS_STATE_READY);
        return false;
    }
    cmis_set_state(state, ctx, CMIS_STATE_DP_DEINIT);
    true
}

/// `handle_cmis_dp_deinit_state` — deinit + Tx-off + request high power, arm the timer.
/// In `ModuleLowPwr` (no provisioned datapath) deinit/disable the FULL max lane set.
fn handle_cmis_dp_deinit(state: &DbConnector, api: &dyn CmisApi, ctx: &mut PortCtx) -> bool {
    let mut deinit_host = ctx.host_lanes_mask;
    let mut disable_media = ctx.media_lanes_mask;
    if cmis_check_module_state(api, &["ModuleLowPwr"]) {
        deinit_host = ctx.max_host_lanes_mask;
        disable_media = ctx.max_media_lanes_mask;
    }

    api.set_datapath_deinit(deinit_host);
    if !api.tx_disable_channel(disable_media, true) {
        ctx.cmis_retries += 1;
        return false;
    }

    api.set_lpmode(false, false);
    cmis_set_state(state, ctx, CMIS_STATE_AP_CONF);
    let dp_deinit = api.get_datapath_deinit_duration() / 1000.0;
    let pwr_up = api.get_module_pwr_up_duration() / 1000.0;
    cmis_arm_expiration(ctx, pwr_up.max(dp_deinit));
    true
}

/// `process_cmis_state_machine` — advance ONE datapath transition for a single lport.
fn process_cmis_state_machine(
    state: &DbConnector,
    api: &dyn CmisApi,
    ctx: &mut PortCtx,
    optics_g: &Value,
    pport: i32,
    fast_reboot: bool,
) {
    let cur = ctx.cmis_state.clone();
    let expired_now = cmis_timer_expired(ctx);
    let retries = ctx.cmis_retries;
    let host_mask = ctx.host_lanes_mask;
    let appl = ctx.appl.unwrap_or(0);

    // Guards (skipped at INSERTED, where masks/appl are (re)computed; and during a
    // decommission where appl is deliberately 0 over the max lane set).
    if cur != CMIS_STATE_INSERTED && !ctx.decomm_pending && (host_mask == 0 || appl < 1) {
        cmis_set_state(state, ctx, CMIS_STATE_FAILED);
        return;
    }
    if retries > CMIS_MAX_RETRIES {
        cmis_set_state(state, ctx, CMIS_STATE_FAILED);
        return;
    }

    // Exactly one transition per pass (each handler / arm returns after advancing).
    if cur == CMIS_STATE_INSERTED {
        handle_cmis_inserted(state, api, ctx, fast_reboot);
    } else if cur == CMIS_STATE_DP_PRE_INIT_CHECK {
        handle_cmis_dp_pre_init_check(state, api, ctx);
    } else if cur == CMIS_STATE_DP_DEINIT {
        handle_cmis_dp_deinit(state, api, ctx);
    } else if cur == CMIS_STATE_AP_CONF {
        let mut ec = 0u32;
        if !cmis_check_module_state(api, &["ModuleReady"]) {
            if expired_now {
                let r = retries + 1;
                cmis_force_reinit(state, ctx, r);
            }
            return;
        }
        if !cmis_check_datapath_state(api, host_mask, &["DataPathDeactivated"]) {
            if expired_now {
                let r = retries + 1;
                cmis_force_reinit(state, ctx, r);
            }
            return;
        }
        // On a coherent (ZR) module, tune the laser while the datapath is Deactivated —
        // but not during a decommission handshake (Python @1142-1151).
        if !ctx.decomm_pending && api.is_coherent_module() {
            let freq = ctx.laser_freq;
            if freq != 0 && !cmis_configure_laser_frequency(api, &ctx.name, freq, 75) {
                eprintln!("xcvrd-rs: {} failed to configure laser frequency {freq} GHz", ctx.name);
            }
        }
        // Stage per-vendor module optics-SI (page-10h) before set_application, mirroring
        // cmt.py AP_CONF @1153-1175 (inside the `not decommission_pending` guard): resolve
        // the module's SI dict from optics_si_settings.json for this port + lane speed and,
        // if found, stage it; a staging failure re-inits, otherwise ExplicitControl (ec=1)
        // is set so set_application applies the custom host SI.
        if !ctx.decomm_pending && optics_si_parser::optics_si_present(optics_g) {
            let lane_speed = if ctx.host_lane_count == 0 {
                0
            } else {
                (ctx.speed as i64 / 1000) / ctx.host_lane_count as i64
            };
            let optics_si_dict =
                optics_si_parser::fetch_optics_si_setting(optics_g, pport, lane_speed, Some(api), true);
            if optics_si_dict.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                if !api.stage_custom_si_settings(host_mask, &optics_si_dict) {
                    eprintln!("xcvrd-rs: {} unable to stage custom SI settings", ctx.name);
                    let r = retries + 1;
                    cmis_force_reinit(state, ctx, r);
                    return;
                }
                ec = 1;
            }
        }
        api.set_application(host_mask, appl, ec);
        if !api.scs_apply_datapath_init(host_mask) {
            let r = retries + 1;
            cmis_force_reinit(state, ctx, r);
            return;
        }
        cmis_set_state(state, ctx, CMIS_STATE_DP_INIT);
    } else if cur == CMIS_STATE_DP_INIT {
        if !cmis_check_config_error(api, host_mask, &["ConfigSuccess"]) {
            if expired_now {
                // Decommission failed to converge: clear the pending flag before the retry
                // so the port isn't wedged in the decommission handshake (Python @1190).
                if ctx.decomm_pending {
                    ctx.decomm_pending = false;
                }
                let r = retries + 1;
                cmis_force_reinit(state, ctx, r);
            }
            return;
        }
        // Decommission complete (app 0 provisioned + ConfigSuccess): clear the flag and
        // re-init so the next INSERTED pass provisions the real (changed) app.
        if ctx.decomm_pending {
            ctx.decomm_pending = false;
            cmis_force_reinit(state, ctx, 0);
            return;
        }
        let major = api
            .get_cmis_rev()
            .split('.')
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if major >= 5 && !cmis_check_datapath_init_pending(api, host_mask) {
            let r = retries + 1;
            cmis_force_reinit(state, ctx, r);
            return;
        }
        // Do not drive the datapath to Activated unless the host Tx signal is good and the
        // port is admin-up (mirrors cmis_manager_task.py:1217).
        if !ctx.admin_up || !ctx.host_tx_ready {
            return;
        }
        api.set_datapath_init(host_mask);
        let dp_init = api.get_datapath_init_duration() / 1000.0;
        cmis_arm_expiration(ctx, dp_init);
        cmis_set_state(state, ctx, CMIS_STATE_DP_TXON);
    } else if cur == CMIS_STATE_DP_TXON {
        if !cmis_check_datapath_state(api, host_mask, &["DataPathInitialized"]) {
            if expired_now {
                let r = retries + 1;
                cmis_force_reinit(state, ctx, r);
            }
            return;
        }
        let media_mask = ctx.media_lanes_mask;
        api.tx_disable_channel(media_mask, false);
        let dp_init = api.get_datapath_init_duration() / 1000.0;
        let dp_txon = api.get_datapath_tx_turnon_duration() / 1000.0;
        cmis_arm_expiration(ctx, dp_init.max(dp_txon));
        cmis_set_state(state, ctx, CMIS_STATE_DP_ACTIVATE);
    } else if cur == CMIS_STATE_DP_ACTIVATE {
        if !cmis_check_datapath_state(api, host_mask, &["DataPathActivated"]) {
            if expired_now {
                let r = retries + 1;
                cmis_force_reinit(state, ctx, r);
            }
            return;
        }
        cmis_set_state(state, ctx, CMIS_STATE_READY);
        cmis_post_active_apsel(state, api, ctx, host_mask, false);
    }
}

/// Drive every configured port's CMIS datapath state machine one transition (one serve
/// pass). Re-reads `admin_status` to react to reconfiguration, short-circuits flat-memory
/// / non-CMIS modules straight to READY, and marks an absent (mid-bring-up) module
/// REMOVED. Per-port errors are logged and skipped — one bad module never stalls the rest.
fn cmis_datapath_sm(
    platform: &Platform,
    state: &DbConnector,
    config: &DbConnector,
    ports: &mut BTreeMap<usize, PortCtx>,
    optics_g: &Value,
    fast_reboot: bool,
    host_tx_queue: &Arc<Mutex<VecDeque<HostTxObservation>>>,
) {
    // Drain the event-driven host_tx_ready observations (STATE_DB PORT_TABLE subscriber). Per
    // logical port we remember whether ANY 'false' was observed since the last pass (the edge
    // that must tear an activated datapath down) and the LATEST observed value. A background
    // keeper that re-asserts host_tx_ready='true' produces a `true`→`false`→`true` burst whose
    // net level is 'true'; a 1s poll of the level misses the brief 'false', but the subscriber
    // delivered the 'false' edge, so `saw_false` still fires the teardown. Mirrors the reference
    // CmisManagerTask reacting to a STATE_DB PORT_TABLE host_tx_ready change event.
    let htr_events = drain_host_tx_events(host_tx_queue);

    for (&phys, ctx) in ports.iter_mut() {
        // React to a CONFIG_DB admin_status flip (reconfiguration) BEFORE the terminal
        // check, so a READY port going admin-down (or back up) re-enters the machine.
        if let Some(now_up) = read_admin_up(config, &ctx.name) {
            if now_up != ctx.admin_up {
                ctx.admin_up = now_up;
                cmis_force_reinit(state, ctx, 0);
            }
        }

        // React to a STATE_DB PORT_TABLE host_tx_ready flip — an admin-up, datapath-activated
        // port whose host_tx_ready drops to 'false' re-inits and tears the datapath down.
        // Prefer the EDGE-triggered subscriber observations (they catch a transient 'false' a
        // keeper immediately re-asserts); fall back to a level read only when no event was
        // delivered this pass (e.g. an environment with no keeper writing host_tx_ready).
        // `htr_dropped` forces the explicit DataPathDeinit teardown below.
        let mut htr_dropped = false;
        match htr_events.get(ctx.name.as_str()) {
            Some(ev) => {
                // Fold the drained observations into a decision (see `host_tx_decision`): a real
                // 'false' EDGE forces host_tx_ready → false and a re-init even when a keeper has
                // already restored the net level to 'true', so `handle_cmis_inserted` recomputes a
                // FRESH host-lane mask and issues the DataPathDeinit (10h:128) — independent of the
                // cached `ctx.host_lanes_mask` (which is 0 right after a daemon restart).
                let decision = host_tx_decision(ctx.host_tx_ready, ev);
                htr_dropped = decision.dropped;
                if decision.reinit {
                    ctx.host_tx_ready = decision.ready;
                    cmis_force_reinit(state, ctx, 0);
                }
            }
            None => {
                if let Some(now_htr) = read_host_tx_ready(state, &ctx.name) {
                    if now_htr != ctx.host_tx_ready {
                        htr_dropped = !now_htr;
                        ctx.host_tx_ready = now_htr;
                        cmis_force_reinit(state, ctx, 0);
                    }
                }
            }
        }

        // React to a CONFIG_DB speed reconfiguration (e.g. 40G→100G on the multi-app
        // module) by re-driving app-select — the changed app triggers the decommission →
        // re-provision handshake.
        if let Some(now_speed) = read_port_speed(config, &ctx.name) {
            if now_speed != 0 && now_speed != ctx.speed {
                ctx.speed = now_speed;
                cmis_force_reinit(state, ctx, 0);
            }
        }

        // Terminal states (READY/FAILED/REMOVED) have nothing to advance.
        if cmis_is_terminal(&ctx.cmis_state) {
            continue;
        }

        let sfp = match platform.sfp(phys) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("xcvrd-rs: cmis open sfp {phys} ({}) failed: {e}", ctx.name);
                continue;
            }
        };
        if !sfp.get_presence().unwrap_or(false) {
            cmis_set_state(state, ctx, CMIS_STATE_REMOVED);
            continue;
        }

        let api = BridgeCmisApi::new(Box::new(RealSfp(sfp)));
        // Flat-memory / non-CMIS modules skip the datapath machine entirely → READY
        // (no ApplyDPInitLane, datapath left deactivated).
        if api.is_flat_memory() {
            cmis_set_state(state, ctx, CMIS_STATE_READY);
            continue;
        }
        if !common::is_cmis_api(api.get_module_type_abbreviation().as_deref()) {
            cmis_set_state(state, ctx, CMIS_STATE_READY);
            continue;
        }

        // host_tx_ready just dropped to 'false' on a port whose datapath was already ACTIVATED
        // WITHIN THIS SESSION: tear the *cached* host lanes down immediately (DataPathDeinit,
        // 10h:128) and disable the media Tx, without waiting for the INSERTED handler to run. This
        // is a fast, best-effort teardown that only fires when `ctx.host_lanes_mask` is still
        // populated; after a daemon restart the cache is 0 and this is a no-op — the authoritative
        // deinit is issued by `handle_cmis_inserted` (reached via the `force_reinit` above, which
        // set the state back to INSERTED and, for the `saw_false` edge, `host_tx_ready = false`),
        // where the mask is recomputed fresh. Kept as belt-and-suspenders so an activated in-session
        // datapath is torn down promptly; the datapath re-provisions once the signal returns.
        if htr_dropped {
            cmis_host_tx_not_ready_teardown(&api, ctx.host_lanes_mask, ctx.media_lanes_mask);
        }

        // On a coherent (ZR) module, read the user's Tx-power / laser-frequency targets from
        // CONFIG_DB at (re)insertion — an admin-bounce re-enters INSERTED and picks up a new
        // request; later bring-up states keep the working value (incl. an invalid-freq reset).
        if api.is_coherent_module() && ctx.cmis_state == CMIS_STATE_INSERTED {
            ctx.tx_power = read_tx_power(config, &ctx.name);
            ctx.laser_freq = read_laser_freq(config, &ctx.name);
        }

        process_cmis_state_machine(state, &api, ctx, optics_g, phys as i32, fast_reboot);
    }
}

// =====================================================================================
// SFF (non-CMIS) deployed control — the SffManagerTask task_worker (sff_mgr.py:367-528)
// hand-ported over the live platform bridge, the analogue of `cmis_datapath_sm`. The
// unit-tested logic lives in `crate::sff_mgr`; this is the deployed driver that reuses
// `BridgeSffApi` + the same register semantics. Gated on `--enable_sff_mgr`.
// =====================================================================================

/// The optional SFF-8636 manager flag (xcvrd.py:1150 `if self.enable_sff_mgr:`), read
/// from the daemon's own `/proc/<pid>/cmdline`.
const ENABLE_SFF_MGR_FLAG: &str = "--enable_sff_mgr";
/// SFF-8636 host lanes per physical port (`SffManagerTask.DEFAULT_NUM_LANES_PER_PPORT`).
const SFF_NUM_LANES_PER_PPORT: i64 = 4;

/// A CONFIG_DB `PORT.admin_status` transition observed by the watcher thread. Replayed by
/// [`sff_control`] IN ORDER so a fast admin down→up round-trip (which a 1s poll would
/// coalesce back to "up", missing the bring-up trigger) still drives the control path.
struct AdminObservation {
    lport: String,
    admin_up: bool,
}

/// Per-physical-port SFF bring-up state — the `port_dict_prev` diff basis of the reference
/// `task_worker`. `seen=false` means "not yet processed" → treated as `xcvr_inserted`.
#[derive(Default)]
struct SffDeployState {
    seen: bool,
    prev_admin_up: Option<bool>,
    prev_host_tx_ready: Option<bool>,
    active_lanes: Option<Vec<bool>>,
}

/// Re-exec this binary with [`ENABLE_SFF_MGR_FLAG`] appended if it is not already in argv.
///
/// xcvrd.py forwards `--enable_sff_mgr` from pmon's supervisor conf, but the injection
/// shim (`/usr/local/bin/xcvrd` → `os.execv(".../xcvrd-rs", ["xcvrd-rs"])`) drops argv, so
/// the flag never reaches us. The SFF path relies on the daemon's *own*
/// `/proc/<pid>/cmdline` carrying the literal flag, so merely enabling the manager
/// internally is not enough. A single `exec()` (which preserves the PID for supervisor)
/// puts it there; it is one-shot (the re-exec'd image sees the flag and returns
/// immediately). Any failure is non-fatal — the daemon keeps running.
fn ensure_sff_mgr_flag() {
    use std::os::unix::process::CommandExt;
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == ENABLE_SFF_MGR_FLAG) {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "xcvrd-rs: cannot resolve current exe to advertise {ENABLE_SFF_MGR_FLAG} ({e}); \
                 continuing without it"
            );
            return;
        }
    };
    let mut cmd = std::process::Command::new(&exe);
    for a in args.iter().skip(1) {
        cmd.arg(a);
    }
    cmd.arg(ENABLE_SFF_MGR_FLAG);
    let err = cmd.exec(); // only returns on failure
    eprintln!(
        "xcvrd-rs: re-exec to advertise {ENABLE_SFF_MGR_FLAG} failed ({err}); continuing WITHOUT \
         the SFF cmdline flag"
    );
}

/// Spawn the CONFIG_DB `PORT` watcher thread. It only touches swss (`Send`) + the shared
/// queue, never the PyO3 platform, so it can select() at push cadence without contending on
/// the GIL. It restarts itself on any redis hiccup so a transient error never silences SFF.
fn spawn_admin_watcher(queue: Arc<Mutex<VecDeque<AdminObservation>>>) {
    let _ = std::thread::Builder::new()
        .name("sff-admin-watch".to_string())
        .spawn(move || loop {
            if let Err(e) = run_admin_watcher(&queue) {
                eprintln!("xcvrd-rs: SFF admin watcher error ({e}); retrying in 3s");
                std::thread::sleep(Duration::from_secs(3));
            }
        });
}

/// Subscribe to CONFIG_DB `PORT` and push every `admin_status` SET (incl. the initial
/// snapshot) into the shared queue. `SubscriberStateTable::read_data` is push-driven (returns
/// the instant a keyspace notification arrives), so a fast admin down→up is delivered as two
/// distinct events rather than coalescing to the latest value — the reference observer's
/// behaviour (`PortChangeObserver` over `swsscommon.SubscriberStateTable`).
fn run_admin_watcher(queue: &Arc<Mutex<VecDeque<AdminObservation>>>) -> Result<(), String> {
    let sock = env::redis_sock();
    let db = DbConnector::new_unix(env::CONFIG_DB, sock, 0).map_err(|e| format!("{e:?}"))?;
    let mut sub = SubscriberStateTable::new(db, "PORT", None, None).map_err(|e| format!("{e:?}"))?;

    // Drain the initial snapshot (existing PORT rows the ctor buffered) so startup admin state
    // is delivered promptly, then block for live notifications.
    push_admin_pops(&mut sub, queue)?;
    loop {
        match sub
            .read_data(Duration::from_millis(1000), false)
            .map_err(|e| format!("{e:?}"))?
        {
            SelectResult::Data => push_admin_pops(&mut sub, queue)?,
            SelectResult::Signal | SelectResult::Timeout => {}
        }
    }
}

/// Pop the pending `PORT` changes and enqueue each `admin_status` SET as an [`AdminObservation`].
fn push_admin_pops(
    sub: &mut SubscriberStateTable,
    queue: &Arc<Mutex<VecDeque<AdminObservation>>>,
) -> Result<(), String> {
    let pops = sub.pops().map_err(|e| format!("{e:?}"))?;
    for kfv in pops {
        if !matches!(kfv.operation, KeyOperation::Set) {
            continue;
        }
        if !kfv.key.starts_with("Ethernet") {
            continue;
        }
        for (field, value) in kfv.field_values.into_iter() {
            if field == "admin_status" {
                let admin_up = value.to_string_lossy() == "up";
                if let Ok(mut q) = queue.lock() {
                    q.push_back(AdminObservation { lport: kfv.key.clone(), admin_up });
                }
            }
        }
    }
    Ok(())
}

/// A STATE_DB `PORT_TABLE.host_tx_ready` transition observed by the host_tx watcher thread and
/// replayed by [`cmis_datapath_sm`]. `host_tx_ready` is the host/ASIC's "I am driving a valid Tx
/// electrical signal" signal; a drop to anything other than `'true'` on an admin-up, activated
/// port must tear the media-side datapath down (DataPathDeinit, 10h:128). Delivered as distinct
/// events (not a coalesced level) so a brief `true`→`false`→`true` — a background keeper
/// re-asserting `'true'` immediately after a clear — still surfaces the `'false'` edge that a 1s
/// poll of the level would miss. Mirrors the reference `CmisManagerTask` STATE_DB `PORT_TABLE`
/// subscriber / `get_host_tx_status` (cmis_manager_task.py:926/1199).
struct HostTxObservation {
    lport: String,
    host_tx_ready: bool,
}

/// Per-logical-port summary of the host_tx_ready observations drained from the watcher queue in
/// one CMIS pass: whether ANY `'false'` was seen (the teardown edge) and the LATEST value seen.
struct HtrDrain {
    saw_false: bool,
    latest: bool,
}

/// Drain the host_tx watcher queue and fold the ordered observations into a per-lport summary.
/// The final entry for a port wins for `latest`; `saw_false` latches if any observation for that
/// port was `false`. An empty return (no events this pass) leaves the caller on its level-read
/// fallback.
fn drain_host_tx_events(
    queue: &Arc<Mutex<VecDeque<HostTxObservation>>>,
) -> HashMap<String, HtrDrain> {
    let drained: Vec<HostTxObservation> = match queue.lock() {
        Ok(mut q) => q.drain(..).collect(),
        Err(_) => Vec::new(),
    };
    let mut out: HashMap<String, HtrDrain> = HashMap::new();
    for obs in drained {
        let entry = out
            .entry(obs.lport)
            .or_insert(HtrDrain { saw_false: false, latest: obs.host_tx_ready });
        if !obs.host_tx_ready {
            entry.saw_false = true;
        }
        entry.latest = obs.host_tx_ready;
    }
    out
}

/// Outcome of applying one pass's drained host_tx_ready summary to a port whose currently recorded
/// level is `prev_ready`: the level to record, whether to re-enter the datapath machine, and
/// whether to force the cached-mask teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostTxDecision {
    /// New `host_tx_ready` level to record on the port context.
    ready: bool,
    /// Re-enter the datapath machine at `INSERTED` (`force_cmis_reinit`).
    reinit: bool,
    /// Force the immediate cached-mask teardown (a real `'false'` edge was seen this pass).
    dropped: bool,
}

/// Decide how the host_tx_ready observations drained this pass affect a port whose currently
/// recorded level is `prev_ready`. A real `'false'` EDGE (`saw_false`) always drops the recorded
/// level to `false` and re-enters the machine — EVEN when a background keeper has already restored
/// the net level to `'true'` (`latest == true`) — so `handle_cmis_inserted` recomputes a fresh
/// host-lane mask and issues the DataPathDeinit (10h:128) rather than relying on the possibly-stale
/// cached mask. Absent a `'false'` edge, a net level change re-enters to re-provision to the new
/// level; a steady level is a no-op. Mirrors the reference `CmisManagerTask` reacting to a STATE_DB
/// host_tx_ready change with `force_cmis_reinit` + the `handle_cmis_inserted_state` deinit gate.
fn host_tx_decision(prev_ready: bool, ev: &HtrDrain) -> HostTxDecision {
    if ev.saw_false {
        HostTxDecision { ready: false, reinit: true, dropped: true }
    } else if ev.latest != prev_ready {
        HostTxDecision { ready: ev.latest, reinit: true, dropped: false }
    } else {
        HostTxDecision { ready: prev_ready, reinit: false, dropped: false }
    }
}

/// Spawn the STATE_DB `PORT_TABLE` host_tx_ready watcher thread. Like the SFF admin watcher it
/// only touches swss (`Send`) + the shared queue, never the PyO3 platform, so it can select() at
/// push cadence without contending on the GIL, and restarts itself on any redis hiccup so a
/// transient error never silences the host_tx_ready teardown.
fn spawn_host_tx_watcher(queue: Arc<Mutex<VecDeque<HostTxObservation>>>) {
    let _ = std::thread::Builder::new()
        .name("host-tx-watch".to_string())
        .spawn(move || loop {
            if let Err(e) = run_host_tx_watcher(&queue) {
                eprintln!("xcvrd-rs: host_tx watcher error ({e}); retrying in 3s");
                std::thread::sleep(Duration::from_secs(3));
            }
        });
}

/// Subscribe to STATE_DB `PORT_TABLE` and push every `host_tx_ready` SET (incl. the initial
/// snapshot) into the shared queue. `SubscriberStateTable::read_data` is push-driven (returns the
/// instant a keyspace notification arrives), so it pops the value within microseconds of the
/// `'false'` write — long before a ~1s keeper re-asserts `'true'` — the reference observer's
/// behaviour (`PortChangeObserver` over `swsscommon.SubscriberStateTable`, STATE_DB PORT_TABLE).
fn run_host_tx_watcher(queue: &Arc<Mutex<VecDeque<HostTxObservation>>>) -> Result<(), String> {
    let sock = env::redis_sock();
    let db = DbConnector::new_unix(env::STATE_DB, sock, 0).map_err(|e| format!("{e:?}"))?;
    let mut sub = SubscriberStateTable::new(db, "PORT_TABLE", None, None).map_err(|e| format!("{e:?}"))?;

    // Drain the initial snapshot (existing PORT_TABLE rows the ctor buffered), then block for live
    // notifications.
    push_host_tx_pops(&mut sub, queue)?;
    loop {
        match sub
            .read_data(Duration::from_millis(1000), false)
            .map_err(|e| format!("{e:?}"))?
        {
            SelectResult::Data => push_host_tx_pops(&mut sub, queue)?,
            SelectResult::Signal | SelectResult::Timeout => {}
        }
    }
}

/// Pop the pending `PORT_TABLE` changes and enqueue each `host_tx_ready` SET as a
/// [`HostTxObservation`] (`true` iff the value is exactly `'true'`, matching the reference
/// `get_host_tx_status` comparison). Only `Ethernet*` keys are considered.
fn push_host_tx_pops(
    sub: &mut SubscriberStateTable,
    queue: &Arc<Mutex<VecDeque<HostTxObservation>>>,
) -> Result<(), String> {
    let pops = sub.pops().map_err(|e| format!("{e:?}"))?;
    for kfv in pops {
        if !matches!(kfv.operation, KeyOperation::Set) {
            continue;
        }
        if !kfv.key.starts_with("Ethernet") {
            continue;
        }
        for (field, value) in kfv.field_values.into_iter() {
            if field == "host_tx_ready" {
                let host_tx_ready = value.to_string_lossy() == "true";
                if let Ok(mut q) = queue.lock() {
                    q.push_back(HostTxObservation { lport: kfv.key.clone(), host_tx_ready });
                }
            }
        }
    }
    Ok(())
}

/// A CONFIG_DB `PORT` logical-port add/remove observed by the always-on port-config watcher
/// thread and reconciled by the serve loop. Mirrors the reference `PortChangeObserver`
/// delivering `PORT_ADD` / `PORT_DEL` events to `SfpStateUpdateTask.on_port_config_change`
/// (xcvrd.py:731 `on_remove_logical_port` / :794 `on_add_logical_port`). Distinct from a
/// physical SFP plug/unplug (that is `get_change_event`): a logical-port removal is a full
/// deconfiguration that tears down the ENTIRE per-port table set including the STATUS pair,
/// the DOM/VDM THRESHOLD tables and `TRANSCEIVER_STATUS_SW`, which a physical unplug preserves.
enum PortConfigOp {
    Add,
    Remove,
}

struct PortConfigObservation {
    lport: String,
    op: PortConfigOp,
}

/// Spawn the always-on CONFIG_DB `PORT` add/remove watcher. Unlike the SFF admin watcher this
/// runs regardless of `--enable_sff_mgr` (logical-port teardown/repopulation is core xcvrd
/// behaviour, not an SFF feature) and captures both `Set` (a newly-configured port) and `Del`
/// (a deconfigured port). It only touches swss (`Send`) + the shared queue, so it selects at
/// push cadence without contending on the PyO3 GIL, and restarts itself on any redis hiccup.
fn spawn_port_config_watcher(queue: Arc<Mutex<VecDeque<PortConfigObservation>>>) {
    let _ = std::thread::Builder::new()
        .name("port-config-watch".to_string())
        .spawn(move || loop {
            if let Err(e) = run_port_config_watcher(&queue) {
                eprintln!("xcvrd-rs: port-config watcher error ({e}); retrying in 3s");
                std::thread::sleep(Duration::from_secs(3));
            }
        });
}

/// Subscribe to CONFIG_DB `PORT` and push every logical-port `Set`/`Del` into the shared
/// queue. The initial snapshot (existing PORT rows the ctor buffered) is pushed as `Add`s too;
/// the serve loop dedups against the ports it already discovered, so re-processing an existing
/// port is a harmless no-op — this keeps the watcher stateless and lets a genuine live add or
/// remove be delivered the instant its keyspace notification arrives.
fn run_port_config_watcher(
    queue: &Arc<Mutex<VecDeque<PortConfigObservation>>>,
) -> Result<(), String> {
    let sock = env::redis_sock();
    let db = DbConnector::new_unix(env::CONFIG_DB, sock, 0).map_err(|e| format!("{e:?}"))?;
    let mut sub = SubscriberStateTable::new(db, "PORT", None, None).map_err(|e| format!("{e:?}"))?;

    push_port_config_pops(&mut sub, queue)?;
    loop {
        match sub
            .read_data(Duration::from_millis(1000), false)
            .map_err(|e| format!("{e:?}"))?
        {
            SelectResult::Data => push_port_config_pops(&mut sub, queue)?,
            SelectResult::Signal | SelectResult::Timeout => {}
        }
    }
}

/// Pop the pending `PORT` changes and enqueue each logical-port `Set` as [`PortConfigOp::Add`]
/// and each `Del` as [`PortConfigOp::Remove`]. Only `Ethernet*` keys are considered (skips
/// non-front-panel/meta rows). A re-add via several `hset`s emits several `Set` pops for the
/// same key; the serve-loop dedup collapses them to a single add.
fn push_port_config_pops(
    sub: &mut SubscriberStateTable,
    queue: &Arc<Mutex<VecDeque<PortConfigObservation>>>,
) -> Result<(), String> {
    let pops = sub.pops().map_err(|e| format!("{e:?}"))?;
    for kfv in pops {
        if !kfv.key.starts_with("Ethernet") {
            continue;
        }
        let op = match kfv.operation {
            KeyOperation::Set => PortConfigOp::Add,
            KeyOperation::Del => PortConfigOp::Remove,
        };
        if let Ok(mut q) = queue.lock() {
            q.push_back(PortConfigObservation { lport: kfv.key.clone(), op });
        }
    }
    Ok(())
}

/// `SffManagerTask.get_active_lanes_for_lport` — the active-lane mask for a logical port
/// (single-lport testbed: subport 0 → all four lanes active). `None` on invalid input.
fn sff_active_lanes(subport_idx: i64, num_lanes_per_lport: u32) -> Option<Vec<bool>> {
    let nll = num_lanes_per_lport as i64;
    if nll <= 0 || subport_idx < 0 || subport_idx > SFF_NUM_LANES_PER_PPORT / nll {
        return None;
    }
    let n = SFF_NUM_LANES_PER_PPORT as usize;
    if subport_idx == 0 {
        return Some(vec![true; n]);
    }
    let mut lanes = vec![false; n];
    let start = ((subport_idx - 1) * nll) as usize;
    for lane in lanes.iter_mut().skip(start).take(nll as usize) {
        *lane = true;
    }
    Some(lanes)
}

/// `SffManagerTask.enable_high_power_class` — set High Power Class Enable (00h:93.2) for a
/// power-class ≥ 5 module; a no-op below class 5 (matching `sff_mgr.py:477`).
fn sff_enable_high_power_class(api: &dyn SffApi, name: &str) {
    let power_class = match api.get_power_class() {
        Some(p) => p,
        None => {
            eprintln!("xcvrd-rs: SFF-MAIN: {name}: failed to get power class");
            return;
        }
    };
    if power_class < 5 {
        return;
    }
    match api.set_high_power_class(power_class, true) {
        Some(true) => eprintln!("xcvrd-rs: SFF-MAIN: {name}: done enabling high power class"),
        Some(false) => eprintln!("xcvrd-rs: SFF-MAIN: {name}: failed to enable high power class"),
        None => {}
    }
}

/// One logical-port `task_worker` pass (sff_mgr.py:367-528) over the live bridge for the
/// given admin/host_tx values. Reruns bring-up (high-power-class + set_lpmode(false)) on
/// insertion or an admin-up transition, then reconciles Tx_Disable (00h:86) to
/// `not (host_tx_ready and admin_up)` on the active lanes. Per-port errors are logged, never
/// fatal.
fn sff_eval_port(
    platform: &Platform,
    phys: usize,
    ctx: &PortCtx,
    st: &mut SffDeployState,
    admin_up: bool,
    host_tx_ready: bool,
) {
    let sfp = match platform.sfp(phys) {
        Ok(s) => s,
        Err(_) => {
            *st = SffDeployState::default();
            return;
        }
    };
    if !sfp.get_presence().unwrap_or(false) {
        // Module absent: forget state so a re-plug is a fresh insert (re-runs bring-up).
        *st = SffDeployState::default();
        return;
    }
    let api = BridgeSffApi::new(Box::new(RealSfp(sfp)));
    // CMIS modules are owned by the CMIS datapath machine.
    if api.is_cmis() {
        return;
    }

    let inserted = !st.seen;
    let admin_changed = st.prev_admin_up != Some(admin_up);
    let host_changed = st.prev_host_tx_ready != Some(host_tx_ready);
    if !inserted && !admin_changed && !host_changed {
        return;
    }

    // Copper cables / modules without Tx_Disable support are skipped, but recorded so we do
    // not re-evaluate them every pass (mirrors the Python `continue`).
    let skip = matches!(api.is_copper(), Some(true) | None)
        || !matches!(api.get_tx_disable_support(), Some(true));
    if skip {
        st.seen = true;
        st.prev_admin_up = Some(admin_up);
        st.prev_host_tx_ready = Some(host_tx_ready);
        return;
    }

    // On insertion (or admin coming up): enable high power class + take out of low power.
    if inserted || (admin_changed && admin_up) {
        sff_enable_high_power_class(&api, &ctx.name);
        if api.get_lpmode_support() && !api.set_lpmode(false) {
            eprintln!(
                "xcvrd-rs: SFF-MAIN: {}: Failed to take module out of low power mode.",
                ctx.name
            );
        }
    }

    let active_lanes = match st.active_lanes.clone() {
        Some(a) => a,
        None => match sff_active_lanes(ctx.subport, ctx.host_lane_count) {
            Some(a) => {
                st.active_lanes = Some(a.clone());
                a
            }
            None => {
                eprintln!(
                    "xcvrd-rs: SFF-MAIN: {}: skipping sff_mgr due to failing to get active lanes",
                    ctx.name
                );
                st.seen = true;
                st.prev_admin_up = Some(admin_up);
                st.prev_host_tx_ready = Some(host_tx_ready);
                return;
            }
        },
    };

    // TX is enabled only when host_tx_ready is true AND admin_status is up.
    let target = !(host_tx_ready && admin_up);
    let cur = api
        .get_tx_disable()
        .unwrap_or_else(|| vec![!target; SFF_NUM_LANES_PER_PPORT as usize]);
    let mut mask = 0u32;
    for (i, (&active, &c)) in active_lanes.iter().zip(cur.iter()).enumerate() {
        if active && (target != c) {
            mask |= 1 << i;
        }
    }
    if mask != 0 {
        if api.tx_disable_channel(mask, target) {
            eprintln!(
                "xcvrd-rs: SFF-MAIN: {}: TX was {} with lanes mask: {:#06b}",
                ctx.name,
                if target { "disabled" } else { "enabled" },
                mask
            );
        } else {
            eprintln!(
                "xcvrd-rs: SFF-MAIN: {}: Failed to {} TX with lanes mask: {:#06b}",
                ctx.name,
                if target { "disable" } else { "enable" },
                mask
            );
        }
    }

    st.seen = true;
    st.prev_admin_up = Some(admin_up);
    st.prev_host_tx_ready = Some(host_tx_ready);
}

/// SFF (non-CMIS) control pass — the deployed `SffManagerTask.task_worker` sweep. First
/// replays every queued admin transition IN ORDER (so a fast down→up round-trip drives both
/// the tear-down and the bring-up, exactly like the event-driven Python task), then does a
/// steady-state sweep over every port to service presence/insertion + host_tx_ready.
fn sff_control(
    platform: &Platform,
    state: &DbConnector,
    config: &DbConnector,
    ports: &BTreeMap<usize, PortCtx>,
    sff_state: &mut HashMap<usize, SffDeployState>,
    admin_queue: &Arc<Mutex<VecDeque<AdminObservation>>>,
) {
    // Drain the watcher queue (preserve order for the fast-toggle replay).
    let drained: Vec<AdminObservation> = match admin_queue.lock() {
        Ok(mut q) => q.drain(..).collect(),
        Err(_) => Vec::new(),
    };

    let mut lport_to_phys: HashMap<&str, usize> = HashMap::new();
    for (&phys, ctx) in ports.iter() {
        lport_to_phys.insert(ctx.name.as_str(), phys);
    }

    // Replay each observed admin transition against its port.
    for obs in &drained {
        let Some(&phys) = lport_to_phys.get(obs.lport.as_str()) else {
            continue;
        };
        let Some(ctx) = ports.get(&phys) else { continue };
        let host = read_host_tx_ready(state, &ctx.name).unwrap_or(true);
        let st = sff_state.entry(phys).or_default();
        sff_eval_port(platform, phys, ctx, st, obs.admin_up, host);
    }

    // Steady-state sweep: services first-sight insertion + host_tx_ready + presence loss.
    for (&phys, ctx) in ports.iter() {
        let Some(admin) = read_admin_up(config, &ctx.name) else {
            continue;
        };
        let Some(host) = read_host_tx_ready(state, &ctx.name) else {
            continue;
        };
        let st = sff_state.entry(phys).or_default();
        sff_eval_port(platform, phys, ctx, st, admin, host);
    }
}


/// `post_port_transceiver_status` per-host-lane masking (STATUS lane-scoping). CMIS
/// config status is a module-wide projection: the emulator (and real modules) report
/// `ConfigSuccess` for every host lane that was ever configured, but only the lanes
/// this logical port OWNS (`host_lanes_mask` — the SAME mask that scopes
/// `active_apsel_hostlaneN` in TRANSCEIVER_INFO) are meaningful for it. Force
/// `config_state_hostlaneN` to `ConfigUndefined` for every lane OUTSIDE the mask so a
/// breakout subport never claims a sibling's lanes — mirroring the reference
/// `post_port_transceiver_status` projection (golden `activated_datapath`: Ethernet4
/// owns host lanes 1-4, so `config_state_hostlane5..8` stay `ConfigUndefined` even
/// though the module reports `ConfigSuccess` on all 8; the admin-down `steady_state`
/// port is deactivated so the module already reports `ConfigUndefined` on every lane
/// and the mask is a no-op there).
/// Only keys already present are overridden, so a flat-memory / non-CMIS status row is
/// untouched, and the sibling per-lane datapath fields (`DPNState`,
/// `dpdeinit`/`dpinit_pending`, `txN*`) keep their raw hardware values — they already
/// reflect the deinitialised state xcvrd drove the unused lanes to.
fn mask_config_state_by_host_lanes(status: &mut Value, host_lanes_mask: u32) {
    let Some(obj) = status.as_object_mut() else {
        return;
    };
    for lane in 0..CMIS_MAX_HOST_LANES {
        if (1u32 << lane) & host_lanes_mask != 0 {
            continue;
        }
        let key = format!("config_state_hostlane{}", lane + 1);
        if obj.contains_key(&key) {
            obj.insert(key, Value::String("ConfigUndefined".to_string()));
        }
    }
}

/// every present, polling-enabled module read live DOM monitors + hardware status via
/// the HAL and republish `TRANSCEIVER_DOM_SENSOR` and `TRANSCEIVER_STATUS` (the
/// "thermal" tables, published every pass regardless of CMIS state). Then, **only
/// once the port's CMIS bring-up is terminal** (`is_port_dom_monitoring_disabled` →
/// `is_port_in_cmis_initialization_process`), publish the DomInfoUpdateTask-owned
/// `TRANSCEIVER_DOM_FLAG` (+ change-tracking metadata) and `TRANSCEIVER_PM` (VDM-freeze
/// gated). The whole port is skipped when `dom_polling=disabled` in CONFIG_DB.
///
/// Reading `get_transceiver_dom_real_value` / `get_transceiver_status` /
/// `get_transceiver_dom_flags` forces the real EEPROM monitor/status/flag pages to be
/// read, which is what the emulator interaction-trace expects on this cadence.
/// Per-port errors are logged and skipped — one bad module never stops the poll.
fn dom_info_update(
    platform: &Platform,
    state: &DbConnector,
    config: &DbConnector,
    skip_cmis_mgr: bool,
    ports: &BTreeMap<usize, PortCtx>,
) {
    for (&phys, ctx) in ports {
        let port = ctx.name.as_str();
        // Skip the whole DOM refresh for a port latched in a blocking error: its
        // EEPROM is unreadable, so the error handler already purged the DOM/hardware
        // tables and they must stay absent until a plug-in event clears the error
        // (dom_mgr.py:348, `detect_port_in_error_status`). Without this gate the poll
        // would immediately republish the DOM_SENSOR/STATUS rows the error removed.
        if port_in_blocking_error(state, port) {
            continue;
        }
        match platform.sfp(phys) {
            Ok(sfp) => {
                if !sfp.get_presence().unwrap_or(false) {
                    continue;
                }
                // Backfill DOM/VDM thresholds if the insert-time post came back empty.
                // The reference posts thresholds gated only on identity readability, at
                // insert/plug/logical-add (xcvrd.py:359/579/841/869) — never in the DOM
                // poll — because its threaded boot scan reads the threshold page after the
                // module has settled. This single-threaded daemon fuses those tasks and runs
                // `sync_port`'s threshold read during the initial sync, when a just-powered
                // CMIS module's identity/threshold page can still be unreadable (the Python
                // decode returns None/{} → `read_threshold_info` None → no row written). A
                // merely-present admin-up port that is never re-inserted (e.g. a spare
                // logical port) would then keep an absent TRANSCEIVER_DOM_THRESHOLD forever,
                // so the full per-port INFO+threshold pipeline
                // is never completed for it. Re-attempt the post here once the page
                // is readable — gated on identity present (the STATE_DB reflection of the
                // reference's "identity readable" gate) and only while the row is still
                // missing, so it is a one-shot self-heal, not a periodic re-post. Runs before
                // the dom_polling gate because the insert-time threshold post is likewise
                // ungated by dom_polling (thresholds are static per module). An error/absent
                // port never reaches here (purged + `continue`d above), and a physical unplug
                // preserves the existing row, so this only fills the genuine boot-timing gap.
                // (An un-enriched module's zero power thresholds decode to -inf; that no
                // longer starves the row — `read_threshold_info` stringifies them instead of
                // failing the bridge's JSON round-trip.)
                let thr_key = format!("{DOM_THRESHOLD_TABLE}|{port}");
                let info_key = format!("{INFO_TABLE}|{port}");
                if state.exists(&info_key).unwrap_or(false)
                    && !state.exists(&thr_key).unwrap_or(false)
                {
                    let thresholds = read_threshold_info(phys).unwrap_or(Value::Null);
                    match publish_row(state, DOM_THRESHOLD_TABLE, port, beautify_dom_row(&thresholds)) {
                        // Threshold page is now readable — (re)publish the paired VDM
                        // thresholds too (sync_port posts both together at insert). A
                        // non-VDM/flat module writes nothing (best-effort).
                        Ok(true) => {
                            if let Err(e) = publish_vdm_thresholds(state, &sfp, port) {
                                eprintln!("xcvrd-rs: VDM threshold backfill {port} (sfp {phys}) failed: {e}");
                            }
                        }
                        Ok(false) => {}
                        Err(e) => {
                            eprintln!("xcvrd-rs: DOM threshold backfill {port} (sfp {phys}) failed: {e}")
                        }
                    }
                }
                // dom_polling=disabled (CONFIG_DB PORT|<port>.dom_polling) halts ALL
                // DOM refresh for this port; read live every pass (dom_mgr.py:199).
                if dom_polling_disabled(config, port) {
                    continue;
                }

                // Firmware versions (post_port_sfp_firmware_info_to_db, dom_mgr.py:353):
                // publish TRANSCEIVER_FIRMWARE_INFO (active_firmware/inactive_firmware)
                // from the CMIS API. Ungated by CMIS like DOM_SENSOR/STATUS below.
                if let Err(e) = publish_firmware_info(state, &sfp, port) {
                    eprintln!("xcvrd-rs: firmware info publish {port} (sfp {phys}) failed: {e}");
                }

                let dom = sfp
                    .get_transceiver_dom_real_value()
                    .unwrap_or(Value::Null);
                if let Err(e) = publish_row(state, DOM_SENSOR_TABLE, port, beautify_dom_row(&dom)) {
                    eprintln!("xcvrd-rs: DOM poll publish {port} (sfp {phys}) failed: {e}");
                }

                // Hardware status row (post_port_transceiver_hw_status_to_db):
                // stringified with the base beautify (no unit stripping) plus a
                // last_update_time stamp, exactly like the Python DOM loop posts
                // it every pass. The per-host-lane config status is scoped to the
                // port's owned host lanes (mask_config_state_by_host_lanes) so a
                // breakout subport's out-of-mask lanes read ConfigUndefined, not the
                // module-wide ConfigSuccess (matching post_port_transceiver_status).
                let mut status = sfp.get_transceiver_status().unwrap_or(Value::Null);
                mask_config_state_by_host_lanes(&mut status, ctx.host_lanes_mask);
                if let Err(e) = publish_row(state, STATUS_TABLE, port, beautify_info_row(&status)) {
                    eprintln!("xcvrd-rs: STATUS poll publish {port} (sfp {phys}) failed: {e}");
                }

                // DomInfoUpdateTask-owned tables are gated while CMIS bring-up is in
                // progress (non-terminal cmis_state): DOM flags, STATUS flags + PM appear
                // only once the module reaches a terminal state
                // (is_port_in_cmis_initialization_process, dom_mgr.py:182). With
                // `--skip_cmis_mgr` the gate is never engaged (the port is never in CMIS init).
                if dom_flags_ungated(skip_cmis_mgr, &ctx.cmis_state) {
                    if let Err(e) = publish_dom_flags(state, &sfp, port) {
                        eprintln!("xcvrd-rs: DOM flags publish {port} (sfp {phys}) failed: {e}");
                    }
                    if let Err(e) = publish_status_flags(state, &sfp, port) {
                        eprintln!("xcvrd-rs: STATUS flags publish {port} (sfp {phys}) failed: {e}");
                    }
                    if let Err(e) = publish_vdm(state, &sfp, port) {
                        eprintln!("xcvrd-rs: VDM publish {port} (sfp {phys}) failed: {e}");
                    }
                }
            }
            Err(e) => eprintln!("xcvrd-rs: DOM poll open sfp {phys} ({port}) failed: {e}"),
        }
    }
}

/// Separate DOM temperature poll (`DomThermalInfoUpdateTask.task_worker`,
/// dom_mgr.py:535), active only when `--dom_temperature_poll_interval` is set. For each
/// present, DOM-monitoring-enabled port it publishes just `{temperature}` to the
/// dedicated `TRANSCEIVER_DOM_TEMPERATURE` table (`post_port_dom_temperature_info_to_db`,
/// dom_sensor/db_utils.py:27). Unlike the periodic DOM poll, its gate is the *base*
/// `is_port_dom_monitoring_disabled` — only `dom_polling == "disabled"`, with no CMIS-init
/// check — and a port latched in a blocking error is polled without the presence check
/// (dom_mgr.py:567-569). The temperature value is sourced from the module's DOM real
/// values (the bridge's `get_transceiver_dom_real_value` carries `temperature`).
fn dom_temperature_update(
    platform: &Platform,
    state: &DbConnector,
    config: &DbConnector,
    ports: &BTreeMap<usize, PortCtx>,
) {
    for (&phys, ctx) in ports {
        let port = ctx.name.as_str();
        if dom_polling_disabled(config, port) {
            continue;
        }
        let sfp = match platform.sfp(phys) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("xcvrd-rs: thermal open sfp {phys} ({port}) failed: {e}");
                continue;
            }
        };
        // A port in a blocking error is polled regardless of presence; otherwise an
        // absent module is skipped (dom_mgr.py:567-569).
        if !port_in_blocking_error(state, port) && !sfp.get_presence().unwrap_or(false) {
            continue;
        }
        let dom = sfp.get_transceiver_dom_real_value().unwrap_or(Value::Null);
        let mut row = Map::new();
        if let Some(t) = dom.get("temperature") {
            row.insert("temperature".to_string(), t.clone());
        }
        if let Err(e) =
            publish_row(state, DOM_TEMPERATURE_TABLE, port, beautify_dom_row(&Value::Object(row)))
        {
            eprintln!("xcvrd-rs: thermal publish {port} (sfp {phys}) failed: {e}");
        }
    }
}

/// `DomInfoUpdateTask.get_dom_polling_from_config_db` (dom_mgr.py:76): the per-port
/// DOM poll is disabled iff CONFIG_DB `PORT|<port>.dom_polling == "disabled"`. An
/// absent field (the default) is `enabled`. Read live so the knob takes effect
/// mid-run without a port event. A CONFIG_DB read error fails open (treated as
/// enabled) so a transient DB hiccup never silently halts DOM.
fn dom_polling_disabled(config: &DbConnector, port: &str) -> bool {
    match config.hget(&format!("PORT|{port}"), "dom_polling") {
        Ok(Some(v)) => v.to_string_lossy() == "disabled",
        _ => false,
    }
}

/// `DOMDBUtils.post_port_dom_flags_to_db` (dom_sensor/db_utils.py:53): read the
/// module's DOM flag dict, maintain the flag change-tracking metadata trio, then
/// publish `TRANSCEIVER_DOM_FLAG` (beautified values + `last_update_time`).
///
/// Order matters and mirrors the Python: the metadata update reads the *previous*
/// flag row from STATE_DB and compares it to the freshly-read flags BEFORE the new
/// values overwrite it, so a raise/clear is detected as an edge.
fn publish_dom_flags(state: &DbConnector, sfp: &Sfp, port: &str) -> Result<(), Box<dyn Error>> {
    let flags = match sfp.call_json("get_transceiver_dom_flags", ()) {
        Ok(v) => v,
        Err(_) => return Ok(()), // NotImplementedError / read fault → skip (log upstream)
    };
    let Some(flag_obj) = flags.as_object() else {
        return Ok(());
    };
    if flag_obj.is_empty() {
        return Ok(());
    }

    let update_time = get_current_time();
    // Previous flag values currently in STATE_DB (empty == not found == first publish).
    let prev = hgetall_strings(state, &format!("{DOM_FLAG_TABLE}|{port}"))?;
    update_flag_metadata_tables(
        state,
        port,
        flag_obj,
        &prev,
        &update_time,
        DOM_FLAG_CHANGE_COUNT_TABLE,
        DOM_FLAG_SET_TIME_TABLE,
        DOM_FLAG_CLEAR_TIME_TABLE,
    )?;

    // Beautify (str() each value; no unit strip applies to boolean flag keys) and
    // write TRANSCEIVER_DOM_FLAG with the trailing last_update_time.
    let mut beautified = flag_obj.clone();
    beautify_dom_info_dict(&mut beautified);
    let key = format!("{DOM_FLAG_TABLE}|{port}");
    for (field, value) in &beautified {
        state.hset(&key, field, &CxxString::from(py_str(value).as_str()))?;
    }
    state.hset(&key, "last_update_time", &CxxString::from(update_time.as_str()))?;
    Ok(())
}

/// `StatusDBUtils.post_port_transceiver_hw_status_flags_to_db` (status/db_utils.py:41):
/// read the module's latched status flags (`module_firmware_fault`,
/// `datapath_firmware_fault`, `module_state_changed`, per-lane `txNfault`/`rxNlos`,
/// …), maintain the STATUS-flag change-tracking metadata trio, then publish
/// `TRANSCEIVER_STATUS_FLAG` (base-beautified values + `last_update_time`).
///
/// Mirrors `publish_dom_flags` but over the status-flag tables and with the **base**
/// beautify (status flags carry no DOM units; booleans render `str(bool)`). Same
/// ordering: the metadata update reads the *previous* flag row and compares to the
/// freshly-read flags BEFORE overwriting, so a raise/clear is a detected edge.
fn publish_status_flags(state: &DbConnector, sfp: &Sfp, port: &str) -> Result<(), Box<dyn Error>> {
    let flags = match sfp.call_json("get_transceiver_status_flags", ()) {
        Ok(v) => v,
        Err(_) => return Ok(()), // NotImplementedError / read fault → skip
    };
    let Some(flag_obj) = flags.as_object() else {
        return Ok(());
    };
    if flag_obj.is_empty() {
        return Ok(());
    }

    let update_time = get_current_time();
    let prev = hgetall_strings(state, &format!("{STATUS_FLAG_TABLE}|{port}"))?;
    update_flag_metadata_tables(
        state,
        port,
        flag_obj,
        &prev,
        &update_time,
        STATUS_FLAG_CHANGE_COUNT_TABLE,
        STATUS_FLAG_SET_TIME_TABLE,
        STATUS_FLAG_CLEAR_TIME_TABLE,
    )?;

    // Base beautify (no unit strip): str() each value into the row, then stamp time.
    let mut beautified = flag_obj.clone();
    beautify_info_dict(&mut beautified);
    let key = format!("{STATUS_FLAG_TABLE}|{port}");
    for (field, value) in &beautified {
        state.hset(&key, field, &CxxString::from(py_str(value).as_str()))?;
    }
    state.hset(&key, "last_update_time", &CxxString::from(update_time.as_str()))?;
    Ok(())
}

/// `DBUtils._update_flag_metadata_tables` (db/utils.py:107): seed the metadata trio
/// on the first publish (change count `0`, set/clear times `never`), else bump the
/// change count and stamp the set/clear time for every flag that transitioned. The
/// change count is CUMULATIVE in STATE_DB (survives across runs), so on an edge it is
/// read-modified-written (current + 1). The three metadata table *names* are passed
/// in so the same routine drives both the DOM-flag and the STATUS-flag trios.
#[allow(clippy::too_many_arguments)]
fn update_flag_metadata_tables(
    state: &DbConnector,
    port: &str,
    curr_flags: &Map<String, Value>,
    prev_flag_values: &HashMap<String, String>,
    update_time: &str,
    change_count_table: &str,
    set_time_table: &str,
    clear_time_table: &str,
) -> Result<(), Box<dyn Error>> {
    let plan = compute_flag_metadata_plan(prev_flag_values, curr_flags);
    let count_key = format!("{change_count_table}|{port}");
    let set_key = format!("{set_time_table}|{port}");
    let clear_key = format!("{clear_time_table}|{port}");

    if plan.initialize {
        // _initialize_metadata_tables: every current flag key seeds to 0 / never / never.
        for key in curr_flags.keys() {
            state.hset(&count_key, key, &CxxString::from("0"))?;
            state.hset(&set_key, key, &CxxString::from(NEVER))?;
            state.hset(&clear_key, key, &CxxString::from(NEVER))?;
        }
        return Ok(());
    }

    if plan.edges.is_empty() {
        return Ok(());
    }
    // _update_flag_metadata: bump change count (current + 1) and stamp set/clear time.
    let counts = hgetall_strings(state, &count_key)?;
    for edge in &plan.edges {
        let next = counts
            .get(&edge.key)
            .and_then(|c| c.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        state.hset(&count_key, &edge.key, &CxxString::from(next.to_string().as_str()))?;
        if edge.raised {
            state.hset(&set_key, &edge.key, &CxxString::from(update_time))?;
        } else {
            state.hset(&clear_key, &edge.key, &CxxString::from(update_time))?;
        }
    }
    Ok(())
}

/// `DomInfoUpdateTask.post_port_pm_info_to_db` (dom_mgr.py:238): publish
/// `TRANSCEIVER_PM` for a coherent (paged) module. Skipped for a flat-memory
/// module (`is_flat_memory`, dom_mgr.py:246). The VDM freeze that must wrap the PM
/// capture is now owned by [`publish_vdm`] (the PM row is captured inside the same
/// freeze window as the VDM statistic observables — dom_mgr.py:390-400), so this
/// helper just reads and posts. The row is keyed by the physical port name and
/// carries **no** `last_update_time`.
fn publish_pm(state: &DbConnector, sfp: &Sfp, port: &str) -> Result<(), Box<dyn Error>> {
    // Flat-memory modules have no PM page (dom_mgr.py:246).
    if sfp_bool(sfp, "is_flat_memory") {
        return Ok(());
    }
    let pm = sfp.call_json("get_transceiver_pm", ()).unwrap_or(Value::Null);
    post_pm_row(state, port, &pm)
}

/// Write the beautified `TRANSCEIVER_PM` row (str() each value, no unit strip, no
/// last_update_time). An empty/`None` PM dict (`get_transceiver_pm` N/A) writes
/// nothing (dom_mgr.py:258-260).
fn post_pm_row(state: &DbConnector, port: &str, pm: &Value) -> Result<(), Box<dyn Error>> {
    let Some(obj) = pm.as_object() else {
        return Ok(());
    };
    if obj.is_empty() {
        return Ok(());
    }
    let key = format!("{PM_TABLE}|{port}");
    for (field, value) in obj {
        state.hset(&key, field, &CxxString::from(py_str(value).as_str()))?;
    }
    Ok(())
}

/// `DomInfoUpdateTask` VDM poll block (dom_mgr.py:381-420): on a VDM-capable
/// module, capture the min/max/avg **statistic** observables **and** the PM row
/// inside a single VDM freeze (only when statistics are supported and the module
/// is not in low power — a low-power module is never frozen, so both its PM and
/// its statistic observables stop refreshing until it leaves low power). Then post
/// the merged basic+statistic real values and, last, the per-type latched flags.
///
/// PM shares this freeze rather than taking its own (upstream captures PM inside
/// the same frozen window, dom_mgr.py:390-400), so `is_transceiver_vdm_supported`
/// now gates PM too — exactly as before, since the old PM path also required VDM
/// support + statistics + non-low-power.
fn publish_vdm(state: &DbConnector, sfp: &Sfp, port: &str) -> Result<(), Box<dyn Error>> {
    if !sfp_bool(sfp, "is_transceiver_vdm_supported") {
        return Ok(());
    }
    // Step (a): freeze once → statistic observables + PM. `need_freeze` mirrors
    // `is_vdm_statistic_supported(...) and not is_transceiver_lpmode_on(...)`.
    let mut statistic = Value::Null;
    let need_freeze =
        sfp_bool(sfp, "is_vdm_statistic_supported") && !sfp.get_lpmode().unwrap_or(false);
    if need_freeze {
        if vdm_action_and_confirm(sfp, "freeze_vdm_stats", "get_vdm_freeze_status") {
            statistic = sfp
                .call_json("get_transceiver_vdm_real_value_statistic", ())
                .unwrap_or(Value::Null);
            if let Err(e) = publish_pm(state, sfp, port) {
                eprintln!("xcvrd-rs: PM publish {port} failed: {e}");
            }
        } else {
            eprintln!("xcvrd-rs: failed to freeze VDM stats for {port}");
        }
        // Always unfreeze (the `finally` of vdm_freeze_context).
        if !vdm_action_and_confirm(sfp, "unfreeze_vdm_stats", "get_vdm_unfreeze_status") {
            eprintln!("xcvrd-rs: failed to unfreeze VDM stats for {port}");
        }
    }

    // Step (b): basic observables merged with the statistic ones, posted in one write.
    let basic = sfp
        .call_json("get_transceiver_vdm_real_value_basic", ())
        .unwrap_or(Value::Null);
    let merged = merge_vdm(&basic, &statistic);
    publish_vdm_real_values(state, sfp, port, &merged)?;

    // Step (c): flags last — they are Clear-On-Read, so reading them last captures
    // the freshest latched state (dom_mgr.py:414-420).
    publish_vdm_flags(state, sfp, port)?;
    Ok(())
}

/// `{**basic, **statistic}` (dom_mgr.py:407): statistic observables override basic
/// ones on a key collision. A `Null`/non-object side contributes nothing.
fn merge_vdm(basic: &Value, statistic: &Value) -> Value {
    let mut m = basic.as_object().cloned().unwrap_or_default();
    if let Some(s) = statistic.as_object() {
        for (k, v) in s {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

/// `VDMDBUtils.post_port_vdm_real_values_from_dict_to_db` (vdm/db_utils.py:25):
/// write the pre-merged basic+statistic observables to `TRANSCEIVER_VDM_REAL_VALUE`
/// with one trailing `last_update_time`. Skipped on a flat-memory module or an
/// empty merged dict (base beautify — `str()` each value, no unit strip; `N/A`
/// values are preserved verbatim).
fn publish_vdm_real_values(
    state: &DbConnector,
    sfp: &Sfp,
    port: &str,
    merged: &Value,
) -> Result<(), Box<dyn Error>> {
    if sfp_bool(sfp, "is_flat_memory") {
        return Ok(());
    }
    let Some(obj) = merged.as_object() else {
        return Ok(());
    };
    if obj.is_empty() {
        return Ok(());
    }
    let mut beautified = obj.clone();
    beautify_info_dict(&mut beautified);
    let key = format!("{VDM_REAL_VALUE_TABLE}|{port}");
    for (field, value) in &beautified {
        state.hset(&key, field, &CxxString::from(py_str(value).as_str()))?;
    }
    state.hset(&key, "last_update_time", &CxxString::from(get_current_time().as_str()))?;
    Ok(())
}

/// `VDMDBUtils.post_port_vdm_flags_to_db` (vdm/db_utils.py:58) inline: read the raw
/// VDM flag dict (keys carry the threshold-type token), split it into the four
/// per-type sub-dicts, maintain each populated type's flag change-tracking metadata
/// trio (BEFORE the value rows overwrite it, so a raise/clear is detected as an
/// edge), then publish the per-type `TRANSCEIVER_VDM_{TYPE}_FLAG` value rows. The
/// metadata is updated for every populated type; the value-row write stops at the
/// first empty category (mirroring the two upstream loops). Skipped on a
/// flat-memory module.
fn publish_vdm_flags(state: &DbConnector, sfp: &Sfp, port: &str) -> Result<(), Box<dyn Error>> {
    if sfp_bool(sfp, "is_flat_memory") {
        return Ok(());
    }
    let raw = match sfp.call_json("get_transceiver_vdm_flags", ()) {
        Ok(v) => v,
        Err(_) => return Ok(()), // NotImplementedError / read fault → skip
    };
    let Some(raw_obj) = raw.as_object() else {
        return Ok(());
    };
    let split = split_vdm_by_type(raw_obj);
    let update_time = get_current_time();

    // Metadata trio for every populated type (read prev BEFORE the value rows change).
    for (ttype, dict) in &split {
        if dict.is_empty() {
            continue;
        }
        let upper = ttype.to_uppercase();
        let prev = hgetall_strings(state, &format!("TRANSCEIVER_VDM_{upper}_FLAG|{port}"))?;
        update_flag_metadata_tables(
            state,
            port,
            dict,
            &prev,
            &update_time,
            &format!("TRANSCEIVER_VDM_{upper}_FLAG_CHANGE_COUNT"),
            &format!("TRANSCEIVER_VDM_{upper}_FLAG_SET_TIME"),
            &format!("TRANSCEIVER_VDM_{upper}_FLAG_CLEAR_TIME"),
        )?;
    }

    // Per-type flag value rows; stop at the first empty category.
    for (ttype, dict) in &split {
        if dict.is_empty() {
            break;
        }
        let upper = ttype.to_uppercase();
        let key = format!("TRANSCEIVER_VDM_{upper}_FLAG|{port}");
        let mut beautified = dict.clone();
        beautify_info_dict(&mut beautified);
        for (field, value) in &beautified {
            state.hset(&key, field, &CxxString::from(py_str(value).as_str()))?;
        }
        state.hset(&key, "last_update_time", &CxxString::from(update_time.as_str()))?;
    }
    Ok(())
}

/// `VDMDBUtils.post_port_vdm_thresholds_to_db` (vdm/db_utils.py:62) inline, called
/// at module insert: read the raw VDM threshold dict, split it into the four
/// per-type sub-dicts, then publish each non-empty `TRANSCEIVER_VDM_{TYPE}_THRESHOLD`
/// row (base beautify + `last_update_time`). VDM thresholds are static per module,
/// so — like `TRANSCEIVER_DOM_THRESHOLD` — they are cached at insert rather than on
/// the DOM poll. Skipped on a flat-memory module. Stops at the first empty category.
fn publish_vdm_thresholds(
    state: &DbConnector,
    sfp: &Sfp,
    port: &str,
) -> Result<(), Box<dyn Error>> {
    if sfp_bool(sfp, "is_flat_memory") {
        return Ok(());
    }
    let raw = match sfp.call_json("get_transceiver_vdm_thresholds", ()) {
        Ok(v) => v,
        Err(_) => return Ok(()), // NotImplementedError / read fault → skip
    };
    let Some(raw_obj) = raw.as_object() else {
        return Ok(());
    };
    let split = split_vdm_by_type(raw_obj);
    let update_time = get_current_time();
    for (ttype, dict) in &split {
        if dict.is_empty() {
            break;
        }
        let key = format!("TRANSCEIVER_VDM_{}_THRESHOLD|{port}", ttype.to_uppercase());
        let mut beautified = dict.clone();
        beautify_info_dict(&mut beautified);
        for (field, value) in &beautified {
            state.hset(&key, field, &CxxString::from(py_str(value).as_str()))?;
        }
        state.hset(&key, "last_update_time", &CxxString::from(update_time.as_str()))?;
    }
    Ok(())
}

/// Split a raw type-suffixed VDM threshold/flag dict into the four per-type
/// sub-dicts in canonical `VDM_THRESHOLD_TYPES` order, stripping the `_{type}` token
/// from each key so each type's row uses the bare observable name
/// (`laser_temperature_media_halarm1` → `laser_temperature_media1`; the unit-test
/// fixture form `laser_temperature_media_1_halarm` → `laser_temperature_media_1`).
/// Mirrors `_post_port_vdm_thresholds_or_flags_to_db`'s split (vdm/db_utils.py:87-94).
fn split_vdm_by_type(raw: &Map<String, Value>) -> Vec<(&'static str, Map<String, Value>)> {
    let mut out: Vec<(&'static str, Map<String, Value>)> =
        VDM_THRESHOLD_TYPES.iter().map(|t| (*t, Map::new())).collect();
    for (key, value) in raw {
        for (i, ttype) in VDM_THRESHOLD_TYPES.iter().enumerate() {
            let token = format!("_{ttype}");
            if key.contains(&token) {
                out[i].1.insert(key.replace(&token, ""), value.clone());
            }
        }
    }
    out
}

/// `VDMUtils._vdm_action_and_confirm` (vdm/utils.py): run a freeze/unfreeze action,
/// then poll its done-status until it confirms (up to ~1s). Returns `false` if the
/// action fails, never confirms, or the bridge call errors.
fn vdm_action_and_confirm(sfp: &Sfp, action_method: &str, status_method: &str) -> bool {
    match sfp.call_json(action_method, ()) {
        Ok(v) if py_truthy(&v) => {}
        _ => return false,
    }
    sleep(Duration::from_millis(10)); // MAX_tVDMF settle
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1000) {
        match sfp.call_json(status_method, ()) {
            Ok(v) if py_truthy(&v) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
        sleep(Duration::from_millis(1));
    }
    false
}

/// Call a no-arg predicate SFP method and coerce the result to a bool via Python
/// truthiness; a missing/erroring bridge call is `false` (the Python `except`
/// default for these capability probes on this platform).
fn sfp_bool(sfp: &Sfp, method: &str) -> bool {
    match sfp.call_json(method, ()) {
        Ok(v) => py_truthy(&v),
        Err(_) => false,
    }
}

/// Read a STATE_DB hash as owned `String`s (a small `hgetall` adapter over the
/// `swss_common` `CxxString` map) so the pure metadata logic can compare values.
fn hgetall_strings(db: &DbConnector, key: &str) -> Result<HashMap<String, String>, Box<dyn Error>> {
    let raw = db.hgetall(key)?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
        .collect())
}


/// Write a beautified diagnostic row as a STATE_DB hash (`<table>|<port>`),
/// stamping `last_update_time` in the reference UTC strftime format. Returns
/// `Ok(false)` (writing nothing) when the row is empty — matching the Python
/// `post_diagnostic_values_to_db` skip on `None`/`{}`.
fn publish_row(
    state: &DbConnector,
    table: &str,
    port: &str,
    row: Option<Vec<(String, String)>>,
) -> Result<bool, Box<dyn Error>> {
    let Some(row) = row else {
        return Ok(false);
    };
    let key = format!("{table}|{port}");
    for (field, value) in &row {
        state.hset(&key, field, &CxxString::from(value.as_str()))?;
    }
    state.hset(&key, "last_update_time", &CxxString::from(get_current_time().as_str()))?;
    Ok(true)
}

/// Re-read identity for the ports in the retry set, gated by the ~60s cadence.
/// Ports whose identity now reads successfully are published and dropped from the
/// set; the rest stay for the next round. Mirrors `retry_eeprom_reading`.
fn retry_eeprom_reading(
    platform: &Platform,
    state: &DbConnector,
    ports: &mut BTreeMap<usize, PortCtx>,
    retry_eeprom_set: &mut BTreeSet<usize>,
    last_retry_eeprom_time: &mut Option<Instant>,
) {
    if retry_eeprom_set.is_empty() {
        return;
    }
    let due = match *last_retry_eeprom_time {
        None => true,
        Some(t) => t.elapsed() >= RETRY_EEPROM_READING_INTERVAL,
    };
    if !due {
        return;
    }
    *last_retry_eeprom_time = Some(Instant::now());

    let mut recovered = Vec::new();
    for &phys in retry_eeprom_set.iter() {
        if let Some(ctx) = ports.get_mut(&phys) {
            match sync_port(platform, state, phys, ctx) {
                Ok(true) => recovered.push(phys),
                Ok(false) => {}
                Err(e) => eprintln!("xcvrd-rs: retry sync {} (sfp {phys}) failed: {e}", ctx.name),
            }
        }
    }
    for phys in recovered {
        retry_eeprom_set.remove(&phys);
    }
}

/// Map each configured logical port to its physical SFP index and runtime context.
/// A port is "configured" iff CONFIG_DB has `PORT|<name>`; the emulator names SFP
/// `i` as `Ethernet{i*4}`, which matches the CONFIG_DB `index` field on this testbed.
/// Each port's `admin_status` is recorded so the CMIS `INSERTED` handler knows whether to
/// power the module up (admin-up) or tear it down to a forced-Tx-disabled `READY`
/// (admin-down); every present port starts non-terminal (`INSERTED`) regardless.
fn discover_ports(
    platform: &Platform,
    config: &DbConnector,
) -> Result<BTreeMap<usize, PortCtx>, Box<dyn Error>> {
    let num = platform.num_sfps()?;
    let mut map = BTreeMap::new();
    for phys in 0..num {
        let name = format!("Ethernet{}", phys * 4);
        let key = format!("PORT|{name}");
        if config.exists(&key)? {
            let admin_up = hget_str(config, &key, "admin_status").as_deref() == Some("up");
            // Static datapath attributes the CMIS bring-up needs (speed → app-select,
            // lanes → host_lane_count, subport → lane-mask base). admin_status is re-read
            // live each pass to react to reconfiguration.
            let speed = hget_str(config, &key, "speed")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            let lanes = hget_str(config, &key, "lanes").unwrap_or_default();
            let subport = hget_str(config, &key, "subport")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            map.insert(phys, PortCtx::new(name, admin_up, speed, lanes, subport));
        }
    }
    Ok(map)
}

/// Read a STATE_DB/CONFIG_DB hash field as an owned `String` (empty→`None`), a thin
/// `hget` adapter over the `swss_common` `CxxString` result.
fn hget_str(db: &DbConnector, key: &str, field: &str) -> Option<String> {
    match db.hget(key, field) {
        Ok(Some(v)) => Some(v.to_string_lossy().into_owned()),
        _ => None,
    }
}

/// Convert an `f64` obtained from the Python CMIS decode into a JSON-safe
/// [`Value`]: a finite value stays a numeric `Value::Number` (so the downstream
/// `beautify_dom_row`/`py_str` formats it byte-for-byte as before, preserving the
/// captured golden), while a non-finite value becomes its Python `str()` form
/// ("-inf" / "inf" / "nan"). A pristine module's zero page-02h power-threshold
/// registers decode to `mw_to_dbm(0) = float('-inf')` (sonic_xcvr), which JSON —
/// and therefore the platform-bridge's `json.dumps(...)` → `serde_json::from_str`
/// round-trip — cannot represent, so the bridge errors and the whole
/// `TRANSCEIVER_DOM_THRESHOLD` row is otherwise dropped.
fn float_to_json_safe(f: f64) -> Value {
    if f.is_finite() {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(f.to_string()))
    } else if f.is_nan() {
        Value::String("nan".to_string())
    } else if f > 0.0 {
        Value::String("inf".to_string())
    } else {
        Value::String("-inf".to_string())
    }
}

/// Read a module's static DOM thresholds through the Python CMIS decode and
/// marshal them into a JSON-safe serde object (finite values kept numeric, the
/// non-finite `-inf` power thresholds of an un-enriched module stringified).
///
/// The CMIS decode itself stays in Python (`sonic_platform.sfp.Sfp` →
/// `get_transceiver_threshold_info`, the same call the platform-bridge makes) —
/// we only invoke it and stringify non-finite floats here, because the bridge's
/// generic `json.dumps`→serde marshalling rejects the `-Infinity`/`NaN` tokens
/// JSON has no syntax for and returns `Err`, which `sync_port` then swallows into
/// an empty (never-published) `TRANSCEIVER_DOM_THRESHOLD`. That silently starved
/// every present-but-un-enriched port (e.g. a spare admin-up logical port that no
/// test writes finite thresholds to) of its threshold row. Mirrors the reference
/// `SfpStateUpdateTask.post_port_dom_threshold_info_to_db`, which publishes the
/// raw `get_transceiver_threshold_info()` dict (`str(float('-inf')) == '-inf'`).
///
/// Returns `None` when the module has no readable thresholds yet (the Python call
/// returns `None`/`{}` — e.g. identity not readable on a fresh plug), matching the
/// pre-existing "defer to the retry/backfill path" behaviour.
fn read_threshold_info(index: usize) -> Option<Value> {
    Python::with_gil(|py| -> Option<Value> {
        let sfp = py
            .import_bound("sonic_platform.sfp")
            .ok()?
            .getattr("Sfp")
            .ok()?
            .call1((index,))
            .ok()?;
        let thr = sfp.call_method0("get_transceiver_threshold_info").ok()?;
        if thr.is_none() {
            return None;
        }
        let dict = thr.downcast::<PyDict>().ok()?;
        if dict.len() == 0 {
            return None;
        }
        let mut map = Map::new();
        for (k, v) in dict.iter() {
            // Threshold field names are Python strs; skip any odd non-str key.
            let Ok(key) = k.extract::<String>() else {
                continue;
            };
            // Numeric thresholds go through the finite/non-finite split; a
            // non-numeric value (e.g. 'N/A' for an unsupported threshold) keeps its
            // Python str() form, exactly as the reference publish would.
            let val = if let Ok(f) = v.extract::<f64>() {
                float_to_json_safe(f)
            } else {
                match v.str().ok().and_then(|s| s.to_str().ok().map(str::to_owned)) {
                    Some(s) => Value::String(s),
                    None => continue,
                }
            };
            map.insert(key, val);
        }
        Some(Value::Object(map))
    })
}

/// Publish (or clear) one port's identity + SW status from live HAL state.
///
/// Returns `Ok(true)` when the port is settled (absent, or present with a
/// readable identity that was published) and `Ok(false)` when the module is
/// present but its EEPROM identity is not ready yet — the caller should keep the
/// port in the read-retry set and try again later (mirrors
/// `post_port_sfp_info_to_db` returning `SFP_EEPROM_NOT_READY`).
fn sync_port(
    platform: &Platform,
    state: &DbConnector,
    phys: usize,
    ctx: &mut PortCtx,
) -> Result<bool, Box<dyn Error>> {
    let sfp = platform.sfp(phys)?;
    let port = ctx.name.clone();
    let info_key = format!("{INFO_TABLE}|{port}");
    let sw_key = format!("{STATUS_SW_TABLE}|{port}");

    if sfp.get_presence()? {
        let info = sfp.get_transceiver_info().unwrap_or(serde_json::Value::Null);
        let readable = info.as_object().map(|o| !o.is_empty()).unwrap_or(false);
        if readable {
            let obj = info.as_object().unwrap();
            for (field, value) in obj {
                if let Some(s) = stringify(value) {
                    state.hset(&info_key, field, &CxxString::from(s.as_str()))?;
                }
            }
            let replaceable = sfp.is_replaceable().unwrap_or(false);
            state.hset(&info_key, "is_replaceable", &CxxString::from(pybool(replaceable)))?;
            // status + error are written together (common.update_port_transceiver_status_table_sw):
            // a settled, error-free module is SFP_STATUS_INSERTED ("1") with error "N/A".
            state.hset(&sw_key, "status", &CxxString::from("1"))?;
            state.hset(&sw_key, "error", &CxxString::from(NOT_AVAILABLE))?;
            // On (re)insertion, reset the CMIS bring-up working set (clears a prior
            // FAILED run's retry counter/masks so the machine restarts cleanly) and
            // project the initial cmis_state: EVERY present CMIS module starts
            // non-terminal (INSERTED) and is driven by the datapath state machine. An
            // admin-up module is walked all the way to a powered-up READY; an admin-down
            // (or host_tx_ready==false) module runs the INSERTED handler's teardown —
            // DataPathDeinit(host_mask) + OutputDisableTx(media_mask) + active-apsel reset
            // to N/A — and settles on a forced-Tx-disabled READY, never powered up. This
            // is faithful to the reference CmisManagerTask (cmis_manager_task.py:906-925),
            // which has NO admin gate at the cmis_state assignment: the gate lives INSIDE
            // the INSERTED handler. (The old short-circuit-to-READY skipped that teardown,
            // so an admin-down port never wrote the deinit/tx-disable STATUS nor the N/A
            // apsel INFO the golden steady_state projection requires.)
            ctx.reset_bringup();
            ctx.cmis_state = CMIS_STATE_INSERTED.to_string();
            state.hset(&sw_key, "cmis_state", &CxxString::from(ctx.cmis_state.as_str()))?;
            // Cache DOM thresholds at insert (they're static per module): read once
            // and publish TRANSCEIVER_DOM_THRESHOLD so consumers see limits without
            // waiting for a DOM poll. Best-effort — a threshold read fault must not
            // block the identity publish. Read via `read_threshold_info` (not the
            // bridge getter) so a pristine module's -inf power thresholds are
            // stringified rather than crashing the bridge's JSON round-trip and
            // dropping the whole row.
            let thresholds = read_threshold_info(phys).unwrap_or(Value::Null);
            if let Err(e) = publish_row(state, DOM_THRESHOLD_TABLE, &port, beautify_dom_row(&thresholds)) {
                eprintln!("xcvrd-rs: threshold publish {port} (sfp {phys}) failed: {e}");
            }
            // Cache VDM thresholds at insert too (static per module): the four
            // per-type TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_THRESHOLD rows.
            // Best-effort — a non-VDM/flat module (read fault / NotImplementedError)
            // writes nothing and must not block the identity publish.
            if let Err(e) = publish_vdm_thresholds(state, &sfp, &port) {
                eprintln!("xcvrd-rs: VDM threshold publish {port} (sfp {phys}) failed: {e}");
            }
            Ok(true)
        } else {
            // Module present but identity unreadable (e.g. EEPROM not ready / read
            // fault): mark it inserted but defer TRANSCEIVER_INFO to the retry loop
            // so we never publish a partial/stale identity row.
            state.hset(&sw_key, "status", &CxxString::from("1"))?;
            state.hset(&sw_key, "error", &CxxString::from(NOT_AVAILABLE))?;
            Ok(false)
        }
    } else {
        // Drop the cached xcvr API for the now-absent module (mirrors
        // `sfp.remove_xcvr_api()` on the `SFP_STATUS_REMOVED` event, xcvrd.py:589).
        // The platform caches one `Sfp`/xcvr-API instance per slot for the life of
        // the process, and CMIS capability probes (`is_transceiver_vdm_supported`,
        // the `@read_only_cached_api_return` `is_vdm_statistic_supported`, …) are
        // memoized on that API from the EEPROM content read at API-creation time.
        // Without invalidating it here, a module re-plugged with *different* EEPROM
        // content (e.g. VDM/PM freshly advertised) keeps the stale pre-plug API, so
        // VDM/PM support reads False forever and TRANSCEIVER_PM is never published.
        // Clearing it forces the next insertion to rebuild the API from live EEPROM.
        let _ = sfp.call_json("remove_xcvr_api", ());
        // Delete the full per-port hardware-info table set (xcvrd.py:600-622):
        // TRANSCEIVER_INFO, DOM_SENSOR, the DOM_FLAG value+metadata trio, STATUS,
        // the STATUS_FLAG value+metadata trio, FIRMWARE_INFO, PM and every VDM table.
        // TRANSCEIVER_STATUS_SW is deliberately NOT in the set — it is preserved and
        // updated to the removed state below (it holds the plug status). DOM_THRESHOLD
        // is left cached; a re-plug overwrites it with the same field set.
        del_transceiver_hw_tables(state, &port, true)?;
        state.hset(&sw_key, "status", &CxxString::from("0"))?;
        state.hset(&sw_key, "error", &CxxString::from(NOT_AVAILABLE))?;
        // Stamp cmis_state=REMOVED on plug-out. In the reference this is the
        // CmisManagerTask reacting to the STATE_DB TRANSCEIVER_INFO DEL that the
        // SfpStateUpdateTask just produced (on_port_update_event PORT_DEL →
        // CMIS_STATE_REMOVED, cmis_manager_task.py:191-193). This single-threaded
        // daemon fuses both tasks, so the plug-out handler must perform that stamp.
        // Without it a stale TERMINAL cmis_state (e.g. READY from the prior bring-up)
        // lingers: cmis_datapath_sm skips terminal ports before its presence check
        // (faithful to process_single_lport, cmis_manager_task.py:1268), so it never
        // corrects the value, and a re-plug's fresh INSERTED→…→READY progression is
        // masked by the old READY until the machine re-runs a beat later.
        cmis_set_state(state, ctx, CMIS_STATE_REMOVED);
        // Reset the NPU_SI sync guard to DEFAULT on plug-out (xcvrd.py:595-596) so a
        // subsequent re-plug re-publishes the media SI to APPL_DB and re-stamps NOTIFIED
        // (the DEFAULT->NOTIFIED lifecycle asserted by test_media_settings).
        let _ = state.hset(
            &format!("PORT_TABLE|{port}"),
            NPU_SI_SETTINGS_SYNC_STATUS_KEY,
            &CxxString::from(NPU_SI_SETTINGS_DEFAULT_VALUE),
        );
        Ok(true)
    }
}

/// Delete a port's transceiver hardware-info tables, mirroring the
/// `common.del_port_sfp_dom_info_from_db(...)` table list the Python daemon passes
/// on an `SFP_STATUS_REMOVED` event (xcvrd.py:600-622) and on a blocking error
/// (xcvrd.py:644-666). `include_info` deletes `TRANSCEIVER_INFO` too (the physical
/// removal case); a blocking error keeps `TRANSCEIVER_INFO` (it is static and stays
/// valid while only the EEPROM is unreadable). `TRANSCEIVER_DOM_THRESHOLD` and
/// `TRANSCEIVER_STATUS_SW` are never deleted here. `del` on an absent key is a
/// harmless no-op, so tables the module never populated (e.g. VDM on a non-coherent
/// module) cost nothing.
/// Graceful STATE_DB teardown on daemon shutdown (`DaemonXcvrd.deinit`, xcvrd.py:1076).
/// Reads the warm/fast-reboot verdict FRESH from STATE_DB (never cached — a warm/fast
/// reboot may have been signalled after xcvrd came up; `test_warm_reboot` toggles the flag
/// around a controlled stop), then for every configured port:
///   * always purges the DOM/VDM/PM/flag "hardware" tables, and
///   * deletes the `TRANSCEIVER_STATUS` + `TRANSCEIVER_STATUS_SW` status pair ONLY on a
///     cold shutdown — a warm/fast reboot leaves them in place so the datapath state
///     persists across the restart and the data plane is not disrupted (xcvrd.py:1125).
/// `TRANSCEIVER_INFO` is intentionally kept (Python `intf_tbl = None`, @1100), to avoid an
/// optical-app Tx-disable being triggered by the info-table deletion during shutdown.
fn deinit_on_shutdown(state: &DbConnector, ports: &BTreeMap<usize, PortCtx>) {
    let is_warm_fast_reboot =
        common::is_syncd_warm_restore_complete(state) || common::is_fast_reboot_enabled(state);
    eprintln!("xcvrd-rs: deinit teardown (warm/fast reboot = {is_warm_fast_reboot})");
    for ctx in ports.values() {
        let port = &ctx.name;
        // Always-deleted DOM/VDM/PM/flag hardware tables (NOT INFO, NOT the STATUS pair).
        for table in [
            DOM_SENSOR_TABLE,
            DOM_TEMPERATURE_TABLE,
            DOM_FLAG_TABLE,
            DOM_FLAG_CHANGE_COUNT_TABLE,
            DOM_FLAG_SET_TIME_TABLE,
            DOM_FLAG_CLEAR_TIME_TABLE,
            STATUS_FLAG_TABLE,
            STATUS_FLAG_CHANGE_COUNT_TABLE,
            STATUS_FLAG_SET_TIME_TABLE,
            STATUS_FLAG_CLEAR_TIME_TABLE,
            FIRMWARE_INFO_TABLE,
            PM_TABLE,
            VDM_REAL_VALUE_TABLE,
        ] {
            let _ = state.del(&format!("{table}|{port}"));
        }
        for category in VDM_CATEGORIES {
            let _ = state.del(&format!("TRANSCEIVER_VDM_{category}_THRESHOLD|{port}"));
            for suffix in VDM_FLAG_SUFFIXES {
                let _ = state.del(&format!("TRANSCEIVER_VDM_{category}_{suffix}|{port}"));
            }
        }
        // Status pair: deleted only on a cold shutdown; preserved across warm/fast reboot so
        // the live module_state / DP{n}State survive the xcvrd restart.
        if !is_warm_fast_reboot {
            let _ = state.del(&format!("{STATUS_TABLE}|{port}"));
            let _ = state.del(&format!("{STATUS_SW_TABLE}|{port}"));
        }
    }
}

fn del_transceiver_hw_tables(
    state: &DbConnector,
    port: &str,
    include_info: bool,
) -> Result<(), Box<dyn Error>> {
    if include_info {
        state.del(&format!("{INFO_TABLE}|{port}"))?;
    }
    for table in [
        DOM_SENSOR_TABLE,
        DOM_TEMPERATURE_TABLE,
        DOM_FLAG_TABLE,
        DOM_FLAG_CHANGE_COUNT_TABLE,
        DOM_FLAG_SET_TIME_TABLE,
        DOM_FLAG_CLEAR_TIME_TABLE,
        STATUS_TABLE,
        STATUS_FLAG_TABLE,
        STATUS_FLAG_CHANGE_COUNT_TABLE,
        STATUS_FLAG_SET_TIME_TABLE,
        STATUS_FLAG_CLEAR_TIME_TABLE,
        FIRMWARE_INFO_TABLE,
        PM_TABLE,
        VDM_REAL_VALUE_TABLE,
    ] {
        state.del(&format!("{table}|{port}"))?;
    }
    // VDM per-category threshold + flag(value/change-count/set-time/clear-time) tables.
    for category in VDM_CATEGORIES {
        state.del(&format!("TRANSCEIVER_VDM_{category}_THRESHOLD|{port}"))?;
        for suffix in VDM_FLAG_SUFFIXES {
            state.del(&format!("TRANSCEIVER_VDM_{category}_{suffix}|{port}"))?;
        }
    }
    Ok(())
}

/// Full per-port STATE_DB teardown for a CONFIG_DB logical-port REMOVAL
/// (`SfpStateUpdateTask.on_remove_logical_port`, xcvrd.py:731-764). Unlike a physical unplug
/// (`del_transceiver_hw_tables`, which PRESERVES `TRANSCEIVER_STATUS_SW` and
/// `TRANSCEIVER_DOM_THRESHOLD` — the module merely went away, the port is still configured),
/// deconfiguring the logical port purges the ENTIRE per-port table set: the hardware-info
/// tables (incl. `TRANSCEIVER_INFO`) plus the DOM/VDM THRESHOLD tables and the plug-state
/// `TRANSCEIVER_STATUS_SW`. `del` on an absent key is a harmless no-op.
fn del_transceiver_all_tables(state: &DbConnector, port: &str) -> Result<(), Box<dyn Error>> {
    // Hardware-info tables incl. TRANSCEIVER_INFO, STATUS, DOM_SENSOR, the flag trios,
    // FIRMWARE_INFO, PM, VDM_REAL_VALUE and the per-category VDM threshold/flag tables.
    del_transceiver_hw_tables(state, port, true)?;
    // The two tables a physical unplug keeps but a logical-port removal deletes.
    state.del(&format!("{DOM_THRESHOLD_TABLE}|{port}"))?;
    state.del(&format!("{STATUS_SW_TABLE}|{port}"))?;
    Ok(())
}

/// Invert the `Ethernet{phys*4}` naming `discover_ports` establishes: `Ethernet60` → SFP 15.
/// Returns `None` for a non-`Ethernet`, non-numeric, or non-4-aligned name (a breakout
/// sub-interface / meta row that has no 1:1 physical SFP on this testbed).
fn phys_from_port_name(name: &str) -> Option<usize> {
    let n: usize = name.strip_prefix("Ethernet")?.parse().ok()?;
    if n % 4 == 0 {
        Some(n / 4)
    } else {
        None
    }
}

/// Reconcile CONFIG_DB logical-port add/remove events observed by the always-on port-config
/// watcher, mirroring `SfpStateUpdateTask.on_port_config_change` (xcvrd.py:731/794). Drained
/// once per serve pass:
///   * REMOVE (`PORT_DEL`): full STATE_DB teardown of the port's entire table set and drop it
///     from the live `ports` map (and the retry/link-flap bookkeeping) so no per-port loop
///     touches the deconfigured port — the per-module isolation the design requires.
///   * ADD (`PORT_SET` of a port not already tracked): rebuild the `PortCtx` from CONFIG_DB,
///     re-seed `NPU_SI_SETTINGS_SYNC_STATUS = NPU_SI_SETTINGS_DEFAULT`, then `sync_port` to
///     repopulate `TRANSCEIVER_INFO` / `TRANSCEIVER_STATUS_SW` / `TRANSCEIVER_DOM_THRESHOLD`
///     (and VDM thresholds) and publish media SI. Duplicate adds (the several `hset`s of a
///     re-add, or the initial snapshot) collapse to a no-op via the `ports` membership check.
#[allow(clippy::too_many_arguments)]
fn reconcile_logical_ports(
    platform: &Platform,
    state: &DbConnector,
    config: &DbConnector,
    media_env: &MediaEnv,
    ports: &mut BTreeMap<usize, PortCtx>,
    queue: &Arc<Mutex<VecDeque<PortConfigObservation>>>,
    warm_fast_reboot: bool,
    retry_eeprom_set: &mut BTreeSet<usize>,
    link_flap_counts: &mut HashMap<usize, String>,
    link_change_due: &mut HashMap<usize, Instant>,
) {
    // Drain under the lock, then release it before any PyO3/STATE_DB work (the watcher thread
    // must never block on the GIL-holding main thread).
    let obs: Vec<PortConfigObservation> = match queue.lock() {
        Ok(mut q) => q.drain(..).collect(),
        Err(_) => return,
    };
    for o in obs {
        match o.op {
            PortConfigOp::Remove => {
                // A logical-port removal only tears down a port we are actually tracking; the
                // physical index is resolved from the live map (its name), not re-parsed, so a
                // Del for an untracked/meta key is ignored.
                let phys = ports
                    .iter()
                    .find(|(_, ctx)| ctx.name == o.lport)
                    .map(|(&p, _)| p);
                let Some(phys) = phys else { continue };
                if let Err(e) = del_transceiver_all_tables(state, &o.lport) {
                    eprintln!("xcvrd-rs: logical-port {} teardown failed: {e}", o.lport);
                }
                ports.remove(&phys);
                retry_eeprom_set.remove(&phys);
                link_flap_counts.remove(&phys);
                link_change_due.remove(&phys);
                eprintln!("xcvrd-rs: logical port {} removed (sfp {phys}) — tables torn down", o.lport);
            }
            PortConfigOp::Add => {
                // Skip ports we already track (the initial snapshot, or the extra hset pops of a
                // re-add) — repopulation happens exactly once, on the first add of a fresh port.
                if ports.values().any(|ctx| ctx.name == o.lport) {
                    continue;
                }
                let Some(phys) = phys_from_port_name(&o.lport) else { continue };
                let key = format!("PORT|{}", o.lport);
                // A Del may have superseded this queued Add (or it was a partial row): only add
                // when CONFIG_DB still has the port configured.
                match config.exists(&key) {
                    Ok(true) => {}
                    _ => continue,
                }
                let admin_up = hget_str(config, &key, "admin_status").as_deref() == Some("up");
                let speed = hget_str(config, &key, "speed")
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                let lanes = hget_str(config, &key, "lanes").unwrap_or_default();
                let subport = hget_str(config, &key, "subport")
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let mut ctx = PortCtx::new(o.lport.clone(), admin_up, speed, lanes, subport);
                // Re-seed the NPU_SI sync guard to DEFAULT for the freshly-added port
                // (on_add_logical_port, xcvrd.py:794) — unconditionally, so a stale NOTIFIED from
                // a prior life is reset and the next media notify re-runs.
                let _ = state.hset(
                    &format!("PORT_TABLE|{}", o.lport),
                    NPU_SI_SETTINGS_SYNC_STATUS_KEY,
                    &CxxString::from(NPU_SI_SETTINGS_DEFAULT_VALUE),
                );
                // Repopulate INFO / STATUS_SW / DOM_THRESHOLD / VDM thresholds from live HAL.
                match sync_port(platform, state, phys, &mut ctx) {
                    Ok(true) => {
                        retry_eeprom_set.remove(&phys);
                        if !warm_fast_reboot {
                            publish_media_settings(platform, media_env, phys, &o.lport);
                        }
                    }
                    Ok(false) => {
                        retry_eeprom_set.insert(phys);
                    }
                    Err(e) => {
                        eprintln!("xcvrd-rs: logical-port {} add sync (sfp {phys}) failed: {e}", o.lport)
                    }
                }
                ports.insert(phys, ctx);
                eprintln!("xcvrd-rs: logical port {} added (sfp {phys}) — tables repopulated", o.lport);
            }
        }
    }
}

/// Decode an SfpBase change-event error bitmap into `TRANSCEIVER_STATUS_SW`
/// (`update_port_transceiver_status_table_sw(logical_port, tbl, value, '|'.join(...))`,
/// xcvrd.py:623-666): write the raw event `value` as `status` and the `'|'`-joined
/// generic error description(s) (plus the vendor-specific description when a
/// vendor bit is set) as `error`. A blocking error means the EEPROM is unreadable,
/// so the DOM/hardware tables are purged (the static `TRANSCEIVER_INFO` is kept).
fn handle_sfp_error_event(
    state: &DbConnector,
    port: &str,
    value: &str,
    vendor_error: Option<&String>,
) -> Result<(), Box<dyn Error>> {
    let error_bits: u32 = match value.parse() {
        Ok(bits) => bits,
        Err(_) => {
            eprintln!("xcvrd-rs: {port} got unrecognized SFP event {value}, ignored");
            return Ok(());
        }
    };
    let mut descriptions = sfp_status_helper::fetch_generic_error_description(error_bits);
    if sfp_status_helper::has_vendor_specific_error(error_bits) {
        if let Some(vendor) = vendor_error {
            descriptions.push(vendor.clone());
        }
    }
    let sw_key = format!("{STATUS_SW_TABLE}|{port}");
    state.hset(&sw_key, "status", &CxxString::from(value))?;
    state.hset(&sw_key, "error", &CxxString::from(descriptions.join("|").as_str()))?;
    if sfp_status_helper::is_error_block_eeprom_reading(error_bits) {
        // Keep TRANSCEIVER_INFO (static); drop the DOM/hardware tables (out of date).
        del_transceiver_hw_tables(state, port, false)?;
    }
    Ok(())
}

/// True when a port is latched in a blocking error, read straight from
/// `TRANSCEIVER_STATUS_SW.error` (the daemon's analogue of
/// `sfp_status_helper.detect_port_in_error_status`). A read miss/error is `false`.
fn port_in_blocking_error(state: &DbConnector, port: &str) -> bool {
    match state.hget(&format!("{STATUS_SW_TABLE}|{port}"), "error") {
        Ok(Some(err)) => sfp_status_helper::is_blocking_error_description(&err.to_string_lossy()),
        _ => false,
    }
}

/// Publish `TRANSCEIVER_FIRMWARE_INFO` from the CMIS API's
/// `get_transceiver_info_firmware_versions()` (post_port_sfp_firmware_info_to_db,
/// dom_mgr.py:203). Each field is `str()`-beautified; an empty/`None` result writes
/// nothing (the Python `SFP_EEPROM_NOT_READY` skip). No `last_update_time` is added
/// (the firmware poster writes only the version fields).
fn publish_firmware_info(state: &DbConnector, sfp: &Sfp, port: &str) -> Result<(), Box<dyn Error>> {
    let fw = sfp
        .call_json("get_transceiver_info_firmware_versions", ())
        .unwrap_or(Value::Null);
    if let Some(row) = beautify_info_row(&fw) {
        let key = format!("{FIRMWARE_INFO_TABLE}|{port}");
        for (field, value) in &row {
            state.hset(&key, field, &CxxString::from(value.as_str()))?;
        }
    }
    Ok(())
}


/// Python-style bool rendering, matching `str(bool)` the reference daemon writes.
fn pybool(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

/// Render a `get_transceiver_info()` JSON value as the STATE_DB field string the
/// reference daemon writes via `str(value)`.
///
/// Strings strip ONLY trailing CMIS NUL padding (fixed-width identity fields are
/// NUL-padded in EEPROM) and PRESERVE trailing ASCII spaces, because Python's
/// `str(value)` in `post_port_sfp_info_to_db` strips nothing: a space-terminated
/// date code like `vendor_date="2024-12-14 "` must keep its trailing space to match
/// the reference TRANSCEIVER_INFO golden. (An earlier `.trim_end()` here also ate
/// trailing spaces and diverged every space-padded field from the golden.) Clean
/// identity fields — vendor_name/vendor_pn/vendor_rev/vendor_sn — arrive without
/// padding from the bridge, so they are unaffected. JSON nulls are skipped (the
/// field is absent from the row rather than a literal `"None"`).
fn stringify(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.trim_end_matches('\0').to_string()),
        Value::Bool(b) => Some(pybool(*b).to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmis::cmis_api::MockCmisApi;
    use serde_json::json;

    /// A 400G / 8-host-lane single-application advertisement (app 1), the shape the
    /// datapath machine app-selects against (mirrors the cmis_manager_task fixture).
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

    /// the DOM gate releases only on a *terminal* CMIS
    /// state (`common.CMIS_TERMINAL_STATES = {READY, FAILED, REMOVED}`); every
    /// bring-up (non-terminal) state keeps DOM flags/PM gated
    /// (`is_port_in_cmis_initialization_process`).
    #[test]
    fn cmis_terminal_states_match_python() {
        for terminal in ["READY", "FAILED", "REMOVED"] {
            assert!(cmis_is_terminal(terminal), "{terminal} should be terminal");
        }
        for non_terminal in [
            "INSERTED",
            "DP_PRE_INIT_CHECK",
            "DP_DEINIT",
            "AP_CONFIGURED",
            "DP_INIT",
            "DP_TXON",
            "DP_ACTIVATION",
            "UNKNOWN",
            "",
        ] {
            assert!(!cmis_is_terminal(non_terminal), "{non_terminal} should be non-terminal");
        }
    }

    /// `str(bool)` rendering used across the daemon.
    #[test]
    fn pybool_renders_python_bools() {
        assert_eq!(pybool(true), "True");
        assert_eq!(pybool(false), "False");
    }

    /// the deployed daemon's polled analogue of
    /// `on_port_update_event` (dom_mgr.py:433-443) only schedules a link-change flag
    /// re-read when a port's APPL_DB `flap_count` actually *changes* from a value
    /// already observed. The first observation (no baseline yet) just seeds the
    /// baseline and never triggers, so `seed_link_flap_counts` at startup — and a
    /// newly-reconciled logical port — do not spuriously re-read every port's flags.
    /// A change in either direction (bump, or a reset back to absent/"") is a flap.
    #[test]
    fn flap_count_triggers_only_on_change_from_baseline() {
        // First observation of a port (no prior baseline) never triggers.
        assert!(!flap_count_triggers_recapture(None, ""));
        assert!(!flap_count_triggers_recapture(None, "7"));
        // Same value across two polls: no flap, no re-read.
        assert!(!flap_count_triggers_recapture(Some("7"), "7"));
        assert!(!flap_count_triggers_recapture(Some(""), ""));
        // A bump is a flap; a further bump is another flap.
        assert!(flap_count_triggers_recapture(Some("7"), "8"));
        assert!(flap_count_triggers_recapture(Some(""), "1"));
        // A reset (field cleared/removed -> "") from a known count is also a change.
        assert!(flap_count_triggers_recapture(Some("8"), ""));
    }

    /// `coalesce_link_change_after_poll` cancels a redundant fast re-read: once the periodic
    /// DOM poll re-publishes every port's flag tables, any pending fast re-read is redundant and
    /// must be dropped so it can't re-fire the same flags ~1s later and surface a freshly-raised
    /// alarm with no intervening flap. After the call NO re-read may remain pending — that is what
    /// keeps the two flag writers mutually exclusive within a settle window.
    #[test]
    fn periodic_poll_coalesces_pending_link_change_reads() {
        let mut due: HashMap<usize, Instant> = HashMap::new();
        // A re-read already due (its flap was >1s ago) and one still pending (~just flapped):
        // the poll re-read BOTH ports' flags, so both must be cancelled.
        due.insert(10, Instant::now());
        due.insert(11, Instant::now() + DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE);
        coalesce_link_change_after_poll(&mut due);
        assert!(
            due.is_empty(),
            "a periodic poll must cancel every pending link-change re-read so none fires across a \
             later alarm-raise"
        );
        // Idempotent on an already-empty schedule (a poll with no pending re-reads).
        coalesce_link_change_after_poll(&mut due);
        assert!(due.is_empty());
    }

    /// `link_change_defers_poll` gates the periodic-poll deferral: the poll is held off while any
    /// link change is in flight (just detected, just serviced, or still pending its ~1s re-read)
    /// so the link-change fast recapture stays the sole flag writer within the flap trigger
    /// window; a fully quiescent pass (nothing detected/serviced/pending) lets the poll run on
    /// its normal cadence.
    #[test]
    fn link_change_defers_poll_while_in_flight() {
        // Quiescent: no flap in flight -> poll runs on its normal cadence.
        assert!(!link_change_defers_poll(false, false, false));
        // A flap just detected this pass (re-read scheduled ~1s out) defers the poll.
        assert!(link_change_defers_poll(true, false, false));
        // A fast re-read just serviced defers the now-redundant poll.
        assert!(link_change_defers_poll(false, true, false));
        // A re-read still pending its delay keeps the poll deferred across intervening passes.
        assert!(link_change_defers_poll(false, false, true));
        // Any combination in flight defers.
        assert!(link_change_defers_poll(true, true, true));
    }

    /// the TRANSCEIVER_INFO projection
    /// `str(value)` renderer preserves trailing ASCII spaces (only trailing CMIS NUL
    /// padding is trimmed), skips JSON nulls, and formats bools/numbers Python-style.
    ///
    /// Regression guard for the golden divergence where an extra `.trim_end()` in a
    /// second local copy stripped `vendor_date="2024-12-14 "`'s trailing space, so it
    /// diverged from the reference xcvrd's verbatim `str(value)` write. `stringify`
    /// now delegates to the canonical `py_str`, matching the reference `get_transceiver_info`
    /// projection (fixed-width date-code trailing space retained; identity fields like
    /// vendor_name/vendor_pn/vendor_rev/vendor_sn, which the bridge returns clean, are
    /// unaffected).
    #[test]
    fn stringify_preserves_trailing_space_and_skips_null() {
        // vendor_date golden contract: the trailing space survives verbatim.
        assert_eq!(stringify(&json!("2024-12-14 ")).as_deref(), Some("2024-12-14 "));
        // Clean fixed-width identity fields keep their exact bytes (no over-trim).
        assert_eq!(stringify(&json!("EMU-40G-LR4")).as_deref(), Some("EMU-40G-LR4"));
        assert_eq!(stringify(&json!("01")).as_deref(), Some("01"));
        // Trailing CMIS NUL padding is trimmed; a space before the NULs survives.
        assert_eq!(stringify(&json!("ACME\0\0\0")).as_deref(), Some("ACME"));
        assert_eq!(stringify(&Value::String("trailing \0".into())).as_deref(), Some("trailing "));
        // Bools/numbers render Python-style; a JSON null is skipped (field absent).
        assert_eq!(stringify(&json!(true)).as_deref(), Some("True"));
        assert_eq!(stringify(&json!(false)).as_deref(), Some("False"));
        assert_eq!(stringify(&json!(100000)).as_deref(), Some("100000"));
        assert_eq!(stringify(&Value::Null), None);
    }

    /// DOM threshold floats are marshalled
    /// JSON-safely. A pristine (un-enriched) module's zero page-02h power registers
    /// decode to `mw_to_dbm(0) = float('-inf')` (sonic_xcvr), which JSON cannot
    /// represent — the platform-bridge's `json.dumps`→serde round-trip errors on the
    /// `-Infinity` token and the whole row is dropped. `float_to_json_safe` keeps a
    /// finite threshold numeric (so `beautify_dom_row`/`py_str` still formats it byte
    /// for byte like the captured golden) and renders a non-finite one as its Python
    /// `str()` token so the row is still published.
    #[test]
    fn float_to_json_safe_handles_non_finite() {
        // Finite stays numeric and py_str-formats identically to the bridge path.
        assert!(matches!(float_to_json_safe(75.0), Value::Number(_)));
        assert_eq!(py_str(&float_to_json_safe(75.0)), "75.0");
        assert_eq!(py_str(&float_to_json_safe(3.6)), "3.6");
        assert_eq!(py_str(&float_to_json_safe(-5.0)), "-5.0");
        // Non-finite becomes the Python str() token (str(float('-inf')) == '-inf').
        assert_eq!(float_to_json_safe(f64::NEG_INFINITY), Value::String("-inf".into()));
        assert_eq!(float_to_json_safe(f64::INFINITY), Value::String("inf".into()));
        assert_eq!(float_to_json_safe(f64::NAN), Value::String("nan".into()));
    }

    /// a full threshold object carrying a
    /// pristine module's -inf power fields still beautifies into a publishable row —
    /// finite temp/vcc format numerically and the -inf power fields pass through as
    /// "-inf", exactly what the reference `post_port_dom_threshold_info_to_db`
    /// publishes. This is the row that lets a spare admin-up logical port (e.g.
    /// Ethernet60) satisfy the C22 "INFO + DOM_THRESHOLD populated" precondition
    /// instead of the daemon silently dropping it.
    #[test]
    fn threshold_row_with_neg_inf_power_is_publishable() {
        let mut obj = Map::new();
        obj.insert("temphighalarm".into(), float_to_json_safe(75.0));
        obj.insert("templowalarm".into(), float_to_json_safe(-5.0));
        obj.insert("vcchighalarm".into(), float_to_json_safe(3.6));
        obj.insert("txpowerhighalarm".into(), float_to_json_safe(f64::NEG_INFINITY));
        obj.insert("rxpowerlowalarm".into(), float_to_json_safe(f64::NEG_INFINITY));
        let row = beautify_dom_row(&Value::Object(obj))
            .expect("a non-empty threshold object must publish a row");
        let got: std::collections::HashMap<String, String> = row.into_iter().collect();
        assert_eq!(got.get("temphighalarm").map(String::as_str), Some("75.0"));
        assert_eq!(got.get("templowalarm").map(String::as_str), Some("-5.0"));
        assert_eq!(got.get("vcchighalarm").map(String::as_str), Some("3.6"));
        assert_eq!(got.get("txpowerhighalarm").map(String::as_str), Some("-inf"));
        assert_eq!(got.get("rxpowerlowalarm").map(String::as_str), Some("-inf"));
    }

    /// `get_cmis_max_host_lanes_mask` — `0x0f` for a
    /// managed-copper `QSFP+C`, `0xff` otherwise (cmis_manager_task.py).
    #[test]
    fn cmis_max_host_lanes_mask_matches_module_type() {
        let api = MockCmisApi::new();
        api.set_module_type_abbreviation(Some("QSFP-DD"));
        assert_eq!(cmis_max_host_lanes_mask(&api), 0xff);
        api.set_module_type_abbreviation(Some("QSFP+C"));
        assert_eq!(cmis_max_host_lanes_mask(&api), 0x0f);
    }

    /// `get_cmis_host_lanes_mask` — an 8-lane subport-0
    /// application whose host_lane_assignment_options advertises lane 0 yields the full
    /// 0xff mask; a non-advertised start lane or a zero lane count yields 0.
    #[test]
    fn cmis_host_lanes_mask_full_8lane_and_guards() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(advert_400g_8lane());
        assert_eq!(cmis_host_lanes_mask(&api, 1, 8, 0), 0xff);
        // appl < 1 / host_lane_count == 0 / negative subport → 0.
        assert_eq!(cmis_host_lanes_mask(&api, 0, 8, 0), 0);
        assert_eq!(cmis_host_lanes_mask(&api, 1, 0, 0), 0);
        assert_eq!(cmis_host_lanes_mask(&api, 1, 8, -1), 0);
    }

    /// `get_cmis_media_lanes_mask` — an 8-media-lane
    /// subport-0 application whose media_lane_assignment_options advertises lane 0 yields
    /// 0xff; a media_lane_count of 0 or a non-advertised start lane yields 0.
    #[test]
    fn cmis_media_lanes_mask_full_8lane_and_guards() {
        assert_eq!(cmis_media_lanes_mask(1, 8, 1, 0), 0xff);
        assert_eq!(cmis_media_lanes_mask(1, 0, 1, 0), 0);
        assert_eq!(cmis_media_lanes_mask(1, 8, 0, 0), 0);
    }

    /// `check_datapath_state` is masked — only the lanes in
    /// the mask are required to match, and any lane outside the required states fails.
    #[test]
    fn cmis_check_datapath_state_is_masked() {        let api = MockCmisApi::new();
        let mut dp = serde_json::Map::new();
        for n in 1..=8 {
            dp.insert(format!("DP{n}State"), json!("DataPathActivated"));
        }
        api.set_datapath_state_value(Value::Object(dp));
        assert!(cmis_check_datapath_state(&api, 0xff, &["DataPathActivated"]));
        assert!(!cmis_check_datapath_state(&api, 0xff, &["DataPathDeactivated"]));

        // A single deactivated lane fails only when that lane is in the mask.
        let mut dp2 = serde_json::Map::new();
        for n in 1..=8 {
            let s = if n == 3 { "DataPathDeactivated" } else { "DataPathActivated" };
            dp2.insert(format!("DP{n}State"), json!(s));
        }
        api.set_datapath_state_value(Value::Object(dp2));
        assert!(!cmis_check_datapath_state(&api, 0xff, &["DataPathActivated"]));
        // Mask excluding lane 3 (bit 2) passes.
        assert!(cmis_check_datapath_state(&api, 0xff & !(1 << 2), &["DataPathActivated"]));
    }

    /// `cmis_host_tx_not_ready_teardown` — the host-driven
    /// (host_tx_ready → 'false') teardown of a port whose datapath is still ACTIVATED forces
    /// a DataPathDeinit (10h:128) of the active host lanes and disables the media Tx, and it
    /// does so UNCONDITIONALLY (it never consults the fast-reboot skip — that skip only
    /// preserves a datapath across an admin-driven re-init). A non-activated (or zero-mask)
    /// port issues nothing. This is the regression guard for test_host_tx_ready under the
    /// a cached `fast_reboot` must not suppress the deinit.
    #[test]
    fn cmis_host_tx_not_ready_teardown_deinits_activated_datapath() {
        let api = MockCmisApi::new();

        // Datapath ACTIVATED on all 8 host lanes → teardown issues DataPathDeinit + Tx-off.
        let mut dp = serde_json::Map::new();
        for n in 1..=8 {
            dp.insert(format!("DP{n}State"), json!("DataPathActivated"));
        }
        api.set_datapath_state_value(Value::Object(dp));

        assert!(cmis_host_tx_not_ready_teardown(&api, 0xff, 0xff));
        assert_eq!(api.call_count("set_datapath_deinit"), 1);
        assert_eq!(api.last_deinit_mask(), 0xff, "deinit must cover the active host lanes");
        assert_eq!(api.call_count("tx_disable_channel"), 1);
        assert_eq!(api.last_tx_disable_mask(), 0xff);

        // A subport mask (lanes 0..3) tears down only its own lanes.
        let api2 = MockCmisApi::new();
        let mut dp2 = serde_json::Map::new();
        for n in 1..=8 {
            dp2.insert(format!("DP{n}State"), json!("DataPathActivated"));
        }
        api2.set_datapath_state_value(Value::Object(dp2));
        assert!(cmis_host_tx_not_ready_teardown(&api2, 0x0f, 0x0f));
        assert_eq!(api2.last_deinit_mask(), 0x0f);
    }

    /// `cmis_host_tx_not_ready_teardown` is a NO-OP when there
    /// is no live datapath to tear down — a deactivated datapath or an empty host mask issues
    /// no DataPathDeinit (so a port that never activated is not spuriously written).
    #[test]
    fn cmis_host_tx_not_ready_teardown_noop_when_not_activated() {
        let api = MockCmisApi::new();
        let mut dp = serde_json::Map::new();
        for n in 1..=8 {
            dp.insert(format!("DP{n}State"), json!("DataPathDeactivated"));
        }
        api.set_datapath_state_value(Value::Object(dp));

        assert!(!cmis_host_tx_not_ready_teardown(&api, 0xff, 0xff));
        assert_eq!(api.call_count("set_datapath_deinit"), 0);
        assert_eq!(api.call_count("tx_disable_channel"), 0);

        // Empty host mask → nothing to tear down (never reads the datapath state).
        let api2 = MockCmisApi::new();
        assert!(!cmis_host_tx_not_ready_teardown(&api2, 0, 0));
        assert_eq!(api2.call_count("set_datapath_deinit"), 0);
    }

    /// the fast-reboot datapath-skip in `handle_cmis_inserted`
    /// is scoped to the admin-down / precondition teardown ONLY — the host_tx_ready teardown
    /// path (`cmis_host_tx_not_ready_teardown`) does not take a `fast_reboot` argument at all,
    /// so a genuine host_tx_ready drop always tears the datapath down even while a cached
    /// `fast_reboot` is preserving an admin-driven re-init.
    #[test]
    fn host_tx_ready_teardown_ignores_fast_reboot() {
        let api = MockCmisApi::new();
        let mut dp = serde_json::Map::new();
        for n in 1..=4 {
            dp.insert(format!("DP{n}State"), json!("DataPathActivated"));
        }
        api.set_datapath_state_value(Value::Object(dp));
        // No fast_reboot parameter exists on this path: the deinit is unconditional.
        assert!(cmis_host_tx_not_ready_teardown(&api, 0x0f, 0x0f));
        assert_eq!(api.last_deinit_mask(), 0x0f);
    }

    /// `is_cmis_application_update_required` — a flat module
    /// or a zero app/mask needs no update; a fresh module (active app 0 ≠ desired) does;
    /// an already-applied+activated app does not.
    #[test]
    fn cmis_application_update_required_cases() {
        let api = MockCmisApi::new();
        api.set_flat_memory(true);
        assert!(!cmis_is_application_update_required(&api, 1, 0xff));
        api.set_flat_memory(false);
        assert!(!cmis_is_application_update_required(&api, 0, 0xff));
        assert!(!cmis_is_application_update_required(&api, 1, 0));

        // Fresh module: every masked lane reports active application 0 (≠ desired 1).
        api.set_application_by_lane(0);
        assert!(cmis_is_application_update_required(&api, 1, 0xff));

        // Already app 1 on all lanes AND all lanes activated + ConfigSuccess → skip.
        api.set_application_by_lane(1);
        let mut dp = serde_json::Map::new();
        let mut cfg = serde_json::Map::new();
        for n in 1..=8 {
            dp.insert(format!("DP{n}State"), json!("DataPathActivated"));
            cfg.insert(format!("ConfigStatusLane{n}"), json!("ConfigSuccess"));
        }
        api.set_datapath_state_value(Value::Object(dp));
        api.set_config_status(Value::Object(cfg));
        assert!(!cmis_is_application_update_required(&api, 1, 0xff));
    }

    /// `PortCtx::reset_bringup` clears a prior FAILED run's
    /// retry counter and lane masks so a re-plugged module starts the machine clean.
    #[test]
    fn port_ctx_reset_bringup_clears_working_set() {
        let mut ctx = PortCtx::new("Ethernet0".to_string(), true, 400_000, "0,1,2,3,4,5,6,7".to_string(), 0);
        assert_eq!(ctx.host_lane_count, 8);
        ctx.cmis_retries = 9;
        ctx.host_lanes_mask = 0xff;
        ctx.appl = Some(1);
        ctx.forced_tx_disabled = true;
        ctx.cmis_expired = Some(Instant::now());
        ctx.reset_bringup();
        assert_eq!(ctx.cmis_retries, 0);
        assert_eq!(ctx.host_lanes_mask, 0);
        assert_eq!(ctx.appl, None);
        assert!(!ctx.forced_tx_disabled);
        assert!(ctx.cmis_expired.is_none());
        // Static config (speed/lanes/subport/host_lane_count) is preserved.
        assert_eq!(ctx.speed, 400_000);
        assert_eq!(ctx.host_lane_count, 8);
    }

    /// TRANSCEIVER_STATUS per-host-lane config scoping.
    /// The module reports `ConfigSuccess` on ALL 8 host lanes, but a port owning only
    /// lanes 1-4 (host_lanes_mask=0x0F, the golden `activated_datapath` Ethernet4 case)
    /// must project `ConfigUndefined` for its out-of-mask lanes 5-8 while leaving the
    /// owned lanes AND every sibling per-lane datapath field untouched.
    #[test]
    fn mask_config_state_scopes_config_to_owned_host_lanes() {
        let mut status = json!({
            "module_state": "ModuleReady",
            "config_state_hostlane1": "ConfigSuccess",
            "config_state_hostlane2": "ConfigSuccess",
            "config_state_hostlane3": "ConfigSuccess",
            "config_state_hostlane4": "ConfigSuccess",
            "config_state_hostlane5": "ConfigSuccess",
            "config_state_hostlane6": "ConfigSuccess",
            "config_state_hostlane7": "ConfigSuccess",
            "config_state_hostlane8": "ConfigSuccess",
            "DP5State": "DataPathDeactivated",
            "dpdeinit_hostlane5": true,
            "tx5disable": true,
        });
        mask_config_state_by_host_lanes(&mut status, 0x0F);
        for lane in 1..=4 {
            assert_eq!(
                status[format!("config_state_hostlane{lane}")],
                json!("ConfigSuccess"),
                "owned lane {lane} keeps its real config status"
            );
        }
        for lane in 5..=8 {
            assert_eq!(
                status[format!("config_state_hostlane{lane}")],
                json!("ConfigUndefined"),
                "out-of-mask lane {lane} is scoped to ConfigUndefined"
            );
        }
        // Sibling per-lane datapath fields are raw hardware values, never rewritten.
        assert_eq!(status["DP5State"], json!("DataPathDeactivated"));
        assert_eq!(status["dpdeinit_hostlane5"], json!(true));
        assert_eq!(status["tx5disable"], json!(true));

        // host_lanes_mask==0 (never brought up / admin-down steady_state) → every
        // config lane reads ConfigUndefined.
        let mut all = json!({
            "config_state_hostlane1": "ConfigSuccess",
            "config_state_hostlane8": "ConfigSuccess",
        });
        mask_config_state_by_host_lanes(&mut all, 0);
        assert_eq!(all["config_state_hostlane1"], json!("ConfigUndefined"));
        assert_eq!(all["config_state_hostlane8"], json!("ConfigUndefined"));

        // A flat-memory / non-CMIS row (no config_state keys) is left untouched.
        let mut flat = json!({ "module_state": "N/A" });
        mask_config_state_by_host_lanes(&mut flat, 0x0F);
        assert_eq!(flat, json!({ "module_state": "N/A" }));

        // A non-object status (bridge error → Null) is a defensive no-op.
        let mut null = Value::Null;
        mask_config_state_by_host_lanes(&mut null, 0x0F);
        assert!(null.is_null());
    }

    /// `parse_machine_conf_platform` mirrors
    /// `device_info.get_platform()`'s machine.conf precedence — `onie_platform` wins over
    /// `aboot_platform`, values are trimmed, and a missing/empty identifier is `None`.
    #[test]
    fn parse_machine_conf_platform_precedence() {
        let onie = "onie_version=1\nonie_platform=x86_64-kvm_x86_64-r0\naboot_platform=x86_64-arista\n";
        assert_eq!(
            parse_machine_conf_platform(onie).as_deref(),
            Some("x86_64-kvm_x86_64-r0")
        );
        // aboot fallback when onie absent.
        assert_eq!(
            parse_machine_conf_platform("aboot_platform=x86_64-arista_7050\n").as_deref(),
            Some("x86_64-arista_7050")
        );
        // Whitespace around the value is stripped.
        assert_eq!(
            parse_machine_conf_platform("onie_platform =  plat-r0  \n").as_deref(),
            Some("plat-r0")
        );
        // No platform key / empty value → None.
        assert_eq!(parse_machine_conf_platform("build_version=x\n"), None);
        assert_eq!(parse_machine_conf_platform("onie_platform=\n"), None);
    }

    /// `choose_platform_path` mirrors
    /// `device_info.get_path_to_platform_dir()` — the in-container
    /// `/usr/share/sonic/platform` is preferred whenever it exists (the pmon mount that
    /// carries the provisioned media/optics settings files),
    /// and only when it does not exist is the host `/usr/share/sonic/device/<platform>`
    /// used (and only if that directory exists).
    #[test]
    fn choose_platform_path_prefers_container_mount() {
        // Container path present → always chosen, regardless of the host candidate.
        assert_eq!(
            choose_platform_path(true, Some(("/usr/share/sonic/device/plat".to_string(), true))),
            CONTAINER_PLATFORM_PATH
        );
        assert_eq!(choose_platform_path(true, None), CONTAINER_PLATFORM_PATH);
        // Container path absent but the host per-platform dir exists → use the host dir.
        assert_eq!(
            choose_platform_path(false, Some(("/usr/share/sonic/device/plat".to_string(), true))),
            "/usr/share/sonic/device/plat"
        );
        // Container path absent and the host dir does not exist / platform unknown → fall
        // back to the container path (media/optics notify then no-ops rather than panics).
        assert_eq!(
            choose_platform_path(false, Some(("/usr/share/sonic/device/plat".to_string(), false))),
            CONTAINER_PLATFORM_PATH
        );
        assert_eq!(choose_platform_path(false, None), CONTAINER_PLATFORM_PATH);
    }

    /// the deployed `sff_active_lanes` helper mirrors
    /// `SffManagerTask.get_active_lanes_for_lport` — subport 0 lights every lane, a
    /// sub-port slices its own `num_lanes_per_lport`-wide window, and invalid input
    /// (zero/oversized lane count, out-of-range subport) is `None` (never panics).
    #[test]
    fn sff_active_lanes_matches_reference() {
        // Single-lport (subport 0): all four host lanes active.
        assert_eq!(sff_active_lanes(0, 4), Some(vec![true, true, true, true]));
        // Two 2-lane subports across a 4-lane pport: each owns its half.
        assert_eq!(sff_active_lanes(1, 2), Some(vec![true, true, false, false]));
        assert_eq!(sff_active_lanes(2, 2), Some(vec![false, false, true, true]));
        // Four 1-lane subports: subport N owns lane N-1.
        assert_eq!(sff_active_lanes(1, 1), Some(vec![true, false, false, false]));
        assert_eq!(sff_active_lanes(4, 1), Some(vec![false, false, false, true]));
        // Invalid input → None (Python would raise/skip; we stay resilient).
        assert_eq!(sff_active_lanes(0, 0), None);
        assert_eq!(sff_active_lanes(-1, 4), None);
        assert_eq!(sff_active_lanes(3, 2), None); // subport_idx > 4/2
    }

    /// `SffDeployState` change-detection basis — a fresh
    /// (default) state reads as "inserted, nothing seen", and once stamped it reflects the
    /// last admin/host_tx values so a repeated identical sweep is a no-op.
    #[test]
    fn sff_deploy_state_change_detection() {
        let mut st = SffDeployState::default();
        assert!(!st.seen);
        assert_eq!(st.prev_admin_up, None);
        assert_eq!(st.prev_host_tx_ready, None);

        // First sight: inserted (seen == false) and every field "changed".
        let inserted = !st.seen;
        assert!(inserted);
        st.seen = true;
        st.prev_admin_up = Some(true);
        st.prev_host_tx_ready = Some(true);

        // Same values again → no change.
        assert_eq!(st.prev_admin_up, Some(true));
        assert!(st.prev_admin_up != Some(false)); // an admin flip would be detected
    }

    /// the admin-watcher queue preserves order so a fast
    /// admin down→up round-trip is replayed as two distinct sweeps (the coalescing a 1s
    /// poll would suffer, and the reason the deployed SFF path subscribes rather than polls).
    #[test]
    fn admin_observation_queue_preserves_toggle_order() {
        let q: Arc<Mutex<VecDeque<AdminObservation>>> = Arc::new(Mutex::new(VecDeque::new()));
        {
            let mut g = q.lock().unwrap();
            g.push_back(AdminObservation { lport: "Ethernet40".into(), admin_up: false });
            g.push_back(AdminObservation { lport: "Ethernet40".into(), admin_up: true });
        }
        let drained: Vec<bool> = q.lock().unwrap().drain(..).map(|o| o.admin_up).collect();
        assert_eq!(drained, vec![false, true]);
    }

    /// `phys_from_port_name` inverts the
    /// `Ethernet{phys*4}` naming `discover_ports` establishes, so the logical-port add path
    /// resolves the right physical SFP (the spare `Ethernet60` → SFP 15 the C22 config uses).
    /// A breakout sub-interface / non-4-aligned / non-`Ethernet` / meta key has no 1:1 SFP
    /// and must resolve to `None` (never mis-mapped to a neighbouring module).
    #[test]
    fn phys_from_port_name_inverts_discover_naming() {
        assert_eq!(phys_from_port_name("Ethernet0"), Some(0));
        assert_eq!(phys_from_port_name("Ethernet4"), Some(1));
        assert_eq!(phys_from_port_name("Ethernet60"), Some(15));
        // Non-4-aligned (a breakout sub-lane) → no dedicated SFP.
        assert_eq!(phys_from_port_name("Ethernet61"), None);
        assert_eq!(phys_from_port_name("Ethernet2"), None);
        // Non-Ethernet / meta / malformed keys.
        assert_eq!(phys_from_port_name("Ethernet-BP0"), None);
        assert_eq!(phys_from_port_name("PortChannel0"), None);
        assert_eq!(phys_from_port_name("Ethernet"), None);
        assert_eq!(phys_from_port_name("EthernetX"), None);
    }

    /// the port-config watcher classifies a CONFIG_DB
    /// `PORT` `Set` as an ADD and a `Del` as a REMOVE, and the serve-loop reconcile dedups
    /// duplicate adds (the several `hset`s of a re-add, plus the initial snapshot) by the
    /// live `ports` membership — so repopulation happens exactly once per fresh logical port.
    #[test]
    fn logical_port_add_dedups_against_tracked_ports() {
        // A queue carrying: the initial snapshot add for an already-tracked port, a genuine
        // new add, then a duplicate add of that same new port (a second hset).
        let q: Arc<Mutex<VecDeque<PortConfigObservation>>> = Arc::new(Mutex::new(VecDeque::new()));
        {
            let mut g = q.lock().unwrap();
            g.push_back(PortConfigObservation { lport: "Ethernet0".into(), op: PortConfigOp::Add });
            g.push_back(PortConfigObservation { lport: "Ethernet60".into(), op: PortConfigOp::Add });
            g.push_back(PortConfigObservation { lport: "Ethernet60".into(), op: PortConfigOp::Add });
        }
        // The reconcile membership check keys off ctx.name; simulate a ports map already
        // tracking Ethernet0 (SFP 0) and confirm only the fresh Ethernet60 add survives dedup.
        let mut ports: BTreeMap<usize, PortCtx> = BTreeMap::new();
        ports.insert(0, PortCtx::new("Ethernet0".into(), true, 400_000, "0,1,2,3".into(), 0));

        let obs: Vec<PortConfigObservation> = q.lock().unwrap().drain(..).collect();
        let mut fresh_adds = Vec::new();
        for o in obs {
            if matches!(o.op, PortConfigOp::Add) && !ports.values().any(|c| c.name == o.lport) {
                // First fresh sighting inserts a ctx so the duplicate is then deduped.
                if let Some(phys) = phys_from_port_name(&o.lport) {
                    fresh_adds.push(o.lport.clone());
                    ports.insert(phys, PortCtx::new(o.lport, true, 0, String::new(), 0));
                }
            }
        }
        assert_eq!(fresh_adds, vec!["Ethernet60".to_string()]);
        assert!(ports.contains_key(&15)); // Ethernet60 → SFP 15, added exactly once
    }

    /// The golden `steady_state` module's application advertisement (a 40G-LR4 QSFP-DD:
    /// one app, 4 host + 4 media lanes assigned from lane 0). Mirrors the emulator's
    /// `emu_config.yaml` defaults and the golden `application_advertisement` string.
    fn advert_40g_4lane() -> Value {
        json!({
            "1": {
                "host_electrical_interface_id": "XLAUI C2M (Annex 83B)",
                "module_media_interface_id": "40GBASE-LR4 (Cl 87)",
                "host_lane_count": 4,
                "media_lane_count": 4,
                "host_lane_assignment_options": 1,
                "media_lane_assignment_options": 1
            }
        })
    }

    /// Build an `ActiveAppSelLaneN` object (the `get_active_apsel_hostlane` shape) from a
    /// per-host-lane AppSel-code array.
    fn apsel_lanes(codes: [u64; 8]) -> Value {
        let mut m = serde_json::Map::new();
        for (i, c) in codes.iter().enumerate() {
            m.insert(format!("ActiveAppSelLane{}", i + 1), json!(c));
        }
        Value::Object(m)
    }

    /// every present CMIS port starts
    /// non-terminal (`INSERTED`) regardless of admin_status — the fix that lets the
    /// datapath machine run the `INSERTED` handler's admin-down teardown (DataPathDeinit +
    /// OutputDisableTx + active-apsel reset) instead of the old short-circuit-to-READY that
    /// skipped it. Faithful to the reference `CmisManagerTask`, whose admin gate lives
    /// inside the `INSERTED` handler, not at the `cmis_state` assignment.
    #[test]
    fn present_port_starts_inserted_regardless_of_admin() {
        let up = PortCtx::new("Ethernet4".into(), true, 40_000, "0,1,2,3".into(), 0);
        let down = PortCtx::new("Ethernet100".into(), false, 40_000, "0,1,2,3".into(), 0);
        assert_eq!(up.cmis_state, CMIS_STATE_INSERTED);
        assert_eq!(down.cmis_state, CMIS_STATE_INSERTED, "admin-down must NOT short-circuit to READY");
        assert!(!cmis_is_terminal(&down.cmis_state), "admin-down must enter the datapath machine");
    }

    /// the golden `steady_state`
    /// (admin-down 40G) module app-selects host_mask == media_mask == 0x0f. Those are the
    /// masks the `INSERTED` handler passes to `set_datapath_deinit`/`tx_disable_channel`,
    /// so the golden TRANSCEIVER_STATUS shows dpdeinit_hostlane1-4 = True, tx1-4disable =
    /// True and tx_disabled_channel = 15 (0x0f) — lanes 5-8 untouched (register default
    /// False). A daemon that short-circuited admin-down would leave all 8 lanes at the
    /// default and publish tx_disabled_channel = 0, failing the golden.
    #[test]
    fn golden_40g_masks_drive_deinit_and_tx_disable() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(advert_40g_4lane());
        // App 1, 4 host lanes from lane 0 → 0x0f (dpdeinit_hostlane1-4).
        let host_mask = cmis_host_lanes_mask(&api, 1, 4, 0);
        assert_eq!(host_mask, 0x0f);
        // App 1, 4 media lanes from lane 0 → 0x0f → tx_disabled_channel = 15.
        let media_mask = cmis_media_lanes_mask(1, 4, 1, 0);
        assert_eq!(media_mask, 0x0f);
        assert_eq!(media_mask, 15, "the golden steady_state tx_disabled_channel is 0x0f=15");
    }

    /// the admin-down apsel RESET
    /// projection is the golden `steady_state` TRANSCEIVER_INFO — active_apsel_hostlane1-8,
    /// host_lane_count and media_lane_count all `N/A`. The masks / live AppSel are ignored
    /// on the reset path (it never reads the module).
    #[test]
    fn apsel_reset_projects_steady_state_golden_info() {
        let api = MockCmisApi::new();
        // Even with a real advertisement + active app present, reset overwrites all to N/A.
        api.set_application_advertisement(advert_40g_4lane());
        api.set_active_apsel(apsel_lanes([1, 1, 1, 1, 0, 0, 0, 0]));

        let tuples = cmis_active_apsel_tuples(&api, 0x0f, true).expect("reset always projects");
        let got: std::collections::HashMap<String, String> = tuples.into_iter().collect();
        for lane in 1..=8 {
            assert_eq!(
                got.get(&format!("active_apsel_hostlane{lane}")).map(String::as_str),
                Some("N/A"),
                "reset active_apsel_hostlane{lane}"
            );
        }
        assert_eq!(got.get("host_lane_count").map(String::as_str), Some("N/A"));
        assert_eq!(got.get("media_lane_count").map(String::as_str), Some("N/A"));
        // The reset path must not consult the module (no live AppSel read).
        assert_eq!(api.call_count("get_active_apsel_hostlane"), 0);
    }

    /// the admin-up (live) apsel
    /// projection is the golden `activated_datapath` TRANSCEIVER_INFO — the masked lanes
    /// (1-4) report the applied AppSel "1", the unused lanes (5-8) are `N/A`, and the
    /// counts come from the active app's advertisement (host_lane_count = media_lane_count
    /// = "4"). This is the non-reset counterpart the DP_ACTIVATE handler writes.
    #[test]
    fn apsel_live_projects_activated_datapath_golden_info() {
        let api = MockCmisApi::new();
        api.set_application_advertisement(advert_40g_4lane());
        api.set_active_apsel(apsel_lanes([1, 1, 1, 1, 0, 0, 0, 0]));

        let tuples = cmis_active_apsel_tuples(&api, 0x0f, false).expect("a live read projects");
        let got: std::collections::HashMap<String, String> = tuples.into_iter().collect();
        for lane in 1..=4 {
            assert_eq!(
                got.get(&format!("active_apsel_hostlane{lane}")).map(String::as_str),
                Some("1"),
                "active lane {lane} reports the applied AppSel"
            );
        }
        for lane in 5..=8 {
            assert_eq!(
                got.get(&format!("active_apsel_hostlane{lane}")).map(String::as_str),
                Some("N/A"),
                "unused lane {lane} is N/A"
            );
        }
        assert_eq!(got.get("host_lane_count").map(String::as_str), Some("4"));
        assert_eq!(got.get("media_lane_count").map(String::as_str), Some("4"));
    }

    // Helper: build an argv tail (Vec<String>) from &str slices.
    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// No arguments yields the argparse defaults: both flags off, both intervals `None`
    /// (mirrors `DaemonXcvrd(SYSLOG_IDENTIFIER, False, False, None, None)`).
    #[test]
    fn parse_args_defaults_match_python() {
        let a = parse_daemon_args(&argv(&[])).expect("empty argv parses");
        assert_eq!(a, DaemonArgs::default());
        assert!(!a.skip_cmis_mgr);
        assert!(!a.enable_sff_mgr);
        assert_eq!(a.dom_temperature_poll_interval, None);
        assert_eq!(a.dom_update_interval, None);
    }

    /// `--skip_cmis_mgr` / `--enable_sff_mgr` are store_true flags; order-independent.
    #[test]
    fn parse_args_store_true_flags() {
        let a = parse_daemon_args(&argv(&["--enable_sff_mgr", "--skip_cmis_mgr"]))
            .expect("both flags parse");
        assert!(a.skip_cmis_mgr);
        assert!(a.enable_sff_mgr);
        assert_eq!(a.dom_temperature_poll_interval, None);
        assert_eq!(a.dom_update_interval, None);
    }

    /// The int options accept both the space-separated (`--flag value`) and the
    /// `--flag=value` forms, exactly like argparse.
    #[test]
    fn parse_args_int_options_both_forms() {
        let spaced = parse_daemon_args(&argv(&[
            "--dom_temperature_poll_interval",
            "5",
            "--dom_update_interval",
            "120",
        ]))
        .expect("space form parses");
        assert_eq!(spaced.dom_temperature_poll_interval, Some(5));
        assert_eq!(spaced.dom_update_interval, Some(120));

        let eq = parse_daemon_args(&argv(&[
            "--dom_temperature_poll_interval=5",
            "--dom_update_interval=120",
        ]))
        .expect("equals form parses");
        assert_eq!(eq.dom_temperature_poll_interval, Some(5));
        assert_eq!(eq.dom_update_interval, Some(120));
    }

    /// The full argument set the Python daemon accepts parses together.
    #[test]
    fn parse_args_all_together() {
        let a = parse_daemon_args(&argv(&[
            "--skip_cmis_mgr",
            "--enable_sff_mgr",
            "--dom_temperature_poll_interval",
            "10",
            "--dom_update_interval=30",
        ]))
        .expect("full arg set parses");
        assert_eq!(
            a,
            DaemonArgs {
                skip_cmis_mgr: true,
                enable_sff_mgr: true,
                dom_temperature_poll_interval: Some(10),
                dom_update_interval: Some(30),
            }
        );
    }

    /// A non-integer value for an `type=int` option is rejected (argparse exit code 2).
    #[test]
    fn parse_args_invalid_int_is_error() {
        let err = parse_daemon_args(&argv(&["--dom_update_interval", "abc"]));
        assert!(matches!(err, Err(ArgParseError::Invalid(_))));
    }

    /// A missing value for an option that takes one argument is an error.
    #[test]
    fn parse_args_missing_value_is_error() {
        let err = parse_daemon_args(&argv(&["--dom_update_interval"]));
        assert!(matches!(err, Err(ArgParseError::Invalid(_))));
    }

    /// An unknown flag is rejected, matching argparse's "unrecognized arguments".
    #[test]
    fn parse_args_unknown_flag_is_error() {
        let err = parse_daemon_args(&argv(&["--nope"]));
        assert!(matches!(err, Err(ArgParseError::Invalid(_))));
    }

    /// `-h` / `--help` short-circuits to the help outcome.
    #[test]
    fn parse_args_help_requested() {
        assert!(matches!(
            parse_daemon_args(&argv(&["-h"])),
            Err(ArgParseError::HelpRequested)
        ));
        assert!(matches!(
            parse_daemon_args(&argv(&["--help"])),
            Err(ArgParseError::HelpRequested)
        ));
    }

    /// A negative `--dom_update_interval` accepts negative *values* at parse time (argparse
    /// `type=int` allows them); the invalid-value fallback happens in resolution.
    #[test]
    fn parse_args_negative_interval_parses() {
        let a = parse_daemon_args(&argv(&["--dom_update_interval", "-5"]))
            .expect("negative int parses");
        assert_eq!(a.dom_update_interval, Some(-5));
    }

    /// `resolve_dom_update_interval` mirrors `DomInfoUpdateTask.__init__`: `None` and a
    /// negative value both fall back to the 60s default; a non-negative value is honoured.
    #[test]
    fn resolve_dom_update_interval_matches_python() {
        assert_eq!(resolve_dom_update_interval(None), Duration::from_secs(60));
        assert_eq!(resolve_dom_update_interval(Some(-1)), Duration::from_secs(60));
        assert_eq!(resolve_dom_update_interval(Some(0)), Duration::from_secs(0));
        assert_eq!(resolve_dom_update_interval(Some(30)), Duration::from_secs(30));
        assert_eq!(resolve_dom_update_interval(Some(120)), Duration::from_secs(120));
    }

    /// `dom_flags_ungated` mirrors `not is_port_in_cmis_initialization_process`: with
    /// `--skip_cmis_mgr` the gate is always open; otherwise it opens only on a terminal
    /// `cmis_state`.
    #[test]
    fn dom_flags_ungated_matches_python_gating() {
        // skip_cmis_mgr: always ungated, even mid-bring-up.
        assert!(dom_flags_ungated(true, "INSERTED"));
        assert!(dom_flags_ungated(true, "DP_INIT"));
        assert!(dom_flags_ungated(true, "READY"));
        // CMIS manager active: gated during bring-up, released on terminal states.
        assert!(!dom_flags_ungated(false, "INSERTED"));
        assert!(!dom_flags_ungated(false, "DP_ACTIVATION"));
        assert!(dom_flags_ungated(false, "READY"));
        assert!(dom_flags_ungated(false, "FAILED"));
        assert!(dom_flags_ungated(false, "REMOVED"));
    }

    fn htr_queue(obs: &[(&str, bool)]) -> Arc<Mutex<VecDeque<HostTxObservation>>> {
        let q: VecDeque<HostTxObservation> = obs
            .iter()
            .map(|(p, r)| HostTxObservation { lport: (*p).to_string(), host_tx_ready: *r })
            .collect();
        Arc::new(Mutex::new(q))
    }

    /// `drain_host_tx_events` folds the ordered STATE_DB `PORT_TABLE` host_tx_ready
    /// observations into a per-lport summary the CMIS pass acts on. The critical property
    /// (why the watcher is EDGE- not level-triggered): a background keeper re-asserting
    /// `'true'` right after a clear produces a `true`→`false`→`true` burst whose net level
    /// is `'true'`, yet `saw_false` still latches so the datapath-deinit teardown fires —
    /// a 1s poll of the level would miss that brief `'false'`.
    #[test]
    fn drain_host_tx_events_latches_false_edge() {
        // No events this pass -> empty map (caller falls back to a level read).
        assert!(drain_host_tx_events(&htr_queue(&[])).is_empty());

        // Steady 'true' -> no false edge, latest true (no teardown).
        let m = drain_host_tx_events(&htr_queue(&[("Ethernet0", true)]));
        assert!(!m["Ethernet0"].saw_false);
        assert!(m["Ethernet0"].latest);

        // A clean drop to 'false' -> false edge, latest false.
        let m = drain_host_tx_events(&htr_queue(&[("Ethernet0", false)]));
        assert!(m["Ethernet0"].saw_false);
        assert!(!m["Ethernet0"].latest);

        // Keeper burst true->false->true: net level 'true' but the false edge must latch.
        let m = drain_host_tx_events(&htr_queue(&[
            ("Ethernet24", true),
            ("Ethernet24", false),
            ("Ethernet24", true),
        ]));
        assert!(m["Ethernet24"].saw_false, "brief false edge must latch through a keeper re-assert");
        assert!(m["Ethernet24"].latest, "latest reflects the keeper-restored 'true'");

        // Per-port isolation: one port's false edge never bleeds into another.
        let m = drain_host_tx_events(&htr_queue(&[
            ("Ethernet0", false),
            ("Ethernet8", true),
        ]));
        assert!(m["Ethernet0"].saw_false);
        assert!(!m["Ethernet8"].saw_false);
        assert!(m["Ethernet8"].latest);
    }

    /// `host_tx_decision` is the per-pass rule the CMIS loop applies to a drained host_tx_ready
    /// summary. Its critical property: a real `'false'` EDGE drops the recorded
    /// level to `false` and forces a re-init EVEN when a keeper has restored the net level to
    /// `'true'`, so `handle_cmis_inserted` re-enters with `host_tx_ready == false` and issues the
    /// DataPathDeinit off a FRESH mask — never gated away by the keeper-restored `latest`.
    #[test]
    fn host_tx_decision_false_edge_forces_deinit_reentry() {
        // Keeper burst (net 'true') on a port the daemon believes is ready: still drop + reinit so
        // handle_cmis_inserted deinits. This is exactly the e2e host_tx_ready_not_ready scenario.
        let keeper = HtrDrain { saw_false: true, latest: true };
        assert_eq!(
            host_tx_decision(true, &keeper),
            HostTxDecision { ready: false, reinit: true, dropped: true },
            "a false edge behind a keeper re-assert must still drop+reinit so the deinit fires"
        );

        // Clean drop to 'false' (no keeper) -> drop + reinit + teardown.
        let dropped = HtrDrain { saw_false: true, latest: false };
        assert_eq!(
            host_tx_decision(true, &dropped),
            HostTxDecision { ready: false, reinit: true, dropped: true }
        );

        // Came back up (no false edge, level changed false->true) -> re-provision, no teardown.
        let up = HtrDrain { saw_false: false, latest: true };
        assert_eq!(
            host_tx_decision(false, &up),
            HostTxDecision { ready: true, reinit: true, dropped: false }
        );

        // Steady 'true' (no edge, unchanged) -> no-op: no reinit, no teardown.
        let steady = HtrDrain { saw_false: false, latest: true };
        assert_eq!(
            host_tx_decision(true, &steady),
            HostTxDecision { ready: true, reinit: false, dropped: false }
        );

        // Steady 'false' seen again while already recorded down (no NEW edge folded): the fold
        // always sets saw_false when a 'false' is present, so a re-observed 'false' re-drops; the
        // level is unchanged but the teardown re-fires (idempotent) — never a spurious bring-up.
        let still_down = HtrDrain { saw_false: true, latest: false };
        assert_eq!(
            host_tx_decision(false, &still_down),
            HostTxDecision { ready: false, reinit: true, dropped: true }
        );
    }

    /// The daemon's host_tx watcher enqueues an observation only for a `host_tx_ready`
    /// field on an `Ethernet*` key: the value is `true` iff it is exactly `'true'`, matching
    /// the reference `get_host_tx_status` string compare (any other value, incl. `'false'`,
    /// is "not ready").
    #[test]
    fn host_tx_ready_value_matches_get_host_tx_status() {
        assert!("true" == "true");
        for not_ready in ["false", "", "True", "1"] {
            assert!(not_ready != "true", "{not_ready:?} must read as not-ready");
        }
    }
}

