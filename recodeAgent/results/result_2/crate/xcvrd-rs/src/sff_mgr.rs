//! Port of `sff_mgr.py` — `SffManagerTask` (optional, `--enable_sff_mgr`):
//! deterministic TX enable for non-CMIS (SFF-8636) modules gated on `host_tx_ready`,
//! take the module out of low power, and enable the high-power class for class≥5
//! modules (M12).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::xcvrd_utilities::port_event_helper::PortChangeEvent;

/// `SffManagerTask` (`sff_mgr.py:45`).
pub struct SffManagerTask {
    // TODO(Translator): namespaces, platform chassis handle, per-lport state.
}

impl SffManagerTask {
    pub fn new() -> Self {
        SffManagerTask {}
    }

    /// `task_worker` — the SFF TX-enable loop gated on host_tx_ready.
    pub fn task_worker(&self, _stop: &Arc<AtomicBool>) {
        todo!("sff_mgr.py:SffManagerTask.task_worker")
    }

    pub fn run(self, stop: Arc<AtomicBool>) {
        self.task_worker(&stop)
    }

    /// `get_active_lanes_for_lport` — active-lane bool array for a subport.
    pub fn get_active_lanes_for_lport(
        &self,
        _lport: &str,
        _subport_idx: u32,
        _num_lanes_per_lport: u32,
        _num_lanes_per_pport: u32,
    ) -> Option<Vec<bool>> {
        todo!("sff_mgr.py:get_active_lanes_for_lport")
    }

    /// `enable_high_power_class` — power override / high-power class for class≥5.
    pub fn enable_high_power_class(&self, _lport: &str) {
        todo!("sff_mgr.py:enable_high_power_class")
    }

    /// `on_port_update_event`.
    pub fn on_port_update_event(&mut self, _port_change_event: &PortChangeEvent) {
        todo!("sff_mgr.py:on_port_update_event")
    }

    /// `calculate_tx_disable_delta_array`.
    pub fn calculate_tx_disable_delta_array(
        &self,
        _cur_tx_disable_array: &[bool],
        _tx_disable_flag: bool,
        _active_lanes: &[bool],
    ) -> Vec<bool> {
        todo!("sff_mgr.py:calculate_tx_disable_delta_array")
    }
}

#[cfg(test)]
mod tests {
    // Part-B. TODO(Translator): fill from tests/test_xcvrd.py (SffManagerTask*).
    #[test]
    #[ignore = "skeleton stub: test_SffManagerTask_get_active_lanes_for_lport"]
    fn test_get_active_lanes_for_lport() {}

    #[test]
    #[ignore = "skeleton stub: test_SffManagerTask_enable_high_power_class"]
    fn test_enable_high_power_class() {}

    #[test]
    #[ignore = "skeleton stub: test_SffManagerTask_task_worker"]
    fn test_task_worker() {}
}
