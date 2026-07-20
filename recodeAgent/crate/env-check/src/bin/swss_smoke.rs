//! swss-smoke -- prove the official `swss-common` Rust crate links against
//! `libswsscommon` and talks to the live STATE_DB inside pmon.
//!
//! Connects to STATE_DB over the Redis unix socket, writes a couple of fields to
//! a throwaway `TRANSCEIVER_INFO|…` hash, reads them back, and deletes the key.
//! Exit 0 means the swss-common half of the agent scaffolding is good.

use swss_common::{CxxString, DbConnector};

const STATE_DB: i32 = 6; // SONiC STATE_DB logical index
const KEY: &str = "TRANSCEIVER_INFO|RECODE_SWSS_SMOKE";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sock = std::env::var("REDIS_SOCK").unwrap_or_else(|_| "/var/run/redis/redis.sock".to_string());
    eprintln!("swss-smoke: connecting STATE_DB (id={STATE_DB}) via {sock}");
    let db = DbConnector::new_unix(STATE_DB, sock, 0)?;

    db.hset(KEY, "manufacturer", &CxxString::from("recode-smoke"))?;
    db.hset(KEY, "model", &CxxString::from("EMU-40G-LR4"))?;

    let all = db.hgetall(KEY)?;
    println!("wrote {} fields to {KEY}:", all.len());
    let mut fields: Vec<_> = all.keys().cloned().collect();
    fields.sort();
    for f in &fields {
        println!("  {f} = {}", all[f].to_string_lossy());
    }

    db.del(KEY)?;
    println!("cleaned up: key_gone={}", !db.exists(KEY)?);
    eprintln!("swss-smoke: OK");
    Ok(())
}
