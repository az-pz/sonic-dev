//! Environment seed for the daemon: thin, documented constructors for the two
//! bindings so there is a single place to obtain a HAL handle and a STATE_DB
//! connection — start here rather than calling the raw bindings ad hoc.

use platform_bridge::Platform;
use swss_common::DbConnector;

/// SONiC STATE_DB logical index (Redis db number).
pub const STATE_DB: i32 = 6;

/// SONiC CONFIG_DB logical index (Redis db number).
pub const CONFIG_DB: i32 = 4;

/// SONiC APPL_DB logical index (Redis db number). `PORT_TABLE` (with `flap_count`)
/// lives here; xcvrd subscribes to it to re-capture the flag tables on a link flap.
pub const APPL_DB: i32 = 0;

/// Redis unix socket path inside pmon (override with the `REDIS_SOCK` env var).
pub fn redis_sock() -> String {
    std::env::var("REDIS_SOCK").unwrap_or_else(|_| "/var/run/redis/redis.sock".to_string())
}

/// Open the transceiver HAL: PyO3 → `sonic_platform.Platform().get_chassis()`.
///
/// Constructing the platform triggers the emulator `List()` RPC (falls back to
/// `XCVR_EMU_NUM_SFPS` placeholders if the emulator isn't up yet — same as the
/// Python daemon at start-up). Hand the returned [`Platform`] out and call
/// `num_sfps()` / `sfp(i)` / `get_change_event(timeout_ms)`.
pub fn open_platform() -> platform_bridge::Result<Platform> {
    Platform::new()
}

/// Open a STATE_DB connection over the Redis unix socket.
///
/// Use the returned [`DbConnector`] for direct hash access (`hset`/`hgetall`/…),
/// or wrap it in a `swss_common::Table` / `ProducerStateTable` for table-scoped
/// writes like `TRANSCEIVER_INFO`.
pub fn open_state_db() -> swss_common::Result<DbConnector> {
    DbConnector::new_unix(STATE_DB, redis_sock(), 0)
}

/// Open a CONFIG_DB connection over the Redis unix socket (for the PORT table /
/// logical↔physical port mapping).
pub fn open_config_db() -> swss_common::Result<DbConnector> {
    DbConnector::new_unix(CONFIG_DB, redis_sock(), 0)
}

/// Open an APPL_DB connection over the Redis unix socket (for `PORT_TABLE` link
/// state — `flap_count` drives the link-change flag re-capture).
pub fn open_appl_db() -> swss_common::Result<DbConnector> {
    DbConnector::new_unix(APPL_DB, redis_sock(), 0)
}
