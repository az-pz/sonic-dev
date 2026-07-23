//! SFF-8636 manager task — port of `sff_mgr.py` (`SffManagerTask`).
//!
//! Non-CMIS TX enable/disable + high-power-class from `host_tx_ready`/`admin_status`.
//! Only enabled with `--enable_sff_mgr`; a late milestone. Stubs only.

#![allow(dead_code, unused_variables)]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::hal::Hal;
use crate::statedb::StateDb;

/// `SffManagerTask` (`sff_mgr.py:45`).
pub struct SffManagerTask<H: Hal, D: StateDb> {
    hal: H,
    db: D,
    stop_event: Arc<AtomicBool>,
}

impl<H: Hal, D: StateDb> SffManagerTask<H, D> {
    pub fn new(hal: H, db: D, stop_event: Arc<AtomicBool>) -> Self {
        Self { hal, db, stop_event }
    }

    /// Thread body: loop `task_worker` until `stop_event`.
    pub fn run(self) {
        todo!("late: SffManagerTask::run")
    }

    /// `task_worker` (`sff_mgr.py:328`): TX enable/disable per host_tx_ready.
    pub fn task_worker(&mut self) {
        todo!("late: SffManagerTask::task_worker")
    }
}
