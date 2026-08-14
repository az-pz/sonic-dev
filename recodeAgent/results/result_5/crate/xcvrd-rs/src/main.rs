//! xcvrd-rs daemon entrypoint.
//!
//! Parses the command-line arguments (`--skip_cmis_mgr`, `--enable_sff_mgr`,
//! `--dom_temperature_poll_interval`, `--dom_update_interval`) and runs the daemon
//! forever. The logic lives in `xcvrd_rs::daemon`, built on the platform-bridge HAL
//! and the swss-common STATE_DB bindings; see the crate lib docs and `examples/` for
//! the binding usage patterns.
//!
//! Runs under the pmon supervisor via a Python shim that execs this binary, so
//! `supervisorctl status xcvrd` reports RUNNING.

fn main() {
    xcvrd_rs::daemon::run();
}
