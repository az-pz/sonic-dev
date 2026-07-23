//! xcvrd-rs daemon entrypoint.
//!
//! Runs the M1 bootstrap (presence + identity): populate TRANSCEIVER_INFO /
//! TRANSCEIVER_STATUS_SW for present transceivers and react to plug/unplug, so the
//! black-box suite gets past the clean-baseline fixture and the M1 tests actually
//! run. The logic lives in `xcvrd_rs::daemon` (built on the platform-bridge HAL +
//! swss-common STATE_DB bindings); the translation agents extend it milestone by
//! milestone. See the crate lib docs + `examples/` for the binding usage patterns.
//!
//! Runs under the pmon supervisor via a Python shim that execs this binary, so
//! `supervisorctl status xcvrd` reports RUNNING.

fn main() {
    xcvrd_rs::daemon::run();
}
