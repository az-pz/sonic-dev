//! xcvrd-rs daemon — M1 bootstrap (presence + identity).
//!
//! This is deliberately a *minimal* daemon: it gets the black-box suite past the
//! clean-baseline fixture (which flushes `TRANSCEIVER_*`, restarts xcvrd, and
//! requires `TRANSCEIVER_INFO` to repopulate) so the real M1 tests execute and
//! pass instead of erroring at setup. The translation agents extend this into the
//! full daemon (DOM, CMIS state machine, errors, …) milestone by milestone.
//!
//! What it does, mirroring the Python `SfpStateUpdateTask` core:
//!   1. Build the logical→physical port map from CONFIG_DB (`PORT|Ethernet{n}`
//!      whose `index` is the emulator SFP index; here `Ethernet{i*4}` ↔ `i`).
//!   2. For every configured port, read identity via the HAL and publish
//!      `TRANSCEIVER_INFO` (every `get_transceiver_info()` field, like the Python
//!      CMIS branch of `post_port_sfp_info_to_db`, plus `is_replaceable`), and set
//!      `TRANSCEIVER_STATUS_SW` `status`/`cmis_state`.
//!   3. React to plug/unplug via `get_change_event`: repopulate on insert, delete
//!      `TRANSCEIVER_INFO` + set `status=0` on removal.
//!
//! Values are written trimmed of trailing NULs (CMIS strings are fixed-width,
//! NUL-padded); the observable result matches the reference xcvrd, whose outputs
//! the suite reads with NULs stripped.

use std::collections::BTreeMap;
use std::error::Error;
use std::time::Duration;

use platform_bridge::Platform;
use swss_common::{CxxString, DbConnector};

use crate::env;

const INFO_TABLE: &str = "TRANSCEIVER_INFO";
const STATUS_SW_TABLE: &str = "TRANSCEIVER_STATUS_SW";

/// Entry point: run the daemon forever. On any setup/serve error we log and retry
/// rather than exit, so the pmon supervisor keeps the daemon RUNNING (and the M0
/// deploy-smoke stays green) even if the emulator or Redis is briefly unavailable.
pub fn run() -> ! {
    eprintln!("xcvrd-rs: starting (M1 bootstrap: presence + identity)");
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
    eprintln!("xcvrd-rs: initial sync complete; watching for change events");

    // React to plug/unplug transitions. get_change_event blocks up to the timeout
    // and returns the set of physical ports whose presence changed.
    loop {
        let ev = platform.get_change_event(1000)?;
        for phys_str in ev.sfp.keys() {
            let Ok(phys) = phys_str.parse::<usize>() else { continue };
            if let Some(port) = ports.get(&phys) {
                if let Err(e) = sync_port(&platform, &state, phys, port) {
                    eprintln!("xcvrd-rs: change sync {port} (sfp {phys}) failed: {e}");
                }
            }
        }
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

/// Publish (or clear) one port's identity + SW status from live HAL state.
fn sync_port(platform: &Platform, state: &DbConnector, phys: usize, port: &str) -> Result<(), Box<dyn Error>> {
    let sfp = platform.sfp(phys)?;
    let info_key = format!("{INFO_TABLE}|{port}");
    let sw_key = format!("{STATUS_SW_TABLE}|{port}");

    if sfp.get_presence()? {
        let info = sfp.get_transceiver_info()?;
        if let Some(obj) = info.as_object() {
            for (field, value) in obj {
                if let Some(s) = stringify(value) {
                    state.hset(&info_key, field, &CxxString::from(s.as_str()))?;
                }
            }
        }
        let replaceable = sfp.is_replaceable().unwrap_or(false);
        state.hset(&info_key, "is_replaceable", &CxxString::from(pybool(replaceable)))?;
        state.hset(&sw_key, "status", &CxxString::from("1"))?;
        state.hset(&sw_key, "cmis_state", &CxxString::from("READY"))?;
    } else {
        state.del(&info_key)?;
        state.hset(&sw_key, "status", &CxxString::from("0"))?;
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
/// reference daemon writes via `str(value)`. Strings are trimmed of trailing NUL
/// padding (and trailing spaces); JSON nulls are skipped.
fn stringify(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.trim_end_matches('\0').trim_end().to_string()),
        Value::Bool(b) => Some(pybool(*b).to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}
