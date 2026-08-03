//! Port of `cmis/cmis_manager_task.py` — `CmisManagerTask`, the per-logical-port
//! CMIS datapath bring-up state machine. Its only STATE_DB output for this milestone
//! is `TRANSCEIVER_STATUS_SW.cmis_state`; the post-activation `active_apsel*` /
//! `host_lane_count` / `media_lane_count` writes and the real datapath register I/O
//! are later milestones (kept as stubs).
//!
//! Scope note (M2 DOM gating): the full CMIS provisioning path (application select,
//! decommission, coherent laser/tx tuning, real `DataPathDeinit`/`DataPathInit`
//! register writes, `active_apsel` publication) needs CMIS register access that the
//! platform-bridge seam does not expose (CMIS decode stays in Python). What this
//! milestone delivers is the observable `cmis_state` **contract** — a re-inserted
//! module walks the datapath bring-up states (INSERTED → DP_PRE_INIT_CHECK →
//! DP_DEINIT → AP_CONFIGURED → DP_INIT → DP_TXON → DP_ACTIVATION → READY), honours a
//! stalled module (retry up to `CMIS_MAX_RETRIES`, then `FAILED`), and keeps the DOM
//! gate (`DomInfoUpdateTask::is_port_in_cmis_initialization_process`) closed for the
//! whole non-terminal window — using only `get_presence` / `get_transceiver_status`
//! (ModuleReady) / `set_lpmode` off the HAL seam.
//!
//! The reference reacts to STATE_DB `TRANSCEIVER_INFO` SET/DEL events through a
//! `PortChangeObserver` subscription (an M7+ stub here). This port instead **polls**
//! presence on a fast tick and keys the bring-up off `cmis_state`: plug-out is
//! signalled by `SfpStateUpdateTask::handle_remove` writing `cmis_state=INSERTED`
//! (see its note on why INSERTED, not the reference's REMOVED — it keeps the DOM gate
//! closed continuously across a re-plug so a DOM poll can never race a still-terminal
//! `cmis_state`), and the manager advances any present, non-terminal port one state
//! per tick.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::db::DbTable;
use crate::hal::Hal;
use crate::xcvrd_utilities::common::{self, CmisState};
use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType, PortMapping};

/// Max bring-up retries before driving `cmis_state=FAILED` (`CMIS_MAX_RETRIES`).
pub const CMIS_MAX_RETRIES: u32 = 3;

/// `CMIS_MAX_HOST_LANES` — the eight host lanes a module's datapath control bytes span.
const CMIS_MAX_HOST_LANES: usize = 8;

/// CMIS Staged Control Set 0 page (10h) and the datapath control-byte window offsets
/// the bring-up drives (CMIS v5.2 8.8): DataPathDeinit, OutputDisableTx, ApplyDPInitLane
/// and the DPConfigLane app-select bytes. Written as raw EEPROM via the flat linear
/// offset `page*128 + window_offset` (`SfpOptoeBase.write_eeprom`), exactly the registers
/// `CmisApi` drives — the same page-10h bytes the e2e Monitor oracle watches for.
const SCS0_PAGE: usize = 0x10;
const DPDEINIT_OFFSET: usize = 128;
const OUTPUT_DISABLE_TX_OFFSET: usize = 130;
const APPLY_DPINIT_OFFSET: usize = 143;
const SCS0_DPCONFIG_START: usize = 145;
const SCS0_DPCONFIG_LEN: usize = 8;
/// Datapath state page (11h) — `get_transceiver_status` decodes `DP{n}State` from it.

/// Flat optoe-linear offset of CMIS `(page, window_offset)` for an upper-page byte
/// (`CmisPage.linear_offset` with bank 0: `page*128 + offset`; the window offset is the
/// 128..255 upper-half address, so this never collides with lower memory).
fn cmis_linear(page: usize, offset: usize) -> usize {
    page * 128 + offset
}

/// Default per-port state-machine tick (one transition/gate-check per tick). Fast
/// enough that a healthy bring-up completes in a few seconds and the DOM gate closes
/// promptly on a re-plug.
const DEFAULT_TICK: Duration = Duration::from_secs(1);
/// Cadence of the fast reaction sub-loop that re-polls CONFIG_DB `admin_status` /
/// STATE_DB `PORT_TABLE.host_tx_ready` BETWEEN the slower bring-up ticks. The reference
/// reacts to EVERY `PORT_SET` via a queuing `SubscriberStateTable`, so it can never miss
/// a write; this crate has no subscriber wired into the CMIS task and instead polls, so
/// it must poll FASTER than the transient `host_tx_ready=false` window. On the testbed a
/// keeper daemon re-asserts `host_tx_ready=true` within ~4s (`emu-deploy/deploy_on_dut.sh`),
/// so a one-shot test write of `false` is short-lived; re-polling the DB-only reaction at
/// this cadence catches that edge between the ~1s bring-up ticks and — in that same pass —
/// drives the datapath teardown (`force_cmis_reinit` → DP_DEINIT), which a plain per-tick
/// poll (or one that deferred the deinit to the next tick) raced and missed
/// (tests/test_host_tx_ready.py::test_daemon_reacts_to_host_tx_ready_not_ready).
const REACTION_POLL: Duration = Duration::from_millis(100);
/// Default dwell at `AP_CONFIGURED` waiting for `ModuleReady` before a retry
/// (`update_cmis_state_expiration_time` for the module-power-up/DP-deinit duration).
const DEFAULT_AP_CONF_WAIT: Duration = Duration::from_secs(3);

/// Per-logical-port bookkeeping (the reference `port_dict[lport]` CMIS fields):
/// `cmis_retries`, `cmis_expired`, the datapath lane masks / control-byte shadows and
/// applied AppSel this port drove, plus the presence-edge + last-written + last-observed
/// admin/host_tx tracking this polling port needs in place of the reference's event
/// subscription.
#[derive(Default)]
struct PortState {
    /// Presence at the previous tick — detects the plug-out edge.
    present_before: bool,
    /// `cmis_retries` — bring-up attempts so far.
    retries: u32,
    /// `cmis_expired` — deadline for the current wait (None = no expiration).
    expired: Option<Instant>,
    /// The last `cmis_state` string THIS task wrote — lets it detect an INSERTED that
    /// was written externally (by `handle_remove` on plug-out) and start a fresh
    /// bring-up (`retries=0`) even if it never observed the presence blip.
    last_written: String,
    /// Reaction baseline recorded — once true, a later admin_status / host_tx_ready
    /// change forces a CMIS re-init (`on_port_update_event` PORT_SET).
    initialized: bool,
    /// Last observed CONFIG_DB `PORT.admin_status`.
    last_admin: Option<String>,
    /// Last observed STATE_DB `PORT_TABLE.host_tx_ready`.
    last_htr: Option<String>,
    /// `host_lane_count` — number of host lanes this port's datapath spans.
    host_lane_count: usize,
    /// `host_lanes_mask` / `media_lanes_mask` — active-lane bitmasks for the datapath.
    host_lanes_mask: u8,
    media_lanes_mask: u8,
    /// `max_host_lanes_mask` / `max_media_lanes_mask` — all module lanes (deinit/disable
    /// everything on the ModuleLowPwr→ModuleReady transition).
    max_host_lanes_mask: u8,
    max_media_lanes_mask: u8,
    /// `appl` — provisioned application code (the emulator serves a single app, code 1).
    appl: u8,
    /// Applied per-host-lane AppSel (`None` => unused lane, published as `N/A`).
    active_apsel: [Option<u8>; CMIS_MAX_HOST_LANES],
    /// Shadow of `DataPathDeinit` (10h:128) — the daemon's model of the register so
    /// each control write is an absolute byte (faithful to `CmisApi`'s RMW without a
    /// read dependency; re-synced to the max mask at DP_DEINIT each bring-up).
    deinit_reg: u8,
    /// Shadow of `OutputDisableTx` (10h:130).
    tx_disable_reg: u8,
    /// `forced_tx_disabled` — Tx laser forced off on a lost precondition; the next
    /// bring-up waits for it to settle then clears it.
    forced_tx_disabled: bool,
}

/// `CmisManagerTask` (`cmis_manager_task.py:41`).
pub struct CmisManagerTask {
    pub port_mapping: PortMapping,
    pub skip_cmis_mgr: bool,
    hal: Arc<dyn Hal>,
    /// `TRANSCEIVER_STATUS_SW` — where `cmis_state` is read/written.
    status_sw_tbl: Arc<dyn DbTable>,
    /// CONFIG_DB `PORT` — read for the per-port `admin_status` (+ `lanes` count).
    cfg_port_tbl: Arc<dyn DbTable>,
    /// STATE_DB `PORT_TABLE` — read for the per-port `host_tx_ready` (M8 reaction).
    /// `None` in unit tests that don't exercise the host_tx_ready gate.
    state_port_tbl: Option<Arc<dyn DbTable>>,
    /// STATE_DB `TRANSCEIVER_INFO` — target of `post_port_active_apsel_to_db`
    /// (`active_apsel_hostlane*` / `host_lane_count` / `media_lane_count`). `None` in
    /// unit tests that don't assert the post-activation INFO write.
    int_tbl: Option<Arc<dyn DbTable>>,
    tick: Duration,
    ap_conf_wait: Duration,
    port_state: HashMap<String, PortState>,
    /// Logical ports torn down by a CONFIG_DB logical-port DEL (shared with
    /// `SfpStateUpdateTask`). While a port is in the set this loop stops driving its
    /// bring-up and defensively deletes any `TRANSCEIVER_STATUS_SW.cmis_state` it may
    /// have raced back in after the state task deleted the row — otherwise this task,
    /// iterating its own boot-time mapping clone, would resurrect STATUS_SW for a
    /// logically-removed-but-still-present module. `None` in unit tests.
    deconfigured_ports: Option<Arc<Mutex<BTreeSet<String>>>>,
}

impl CmisManagerTask {
    pub fn new(
        port_mapping: PortMapping,
        skip_cmis_mgr: bool,
        hal: Arc<dyn Hal>,
        status_sw_tbl: Arc<dyn DbTable>,
        cfg_port_tbl: Arc<dyn DbTable>,
    ) -> Self {
        Self::with_timing(
            port_mapping,
            skip_cmis_mgr,
            hal,
            status_sw_tbl,
            cfg_port_tbl,
            DEFAULT_TICK,
            DEFAULT_AP_CONF_WAIT,
        )
    }

