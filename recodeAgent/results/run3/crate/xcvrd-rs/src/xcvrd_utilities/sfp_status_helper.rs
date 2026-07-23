//! SFP error bitmasks + descriptions — port of `xcvrd_utilities/sfp_status_helper.py`.
//!
//! `get_change_event` value codes: `"1"` inserted, `"0"` removed, anything else
//! is an `SfpBase` error bitmap. These masks classify the bitmap; the decoded
//! descriptions land in `TRANSCEIVER_STATUS_SW.error` (M3). Constants are exact;
//! the decode/lookup bodies are stubs for the Translator.

#![allow(dead_code, unused_variables)]

use crate::statedb::TableApi;

/// `SFP_STATUS_REMOVED = '0'`.
pub const SFP_STATUS_REMOVED: &str = "0";
/// `SFP_STATUS_INSERTED = '1'`.
pub const SFP_STATUS_INSERTED: &str = "1";

/// Blocking error mask (`0x02`): EEPROM read blocked -> drop DOM tables.
pub const SFP_ERRORS_BLOCKING_MASK: u32 = 0x02;
/// Generic (decodable) error bits.
pub const SFP_ERRORS_GENERIC_MASK: u32 = 0x0000_FFFE;
/// Vendor-specific error bits.
pub const SFP_ERRORS_VENDOR_SPECIFIC_MASK: u32 = 0xFFFF_0000;

/// `SfpBase.SFP_ERROR_DESCRIPTION_BLOCKING` — the description substring that marks
/// a blocking (EEPROM-read-blocking) error in `TRANSCEIVER_STATUS_SW.error`.
pub const SFP_ERROR_DESCRIPTION_BLOCKING: &str = "Blocking EEPROM from being read";

/// `SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT` — the generic error-bit -> human
/// description table (dict insertion order == ascending bit). The joined values
/// land in `TRANSCEIVER_STATUS_SW.error`. Strings are exact (the e2e oracle in
/// `xcvrd-tests/lib/errors.py` asserts these substrings).
pub const SFP_ERROR_BIT_TO_DESCRIPTION: [(u32, &str); 7] = [
    (0x02, SFP_ERROR_DESCRIPTION_BLOCKING),
    (0x04, "Power budget exceeded"),
    (0x08, "Bus stuck (I2C data or clock shorted)"),
    (0x10, "Bad or unsupported EEPROM"),
    (0x20, "Unsupported cable"),
    (0x40, "High temperature"),
    (0x80, "Bad cable (module/cable is shorted)"),
];

/// `is_error_block_eeprom_reading`: does the bitmap set the blocking bit?
pub fn is_error_block_eeprom_reading(error_bits: u32) -> bool {
    (error_bits & SFP_ERRORS_BLOCKING_MASK) != 0
}

/// `has_vendor_specific_error`: any vendor-specific bit set?
pub fn has_vendor_specific_error(error_bits: u32) -> bool {
    (error_bits & SFP_ERRORS_VENDOR_SPECIFIC_MASK) != 0
}

/// `fetch_generic_error_description`: map the generic bits to human descriptions
/// (from `SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT`). Joined with `'|'` into
/// `TRANSCEIVER_STATUS_SW.error`. Order follows the table (ascending bit), matching
/// the Python dict-iteration order.
pub fn fetch_generic_error_description(error_bits: u32) -> Vec<String> {
    let generic_error_bits = error_bits & SFP_ERRORS_GENERIC_MASK;
    let mut error_descriptions = Vec::new();
    if generic_error_bits != 0 {
        for (bit, description) in SFP_ERROR_BIT_TO_DESCRIPTION.iter() {
            if bit & generic_error_bits != 0 {
                error_descriptions.push(description.to_string());
            }
        }
    }
    error_descriptions
}

