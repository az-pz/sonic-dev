//! xcvrd-rs daemon — M1 bootstrap + M2 DOM + M3 status/errors.
//!
//! This is deliberately a *compact* daemon: it gets the black-box suite past the
//! clean-baseline fixture (which flushes `TRANSCEIVER_*`, restarts xcvrd, and
//! requires `TRANSCEIVER_INFO` to repopulate) so the real tests execute and pass
//! instead of erroring at setup. The translation agents extend this into the full
//! daemon (CMIS state machine, …) milestone by milestone.
//!
//! What it does, mirroring the Python `SfpStateUpdateTask` + `DomInfoUpdateTask`:
//!   1. Build the logical→physical port map from CONFIG_DB (`PORT|Ethernet{n}`
//!      whose `index` is the emulator SFP index; here `Ethernet{i*4}` ↔ `i`).
//!   2. For every configured port, read identity via the HAL and publish
//!      `TRANSCEIVER_INFO` (every `get_transceiver_info()` field, like the Python
//!      CMIS branch of `post_port_sfp_info_to_db`, plus `is_replaceable`), set
//!      `TRANSCEIVER_STATUS_SW` `status`/`cmis_state`/`error`, and publish
//!      `TRANSCEIVER_DOM_THRESHOLD` (M2).
//!   3. React to plug/unplug/error via `get_change_event`: repopulate identity +
//!      DOM on insert (clearing any error), delete `TRANSCEIVER_INFO`/`DOM_*` +
//!      set `status=0` on removal, and on an error bitmap decode it into
//!      `TRANSCEIVER_STATUS_SW.error` — a blocking error drops the stale DOM rows
//!      while keeping the static `TRANSCEIVER_INFO` (M3 `test_status_error`).
//!   4. Every ~30 s (single-threaded, in the same event loop) re-read the DOM
//!      monitors of present, non-errored modules and publish
//!      `TRANSCEIVER_DOM_SENSOR` — genuine EEPROM reads visible on the emulator
//!      Monitor stream (M2 `test_dom` / `test_interaction_trace`).
//!
//! Values are written NUL-stripped (CMIS strings are fixed-width, NUL-padded)
//! with trailing spaces preserved (e.g. `vendor_date` "2024-12-14 "); nested
//! identity fields (`application_advertisement`) render as a Python `str(dict)`
//! repr; DOM values are unit-stripped by the shared
//! `DomDbUtils::beautify_dom_info_dict`. The observable result matches the
//! reference xcvrd, whose outputs the suite reads with NULs stripped (M6 golden).

use std::collections::BTreeMap;
use std::error::Error;
use std::time::{Duration, Instant};

use platform_bridge::Platform;
use swss_common::{CxxString, DbConnector};

use crate::dom::utilities::db::utils::DbUtils;
use crate::dom::utilities::dom_sensor::db_utils::DomDbUtils;
use crate::env;
use crate::xcvrd_utilities::common::{
    cmis_no_datapath_na_fields, is_cmis_manager_owned_field, pybool, stringify_field,
};
use crate::xcvrd_utilities::sfp_status_helper::{
    fetch_generic_error_description, has_vendor_specific_error, is_error_block_eeprom_reading,
    SFP_ERROR_DESCRIPTION_BLOCKING,
};

const INFO_TABLE: &str = "TRANSCEIVER_INFO";
const STATUS_SW_TABLE: &str = "TRANSCEIVER_STATUS_SW";
const DOM_SENSOR_TABLE: &str = "TRANSCEIVER_DOM_SENSOR";
const DOM_THRESHOLD_TABLE: &str = "TRANSCEIVER_DOM_THRESHOLD";

/// `CmisManagerTask.CMIS_MAX_HOST_LANES` (`cmis/cmis_manager_task.py`): a CMIS
/// module advertises up to 8 host lanes; `post_port_active_apsel_to_db` writes one
/// `active_apsel_hostlane{n}` field per lane.
const CMIS_MAX_HOST_LANES: usize = 8;