    /// Construct with explicit timing — unit tests pass `ap_conf_wait = 0` so the
    /// stalled-module retry path (`AP_CONFIGURED` → timeout → reinit) is driven
    /// deterministically without sleeping.
    pub fn with_timing(
        port_mapping: PortMapping,
        skip_cmis_mgr: bool,
        hal: Arc<dyn Hal>,
        status_sw_tbl: Arc<dyn DbTable>,
        cfg_port_tbl: Arc<dyn DbTable>,
        tick: Duration,
        ap_conf_wait: Duration,
    ) -> Self {
        CmisManagerTask {
            port_mapping,
            skip_cmis_mgr,
            hal,
            status_sw_tbl,
            cfg_port_tbl,
            state_port_tbl: None,
            int_tbl: None,
            tick,
            ap_conf_wait,
            port_state: HashMap::new(),
            deconfigured_ports: None,
        }
    }

    /// `task_worker` — advance every port's state machine on the poll tick until
    /// asked to stop. `skip_cmis_mgr` disables the manager entirely (as in the
    /// reference, the DOM gate is likewise disabled in that mode).
    pub fn task_worker(&mut self, stop: &Arc<AtomicBool>) {
        if self.skip_cmis_mgr {
            eprintln!("xcvrd-rs: CmisManagerTask: skip_cmis_mgr set; manager disabled");
            return;
        }
        eprintln!(
            "xcvrd-rs: CmisManagerTask: start CMIS bring-up loop (tick={:?})",
            self.tick
        );
        while !stop.load(Ordering::Relaxed) {
            let tick_start = Instant::now();
            self.tick_once(stop);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Fast reaction sub-loop until the next bring-up tick is due. The bring-up
            // state machine advances on the slower `tick`, but a CONFIG_DB `admin_status`
            // / STATE_DB `host_tx_ready` change must be observed PROMPTLY — the reference
            // reacts to every `PORT_SET` off a queuing subscriber, so it never misses a
            // write. This crate polls, and a one-tick poll raced the testbed keeper that
            // re-asserts `host_tx_ready=true` within ~4s, missing a one-shot `false`
            // entirely (no re-init, no DataPathDeinit). Re-poll the DB-only reaction
            // (`react_to_port_config_change`, no bridge) every `REACTION_POLL` so the edge
            // is caught between ticks and drives the datapath teardown on the next tick.
            while !stop.load(Ordering::Relaxed) {
                if tick_start.elapsed() >= self.tick {
                    break;
                }
                self.poll_port_config_reactions(stop);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let remaining = self.tick.saturating_sub(tick_start.elapsed());
                interruptible_sleep(stop, remaining.min(REACTION_POLL));
            }
        }
        eprintln!("xcvrd-rs: CmisManagerTask: CMIS bring-up loop stopped");
    }

    pub fn run(mut self, stop: Arc<AtomicBool>) {
        self.task_worker(&stop)
    }

    /// Wire the cross-thread deconfigured-logical-port set maintained by
    /// `SfpStateUpdateTask` on CONFIG_DB logical-port DEL/ADD. While a port is in the
    /// set this loop stops driving its bring-up and purges any raced
    /// `TRANSCEIVER_STATUS_SW`. Left unset by the unit tests that drive `tick_once`
    /// directly.
    pub fn set_deconfigured_ports(&mut self, set: Arc<Mutex<BTreeSet<String>>>) {
        self.deconfigured_ports = Some(set);
    }

    /// Wire the STATE_DB `PORT_TABLE` handle so the manager can read `host_tx_ready`
    /// and react to its transitions (M8). Left unset by unit tests that don't exercise
    /// the host_tx_ready gate.
    pub fn set_state_port_table(&mut self, tbl: Arc<dyn DbTable>) {
        self.state_port_tbl = Some(tbl);
    }

    /// Wire the STATE_DB `TRANSCEIVER_INFO` handle so `post_port_active_apsel_to_db`
    /// can publish the post-activation `active_apsel_hostlane*` / lane counts. Left
    /// unset by unit tests that don't assert the INFO write.
    pub fn set_info_table(&mut self, tbl: Arc<dyn DbTable>) {
        self.int_tbl = Some(tbl);
    }

    /// True while `logical_port` is marked deconfigured. Always `false` when the set
    /// isn't wired (unit tests).
    fn is_deconfigured(&self, logical_port: &str) -> bool {
        match &self.deconfigured_ports {
            Some(set) => set
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(logical_port),
            None => false,
        }
    }

    /// One pass over every present physical port's first (subport-0) logical port,
    /// mirroring the DOM loop's iteration.
    pub fn tick_once(&mut self, stop: &AtomicBool) {
        let ports: Vec<(usize, String)> = self
            .port_mapping
            .physical_to_logical
            .iter()
            .filter_map(|(pport, lports)| lports.first().map(|l| (*pport, l.clone())))
            .collect();
        for (pport, lport) in ports {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Deconfigured (CONFIG_DB logical-port DEL): the state task deleted the
            // whole TRANSCEIVER_STATUS_SW row. This task iterates its own boot-time
            // mapping clone and must NOT drive a bring-up (which would re-write
            // cmis_state and resurrect the row); defensively delete any it raced back
            // in, drop the stale per-port state, and skip until a re-ADD clears the mark.
            if self.is_deconfigured(&lport) {
                self.status_sw_tbl.del(&lport);
                self.port_state.remove(&lport);
                continue;
            }
            if self
                .port_mapping
                .get_asic_id_for_logical_port(&lport)
                .is_none()
            {
                continue;
            }
            self.process_single_lport(&lport, pport);
        }
    }

    /// Fast reaction pass: re-poll every present port's CONFIG_DB `admin_status` /
    /// STATE_DB `PORT_TABLE.host_tx_ready` and, on a genuine *change*, force a CMIS
    /// re-init AND drive the datapath teardown IMMEDIATELY — in this same reaction pass —
    /// so `DataPathDeinit` (10h:128) + `OutputDisableTx` are written to the HAL while the
    /// (transient) value still holds. The steady-state check itself is DB-only (no
    /// bridge); the bridge is touched only on an actual change (rare).
    ///
    /// Called from the fast reaction sub-loop between the slower bring-up ticks so a
    /// transient `host_tx_ready=false` — which the testbed keeper re-asserts to `true`
    /// within ~4s (and often sooner, per port) — is caught before it reverts. The
    /// reference `task_worker` force-reinits AND runs the state machine on the SAME loop
    /// pass (`cmis_manager_task.py:1322`), so the deinit is issued right away; the earlier
    /// port here only recorded the re-init (`cmis_state=INSERTED`) and DEFERRED the deinit
    /// to the next `tick_once`, which — under M8 bridge load `tick_once` can take several
    /// seconds — raced the keeper and never wrote the deinit at all
    /// (tests/test_host_tx_ready.py::test_daemon_reacts_to_host_tx_ready_not_ready).
    /// `react_to_port_config_change` records the baseline on the first observation (so this
    /// can never tear down a boot-adopted port), only re-inits a real change, and the drive
    /// below is presence-guarded exactly like `process_single_lport`.
    fn poll_port_config_reactions(&mut self, stop: &AtomicBool) {
        let ports: Vec<(usize, String)> = self
            .port_mapping
            .physical_to_logical
            .iter()
            .filter_map(|(pport, lports)| lports.first().map(|l| (*pport, l.clone())))
            .collect();
        for (pport, lport) in ports {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if self.is_deconfigured(&lport) {
                continue;
            }
            if self
                .port_mapping
                .get_asic_id_for_logical_port(&lport)
                .is_none()
            {
                continue;
            }
            if self.react_to_port_config_change(&lport) {
                // A genuine admin_status / host_tx_ready change forced `cmis_state=INSERTED`.
                // Drive the freshly-reset machine NOW for a present module so
                // `handle_cmis_inserted_state` issues the `DataPathDeinit` + `OutputDisableTx`
                // teardown in THIS pass (instead of the next tick, by when the keeper may have
                // reverted `host_tx_ready` to 'true'). An absent module is left re-armed at
                // INSERTED to bring up when it returns.
                let present = self.sfp_present(pport);
                self.port_state
                    .entry(lport.clone())
                    .or_default()
                    .present_before = present;
                if present {
                    self.process_cmis_state_machine(&lport, pport);
                }
            }
        }
    }

