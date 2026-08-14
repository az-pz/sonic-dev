#![allow(dead_code)]
//! Port of `xcvrd_utilities/sfp_status_helper.py`: the SFP error-bit model +
//! human-readable descriptions. The bit values and description strings mirror
//! `sonic_platform_base.sfp_base.SfpBase` (the Python source of truth).

use crate::db::Table;

pub const SFP_STATUS_REMOVED: &str = "0";
pub const SFP_STATUS_INSERTED: &str = "1";

// SFP error code masks (aligned with sfp_status_helper.py).
pub const SFP_ERRORS_BLOCKING_MASK: u32 = 0x02;
pub const SFP_ERRORS_GENERIC_MASK: u32 = 0x0000_FFFE;
pub const SFP_ERRORS_VENDOR_SPECIFIC_MASK: u32 = 0xFFFF_0000;

// SfpBase error bits.
pub const SFP_ERROR_BIT_BLOCKING: u32 = 0x0000_0002;
pub const SFP_ERROR_BIT_POWER_BUDGET_EXCEEDED: u32 = 0x0000_0004;
pub const SFP_ERROR_BIT_I2C_STUCK: u32 = 0x0000_0008;
pub const SFP_ERROR_BIT_BAD_EEPROM: u32 = 0x0000_0010;
pub const SFP_ERROR_BIT_UNSUPPORTED_CABLE: u32 = 0x0000_0020;
pub const SFP_ERROR_BIT_HIGH_TEMP: u32 = 0x0000_0040;
pub const SFP_ERROR_BIT_BAD_CABLE: u32 = 0x0000_0080;

// SfpBase error descriptions.
pub const SFP_ERROR_DESCRIPTION_BLOCKING: &str = "Blocking EEPROM from being read";
pub const SFP_ERROR_DESCRIPTION_POWER_BUDGET_EXCEEDED: &str = "Power budget exceeded";
pub const SFP_ERROR_DESCRIPTION_I2C_STUCK: &str = "Bus stuck (I2C data or clock shorted)";
pub const SFP_ERROR_DESCRIPTION_BAD_EEPROM: &str = "Bad or unsupported EEPROM";
pub const SFP_ERROR_DESCRIPTION_UNSUPPORTED_CABLE: &str = "Unsupported cable";
pub const SFP_ERROR_DESCRIPTION_HIGH_TEMP: &str = "High temperature";
pub const SFP_ERROR_DESCRIPTION_BAD_CABLE: &str = "Bad cable (module/cable is shorted)";

/// `SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT`, in insertion order (the order the
/// Python code iterates when composing a `|`-joined error string).
pub const SFP_ERROR_BIT_TO_DESCRIPTION: &[(u32, &str)] = &[
    (SFP_ERROR_BIT_BLOCKING, SFP_ERROR_DESCRIPTION_BLOCKING),
    (SFP_ERROR_BIT_POWER_BUDGET_EXCEEDED, SFP_ERROR_DESCRIPTION_POWER_BUDGET_EXCEEDED),
    (SFP_ERROR_BIT_I2C_STUCK, SFP_ERROR_DESCRIPTION_I2C_STUCK),
    (SFP_ERROR_BIT_BAD_EEPROM, SFP_ERROR_DESCRIPTION_BAD_EEPROM),
    (SFP_ERROR_BIT_UNSUPPORTED_CABLE, SFP_ERROR_DESCRIPTION_UNSUPPORTED_CABLE),
    (SFP_ERROR_BIT_HIGH_TEMP, SFP_ERROR_DESCRIPTION_HIGH_TEMP),
    (SFP_ERROR_BIT_BAD_CABLE, SFP_ERROR_DESCRIPTION_BAD_CABLE),
];

/// True when the blocking bit is set, i.e. the error prevents EEPROM reads.
pub fn is_error_block_eeprom_reading(error_bits: u32) -> bool {
    0 != (error_bits & SFP_ERRORS_BLOCKING_MASK)
}

/// True when any vendor-specific error bit is set.
pub fn has_vendor_specific_error(error_bits: u32) -> bool {
    0 != (error_bits & SFP_ERRORS_VENDOR_SPECIFIC_MASK)
}

/// The list of generic-error descriptions implied by `error_bits`, in dict order.
pub fn fetch_generic_error_description(error_bits: u32) -> Vec<String> {
    let generic_error_bits = error_bits & SFP_ERRORS_GENERIC_MASK;
    let mut descriptions = Vec::new();
    if generic_error_bits != 0 {
        for (error_bit, error_description) in SFP_ERROR_BIT_TO_DESCRIPTION {
            if error_bit & generic_error_bits != 0 {
                descriptions.push(error_description.to_string());
            }
        }
    }
    descriptions
}

/// True when `logical_port_name`'s STATUS_SW row carries a blocking error string.
pub fn detect_port_in_error_status(logical_port_name: &str, status_sw_tbl: &dyn Table) -> bool {
    match status_sw_tbl.get(logical_port_name) {
        Ok(Some(fvp)) => match fvp.iter().find(|(f, _)| f == "error").map(|(_, v)| v) {
            Some(error) => error.contains(SFP_ERROR_DESCRIPTION_BLOCKING),
            None => false,
        },
        _ => false,
    }
}

