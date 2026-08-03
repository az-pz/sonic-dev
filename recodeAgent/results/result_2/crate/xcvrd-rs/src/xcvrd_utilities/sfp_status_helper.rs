//! Port of `xcvrd_utilities/sfp_status_helper.py` — SFP error-bitmap masks and the
//! decode into `TRANSCEIVER_STATUS_SW.error` descriptions.

use crate::db::DbTable;

// --- SFP status codes (aligned with get_change_event of ChassisBase) ------------
pub const SFP_STATUS_REMOVED: &str = "0";
pub const SFP_STATUS_INSERTED: &str = "1";

// --- Error bitmasks (contract data; verbatim) ----------------------------------
pub const SFP_ERRORS_BLOCKING_MASK: u32 = 0x02;
pub const SFP_ERRORS_GENERIC_MASK: u32 = 0x0000FFFE;
pub const SFP_ERRORS_VENDOR_SPECIFIC_MASK: u32 = 0xFFFF0000;

/// `SfpBase.SFP_ERROR_DESCRIPTION_BLOCKING` — the `error` description that marks a
/// port whose EEPROM reads are blocked (`detect_port_in_error_status` gate).
pub const SFP_ERROR_DESCRIPTION_BLOCKING: &str = "Blocking EEPROM from being read";

/// `SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT` — the generic error bit → human
/// description mapping (`sonic_platform_base.sfp_base.SfpBase`). Kept in the same
/// **ascending-bit insertion order** as the Python dict so `fetch_generic_error_
/// description` yields descriptions low-bit-first — the order the daemon `'|'`-joins
/// into `TRANSCEIVER_STATUS_SW.error` (e.g. `0x02|0x04` →
/// `"Blocking EEPROM from being read|Power budget exceeded"`).
pub const SFP_ERROR_BIT_TO_DESCRIPTION: &[(u32, &str)] = &[
    (0x0002, "Blocking EEPROM from being read"),
    (0x0004, "Power budget exceeded"),
    (0x0008, "Bus stuck (I2C data or clock shorted)"),
    (0x0010, "Bad or unsupported EEPROM"),
    (0x0020, "Unsupported cable"),
    (0x0040, "High temperature"),
    (0x0080, "Bad cable (module/cable is shorted)"),
];

/// `is_error_block_eeprom_reading` — blocking bit set ⇒ EEPROM unreadable (delete
/// DOM but keep `TRANSCEIVER_INFO`).
pub fn is_error_block_eeprom_reading(error_bits: u32) -> bool {
    (error_bits & SFP_ERRORS_BLOCKING_MASK) != 0
}

/// `has_vendor_specific_error`.
pub fn has_vendor_specific_error(error_bits: u32) -> bool {
    (error_bits & SFP_ERRORS_VENDOR_SPECIFIC_MASK) != 0
}

/// `fetch_generic_error_description` — map the generic error bits to the
/// `SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT` descriptions. Walks the mapping in
/// ascending-bit order and collects the description of every generic bit set in
/// `error_bits`; the caller `'|'`-joins the result into `TRANSCEIVER_STATUS_SW.error`.
/// Vendor-specific (0xFFFF0000) and status (0x1) bits are masked off.
pub fn fetch_generic_error_description(error_bits: u32) -> Vec<String> {
    let generic_error_bits = error_bits & SFP_ERRORS_GENERIC_MASK;
    let mut error_descriptions = Vec::new();
    if generic_error_bits != 0 {
        for &(error_bit, error_description) in SFP_ERROR_BIT_TO_DESCRIPTION {
            if error_bit & generic_error_bits != 0 {
                error_descriptions.push(error_description.to_string());
            }
        }
    }
    error_descriptions
}

/// `detect_port_in_error_status` — is `STATUS_SW.error` the blocking description?
/// Reads the port's `TRANSCEIVER_STATUS_SW` row; returns `true` iff its `error`
/// field carries the blocking description (which halts EEPROM-driven DOM posting).
pub fn detect_port_in_error_status(logical_port_name: &str, status_sw_tbl: &dyn DbTable) -> bool {
    match status_sw_tbl.get(logical_port_name) {
        Some(row) => row
            .iter()
            .find(|(k, _)| k == "error")
            .map(|(_, error)| error.contains(SFP_ERROR_DESCRIPTION_BLOCKING))
            .unwrap_or(false),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockDbTable;

    // Port of tests/test_xcvrd.py:test_detect_port_in_error_status — blocking error
    // description -> true; a normal/absent error -> false.
    #[test]
    fn test_detect_port_in_error_status() {
        let tbl = MockDbTable::new("TRANSCEIVER_STATUS_SW");
        // No row -> not in error.
        assert!(!detect_port_in_error_status("Ethernet0", &tbl));

        // Normal error string -> not blocking.
        tbl.set(
            "Ethernet0",
            &[
                ("status".to_string(), "1".to_string()),
                ("error".to_string(), "N/A".to_string()),
            ],
        );
        assert!(!detect_port_in_error_status("Ethernet0", &tbl));

        // Blocking description present -> in error.
        tbl.hset("Ethernet0", "error", SFP_ERROR_DESCRIPTION_BLOCKING);
        assert!(detect_port_in_error_status("Ethernet0", &tbl));
    }

    // Port of tests/test_xcvrd.py:test_is_error_sfp_status — bitmaps with the
    // blocking bit (0x02) set report blocking; plain INSERTED/REMOVED do not.
    #[test]
    fn test_is_error_sfp_status() {
        for error_value in [7u32, 11, 19, 35] {
            assert!(is_error_block_eeprom_reading(error_value));
        }
        assert!(!is_error_block_eeprom_reading(SFP_STATUS_INSERTED.parse().unwrap()));
        assert!(!is_error_block_eeprom_reading(SFP_STATUS_REMOVED.parse().unwrap()));
    }

    // fetch_generic_error_description decodes generic bits in ascending-bit order,
    // masking off the status (0x1) and vendor-specific (0xFFFF0000) bits.
    #[test]
    fn test_fetch_generic_error_description() {
        // No generic bits (only the INSERTED status bit) -> empty.
        assert!(fetch_generic_error_description(0x01).is_empty());

        // BLOCKING (0x02) | POWER_BUDGET_EXCEEDED (0x04), plus the INSERTED bit:
        // decoded low-bit-first, status bit ignored.
        assert_eq!(
            fetch_generic_error_description(0x01 | 0x02 | 0x04),
            vec![
                "Blocking EEPROM from being read".to_string(),
                "Power budget exceeded".to_string(),
            ]
        );

        // I2C_STUCK (0x08) alone.
        assert_eq!(
            fetch_generic_error_description(0x08),
            vec!["Bus stuck (I2C data or clock shorted)".to_string()]
        );

        // HIGH_TEMP (0x40) is non-blocking but still a generic description.
        assert_eq!(
            fetch_generic_error_description(0x01 | 0x40),
            vec!["High temperature".to_string()]
        );

        // Vendor-specific bits are masked off by the generic decode.
        assert!(fetch_generic_error_description(0x00010000).is_empty());
        assert!(has_vendor_specific_error(0x00010000));
        assert!(!has_vendor_specific_error(0x02));
    }
}