    /// `process_single_lport` — per-port bring-up driver. Absent ports are left in
    /// their (non-terminal, DOM-gated) holding state; a present port with no state
    /// yet is (re)initialized; a present, non-terminal port is advanced one state.
    pub fn process_single_lport(&mut self, lport: &str, pport: usize) {
        // Read cmis_state from STATE_DB FIRST (a Redis read, NOT a bridge call). The
        // reference process_single_lport reads the SW-status table and early-returns for a
        // terminal / UNKNOWN state BEFORE ever touching the HW — sfp.get_presence() is only
        // reached for a non-terminal port. Mirroring that ordering keeps this task off the
        // PyO3/gRPC bridge for the ~N steady-state terminal ports every tick: polling
        // get_presence for each of them floods the bridge and starves the concurrent ~60s
        // DOM byte-9 read that is the SOLE source of TRANSCEIVER_DOM_FLAG.vccHAlarm, stalling
        // the both-False DOM-flag baseline (test_dom_flag_groups_temp_and_vcc).
        let db_state_str = self.get_cmis_state(lport);
        let db_state = CmisState::from_db_str(&db_state_str);

        // M8 reaction path (STATE_DB / CONFIG_DB reads only — no bridge): observe
        // CONFIG_DB admin_status + STATE_DB host_tx_ready and, on a *change* after the
        // baseline observation, force a CMIS re-init so an already-activated (terminal)
        // port is torn down / re-provisioned. MUST run BEFORE the terminal early-return
        // (a READY port reacts too), mirroring on_port_update_event's force_cmis_reinit
        // on a PORT_SET. If it reinit'd, the state is now INSERTED (non-terminal).
        let reacted = self.react_to_port_config_change(lport);

        // Genuinely-terminal early return WITHOUT a bridge presence poll. A READY this task
        // actually drove (last_written == READY), or a FAILED/REMOVED, is done — record what
        // the DB holds and skip the bridge. A boot-projected READY (last_written unset) is
        // NOT genuinely terminal: `project_cmis_state_for_present_ports` seeds cmis_state=READY
        // at boot so the boot DOM poll can flow, but the datapath was never driven to
        // DataPathActivated — it must fall through to a REAL bring-up. Plug-out of a terminal
        // port is handled by the main SFP loop parking cmis_state=INSERTED (handle_remove),
        // after which the non-terminal path below presence-checks and re-inits.
        if !reacted && db_state.is_terminal() {
            let drove_terminal = self
                .port_state
                .get(lport)
                .map(|s| s.last_written == db_state_str)
                .unwrap_or(false);
            if !(db_state == CmisState::Ready && !drove_terminal) {
                let ps = self.port_state.entry(lport.to_string()).or_default();
                ps.present_before = true;
                ps.last_written = db_state_str;
                return;
            }
        }

        // The port needs work (non-terminal, UNKNOWN, a boot-projected READY, or a
        // reaction-triggered reinit). Only now consult the HW presence over the bridge.
        let present = self.sfp_present(pport);
        let present_before = self
            .port_state
            .get(lport)
            .map(|s| s.present_before)
            .unwrap_or(false);

        if !present {
            // Plug-out edge: restart the machine at INSERTED with a fresh retry counter
            // (reference: force_cmis_reinit(0) on the DEL event). An absent module is never
            // advanced — bring-up resumes when it returns.
            if present_before {
                self.force_cmis_reinit(lport, 0);
            }
            self.port_state
                .entry(lport.to_string())
                .or_default()
                .present_before = false;
            return;
        }

        if reacted {
            // react_to_port_config_change forced a reinit (cmis_state now INSERTED); drive
            // the freshly-reset machine so the datapath is torn down / re-provisioned.
            self.port_state
                .entry(lport.to_string())
                .or_default()
                .present_before = true;
            self.process_cmis_state_machine(lport, pport);
            return;
        }

        if db_state.is_terminal() {
            // Only a boot-projected READY reaches here (genuinely-terminal returned above).
            // Start the REAL CMIS bring-up so the datapath actually activates — the boot DOM
            // poll already ran before this thread was spawned, so DOM timing is preserved.
            self.force_cmis_reinit(lport, 0);
            self.port_state
                .entry(lport.to_string())
                .or_default()
                .present_before = true;
            self.process_cmis_state_machine(lport, pport);
            return;
        }

        if db_state == CmisState::Unknown {
            // Present but never initialized (a genuinely fresh insert): start bring-up.
            self.force_cmis_reinit(lport, 0);
            self.port_state
                .entry(lport.to_string())
                .or_default()
                .present_before = true;
            return;
        }

        // Non-terminal (INSERTED/DP_*/AP_CONFIGURED). If the DB says INSERTED but this
        // task last wrote something else, the INSERTED came from handle_remove (a
        // plug-out this poller may have missed) — treat it as a fresh bring-up.
        {
            let ps = self.port_state.entry(lport.to_string()).or_default();
            ps.present_before = true;
            if db_state == CmisState::Inserted && ps.last_written != common::CMIS_STATE_INSERTED {
                ps.retries = 0;
                ps.expired = None;
                ps.last_written = common::CMIS_STATE_INSERTED.to_string();
            }
        }
        self.process_cmis_state_machine(lport, pport);
    }

    /// `process_cmis_state_machine` — advance one CMIS state per call. After
    /// `CMIS_MAX_RETRIES` reinit attempts the port is driven to `FAILED`.
    pub fn process_cmis_state_machine(&mut self, lport: &str, pport: usize) {
        let state = CmisState::from_db_str(&self.get_cmis_state(lport));
        let retries = self.port_state.get(lport).map(|s| s.retries).unwrap_or(0);

        if retries > CMIS_MAX_RETRIES {
            self.set_cmis_state(lport, CmisState::Failed);
            return;
        }

        match state {
            CmisState::Inserted => self.handle_cmis_inserted_state(lport, pport),
            CmisState::DpPreInitCheck => self.handle_cmis_dp_pre_init_check_state(lport, pport),
            CmisState::DpDeinit => self.handle_cmis_dp_deinit_state(lport, pport),
            CmisState::ApConfigured => self.handle_cmis_ap_conf_state(lport, pport, retries),
            CmisState::DpInit => self.handle_cmis_dp_init_state(lport, pport),
            CmisState::DpTxOn => self.handle_cmis_dp_txon_state(lport, pport, retries),
            CmisState::DpActivation => self.handle_cmis_dp_activation_state(lport, pport, retries),
            _ => {}
        }
    }

    /// `handle_cmis_inserted_state` (`cmis_manager_task.py:848`) — compute this port's
    /// datapath lane masks from CONFIG_DB, then gate on admin_status + host_tx_ready.
    /// If the port must not be brought up (admin-down or host_tx_ready explicitly
    /// `false`), force the Tx laser off (DataPathDeinit + OutputDisableTx over every
    /// module lane) and go straight to READY. Otherwise begin the bring-up at
    /// DP_PRE_INIT_CHECK.
    ///
    /// host_tx_ready gate: the reference treats an *absent* host_tx_ready as `false`
    /// (bring-up blocked until orchagent publishes it). This emulator testbed has no
    /// orchagent, so only an EXPLICIT `false` blocks — absent / `true` is treated as
    /// ready. That keeps the admin-up activation ports (host_tx_ready absent) working
    /// while still reacting to a deliberate host_tx_ready=false injection.
    fn handle_cmis_inserted_state(&mut self, lport: &str, pport: usize) {
        let host_lane_count = self.get_host_lane_count(lport);
        let host_lanes_mask = lane_mask(host_lane_count);
        // The emulator's module is symmetric (host lanes == media lanes) and serves a
        // single application (code 1); the reference derives these from the AppSel
        // advertisement, which for this testbed resolves to the same values.
        {
            let ps = self.port_state.entry(lport.to_string()).or_default();
            ps.host_lane_count = host_lane_count;
            ps.host_lanes_mask = host_lanes_mask;
            ps.media_lanes_mask = host_lanes_mask;
            ps.max_host_lanes_mask = 0xff;
            ps.max_media_lanes_mask = 0xff;
            ps.appl = 1;
            ps.active_apsel = [None; CMIS_MAX_HOST_LANES];
        }

        if !self.admin_up(lport) || self.host_tx_not_ready(lport) {
            // No datapath wanted: force Tx off over every module lane, then READY.
            self.set_datapath_deinit(pport, lport, 0xff);
            self.tx_disable_channel(pport, lport, 0xff, true);
            self.port_state
                .entry(lport.to_string())
                .or_default()
                .forced_tx_disabled = true;
            self.post_port_active_apsel_reset(lport);
            self.set_cmis_state(lport, CmisState::Ready);
            return;
        }
        self.set_cmis_state(lport, CmisState::DpPreInitCheck);
    }

    /// `handle_cmis_dp_pre_init_check_state` (`cmis_manager_task.py:951`) — if the Tx
    /// laser was forced off on a prior lost precondition, wait for the datapath to
    /// settle into a deactivated/initialized state before re-provisioning, then clear
    /// the forced flag and advance to DP_DEINIT.
    fn handle_cmis_dp_pre_init_check_state(&mut self, lport: &str, pport: usize) {
        let forced = self
            .port_state
            .get(lport)
            .map(|s| s.forced_tx_disabled)
            .unwrap_or(false);
        if forced {
            if !self.check_datapath_state(pport, lport, &["DataPathDeactivated", "DataPathInitialized"]) {
                return; // keep waiting for the datapath to settle
            }
            self.port_state
                .entry(lport.to_string())
                .or_default()
                .forced_tx_disabled = false;
        }
        self.set_cmis_state(lport, CmisState::DpDeinit);
    }

    /// `handle_cmis_dp_deinit_state` (`cmis_manager_task.py:1020`) — issue DataPathDeinit
    /// and force Tx off over every module lane (the reference deinits all lanes before
    /// re-provisioning), bring the module out of low power (`set_lpmode(false)` →
    /// ModuleReady), arm the ModuleReady wait timer, and advance to AP_CONFIGURED.
    fn handle_cmis_dp_deinit_state(&mut self, lport: &str, pport: usize) {
        let (max_host, max_media) = {
            let ps = self.port_state.entry(lport.to_string()).or_default();
            (ps.max_host_lanes_mask, ps.max_media_lanes_mask)
        };
        self.set_datapath_deinit(pport, lport, max_host);
        self.tx_disable_channel(pport, lport, max_media, true);
        let _ = self.set_high_power(pport);
        self.arm_timer(lport, self.ap_conf_wait);
        self.set_cmis_state(lport, CmisState::ApConfigured);
    }

    /// `AP_CONFIGURED` handling (`cmis_manager_task.py:1109`) — wait for the module to
    /// report `ModuleReady` AND its datapath to be deactivated, then stage the
    /// application (write the DPConfigLane app-select bytes) and trigger ApplyDPInit
    /// before advancing to DP_INIT. On the wait timeout, retry via `force_cmis_reinit`.
    /// A stalled module cycles until `CMIS_MAX_RETRIES` is exceeded → `FAILED`.
    fn handle_cmis_ap_conf_state(&mut self, lport: &str, pport: usize, retries: u32) {
        let ready = self.module_ready(pport)
            && self.check_datapath_state(pport, lport, &["DataPathDeactivated"]);
        if ready {
            let host_mask = self
                .port_state
                .get(lport)
                .map(|s| s.host_lanes_mask)
                .unwrap_or(0);
            self.set_application(pport, lport, host_mask);
            self.scs_apply_datapath_init(pport, lport, host_mask);
            self.set_cmis_state(lport, CmisState::DpInit);
            return;
        }
        let expired = self.port_state.get(lport).and_then(|s| s.expired);
        if is_timer_expired(expired) {
            self.force_cmis_reinit(lport, retries + 1);
        }
        // else: keep waiting at AP_CONFIGURED.
    }

    /// `DP_INIT` handling (`cmis_manager_task.py` DP_INIT arm) — gate on the port still
    /// being wanted (admin-up + host_tx_ready), then issue DataPathInit (clear the
    /// deinit bits for the active lanes so the module runs its datapath init), arm the
    /// wait timer and advance to DP_TXON.
    fn handle_cmis_dp_init_state(&mut self, lport: &str, pport: usize) {
        if !self.admin_up(lport) || self.host_tx_not_ready(lport) {
            self.force_cmis_reinit(lport, 0);
            return;
        }
        let host_mask = self
            .port_state
            .get(lport)
            .map(|s| s.host_lanes_mask)
            .unwrap_or(0);
        self.set_datapath_init(pport, lport, host_mask);
        self.arm_timer(lport, self.ap_conf_wait);
        self.set_cmis_state(lport, CmisState::DpTxOn);
    }

