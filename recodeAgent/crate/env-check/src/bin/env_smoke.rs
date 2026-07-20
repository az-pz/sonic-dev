//! env-smoke -- the exact interop pattern the translation agents use:
//! read a transceiver through the PyO3 platform-bridge (HAL) and publish it to
//! STATE_DB through swss-common. This is what xcvrd-rs's SfpStateUpdateTask will
//! do for real; here it's a one-shot proof that both halves compose in pmon.

use platform_bridge::Platform;
use swss_common::{CxxString, DbConnector};

const STATE_DB: i32 = 6;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sock = std::env::var("REDIS_SOCK").unwrap_or_else(|_| "/var/run/redis/redis.sock".to_string());

    // HAL: PyO3 -> sonic_platform -> gRPC to xcvr-emu.
    let platform = Platform::new()?;
    // STATE_DB: official swss-common Rust bindings -> libswsscommon.
    let db = DbConnector::new_unix(STATE_DB, sock, 0)?;

    let n = platform.num_sfps()?;
    println!("num_sfps = {n}");

    let idx = 0usize;
    let sfp = platform.sfp(idx)?;
    if !sfp.get_presence()? {
        println!("sfp[{idx}] not present; nothing to publish");
        eprintln!("env-smoke: OK (no module)");
        return Ok(());
    }

    // Read identity via the bridge (serde_json::Value), then project the fields
    // xcvrd would publish to TRANSCEIVER_INFO. CMIS strings are NUL-padded; the
    // daemon logic strips them exactly like the Python original.
    let info = sfp.get_transceiver_info()?;
    let key = format!("TRANSCEIVER_INFO|RECODE_ENV_SMOKE_{idx}");
    let mut wrote = 0usize;
    for field in ["type", "manufacturer", "model", "serial", "vendor_rev", "cmis_rev"] {
        if let Some(v) = info.get(field).and_then(|x| x.as_str()) {
            let v = v.trim_end_matches('\0');
            db.hset(&key, field, &CxxString::from(v))?;
            wrote += 1;
        }
    }
    println!("bridge -> swss: wrote {wrote} fields to {key}");

    let all = db.hgetall(&key)?;
    let mut fields: Vec<_> = all.keys().cloned().collect();
    fields.sort();
    for f in &fields {
        println!("  {f} = {}", all[f].to_string_lossy());
    }

    db.del(&key)?;
    println!("cleaned up STATE_DB key");
    eprintln!("env-smoke: OK");
    Ok(())
}
