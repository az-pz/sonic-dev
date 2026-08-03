//! Port of `xcvrd_utilities/utils.py` — `XCVRDUtils`: presence / flat-memory /
//! lpmode helpers over an SFP handle (via the HAL seam).

use std::collections::BTreeMap;

use crate::hal::SfpHandle;

/// `XCVRDUtils` — thin per-physical-port helpers used by the posters + DOM task.
pub struct XcvrdUtils<'a> {
    /// physical_port → SFP handle (the Rust analogue of `sfp_obj_dict`).
    pub sfp_obj_dict: BTreeMap<usize, &'a dyn SfpHandle>,
}

impl<'a> XcvrdUtils<'a> {
    pub fn new(sfp_obj_dict: BTreeMap<usize, &'a dyn SfpHandle>) -> Self {
        XcvrdUtils { sfp_obj_dict }
    }

    /// `get_transceiver_presence` — false on missing/NotImplemented.
    pub fn get_transceiver_presence(&self, physical_port: usize) -> bool {
        match self.sfp_obj_dict.get(&physical_port) {
            Some(sfp) => sfp.get_presence().unwrap_or(false),
            None => false,
        }
    }

    /// `is_transceiver_flat_memory` — true (safe default) on missing api.
    pub fn is_transceiver_flat_memory(&self, _physical_port: usize) -> bool {
        todo!("utils.py:XCVRDUtils.is_transceiver_flat_memory")
    }

    /// `is_transceiver_lpmode_on` — read the module's low-power state via the SFP
    /// handle. Mirrors `utils.py`: any failure (missing physical port ⇒ `KeyError`,
    /// or `get_lpmode` raising ⇒ `NotImplementedError`/`Exception`) is swallowed and
    /// reported as `false` (not in low power). A falsy `get_lpmode()` (Python `None`
    /// ⇒ `Ok(false)` here) is likewise `false`.
    pub fn is_transceiver_lpmode_on(&self, physical_port: usize) -> bool {
        match self.sfp_obj_dict.get(&physical_port) {
            Some(sfp) => sfp.get_lpmode().unwrap_or(false),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSfp;

    // Port of utils.py:XCVRDUtils.get_transceiver_presence — reads the SFP handle,
    // false for an unknown physical port.
    #[test]
    fn test_get_transceiver_presence() {
        let present = MockSfp::present();
        let absent = MockSfp::default();
        let mut d: BTreeMap<usize, &dyn SfpHandle> = BTreeMap::new();
        d.insert(0, &present);
        d.insert(1, &absent);
        let utils = XcvrdUtils::new(d);
        assert!(utils.get_transceiver_presence(0));
        assert!(!utils.get_transceiver_presence(1));
        // Unknown physical port -> false (KeyError path).
        assert!(!utils.get_transceiver_presence(9));
    }

    // Port of utils.py:XCVRDUtils.is_transceiver_lpmode_on
    // (tests/test_xcvrd.py:test_is_transceiver_lpmode_on) — reads get_lpmode off the
    // SFP handle: true when on, false when off (or a falsy/None read), and false on
    // the failure paths (missing physical port -> KeyError; get_lpmode raising ->
    // NotImplementedError).
    #[test]
    fn test_is_transceiver_lpmode_on() {
        let on = MockSfp { lpmode: true, ..MockSfp::present() };
        let off = MockSfp { lpmode: false, ..MockSfp::present() };
        let err = MockSfp { lpmode_err: true, ..MockSfp::present() };
        let mut d: BTreeMap<usize, &dyn SfpHandle> = BTreeMap::new();
        d.insert(1, &on);
        d.insert(2, &off);
        d.insert(3, &err);
        let utils = XcvrdUtils::new(d);

        // get_lpmode() true -> on.
        assert!(utils.is_transceiver_lpmode_on(1));
        // get_lpmode() false (and the Python None-is-falsy case) -> off.
        assert!(!utils.is_transceiver_lpmode_on(2));
        // get_lpmode() raising (NotImplementedError / Exception) -> false.
        assert!(!utils.is_transceiver_lpmode_on(3));
        // Missing physical port (KeyError) -> false.
        assert!(!utils.is_transceiver_lpmode_on(9));
    }
}