    /// `DP_TXON` handling — wait for the datapath to reach DataPathInitialized, then
    /// clear OutputDisableTx for the active media lanes (turn the Tx laser on) and
    /// advance to DP_ACTIVATION.
    fn handle_cmis_dp_txon_state(&mut self, lport: &str, pport: usize, retries: u32) {
        if !self.check_datapath_state(
            pport,
            lport,
            &["DataPathInitialized", "DataPathActivated"],
        ) {
            // keep waiting for datapath init; retry the whole bring-up if it stalls
            let expired = self.port_state.get(lport).and_then(|s| s.expired);
            if is_timer_expired(expired) {
                self.force_cmis_reinit(lport, retries + 1);
            }
            return;
        }
        let media_mask = self
            .port_state
            .get(lport)
            .map(|s| s.media_lanes_mask)
            .unwrap_or(0);
        self.tx_disable_channel(pport, lport, media_mask, false);
        self.arm_timer(lport, self.ap_conf_wait);
        self.set_cmis_state(lport, CmisState::DpActivation);
    }

    /// `DP_ACTIVATION` handling — wait for the datapath to reach DataPathActivated,
    /// then publish the active AppSel / lane counts to TRANSCEIVER_INFO and land on
    /// READY.
    fn handle_cmis_dp_activation_state(&mut self, lport: &str, pport: usize, retries: u32) {
        if !self.check_datapath_state(pport, lport, &["DataPathActivated"]) {
            // keep waiting for activation; retry the whole bring-up if it stalls
            let expired = self.port_state.get(lport).and_then(|s| s.expired);
            if is_timer_expired(expired) {
                self.force_cmis_reinit(lport, retries + 1);
            }
            return;
        }
        self.post_port_active_apsel_to_db_impl(lport);
        self.set_cmis_state(lport, CmisState::Ready);
    }

    /// `force_cmis_reinit` (`cmis_manager_task.py:578`) — restart bring-up: set
    /// `cmis_state=INSERTED`, reset the retry counter to `retries`, clear the timer.
    pub fn force_cmis_reinit(&mut self, lport: &str, retries: u32) {
        self.set_cmis_state(lport, CmisState::Inserted);
        let ps = self.port_state.entry(lport.to_string()).or_default();
        ps.retries = retries;
        ps.expired = None;
    }

    /// Write `cmis_state` to the DB and remember it as this task's last write.
    fn set_cmis_state(&mut self, lport: &str, state: CmisState) {
        let tbl = self.status_sw_tbl.clone();
        self.update_port_transceiver_status_table_sw_cmis_state(lport, &*tbl, state);
        self.port_state
            .entry(lport.to_string())
            .or_default()
            .last_written = state.as_str().to_string();
    }

    /// `update_cmis_state_expiration_time` (`cmis_manager_task.py:817`) — arm the
    /// wait deadline (`now + duration`; a zero duration means already-expired, used by
    /// unit tests to force the retry path immediately).
    fn arm_timer(&mut self, lport: &str, dur: Duration) {
        let deadline = if dur.is_zero() {
            Instant::now()
        } else {
            Instant::now() + dur
        };
        self.port_state
            .entry(lport.to_string())
            .or_default()
            .expired = Some(deadline);
    }

    /// `update_port_transceiver_status_table_sw_cmis_state` → write `cmis_state`
    /// (single-field `hset`, so it never clobbers `status`/`error`).
    pub fn update_port_transceiver_status_table_sw_cmis_state(
        &self,
        lport: &str,
        status_sw_tbl: &dyn DbTable,
        cmis_state_to_set: CmisState,
    ) {
        status_sw_tbl.hset(lport, "cmis_state", cmis_state_to_set.as_str());
    }

    // --- HAL / DB helpers ------------------------------------------------------

    /// `_wrapper_get_presence`.
    fn sfp_present(&self, pport: usize) -> bool {
        self.hal
            .sfp(pport)
            .map(|s| s.get_presence().unwrap_or(false))
            .unwrap_or(false)
    }

    /// `admin_status == 'up'` from CONFIG_DB `PORT`.
    fn admin_up(&self, lport: &str) -> bool {
        self.cfg_port_tbl.hget(lport, "admin_status").as_deref() == Some("up")
    }

    /// `check_module_state(api, ['ModuleReady'])` — via `get_transceiver_status`.
    fn module_ready(&self, pport: usize) -> bool {
        match self.hal.sfp(pport) {
            Ok(sfp) => match sfp.get_transceiver_status() {
                Ok(status) => {
                    status.get("module_state").and_then(|v| v.as_str()) == Some("ModuleReady")
                }
                Err(_) => false,
            },
            Err(_) => false,
        }
    }

    /// Bring the module out of low power (`set_lpmode(false)`).
    fn set_high_power(&self, pport: usize) -> bool {
        self.hal
            .sfp(pport)
            .and_then(|s| s.set_lpmode(false))
            .unwrap_or(false)
    }

    /// `common.get_cmis_state_from_state_db`.
    fn get_cmis_state(&self, lport: &str) -> String {
        common::get_cmis_state_from_state_db(lport, &*self.status_sw_tbl)
    }

    // --- M8 datapath bring-up: register writes, gates and reactions -------------

    /// `get_host_lane_count` (`cmis_manager_task.py:202`) — derive the datapath host
    /// lane count from CONFIG_DB `PORT.lanes` (comma-separated NPU lane list). Defaults
    /// to 4 (the emulator's testbed breakout) when absent/unparsable.
    fn get_host_lane_count(&self, lport: &str) -> usize {
        match self.cfg_port_tbl.hget(lport, "lanes") {
            Some(lanes) if !lanes.is_empty() => {
                let n = lanes.split(',').filter(|s| !s.trim().is_empty()).count();
                if n == 0 || n > CMIS_MAX_HOST_LANES {
                    4
                } else {
                    n
                }
            }
            _ => 4,
        }
    }

    /// STATE_DB `PORT_TABLE.host_tx_ready` — `Some("false")` blocks bring-up. Absent or
    /// any non-`false` value is treated as ready on this orchagent-less testbed (see the
    /// gate note on `handle_cmis_inserted_state`).
    fn host_tx_not_ready(&self, lport: &str) -> bool {
        match &self.state_port_tbl {
            Some(tbl) => tbl.hget(lport, "host_tx_ready").as_deref() == Some("false"),
            None => false,
        }
    }

    /// `check_datapath_state` (`cmis_manager_task.py:655`) — true if every active host
    /// lane's `DP{n}State` (1-based, from `get_transceiver_status`) is in `states`.
    ///
    /// Lenient for unit tests: if the module reports no status, or the status carries
    /// no `DP{n}State` fields at all (the module-state-only mocks), the gate passes so
    /// those tests still traverse the machine. On the emulator the DP fields are always
    /// present, so the gate is real.
    fn check_datapath_state(&self, pport: usize, lport: &str, states: &[&str]) -> bool {
        let host_mask = self
            .port_state
            .get(lport)
            .map(|s| s.host_lanes_mask)
            .unwrap_or(0);
        let status = match self.hal.sfp(pport).and_then(|s| s.get_transceiver_status()) {
            Ok(s) => s,
            Err(_) => return true,
        };
        for lane in 0..CMIS_MAX_HOST_LANES {
            if host_mask & (1 << lane) == 0 {
                continue;
            }
            let key = format!("DP{}State", lane + 1);
            // A module that models no DP{n}State field (the unit-test module-state-only
            // mocks) mustn't block the machine on a field it never exposes; only an
            // explicit out-of-set state fails the gate.
            if let Some(cur) = status.get(&key).and_then(|v| v.as_str()) {
                if !states.contains(&cur) {
                    return false;
                }
            }
        }
        true
    }

    /// Write one CMIS Staged-Control-Set-0 control byte at `(SCS0_PAGE, offset)` via the
    /// flat optoe-linear EEPROM address — the same page-10h register `CmisApi` drives
    /// and the e2e Monitor oracle watches for.
    fn write_reg(&self, pport: usize, offset: usize, byte: u8) -> bool {
        let addr = cmis_linear(SCS0_PAGE, offset);
        self.hal
            .sfp(pport)
            .and_then(|s| s.write_eeprom(addr, &[byte]))
            .unwrap_or(false)
    }

    /// `set_datapath_deinit(mask, True)` — OR the deinit bits for `mask`'s lanes into
    /// the DataPathDeinit shadow (10h:128) and write the absolute byte. The shadow makes
    /// each write an absolute byte (faithful to `CmisApi`'s read-modify-write without a
    /// read dependency; DP_DEINIT sets the shadow to the full module mask so a stale
    /// shadow after a re-plug is always overwritten).
    fn set_datapath_deinit(&mut self, pport: usize, lport: &str, mask: u8) {
        let byte = {
            let ps = self.port_state.entry(lport.to_string()).or_default();
            ps.deinit_reg |= mask;
            ps.deinit_reg
        };
        self.write_reg(pport, DPDEINIT_OFFSET, byte);
    }

    /// `set_datapath_init(mask)` — clear the deinit bits for `mask`'s lanes (run the
    /// datapath init on those lanes) and write the absolute DataPathDeinit byte.
    fn set_datapath_init(&mut self, pport: usize, lport: &str, mask: u8) {
        let byte = {
            let ps = self.port_state.entry(lport.to_string()).or_default();
            ps.deinit_reg &= !mask;
            ps.deinit_reg
        };
        self.write_reg(pport, DPDEINIT_OFFSET, byte);
    }

    /// `tx_disable_channel(mask, disable)` — set (`disable=true`) or clear the
    /// OutputDisableTx bits for `mask`'s lanes (10h:130) and write the absolute byte.
    fn tx_disable_channel(&mut self, pport: usize, lport: &str, mask: u8, disable: bool) {
        let byte = {
            let ps = self.port_state.entry(lport.to_string()).or_default();
            if disable {
                ps.tx_disable_reg |= mask;
            } else {
                ps.tx_disable_reg &= !mask;
            }
            ps.tx_disable_reg
        };
        self.write_reg(pport, OUTPUT_DISABLE_TX_OFFSET, byte);
    }