/// The pure kernel of [`detect_port_in_error_status`] over an already-read `error`
/// string (the deployed `daemon.rs` reads `TRANSCEIVER_STATUS_SW.error` straight
/// from `swss_common` rather than through the [`Table`] seam): a port is "in error
/// status" iff its STATUS_SW `error` field names the blocking error, i.e. EEPROM is
/// unreadable and the DOM poll must skip the port (dom_mgr.py:348).
pub fn is_blocking_error_description(error: &str) -> bool {
    error.contains(SFP_ERROR_DESCRIPTION_BLOCKING)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockTable;

    /// Port of `tests/test_xcvrd.py::test_detect_port_in_error_status`.
    #[test]
    fn test_detect_port_in_error_status() {
        let tbl = MockTable::new();
        tbl.set("Ethernet0", &[("error".into(), "N/A".into())]).unwrap();
        assert!(!detect_port_in_error_status("Ethernet0", &tbl));

        let tbl2 = MockTable::new();
        tbl2.set("Ethernet0", &[("error".into(), SFP_ERROR_DESCRIPTION_BLOCKING.into())]).unwrap();
        assert!(detect_port_in_error_status("Ethernet0", &tbl2));

        // Absent row → not in error.
        assert!(!detect_port_in_error_status("Ethernet4", &tbl2));
    }

    /// Port of `tests/test_xcvrd.py::test_is_error_sfp_status`.
    #[test]
    fn test_is_error_sfp_status() {
        for error_value in [7u32, 11, 19, 35] {
            assert!(is_error_block_eeprom_reading(error_value));
        }
        assert!(!is_error_block_eeprom_reading(SFP_STATUS_INSERTED.parse().unwrap()));
        assert!(!is_error_block_eeprom_reading(SFP_STATUS_REMOVED.parse().unwrap()));
    }

    #[test]
    fn test_fetch_generic_error_description() {
        // Blocking | power-budget → both descriptions, in dict order.
        let bits = SFP_ERROR_BIT_BLOCKING | SFP_ERROR_BIT_POWER_BUDGET_EXCEEDED;
        assert_eq!(
            fetch_generic_error_description(bits),
            vec![
                SFP_ERROR_DESCRIPTION_BLOCKING.to_string(),
                SFP_ERROR_DESCRIPTION_POWER_BUDGET_EXCEEDED.to_string(),
            ]
        );
        // No generic bits → empty.
        assert!(fetch_generic_error_description(SFP_STATUS_INSERTED.parse().unwrap()).is_empty());
    }

    #[test]
    fn test_has_vendor_specific_error() {
        assert!(has_vendor_specific_error(0x0001_0000));
        assert!(!has_vendor_specific_error(SFP_ERROR_BIT_BLOCKING));
    }

    /// The two descriptions must match `SfpBase` (and the
    /// `lib/errors.py` contract): the I2C and bad-cable strings carry their full
    /// parenthetical text, not the abbreviated form.
    #[test]
    fn test_error_descriptions_match_sfp_base() {
        assert_eq!(SFP_ERROR_DESCRIPTION_I2C_STUCK, "Bus stuck (I2C data or clock shorted)");
        assert_eq!(SFP_ERROR_DESCRIPTION_BAD_CABLE, "Bad cable (module/cable is shorted)");
    }

    /// Decode the exact change-event bitmaps `test_status_error` injects and
    /// assert the `'|'`-joined description(s) match what `TRANSCEIVER_STATUS_SW.error`
    /// must read. `STATUS_INSERTED` (0x01) is not a generic-error bit, so it never
    /// contributes a description.
    #[test]
    fn test_decode_injected_error_events() {
        const STATUS_INSERTED_BIT: u32 = 0x01;
        // I2C_STUCK_EVENT = INSERTED | BLOCKING | I2C_STUCK (blocking).
        let i2c = STATUS_INSERTED_BIT | SFP_ERROR_BIT_BLOCKING | SFP_ERROR_BIT_I2C_STUCK;
        assert!(is_error_block_eeprom_reading(i2c));
        assert_eq!(
            fetch_generic_error_description(i2c).join("|"),
            "Blocking EEPROM from being read|Bus stuck (I2C data or clock shorted)"
        );
        // BAD_EEPROM_EVENT = INSERTED | BLOCKING | BAD_EEPROM (blocking).
        let bad = STATUS_INSERTED_BIT | SFP_ERROR_BIT_BLOCKING | SFP_ERROR_BIT_BAD_EEPROM;
        assert!(is_error_block_eeprom_reading(bad));
        assert_eq!(
            fetch_generic_error_description(bad).join("|"),
            "Blocking EEPROM from being read|Bad or unsupported EEPROM"
        );
        // HIGH_TEMP_EVENT = INSERTED | HIGH_TEMP (non-blocking).
        let hot = STATUS_INSERTED_BIT | SFP_ERROR_BIT_HIGH_TEMP;
        assert!(!is_error_block_eeprom_reading(hot));
        assert_eq!(fetch_generic_error_description(hot).join("|"), "High temperature");
    }

    /// `is_blocking_error_description` (the daemon's DOM-poll gate) returns true only
    /// for a STATUS_SW error string that names the blocking error.
    #[test]
    fn test_is_blocking_error_description() {
        assert!(is_blocking_error_description(
            "Blocking EEPROM from being read|Bus stuck (I2C data or clock shorted)"
        ));
        assert!(!is_blocking_error_description("High temperature"));
        assert!(!is_blocking_error_description("N/A"));
        assert!(!is_blocking_error_description(""));
    }
}
