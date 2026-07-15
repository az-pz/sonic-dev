"""SFP error-event model (mirrors sonic_platform_base.sfp_base).

xcvrd's SfpStateUpdateTask treats a change-event value that is neither '1'
(inserted) nor '0' (removed) as an error bitmap: it decodes the generic bits
into TRANSCEIVER_STATUS_SW.error and, if the BLOCKING bit is set, removes the
port's DOM info (the static TRANSCEIVER_INFO is kept). These constants let the
tests build injectable error events and predict the resulting error strings.
"""

# Bit values (SfpBase.SFP_ERROR_BIT_*)
STATUS_INSERTED = 0x01
BLOCKING = 0x02
POWER_BUDGET_EXCEEDED = 0x04
I2C_STUCK = 0x08
BAD_EEPROM = 0x10
UNSUPPORTED_CABLE = 0x20
HIGH_TEMP = 0x40
BAD_CABLE = 0x80

# Bit -> description (SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT)
DESCRIPTION = {
    BLOCKING: "Blocking EEPROM from being read",
    POWER_BUDGET_EXCEEDED: "Power budget exceeded",
    I2C_STUCK: "Bus stuck (I2C data or clock shorted)",
    BAD_EEPROM: "Bad or unsupported EEPROM",
    UNSUPPORTED_CABLE: "Unsupported cable",
    HIGH_TEMP: "High temperature",
    BAD_CABLE: "Bad cable (module/cable is shorted)",
}

# Ready-to-inject events (module stays INSERTED while erroring).
I2C_STUCK_EVENT = STATUS_INSERTED | BLOCKING | I2C_STUCK      # blocking
BAD_EEPROM_EVENT = STATUS_INSERTED | BLOCKING | BAD_EEPROM    # blocking
HIGH_TEMP_EVENT = STATUS_INSERTED | HIGH_TEMP                 # non-blocking


def is_blocking(bitmap):
    return bool(int(bitmap) & BLOCKING)


def descriptions(bitmap):
    """Generic error descriptions xcvrd will join with '|' for STATUS_SW.error."""
    bitmap = int(bitmap)
    return [d for bit, d in DESCRIPTION.items() if bitmap & bit]