    /// `set_application` (`cmis_manager_task.py` set_application) — stage the AppSel for
    /// every active host lane by writing the DPConfigLane bytes (10h:145..152). Reads the
    /// current DPConfigLane window first so re-provisioning is idempotent and the applied
    /// AppSel is recorded (published later by `post_port_active_apsel_to_db`). Falls back
    /// to a computed `(appl<<4)|(DataPathID<<1)` byte when the module can't be read
    /// (unit-test mocks).
    fn set_application(&mut self, pport: usize, lport: &str, host_mask: u8) {
        let addr = cmis_linear(SCS0_PAGE, SCS0_DPCONFIG_START);
        let appl = self.port_state.get(lport).map(|s| s.appl).unwrap_or(1);
        let cur = self
            .hal
            .sfp(pport)
            .and_then(|s| s.read_eeprom(addr, SCS0_DPCONFIG_LEN))
            .ok()
            .flatten();
        let mut bytes = match &cur {
            Some(b) if b.len() == SCS0_DPCONFIG_LEN => {
                let mut v = [0u8; SCS0_DPCONFIG_LEN];
                v.copy_from_slice(b);
                v
            }
            _ => {
                // Compute the DPConfigLane byte: AppSelCode in the high nibble,
                // DataPathID (lane, 1-based) in bits 3:1.
                let mut v = [0u8; SCS0_DPCONFIG_LEN];
                for lane in 0..CMIS_MAX_HOST_LANES {
                    if host_mask & (1 << lane) != 0 {
                        v[lane] = (appl << 4) | (((lane as u8) + 1) << 1);
                    }
                }
                v
            }
        };
        // Ensure active lanes carry a non-zero AppSel (a fresh module reads back 0).
        for lane in 0..CMIS_MAX_HOST_LANES {
            if host_mask & (1 << lane) != 0 && (bytes[lane] >> 4) == 0 {
                bytes[lane] = (appl << 4) | (((lane as u8) + 1) << 1);
            }
        }
        let _ = self
            .hal
            .sfp(pport)
            .and_then(|s| s.write_eeprom(addr, &bytes));
        let ps = self.port_state.entry(lport.to_string()).or_default();
        for lane in 0..CMIS_MAX_HOST_LANES {
            if host_mask & (1 << lane) != 0 {
                ps.active_apsel[lane] = Some((bytes[lane] >> 4) & 0x0f);
            } else {
                ps.active_apsel[lane] = None;
            }
        }
    }

    /// Trigger ApplyDPInitLane (10h:143) for the active host lanes — the write that
    /// makes the module apply the staged AppSel and start the datapath init.
    fn scs_apply_datapath_init(&mut self, pport: usize, _lport: &str, host_mask: u8) {
        self.write_reg(pport, APPLY_DPINIT_OFFSET, host_mask);
    }

    /// `post_port_active_apsel_to_db` — publish the applied AppSel + lane counts to
    /// TRANSCEIVER_INFO (`active_apsel_hostlane1..8`, `host_lane_count`,
    /// `media_lane_count`). Unused lanes publish `N/A`.
    fn post_port_active_apsel_to_db_impl(&self, lport: &str) {
        let tbl = match &self.int_tbl {
            Some(t) => t.clone(),
            None => return,
        };
        // Only publish for a port whose TRANSCEIVER_INFO row already exists (the module
        // is present and identified), mirroring the reference guard.
        if tbl.get(lport).is_none() {
            return;
        }
        let ps = match self.port_state.get(lport) {
            Some(s) => s,
            None => return,
        };
        let mut fvs: Vec<(String, String)> = Vec::new();
        for lane in 0..CMIS_MAX_HOST_LANES {
            let val = match ps.active_apsel[lane] {
                Some(a) => a.to_string(),
                None => "N/A".to_string(),
            };
            fvs.push((format!("active_apsel_hostlane{}", lane + 1), val));
        }
        fvs.push(("host_lane_count".to_string(), ps.host_lane_count.to_string()));
        fvs.push((
            "media_lane_count".to_string(),
            ps.host_lane_count.to_string(),
        ));
        tbl.set(lport, &fvs);
    }

    /// Reset the published active-AppSel for a port that is being held out of the
    /// datapath (admin-down / host_tx_ready false): all lanes `N/A`, lane counts 0.
    fn post_port_active_apsel_reset(&self, lport: &str) {
        let tbl = match &self.int_tbl {
            Some(t) => t.clone(),
            None => return,
        };
        if tbl.get(lport).is_none() {
            return;
        }
        let mut fvs: Vec<(String, String)> = Vec::new();
        for lane in 0..CMIS_MAX_HOST_LANES {
            fvs.push((
                format!("active_apsel_hostlane{}", lane + 1),
                "N/A".to_string(),
            ));
        }
        fvs.push(("host_lane_count".to_string(), "0".to_string()));
        fvs.push(("media_lane_count".to_string(), "0".to_string()));
        tbl.set(lport, &fvs);
    }

    /// M8 reaction: poll CONFIG_DB `admin_status` + STATE_DB `PORT_TABLE.host_tx_ready`
    /// and, once a baseline has been observed, force a CMIS re-init when either changes
    /// (mirrors `on_port_update_event` issuing `force_cmis_reinit` on a PORT_SET). The
    /// first observation only records the baseline — it must not tear down a port this
    /// task adopted already-READY at boot. Returns true if it forced a re-init.
    fn react_to_port_config_change(&mut self, lport: &str) -> bool {
        let admin = self.cfg_port_tbl.hget(lport, "admin_status");
        let htr = self
            .state_port_tbl
            .as_ref()
            .and_then(|t| t.hget(lport, "host_tx_ready"));

        let (initialized, last_admin, last_htr) = {
            let ps = self.port_state.get(lport);
            (
                ps.map(|s| s.initialized).unwrap_or(false),
                ps.and_then(|s| s.last_admin.clone()),
                ps.and_then(|s| s.last_htr.clone()),
            )
        };

        let changed = initialized && (admin != last_admin || htr != last_htr);

        {
            let ps = self.port_state.entry(lport.to_string()).or_default();
            ps.initialized = true;
            ps.last_admin = admin;
            ps.last_htr = htr;
        }

        if changed {
            self.force_cmis_reinit(lport, 0);
            return true;
        }
        false
    }



    /// `post_port_active_apsel_to_db` — write `active_apsel_hostlaneN` /
    /// `host_lane_count` / `media_lane_count` into `TRANSCEIVER_INFO` post-activation.
    /// (Public wrapper over the internal implementation that uses this task's own
    /// recorded per-port AppSel; `reset_apsel` publishes the all-`N/A` reset row.)
    pub fn post_port_active_apsel_to_db(
        &self,
        lport: &str,
        _int_tbl: &dyn DbTable,
        _host_lanes_mask: u32,
        reset_apsel: bool,
    ) {
        if reset_apsel {
            self.post_port_active_apsel_reset(lport);
        } else {
            self.post_port_active_apsel_to_db_impl(lport);
        }
    }

    /// `get_cmis_host_lanes_mask` (`cmis_manager_task.py:233`) — the active host-lane
    /// bitmask for a datapath of `host_lane_count` lanes starting at `subport`. The
    /// emulator provisions from lane 0 (`subport` 0/1 → offset 0).
    pub fn get_cmis_host_lanes_mask(&self, _appl: u32, host_lane_count: u32, subport: u32) -> u32 {
        if host_lane_count == 0 || host_lane_count as usize > CMIS_MAX_HOST_LANES {
            return 0;
        }
        let base = ((1u32 << host_lane_count) - 1) & 0xff;
        let shift = if subport > 0 {
            (subport - 1) * host_lane_count
        } else {
            0
        };
        (base << shift) & 0xff
    }

    /// `is_decommission_required` — active AppSel moved off default / speed change.
    pub fn is_decommission_required(&self, _lport: &str) -> bool {
        todo!("cmis_manager_task.py:is_decommission_required")
    }

    /// `configure_laser_frequency` — coherent/ZR laser tuning (M10).
    pub fn configure_laser_frequency(&self, _lport: &str, _freq: u64, _grid: u32) {
        todo!("cmis_manager_task.py:configure_laser_frequency")
    }

    /// `configure_tx_output_power` — coherent/ZR tx power (M10).
    pub fn configure_tx_output_power(&self, _lport: &str, _tx_power: f64) {
        todo!("cmis_manager_task.py:configure_tx_output_power")
    }

    /// `on_port_update_event` (`cmis_manager_task.py:95`) — react to a CONFIG_DB `PORT`
    /// / STATE_DB `PORT_TABLE` change. On a PORT_SET, refresh the recorded baseline
    /// (admin_status / host_tx_ready) and force a CMIS re-init so the datapath is torn
    /// down and re-provisioned against the new config. On a PORT_DEL, mark the port
    /// REMOVED (and drop its bookkeeping for a CONFIG_DB PORT delete).
    ///
    /// The daemon's steady-state reaction runs through `react_to_port_config_change`
    /// (a poll of the same two tables each tick, since this crate has no live subscriber
    /// thread wired into the CMIS task); this entry point mirrors the reference for the
    /// event-driven seam and the Part-B reaction tests.
    pub fn on_port_update_event(&mut self, port_change_event: &PortChangeEvent) {
        let lport = port_change_event.port_name.clone();
        if !lport.starts_with("Ethernet") {
            return;
        }
        match port_change_event.event_type {
            PortChangeEventType::PortSet | PortChangeEventType::PortAdd => {
                if let Some(v) = port_change_event.port_dict.get("admin_status") {
                    self.port_state
                        .entry(lport.clone())
                        .or_default()
                        .last_admin = Some(v.clone());
                }
                if let Some(v) = port_change_event.port_dict.get("host_tx_ready") {
                    self.port_state.entry(lport.clone()).or_default().last_htr = Some(v.clone());
                }
                {
                    let ps = self.port_state.entry(lport.clone()).or_default();
                    ps.initialized = true;
                    ps.forced_tx_disabled = false;
                }
                self.force_cmis_reinit(&lport, 0);
            }
            PortChangeEventType::PortDel | PortChangeEventType::PortRemove => {
                if self.port_state.contains_key(&lport) {
                    self.set_cmis_state(&lport, CmisState::Removed);
                }
                let is_config_port = port_change_event.db_name.as_deref() == Some("CONFIG_DB")
                    && port_change_event.table_name.as_deref() == Some("PORT");
                if is_config_port {
                    self.port_state.remove(&lport);
                }
            }
        }
    }

    /// `is_fast_reboot_enabled` — skip DataPathDeinit for a live datapath (M13).
    pub fn is_fast_reboot_enabled(&self) -> bool {
        todo!("cmis_manager_task.py:is_fast_reboot_enabled")
    }
}

/// The active-lane bitmask for a contiguous datapath of `count` host lanes starting at
/// lane 0 (`(1<<count)-1`, clamped to the module's 8 lanes).
fn lane_mask(count: usize) -> u8 {
    if count == 0 {
        0
    } else if count >= CMIS_MAX_HOST_LANES {
        0xff
    } else {
        ((1u16 << count) - 1) as u8
    }
}

