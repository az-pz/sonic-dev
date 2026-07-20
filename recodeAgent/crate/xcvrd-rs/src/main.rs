//! xcvrd-rs — Rust reimplementation of the SONiC xcvrd transceiver daemon.
//!
//! SKELETON (Phase 0 / milestone M0). Currently just a supervised daemon that
//! stays RUNNING so the build->inject->test->restore harness can be proven end
//! to end (gate: `test_xcvrd_running`). The translation agents flesh this out
//! milestone by milestone on top of:
//!   - platform-bridge  (PyO3 wrappers around the Python sonic_platform plugin)
//!   - swss-common      (STATE_DB access)
//!
//! Runs under the pmon supervisor via a Python shim that execs this binary, so
//! `supervisorctl status xcvrd` reports RUNNING.

use std::{thread, time::Duration};

fn main() {
    eprintln!("xcvrd-rs: starting (skeleton M0)");
    // Default SIGTERM behaviour (terminate) is fine for supervisor stop/restart.
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