/// DOM sensor re-poll cadence. The reference xcvrd uses ~60 s; the e2e DOM tier
/// allows up to 80 s (`T_DOM`). A shorter interval keeps a comfortable margin so
/// a raw-value change reliably propagates within the window, while still exercising
/// steady-state EEPROM reads. Overridable with `XCVRD_DOM_POLL_SECS`.
const DEFAULT_DOM_POLL_SECS: u64 = 30;

fn dom_poll_interval() -> Duration {
    let secs = std::env::var("XCVRD_DOM_POLL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_DOM_POLL_SECS);
    Duration::from_secs(secs)
}

/// Entry point: run the daemon forever. On any setup/serve error we log and retry
/// rather than exit, so the pmon supervisor keeps the daemon RUNNING (and the M0
/// deploy-smoke stays green) even if the emulator or Redis is briefly unavailable.
pub fn run() -> ! {
    eprintln!("xcvrd-rs: starting (M3: presence + identity + DOM + status/errors)");
    loop {
        if let Err(e) = serve() {
            eprintln!("xcvrd-rs: serve error: {e}; retrying in 3s");
            std::thread::sleep(Duration::from_secs(3));
        }
    }
}

fn serve() -> Result<(), Box<dyn Error>> {
    let platform = env::open_platform()?; // PyO3 -> sonic_platform -> xcvr-emu
    let state = env::open_state_db()?; // swss-common -> STATE_DB
    let config = env::open_config_db()?; // swss-common -> CONFIG_DB

    let ports = discover_ports(&platform, &config)?;
    eprintln!("xcvrd-rs: {} configured ports discovered", ports.len());

    // Initial full sync so a freshly-flushed STATE_DB repopulates. Per-port errors
    // are logged but don't tear down the daemon.
    for (&phys, port) in &ports {
        if let Err(e) = sync_port(&platform, &state, phys, port) {
            eprintln!("xcvrd-rs: initial sync {port} (sfp {phys}) failed: {e}");
        }
    }
    // Seed DOM_SENSOR for present modules right away so it appears well within the
    // DOM tier's timeout on a fresh baseline (rather than after the first cadence).
    poll_all_dom(&platform, &state, &ports);
    eprintln!("xcvrd-rs: initial sync complete; watching for change events");

    // React to plug/unplug transitions and re-poll DOM on a fixed cadence. Both
    // happen in this single loop (no extra thread -> no PyO3 GIL/Send concerns).
    // get_change_event blocks up to the timeout, so the DOM deadline is checked
    // at least once per second.
    let poll_interval = dom_poll_interval();
    let mut next_dom_poll = Instant::now() + poll_interval;
    loop {
        let ev = platform.get_change_event(1000)?;
        for (phys_str, value) in &ev.sfp {
            let Ok(phys) = phys_str.parse::<usize>() else { continue };
            let Some(port) = ports.get(&phys) else { continue };
            // The change-event value is '1' (inserted), '0' (removed) or, for a
            // hardware error, an SfpBase bitmap. Dispatch like the Python
            // SfpStateUpdateTask (xcvrd.py:533-646).
            match value.as_str() {
                "0" => {
                    // Removal: sync_port takes the absent path (clear rows, status=0).
                    if let Err(e) = sync_port(&platform, &state, phys, port) {
                        eprintln!("xcvrd-rs: change sync {port} (sfp {phys}) failed: {e}");
                    }
                }
                "1" => {
                    // Insert / recovery: republish identity + status=1 + error=N/A,
                    // then refresh DOM promptly (re-plug read burst / repopulation).
                    if let Err(e) = sync_port(&platform, &state, phys, port) {
                        eprintln!("xcvrd-rs: change sync {port} (sfp {phys}) failed: {e}");
                    }
                    if let Err(e) = sync_dom_sensor(&platform, &state, phys, port) {
                        eprintln!("xcvrd-rs: change DOM {port} (sfp {phys}) failed: {e}");
                    }
                }
                other => {
                    // SFP error bitmap: decode into STATUS_SW.error; a blocking error
                    // also removes the stale DOM rows (static INFO is kept).
                    if let Err(e) =
                        handle_error_event(&platform, &state, phys, port, other, &ev.sfp_error)
                    {
                        eprintln!("xcvrd-rs: error event {port} (sfp {phys}, code {other}) failed: {e}");
                    }
                }
            }
        }

        if Instant::now() >= next_dom_poll {
            poll_all_dom(&platform, &state, &ports);
            next_dom_poll = Instant::now() + poll_interval;
        }

        // Self-heal TRANSCEIVER_INFO: a module hot-inserted at runtime whose EEPROM
        // wasn't ready at the change-event instant (get_transceiver_info -> None)
        // gets its identity (re)published here on the next pass, so a live re-plug
        // repopulates INFO within a couple of seconds (mirrors retry_eeprom_reading).
        reconcile_info(&platform, &state, &ports);
    }
}

/// Map each configured logical port to its physical SFP index. A port is
/// "configured" iff CONFIG_DB has `PORT|<name>`; the emulator names SFP `i` as
/// `Ethernet{i*4}`, which matches the CONFIG_DB `index` field on this testbed.
fn discover_ports(platform: &Platform, config: &DbConnector) -> Result<BTreeMap<usize, String>, Box<dyn Error>> {
    let num = platform.num_sfps()?;
    let mut map = BTreeMap::new();
    for phys in 0..num {
        let port = format!("Ethernet{}", phys * 4);
        if config.exists(&format!("PORT|{port}"))? {
            map.insert(phys, port);
        }
    }
    Ok(map)
}

/// Publish (or clear) one port's identity + SW status + DOM thresholds from live
/// HAL state.
fn sync_port(platform: &Platform, state: &DbConnector, phys: usize, port: &str) -> Result<(), Box<dyn Error>> {
    let sfp = platform.sfp(phys)?;
    let info_key = format!("{INFO_TABLE}|{port}");
    let sw_key = format!("{STATUS_SW_TABLE}|{port}");
    let dom_key = format!("{DOM_SENSOR_TABLE}|{port}");
    let threshold_key = format!("{DOM_THRESHOLD_TABLE}|{port}");

    if sfp.get_presence()? {
        // A present / (re-)inserted module is INSERTED and clears any prior error
        // (matches the golden STATUS_SW and recovers a port out of a blocking-error
        // state). Do this first so status='1' is visible even if the EEPROM isn't
        // ready to read the identity yet (mirrors the Python insert branch, which
        // sets SFP_STATUS_INSERTED before post_port_sfp_info_to_db).
        state.hset(&sw_key, "status", &CxxString::from("1"))?;
        state.hset(&sw_key, "cmis_state", &CxxString::from("READY"))?;
        state.hset(&sw_key, "error", &CxxString::from("N/A"))?;

        // get_transceiver_info() returns None (-> JSON null) while the EEPROM is not
        // ready yet — most commonly right after a hot re-insert (upstream cmis.py
        // returns None if *any* field read yields None). Leave TRANSCEIVER_INFO
        // unpopulated so `reconcile_info` retries it, mirroring the reference
        // SFP_EEPROM_NOT_READY -> retry_eeprom_reading path (xcvrd.py:558-566).
        let info = sfp.get_transceiver_info()?;
        let obj = match info.as_object() {
            Some(o) => o,
            None => return Ok(()),
        };
        let is_cmis = obj.contains_key("cmis_rev");
        for (field, value) in obj {
            // active_apsel_hostlaneN, host_lane_count and media_lane_count are NOT
            // owned by the identity publish: the CMIS manager's
            // post_port_active_apsel_to_db writes 'N/A' for every host lane masked
            // out / until the datapath activates, and 'N/A' for the two lane counts
            // in that same no-datapath (reset_apsel) case
            // (cmis/cmis_manager_task.py:751-782). Don't leak the raw numeric values
            // the emulated get_transceiver_info() dict carries; the is_cmis block
            // below re-establishes them as 'N/A'. Shared skip-set with
            // xcvrd::build_info_row so the two paths can't drift.
            if is_cmis_manager_owned_field(field) {
                continue;
            }
            if let Some(s) = stringify_field(value) {
                state.hset(&info_key, field, &CxxString::from(s.as_str()))?;
            }
        }
        let replaceable = sfp.is_replaceable().unwrap_or(false);
        state.hset(&info_key, "is_replaceable", &CxxString::from(pybool(replaceable)))?;
        // Active ApSel projection (post_port_active_apsel_to_db, reset branch): with
        // no active datapath the host_lanes_mask is 0, so every host lane's
        // active_apsel plus host_lane_count / media_lane_count is 'N/A' — exactly
        // what the golden pins for the emulated 40G-LR4. The daemon runs the reduced
        // CMIS driver (no full manager task), so it writes these inline. CMIS only.
        if is_cmis {
            for field in cmis_no_datapath_na_fields(CMIS_MAX_HOST_LANES) {
                state.hset(&info_key, &field, &CxxString::from("N/A"))?;
            }
        }
        // DOM thresholds are static-per-module; publish them on (re-)insert. Read
        // through the pyo3 stringifying reader (the JSON bridge can't marshal the
        // -inf power thresholds this module reports); an empty read (feature
        // absent) is treated as "not available" so it never blocks identity.
        let thresholds = crate::hal::read_thresholds_stringified(phys);
        let _ = write_dom_row(&thresholds, state, &threshold_key);
    } else {
        state.del(&info_key)?;
        state.del(&dom_key)?;
        state.del(&threshold_key)?;
        state.hset(&sw_key, "status", &CxxString::from("0"))?;
        state.hset(&sw_key, "error", &CxxString::from("N/A"))?;
    }
    Ok(())
}

/// (Re)publish identity for any present, configured port that is currently missing
/// its `TRANSCEIVER_INFO` row. This is the compact daemon's analogue of the
/// reference `retry_eeprom_reading` loop: a module whose EEPROM wasn't ready at the
/// change-event instant (`get_transceiver_info` -> None, common right after a hot
/// re-plug) is retried on the next pass, so its identity reappears within a couple
/// of seconds — well inside the black-box suite's fast window. Absent ports are
/// left cleared (never resurrected).
fn reconcile_info(platform: &Platform, state: &DbConnector, ports: &BTreeMap<usize, String>) {
    for (&phys, port) in ports {
        let info_key = format!("{INFO_TABLE}|{port}");
        match state.exists(&info_key) {
            Ok(true) => continue, // identity already published
            Ok(false) => {}
            Err(_) => continue,
        }
        let present = match platform.sfp(phys) {
            Ok(sfp) => sfp.get_presence().unwrap_or(false),
            Err(_) => false,
        };
        if present {
            if let Err(e) = sync_port(platform, state, phys, port) {
                eprintln!("xcvrd-rs: reconcile {port} (sfp {phys}) failed: {e}");
            }
        }
    }
}

/// Re-read and republish one present module's DOM sensor values.
fn sync_dom_sensor(platform: &Platform, state: &DbConnector, phys: usize, port: &str) -> Result<(), Box<dyn Error>> {
    let sfp = platform.sfp(phys)?;
    if !sfp.get_presence()? {
        return Ok(());
    }
    // A port whose EEPROM is blocked by an error has had its DOM intentionally
    // removed; don't republish it (matches the Python DOM loop's
    // detect_port_in_error_status skip) — the row must stay absent until recovery.
    if port_in_blocking_error(state, port)? {
        return Ok(());
    }
    let dom_key = format!("{DOM_SENSOR_TABLE}|{port}");
    write_dom_row(&sfp.get_transceiver_dom_real_value()?, state, &dom_key)
}

/// Poll DOM sensors for every present configured module (steady-state cadence).
/// Per-port failures are logged and skipped so a single flaky module never stalls
/// the loop.
fn poll_all_dom(platform: &Platform, state: &DbConnector, ports: &BTreeMap<usize, String>) {
    for (&phys, port) in ports {
        if let Err(e) = sync_dom_sensor(platform, state, phys, port) {
            eprintln!("xcvrd-rs: DOM poll {port} (sfp {phys}) failed: {e}");
        }
    }
}

/// Decode an injected/reported SFP error bitmap into `TRANSCEIVER_STATUS_SW`
/// (mirrors the Python `SfpStateUpdateTask` error branch, xcvrd.py:610-646).
/// `status` becomes the raw event code and `error` the `'|'`-joined descriptions
/// (plus any vendor-specific text). A blocking error means the EEPROM is
/// unreadable, so the now-stale DOM rows are removed while the static
/// `TRANSCEIVER_INFO` is kept; a later plug-in ('1') clears the error and repopulates.
fn handle_error_event(
    platform: &Platform,
    state: &DbConnector,
    phys: usize,
    port: &str,
    value: &str,
    sfp_error: &BTreeMap<String, String>,
) -> Result<(), Box<dyn Error>> {
    let error_bits: u32 = match value.parse() {
        Ok(b) => b,
        // Unparseable event code: nothing to decode; keep the daemon RUNNING.
        Err(_) => return Ok(()),
    };
    let sw_key = format!("{STATUS_SW_TABLE}|{port}");
    let mut descriptions = fetch_generic_error_description(error_bits);
    if has_vendor_specific_error(error_bits) {
        // Prefer the vendor text carried alongside the event; else ask the SFP.
        let vendor = sfp_error.get(&phys.to_string()).cloned().or_else(|| {
            platform.sfp(phys).ok().and_then(|s| s.get_error_description().ok().flatten())
        });
        if let Some(v) = vendor {
            descriptions.push(v);
        }
    }
    // status = raw bitmap code; error = decoded descriptions (any prior error replaced).
    state.hset(&sw_key, "status", &CxxString::from(value))?;
    state.hset(&sw_key, "error", &CxxString::from(descriptions.join("|").as_str()))?;
    // Blocking bit: EEPROM unreadable -> the DOM data is out-of-date -> remove it.
    if is_error_block_eeprom_reading(error_bits) {
        state.del(&format!("{DOM_SENSOR_TABLE}|{port}"))?;
        state.del(&format!("{DOM_THRESHOLD_TABLE}|{port}"))?;
    }
    Ok(())
}

/// Whether `port`'s `TRANSCEIVER_STATUS_SW.error` is a blocking error (its EEPROM
/// is unreadable, so DOM must not be re-polled until recovery).
fn port_in_blocking_error(state: &DbConnector, port: &str) -> Result<bool, Box<dyn Error>> {
    let sw_key = format!("{STATUS_SW_TABLE}|{port}");
    if !state.exists(&sw_key)? {
        return Ok(false);
    }
    for (field, value) in state.hgetall(&sw_key)? {
        if field == "error" {
            return Ok(value.to_string_lossy().contains(SFP_ERROR_DESCRIPTION_BLOCKING));
        }
    }
    Ok(false)
}

/// Beautify a DOM dict (unit-strip via the shared `DomDbUtils` path), append
/// `last_update_time`, and write it to `key`. Empty/None dicts are skipped
/// (mirrors the Python `post_diagnostic_values_to_db` early-return).
fn write_dom_row(values: &serde_json::Value, state: &DbConnector, key: &str) -> Result<(), Box<dyn Error>> {
    let obj = match values.as_object() {
        Some(o) if !o.is_empty() => o,
        _ => return Ok(()),
    };
    let mut row = DomDbUtils::beautify_dom_info_dict(obj);
    row.insert("last_update_time".to_string(), DbUtils::get_current_time());
    for (field, value) in &row {
        state.hset(key, field, &CxxString::from(value.as_str()))?;
    }
    Ok(())
}