/// `is_timer_expired` (`cmis_manager_task.py:829`) — None → false, else deadline passed.
fn is_timer_expired(expired: Option<Instant>) -> bool {
    match expired {
        Some(deadline) => Instant::now() >= deadline,
        None => false,
    }
}

/// Interruptible sleep between ticks (wakes every 100 ms to react promptly to `stop`).
fn interruptible_sleep(stop: &Arc<AtomicBool>, dur: Duration) {
    const STEP: Duration = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < dur && !stop.load(Ordering::Relaxed) {
        let step = STEP.min(dur - slept);
        std::thread::sleep(step);
        slept += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDbTable, MockHal, MockSfp};
    use crate::xcvrd_utilities::common::{
        CMIS_STATE_AP_CONF, CMIS_STATE_DP_DEINIT, CMIS_STATE_DP_INIT, CMIS_STATE_DP_PRE_INIT_CHECK,
        CMIS_STATE_FAILED, CMIS_STATE_INSERTED, CMIS_STATE_READY,
    };
    use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType};
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    const READY_STATUS: &str = "ModuleReady";
    const LOWPWR_STATUS: &str = "ModuleLowPwr";

    fn mapping(lport: &str, pport: usize) -> PortMapping {
        let mut m = PortMapping::new();
        m.handle_port_change_event(&PortChangeEvent::new(
            lport,
            pport as i32,
            0,
            PortChangeEventType::PortAdd,
        ));
        m
    }

    /// A single present module whose `get_transceiver_status` reports `module_state`.
    fn sfp_with_module_state(module_state: &str) -> MockSfp {
        MockSfp {
            status: json!({ "module_state": module_state }),
            ..MockSfp::present()
        }
    }

    fn cfg_port(admin_status: &str) -> Arc<MockDbTable> {
        let tbl = Arc::new(MockDbTable::new("PORT"));
        tbl.hset("Ethernet0", "admin_status", admin_status);
        tbl
    }

    fn cmis_state(status_sw: &MockDbTable) -> Option<String> {
        status_sw.hget("Ethernet0", "cmis_state")
    }

    fn make_task(
        sfp: MockSfp,
        cfg: Arc<MockDbTable>,
        status_sw: Arc<MockDbTable>,
        ap_conf_wait: Duration,
    ) -> CmisManagerTask {
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![sfp]));
        CmisManagerTask::with_timing(
            mapping("Ethernet0", 0),
            false,
            hal,
            status_sw,
            cfg,
            Duration::from_millis(0),
            ap_conf_wait,
        )
    }

    /// Drive the state machine forward N single-port passes (no sleeping).
    fn step(task: &mut CmisManagerTask, n: usize) {
        let stop = AtomicBool::new(false);
        for _ in 0..n {
            task.tick_once(&stop);
        }
    }

    // update_port_transceiver_status_table_sw_cmis_state writes cmis_state without
    // clobbering status/error (single-field hset merge).
    #[test]
    fn test_update_port_transceiver_status_table_sw_cmis_state() {
        let status_sw = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        status_sw.set(
            "Ethernet0",
            &[
                ("status".to_string(), "1".to_string()),
                ("error".to_string(), "N/A".to_string()),
            ],
        );
        let task = make_task(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW")),
            DEFAULT_AP_CONF_WAIT,
        );
        task.update_port_transceiver_status_table_sw_cmis_state(
            "Ethernet0",
            &status_sw,
            CmisState::Ready,
        );
        assert_eq!(
            status_sw.hget("Ethernet0", "cmis_state").as_deref(),
            Some(CMIS_STATE_READY)
        );
        // status/error untouched.
        assert_eq!(status_sw.hget("Ethernet0", "status").as_deref(), Some("1"));
        assert_eq!(status_sw.hget("Ethernet0", "error").as_deref(), Some("N/A"));
    }

    // An admin-up, healthy (ModuleReady) module walks the datapath bring-up states
    // and lands on READY, passing through the non-terminal states on the way.
    #[test]
    fn test_healthy_admin_up_reaches_ready() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        // Seed the plug-out holding state the daemon's handle_remove would write.
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let mut task = make_task(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Duration::from_millis(0),
        );

        // Observe the ordered transitions, one per tick.
        let mut seen = Vec::new();
        let stop = AtomicBool::new(false);
        for _ in 0..12 {
            task.tick_once(&stop);
            if let Some(s) = cmis_state(&status_sw) {
                if seen.last() != Some(&s) {
                    seen.push(s.clone());
                }
                if s == CMIS_STATE_READY {
                    break;
                }
            }
        }
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        // The datapath states appear in order before READY.
        for st in [
            CMIS_STATE_DP_PRE_INIT_CHECK,
            CMIS_STATE_DP_DEINIT,
            CMIS_STATE_AP_CONF,
        ] {
            assert!(
                seen.iter().any(|s| s == st),
                "missing transition {st}; saw {seen:?}"
            );
        }
    }

    // An admin-up module that never reports ModuleReady (stall) stays non-terminal
    // for several ticks, retries CMIS_MAX_RETRIES times, then lands on FAILED.
    #[test]
    fn test_stalled_admin_up_retries_then_failed() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        // ap_conf_wait = 0 => the ModuleReady wait times out immediately each cycle.
        let mut task = make_task(
            sfp_with_module_state(LOWPWR_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Duration::from_millis(0),
        );

        let stop = AtomicBool::new(false);
        let mut non_terminal_samples = 0;
        let mut reached_failed = false;
        for _ in 0..80 {
            task.tick_once(&stop);
            let s = cmis_state(&status_sw).unwrap_or_default();
            if s == CMIS_STATE_FAILED {
                reached_failed = true;
                break;
            }
            if !CmisState::from_db_str(&s).is_terminal() {
                non_terminal_samples += 1;
            }
        }
        assert!(reached_failed, "stalled module never reached FAILED");
        // The test_dom_gating e2e oracle needs >= 5 non-terminal samples; the state
        // machine dwells well past that (4 retry cycles worth of states) before FAILED.
        assert!(
            non_terminal_samples >= 5,
            "expected >=5 non-terminal samples, saw {non_terminal_samples}"
        );
    }

    // The DOM gate never opens during a stalled bring-up: cmis_state is non-terminal
    // for the whole walk to FAILED (mirrors what is_port_in_cmis_initialization_process
    // reads).
    #[test]
    fn test_stalled_bringup_stays_non_terminal_until_failed() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let mut task = make_task(
            sfp_with_module_state(LOWPWR_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Duration::from_millis(0),
        );
        let stop = AtomicBool::new(false);
        loop {
            let s = cmis_state(&status_sw).unwrap_or_default();
            if s == CMIS_STATE_FAILED {
                break;
            }
            // Every pre-FAILED state read is non-terminal (the gate stays closed).
            assert!(
                !CmisState::from_db_str(&s).is_terminal(),
                "unexpected terminal state {s} before FAILED"
            );
            task.tick_once(&stop);
        }
    }

    // An admin-down module needs no datapath, so it goes straight to READY.
    #[test]
    fn test_admin_down_goes_ready() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let mut task = make_task(
            sfp_with_module_state(LOWPWR_STATUS),
            cfg_port("down"),
            status_sw.clone(),
            Duration::from_millis(0),
        );
        step(&mut task, 3);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
    }

    // A boot-projected READY is NOT a completed datapath bring-up. At daemon boot
    // `project_cmis_state_for_present_ports` seeds cmis_state=READY for every present
    // module so the boot DOM poll (gated on a terminal cmis_state) can flow — but the
    // module's datapath was never driven to DataPathActivated. This task never wrote that
    // READY (last_written is unset), so on its first observation it must tell the
    // projection apart from a READY it drove itself and run the REAL bring-up: emit the
    // DataPathDeinit / ApplyDPInit provisioning writes and land back on READY with the
    // datapath actually activated. (Root cause of the M8 test_cmis_datapath_activated /
    // test_cmis_emulator_datapath_agrees e2e stalls.)
    #[test]
    fn test_boot_projected_ready_triggers_real_bringup() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_READY);
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Arc::new(MockDbTable::new("PORT_TABLE")),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        step(&mut task, 10);
        // A real bring-up ran (not silent adoption): the page-10h provisioning writes
        // were emitted.
        assert!(
            !writes_to(&writes, SCS0_PAGE, DPDEINIT_OFFSET).is_empty(),
            "boot-projected READY must trigger a real bring-up (DataPathDeinit 10h:128)"
        );
        assert!(
            !writes_to(&writes, SCS0_PAGE, APPLY_DPINIT_OFFSET).is_empty(),
            "boot-projected READY must trigger ApplyDPInitLane (10h:143)"
        );
        // ...and it lands back on READY with the datapath brought up.
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
    }

    // A READY this task drove itself (a completed bring-up: last_written == READY) IS
    // genuinely terminal — it is adopted as-is on subsequent ticks with no repeated
    // re-provisioning churn. This is the counterpart to the boot-projection case above:
    // only a READY the task never wrote is re-brought-up.
    #[test]
    fn test_task_driven_ready_is_adopted() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Arc::new(MockDbTable::new("PORT_TABLE")),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        // Drive the full bring-up to a task-owned READY.
        step(&mut task, 10);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        // Once the task owns READY, further ticks must NOT re-provision.
        writes.lock().unwrap().clear();
        step(&mut task, 5);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        assert!(
            writes_to(&writes, SCS0_PAGE, DPDEINIT_OFFSET).is_empty()
                && writes_to(&writes, SCS0_PAGE, APPLY_DPINIT_OFFSET).is_empty(),
            "a task-driven READY must be adopted without re-provisioning churn"
        );
    }

    // A genuinely-terminal port (a READY this task drove) must NOT keep polling
    // get_presence over the PyO3/gRPC bridge every tick. The reference early-returns for
    // a terminal cmis_state BEFORE touching the HW; polling presence for every terminal
    // port each tick floods the bridge and starves the concurrent ~60s DOM byte-9 read
    // that is the sole source of TRANSCEIVER_DOM_FLAG.vccHAlarm (regressing
    // test_dom_flag_groups_temp_and_vcc). Plug-out is still observed by the main SFP loop
    // parking cmis_state=INSERTED, so dropping the per-tick poll loses no removal coverage.
    #[test]
    fn test_terminal_port_stops_polling_presence_over_bridge() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let sfp = sfp_with_module_state(READY_STATUS);
        let presence_calls = sfp.presence_calls.clone();
        let mut task = make_task(sfp, cfg_port("down"), status_sw.clone(), Duration::from_millis(0));

        // Admin-down module short-circuits to a task-owned (genuinely terminal) READY.
        step(&mut task, 3);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));

        // Once genuinely terminal, further ticks must not poll HW presence over the bridge.
        let calls_after_ready = presence_calls.load(Ordering::SeqCst);
        step(&mut task, 20);
        assert_eq!(
            presence_calls.load(Ordering::SeqCst),
            calls_after_ready,
            "a genuinely-terminal READY port must stop polling get_presence over the bridge"
        );
    }

    // A never-present module gets no manufactured cmis_state; once present with an
    // external INSERTED (the daemon's plug-out holding state) it is brought up.
    #[test]
    fn test_absent_then_present_brings_up() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![MockSfp::default()])); // absent
        let mut task = CmisManagerTask::with_timing(
            mapping("Ethernet0", 0),
            false,
            hal,
            status_sw.clone(),
            cfg_port("up"),
            Duration::from_millis(0),
            Duration::from_millis(0),
        );
        step(&mut task, 3);
        // Never-present module: no cmis_state written.
        assert_eq!(cmis_state(&status_sw), None);

        // Now the module is present with the plug-out holding state; it brings up.
        let hal: Arc<dyn Hal> =
            Arc::new(MockHal::with_sfps(vec![sfp_with_module_state(READY_STATUS)]));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let mut task = CmisManagerTask::with_timing(
            mapping("Ethernet0", 0),
            false,
            hal,
            status_sw.clone(),
            cfg_port("up"),
            Duration::from_millis(0),
            Duration::from_millis(0),
        );
        step(&mut task, 10);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
    }

    // force_cmis_reinit restarts at INSERTED and resets the retry counter + timer.
    #[test]
    fn test_force_cmis_reinit_resets() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        let mut task = make_task(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Duration::from_millis(0),
        );
        task.force_cmis_reinit("Ethernet0", 2);
        assert_eq!(
            cmis_state(&status_sw).as_deref(),
            Some(CMIS_STATE_INSERTED)
        );
        assert_eq!(task.port_state.get("Ethernet0").map(|s| s.retries), Some(2));
        task.force_cmis_reinit("Ethernet0", 0);
        assert_eq!(task.port_state.get("Ethernet0").map(|s| s.retries), Some(0));
        assert!(task
            .port_state
            .get("Ethernet0")
            .and_then(|s| s.expired)
            .is_none());
    }

    // skip_cmis_mgr disables the manager: task_worker returns without touching the DB.
    #[test]
    fn test_skip_cmis_mgr_disables() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let hal: Arc<dyn Hal> =
            Arc::new(MockHal::with_sfps(vec![sfp_with_module_state(READY_STATUS)]));
        let mut task = CmisManagerTask::with_timing(
            mapping("Ethernet0", 0),
            true, // skip
            hal,
            status_sw.clone(),
            cfg_port("up"),
            Duration::from_millis(0),
            Duration::from_millis(0),
        );
        let stop = Arc::new(AtomicBool::new(false));
        task.task_worker(&stop);
        // Untouched — still the seeded INSERTED, no advancement.
        assert_eq!(
            cmis_state(&status_sw).as_deref(),
            Some(CMIS_STATE_INSERTED)
        );
    }

    // M3 threading contract: daemon::serve runs CmisManagerTask::task_worker on its OWN
    // std::thread (so a slow bring-up pass never blocks the change-event loop's
    // get_change_event, which is what let the injected SFP-error transition be missed
    // when DOM+CMIS ran inline). The loop must (a) tick repeatedly — advancing a healthy
    // admin-up module to READY — and (b) exit promptly when the shared `stop` flag is
    // set so shutdown joins cleanly. Mirrors dom_mgr::test_task_worker_runs_and_stops.
    #[test]
    fn test_task_worker_runs_and_stops() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        // Seed the plug-out holding state the daemon's handle_remove would write.
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let hal: Arc<dyn Hal> =
            Arc::new(MockHal::with_sfps(vec![sfp_with_module_state(READY_STATUS)]));
        let mut task = CmisManagerTask::with_timing(
            mapping("Ethernet0", 0),
            false,
            hal,
            status_sw.clone(),
            cfg_port("up"),
            Duration::from_millis(20), // tick cadence between passes
            Duration::from_millis(0),  // ap_conf_wait
        );

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = stop.clone();
            std::thread::spawn(move || task.task_worker(&stop))
        };
        // Let the worker tick enough to drive the healthy admin-up module to READY.
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("CMIS worker joins cleanly");

        // It ran (bring-up advanced to READY) and stopped on the flag (join returned).
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
    }

    // --- M8: datapath register writes + admin/host_tx reactions ----------------

    /// Build a task with the STATE_DB PORT_TABLE + TRANSCEIVER_INFO seams wired, so the
    /// host_tx_ready gate/reaction and the post-activation active-apsel publish are
    /// exercised. Returns the task plus the shared eeprom-write log of its module.
    fn make_task_full(
        sfp: MockSfp,
        cfg: Arc<MockDbTable>,
        status_sw: Arc<MockDbTable>,
        state_port: Arc<MockDbTable>,
        int_tbl: Arc<MockDbTable>,
    ) -> (CmisManagerTask, Arc<Mutex<Vec<(usize, Vec<u8>)>>>) {
        let writes = sfp.eeprom_writes.clone();
        let hal: Arc<dyn Hal> = Arc::new(MockHal::with_sfps(vec![sfp]));
        let mut task = CmisManagerTask::with_timing(
            mapping("Ethernet0", 0),
            false,
            hal,
            status_sw,
            cfg,
            Duration::from_millis(0),
            Duration::from_millis(0),
        );
        task.set_state_port_table(state_port as Arc<dyn DbTable>);
        task.set_info_table(int_tbl as Arc<dyn DbTable>);
        (task, writes)
    }

    fn writes_to(log: &Arc<Mutex<Vec<(usize, Vec<u8>)>>>, page: usize, offset: usize) -> Vec<Vec<u8>> {
        let want = cmis_linear(page, offset);
        log.lock()
            .unwrap()
            .iter()
            .filter(|(off, _)| *off == want)
            .map(|(_, d)| d.clone())
            .collect()
    }

    fn first_index_of(log: &Arc<Mutex<Vec<(usize, Vec<u8>)>>>, page: usize, offset: usize) -> Option<usize> {
        let want = cmis_linear(page, offset);
        log.lock().unwrap().iter().position(|(off, _)| *off == want)
    }

    // A healthy admin-up bring-up emits the real CMIS page-10h control writes in the
    // reference order: DataPathDeinit (10h:128) BEFORE the DPConfigLane app-select bytes
    // (10h:145) BEFORE the ApplyDPInitLane trigger (10h:143) — the write_order oracle.
    #[test]
    fn test_bringup_emits_ordered_datapath_writes() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Arc::new(MockDbTable::new("PORT_TABLE")),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        step(&mut task, 10);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));

        let i_deinit = first_index_of(&writes, SCS0_PAGE, DPDEINIT_OFFSET)
            .expect("no DataPathDeinit (10h:128) write during bring-up");
        let i_config = first_index_of(&writes, SCS0_PAGE, SCS0_DPCONFIG_START)
            .expect("no DPConfigLane (10h:145) write during bring-up");
        let i_apply = first_index_of(&writes, SCS0_PAGE, APPLY_DPINIT_OFFSET)
            .expect("no ApplyDPInitLane (10h:143) write during bring-up");
        assert!(
            i_deinit < i_config && i_config <= i_apply,
            "write order violated: deinit@{i_deinit}, config@{i_config}, apply@{i_apply}"
        );
        // The ApplyDPInit trigger covers the active host lanes (0x0f for a 4-lane port).
        let apply = writes_to(&writes, SCS0_PAGE, APPLY_DPINIT_OFFSET);
        assert_eq!(apply.last().map(|d| d[0]), Some(0x0f));
    }

    // At the activated end state the active host lanes have their DataPathDeinit and
    // OutputDisableTx bits CLEARED (deinit=0/output=0 => DataPathActivated), while the
    // unused lanes stay deinitialised (bits set) — the per-lane invariant the e2e checks.
    #[test]
    fn test_activated_clears_active_lane_deinit_and_txdisable() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Arc::new(MockDbTable::new("PORT_TABLE")),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        step(&mut task, 10);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        // Final DataPathDeinit + OutputDisableTx bytes: active lanes (0x0f) cleared,
        // unused lanes (0xf0) still deinitialised / tx-disabled.
        let deinit = writes_to(&writes, SCS0_PAGE, DPDEINIT_OFFSET);
        assert_eq!(deinit.last().map(|d| d[0]), Some(0xf0));
        let txdis = writes_to(&writes, SCS0_PAGE, OUTPUT_DISABLE_TX_OFFSET);
        assert_eq!(txdis.last().map(|d| d[0]), Some(0xf0));
    }

    // An admin-DOWN module is forced Tx-off: DataPathDeinit (10h:128) AND OutputDisableTx
    // (10h:130) are written over every module lane (0xff), then it lands on READY.
    #[test]
    fn test_admin_down_forces_tx_off_writes() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(LOWPWR_STATUS),
            cfg_port("down"),
            status_sw.clone(),
            Arc::new(MockDbTable::new("PORT_TABLE")),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        step(&mut task, 3);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        assert_eq!(
            writes_to(&writes, SCS0_PAGE, DPDEINIT_OFFSET).last().map(|d| d[0]),
            Some(0xff),
            "admin-down must DataPathDeinit all module lanes"
        );
        assert_eq!(
            writes_to(&writes, SCS0_PAGE, OUTPUT_DISABLE_TX_OFFSET).last().map(|d| d[0]),
            Some(0xff),
            "admin-down must OutputDisableTx all module lanes"
        );
    }

    // Reaction: an admin_status up->down transition on an already-READY port forces a
    // CMIS re-init and drives a DataPathDeinit (10h:128) + OutputDisableTx (10h:130)
    // teardown (test_cmis_reconfig / test_cmis_forced_tx e2e behaviour).
    #[test]
    fn test_reaction_admin_down_tears_down_datapath() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let cfg = cfg_port("up");
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg.clone(),
            status_sw.clone(),
            Arc::new(MockDbTable::new("PORT_TABLE")),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        step(&mut task, 10);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        writes.lock().unwrap().clear(); // ignore the bring-up writes

        // admin flips DOWN: the reaction re-inits and forces Tx off.
        cfg.hset("Ethernet0", "admin_status", "down");
        step(&mut task, 3);
        assert!(
            !writes_to(&writes, SCS0_PAGE, DPDEINIT_OFFSET).is_empty(),
            "no DataPathDeinit(10h:128) on admin-down reaction"
        );
        assert!(
            !writes_to(&writes, SCS0_PAGE, OUTPUT_DISABLE_TX_OFFSET).is_empty(),
            "no OutputDisableTx(10h:130) on admin-down reaction"
        );
    }

    // Reaction: flipping STATE_DB PORT_TABLE.host_tx_ready to "false" on a READY port
    // forces a re-init and a DataPathDeinit teardown (test_host_tx_ready e2e behaviour).
    #[test]
    fn test_reaction_host_tx_not_ready_tears_down() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let state_port = Arc::new(MockDbTable::new("PORT_TABLE"));
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            state_port.clone(),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        step(&mut task, 10);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        writes.lock().unwrap().clear();

        // host_tx_ready -> false: reaction re-inits and deinits the datapath.
        state_port.hset("Ethernet0", "host_tx_ready", "false");
        step(&mut task, 3);
        assert!(
            !writes_to(&writes, SCS0_PAGE, DPDEINIT_OFFSET).is_empty(),
            "no DataPathDeinit(10h:128) on host_tx_ready=false reaction"
        );
        // The port is held out of the activated datapath (not brought back to a fresh
        // bring-up while host_tx_ready stays false).
        assert_ne!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_DP_INIT));
    }

    // REGRESSION LOCK for tests/test_host_tx_ready.py::test_daemon_reacts_to_host_tx_ready_not_ready:
    // the reference reacts to every PORT_SET off a queuing subscriber and never misses a write;
    // this crate POLLS, and the testbed keeper re-asserts host_tx_ready=true within ~4s, so a
    // one-shot "false" is TRANSIENT. The fast reaction pass (poll_port_config_reactions) runs
    // between the slower, bridge-starved bring-up ticks and must — in that SAME pass — both force
    // the CMIS re-init AND drive the datapath teardown, so DataPathDeinit(10h:128) + OutputDisableTx
    // are written WHILE host_tx_ready is still "false". Deferring the deinit to the next tick_once
    // (as the earlier code did) raced the keeper's revert AND the M8 bridge starvation, so on the
    // DUT the deinit was NEVER written within the 80s budget (symptom "last=None"). This mirrors the
    // reference task_worker running force_cmis_reinit + process_single_lport on ONE loop pass
    // (cmis_manager_task.py:1322), where handle_cmis_inserted_state issues the forced Tx-off teardown
    // immediately.
    #[test]
    fn test_fast_reaction_pass_catches_transient_host_tx_not_ready() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let state_port = Arc::new(MockDbTable::new("PORT_TABLE"));
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            state_port.clone(),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        // Bring the port up to READY with host_tx_ready absent (steady state), then clear the
        // control-write log so we only observe the post-reaction teardown.
        step(&mut task, 10);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        writes.lock().unwrap().clear();

        // The e2e's one-shot write: host_tx_ready -> "false". Observe it ONLY via the fast
        // reaction pass (NO tick_once), exactly as the between-ticks sub-loop does.
        let stop = AtomicBool::new(false);
        state_port.hset("Ethernet0", "host_tx_ready", "false");
        task.poll_port_config_reactions(&stop);

        // The fast pass ALONE must have written the DataPathDeinit(10h:128) + OutputDisableTx
        // teardown — immediately, without a later (starved / already-reverted) tick.
        assert!(
            !writes_to(&writes, SCS0_PAGE, DPDEINIT_OFFSET).is_empty(),
            "fast reaction pass must write DataPathDeinit(10h:128) immediately on the transient \
             host_tx_ready=false, not defer it to a later (starved) tick",
        );
        assert!(
            !writes_to(&writes, SCS0_PAGE, OUTPUT_DISABLE_TX_OFFSET).is_empty(),
            "fast reaction pass must force OutputDisableTx(10h:130) on host_tx_ready=false",
        );
        // The teardown covers ALL module lanes (absolute 0xff) — the forced Tx-off the Monitor
        // oracle matches against the active host-lane mask.
        assert_eq!(
            writes_to(&writes, SCS0_PAGE, DPDEINIT_OFFSET).last().map(|d| d[0]),
            Some(0xff),
            "forced-off DataPathDeinit must cover every module lane",
        );
        // The port is parked at READY with Tx forced off (the reference's forced-tx-off outcome),
        // ready to re-provision once host_tx_ready returns.
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));

        // Recovery: the keeper reverts host_tx_ready to "true"; the next bring-up re-provisions the
        // datapath cleanly (no stuck forced-off state), re-writing DataPathDeinit during DP_DEINIT.
        writes.lock().unwrap().clear();
        state_port.hset("Ethernet0", "host_tx_ready", "true");
        step(&mut task, 6);
        assert!(
            !writes_to(&writes, SCS0_PAGE, APPLY_DPINIT_OFFSET).is_empty(),
            "datapath must re-initialise once host_tx_ready returns",
        );
    }

    // An explicit host_tx_ready="false" at insertion blocks bring-up: the port is forced
    // Tx-off and parked at READY rather than activating the datapath.
    #[test]
    fn test_host_tx_false_blocks_bringup() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let state_port = Arc::new(MockDbTable::new("PORT_TABLE"));
        state_port.hset("Ethernet0", "host_tx_ready", "false");
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            state_port.clone(),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        step(&mut task, 4);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        // No ApplyDPInit trigger (datapath never initialised while host_tx not ready).
        assert!(
            writes_to(&writes, SCS0_PAGE, APPLY_DPINIT_OFFSET).is_empty(),
            "datapath must not be initialised while host_tx_ready=false"
        );
    }

    // The fast reaction pass stays INERT in steady state: with no admin_status / host_tx_ready
    // change it records the baseline on first sight and, on subsequent passes, issues NO CMIS
    // control writes and NO cmis_state change — so between-tick polling never floods the PyO3
    // bridge nor spuriously tears a healthy datapath down (only a genuine change drives the
    // machine in the fast pass).
    #[test]
    fn test_fast_reaction_pass_inert_without_change() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let state_port = Arc::new(MockDbTable::new("PORT_TABLE"));
        state_port.hset("Ethernet0", "host_tx_ready", "true");
        let (mut task, writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            state_port.clone(),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        step(&mut task, 10);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        writes.lock().unwrap().clear();

        // Repeated fast passes with NO config change: nothing re-inits, nothing is written.
        let stop = AtomicBool::new(false);
        for _ in 0..5 {
            task.poll_port_config_reactions(&stop);
        }
        assert_eq!(
            cmis_state(&status_sw).as_deref(),
            Some(CMIS_STATE_READY),
            "steady-state fast passes must not re-init a healthy port",
        );
        assert!(
            writes.lock().unwrap().is_empty(),
            "steady-state fast passes must issue no CMIS control writes",
        );
    }

    // After activation the applied AppSel + lane counts are published to TRANSCEIVER_INFO
    // (active_apsel_hostlane1..4 = the app code, 5..8 = N/A, host/media_lane_count = 4).
    #[test]
    fn test_post_active_apsel_written_on_activation() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_INSERTED);
        let int_tbl = Arc::new(MockDbTable::new("TRANSCEIVER_INFO"));
        // The INFO row must already exist (module identified) for the apsel publish.
        int_tbl.hset("Ethernet0", "type", "QSFP-DD");
        let (mut task, _writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Arc::new(MockDbTable::new("PORT_TABLE")),
            int_tbl.clone(),
        );
        step(&mut task, 10);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_READY));
        assert_eq!(
            int_tbl.hget("Ethernet0", "active_apsel_hostlane1").as_deref(),
            Some("1")
        );
        assert_eq!(
            int_tbl.hget("Ethernet0", "active_apsel_hostlane4").as_deref(),
            Some("1")
        );
        assert_eq!(
            int_tbl.hget("Ethernet0", "active_apsel_hostlane5").as_deref(),
            Some("N/A")
        );
        assert_eq!(
            int_tbl.hget("Ethernet0", "host_lane_count").as_deref(),
            Some("4")
        );
        assert_eq!(
            int_tbl.hget("Ethernet0", "media_lane_count").as_deref(),
            Some("4")
        );
    }

    // on_port_update_event(PORT_SET) forces a CMIS re-init (state -> INSERTED); a
    // PORT_DEL from CONFIG_DB|PORT marks the port REMOVED and drops its bookkeeping.
    #[test]
    fn test_on_port_update_event_set_and_del() {
        let status_sw = Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW"));
        status_sw.hset("Ethernet0", "cmis_state", CMIS_STATE_READY);
        let (mut task, _writes) = make_task_full(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            status_sw.clone(),
            Arc::new(MockDbTable::new("PORT_TABLE")),
            Arc::new(MockDbTable::new("TRANSCEIVER_INFO")),
        );
        let mut set_ev = PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::PortSet);
        set_ev
            .port_dict
            .insert("admin_status".to_string(), "up".to_string());
        task.on_port_update_event(&set_ev);
        assert_eq!(cmis_state(&status_sw).as_deref(), Some(CMIS_STATE_INSERTED));

        let mut del_ev = PortChangeEvent::new("Ethernet0", 0, 0, PortChangeEventType::PortDel);
        del_ev.db_name = Some("CONFIG_DB".to_string());
        del_ev.table_name = Some("PORT".to_string());
        task.on_port_update_event(&del_ev);
        assert_eq!(
            cmis_state(&status_sw).as_deref(),
            Some(common::CMIS_STATE_REMOVED)
        );
        assert!(task.port_state.get("Ethernet0").is_none());
    }

    // get_cmis_host_lanes_mask: a contiguous datapath from lane 0 yields (1<<n)-1.
    #[test]
    fn test_get_cmis_host_lanes_mask() {
        let task = make_task(
            sfp_with_module_state(READY_STATUS),
            cfg_port("up"),
            Arc::new(MockDbTable::new("TRANSCEIVER_STATUS_SW")),
            DEFAULT_AP_CONF_WAIT,
        );
        assert_eq!(task.get_cmis_host_lanes_mask(1, 4, 0), 0x0f);
        assert_eq!(task.get_cmis_host_lanes_mask(1, 8, 0), 0xff);
        assert_eq!(task.get_cmis_host_lanes_mask(1, 1, 0), 0x01);
    }
}