/// `detect_port_in_error_status` (`sfp_status_helper.py:30`): is the port's current
/// SW `error` a blocking one? Reads `TRANSCEIVER_STATUS_SW`, and returns whether
/// the `error` field contains the blocking description. Absent row/field -> false.
/// (Used by the DOM poll loop to skip ports whose EEPROM is blocked.)
pub fn detect_port_in_error_status<T: TableApi>(
    logical_port_name: &str,
    status_sw_tbl: &T,
) -> bool {
    match status_sw_tbl.get(logical_port_name) {
        Ok(Some(row)) => match row.get("error") {
            Some(error) => error.contains(SFP_ERROR_DESCRIPTION_BLOCKING),
            None => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockStateDb;
    use crate::statedb::{Row, StateDb, TableApi};

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// <- test_detect_port_in_error_status: blocking error -> true; N/A / missing
    /// row / non-blocking error -> false.
    #[test]
    fn detect_port_in_error_status_cases() {
        let db = MockStateDb::new();
        let sw = db.table("TRANSCEIVER_STATUS_SW").unwrap();

        // No row -> false.
        assert!(!detect_port_in_error_status("Ethernet0", &sw));

        // N/A error -> false.
        sw.set("Ethernet0", &row(&[("status", "1"), ("error", "N/A")])).unwrap();
        assert!(!detect_port_in_error_status("Ethernet0", &sw));

        // Non-blocking description -> false.
        sw.set("Ethernet0", &row(&[("error", "High temperature")])).unwrap();
        assert!(!detect_port_in_error_status("Ethernet0", &sw));

        // Blocking description present -> true.
        sw.set("Ethernet0", &row(&[("error", SFP_ERROR_DESCRIPTION_BLOCKING)])).unwrap();
        assert!(detect_port_in_error_status("Ethernet0", &sw));

        // Blocking as one of several '|'-joined descriptions -> true.
        sw.set(
            "Ethernet0",
            &row(&[("error", &format!("Bus stuck|{SFP_ERROR_DESCRIPTION_BLOCKING}"))]),
        )
        .unwrap();
        assert!(detect_port_in_error_status("Ethernet0", &sw));
    }

    /// <- test_is_error_sfp_status: any bitmap with the blocking bit (0x02) set is
    /// blocking; the plain insert/remove codes ('1'/'0' parsed as ints) are not.
    #[test]
    fn is_error_block_eeprom_reading_cases() {
        for error_value in [7u32, 11, 19, 35] {
            assert!(is_error_block_eeprom_reading(error_value));
        }
        // int(SFP_STATUS_INSERTED) == 1, int(SFP_STATUS_REMOVED) == 0 -> not blocking.
        assert!(!is_error_block_eeprom_reading(SFP_STATUS_INSERTED.parse().unwrap()));
        assert!(!is_error_block_eeprom_reading(SFP_STATUS_REMOVED.parse().unwrap()));
    }

    /// Vendor-specific bit mask (`0xFFFF0000`) detection.
    #[test]
    fn has_vendor_specific_error_cases() {
        assert!(has_vendor_specific_error(0x0001_0000));
        assert!(has_vendor_specific_error(0x8000_0002));
        // Only generic/blocking bits -> no vendor error.
        assert!(!has_vendor_specific_error(0x0000_00FF));
        assert!(!has_vendor_specific_error(0x02));
    }

    /// <- generic decode: the blocking + I2C-stuck event (0x01|0x02|0x08 = 11)
    /// decodes to the blocking and bus-stuck descriptions, in ascending bit order;
    /// the STATUS_INSERTED bit (0x01) is masked out of the generic set.
    #[test]
    fn fetch_generic_error_description_cases() {
        // I2C_STUCK_EVENT = 11.
        let descs = fetch_generic_error_description(11);
        assert_eq!(
            descs,
            vec![
                "Blocking EEPROM from being read".to_string(),
                "Bus stuck (I2C data or clock shorted)".to_string(),
            ]
        );

        // BAD_EEPROM_EVENT = 0x01|0x02|0x10 = 19.
        let descs = fetch_generic_error_description(19);
        assert_eq!(
            descs,
            vec![
                "Blocking EEPROM from being read".to_string(),
                "Bad or unsupported EEPROM".to_string(),
            ]
        );

        // HIGH_TEMP_EVENT = 0x01|0x40 = 65 (non-blocking).
        assert_eq!(
            fetch_generic_error_description(65),
            vec!["High temperature".to_string()]
        );

        // No generic bits set (plain insert) -> empty.
        assert!(fetch_generic_error_description(1).is_empty());
    }
}
