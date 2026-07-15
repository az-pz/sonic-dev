"""CMIS register helpers.

Only the bits the v1 tests need: locating and (de)coding the module-level DOM
monitors that live in lower memory (page 00h), per CMIS v5.2 8.10. Values are
stored big-endian in the emulator EEPROM and read back via the same
(bank, page, offset) the bridge's sfp.py uses.
"""

# Module-level monitors, page 00h lower memory (offset < 128).
TEMP = (0, 0, 14, 2)     # (bank, page, offset, length) S16, 1/256 degC
VCC = (0, 0, 16, 2)      # U16, 100 uV
AUX1 = (0, 0, 18, 2)
AUX2 = (0, 0, 20, 2)

# Identity fields (page 00h) used by info-content sanity checks.
SFF8024_IDENTIFIER = (0, 0, 0, 1)


def encode_temperature(celsius):
    """Encode degrees Celsius as CMIS S16 (1/256 degC), big-endian 2 bytes."""
    raw = int(round(celsius * 256.0))
    return raw.to_bytes(2, "big", signed=True)


def decode_temperature(data):
    """Decode 2 big-endian bytes as CMIS S16 temperature -> degrees Celsius."""
    return int.from_bytes(bytes(data), "big", signed=True) / 256.0


def encode_voltage(volts):
    """Encode volts as CMIS U16 (100 uV increments), big-endian 2 bytes."""
    raw = int(round(volts / 100e-6))
    return raw.to_bytes(2, "big", signed=False)


def decode_voltage(data):
    """Decode 2 big-endian bytes as CMIS U16 supply voltage -> volts."""
    return int.from_bytes(bytes(data), "big", signed=False) * 100e-6
