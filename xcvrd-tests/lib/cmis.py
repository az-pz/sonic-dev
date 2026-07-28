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

# ModuleGlobalControls (CMIS 00h:26) -- written by lpmode/reset control ops.
MODULE_GLOBAL_CONTROLS = (0, 0, 26, 1)
MGC_PAGE = 0
MGC_OFFSET = 26
SOFTWARE_RESET_BIT = 0x08   # 00h:26.3 -- set by reset()
LOW_PWR_REQUEST_BIT = 0x10  # 00h:26.4 -- set by set_lpmode(True)

# Identity fields (page 00h) used by info-content sanity checks.
SFF8024_IDENTIFIER = (0, 0, 0, 1)

# Module Flags, page 00h lower memory (CMIS v5.2 8.9). Byte 9 holds the latched
# temp/vcc monitor flags: bit0 TempMonHighAlarm, bit1 TempLowAlarm,
# bit2 TempHighWarn, bit3 TempLowWarn, bit4 VccHighAlarm, ... On real hardware
# these are RO/COR (clear-on-read); the emulator holds the written value (no
# clear-on-read), so a raised flag is a stable stimulus for the DOM-flag scenario.
MODULE_FLAGS_TEMP_VCC = (0, 0, 9, 1)
TEMP_HIGH_ALARM_FLAG = 0x01   # 00h:9.0 TempMonHighAlarmFlag
VCC_HIGH_ALARM_FLAG = 0x10    # 00h:9.4 VccMonHighAlarmFlag

# Module Flags byte 8 (page 0 lower, CMIS v5.2 8.9): module/datapath firmware
# fault + module state-change latched flags. xcvrd decodes bit0 ->
# module_state_changed, bit1 -> module_firmware_fault, bit2 ->
# datapath_firmware_fault in TRANSCEIVER_STATUS_FLAG.
MODULE_FLAGS_FW_STATE = (0, 0, 8, 1)
MODULE_FW_FAULT_FLAG = 0x02   # 00h:8.1 ModuleFirmwareErrorFlag
DP_FW_FAULT_FLAG = 0x04       # 00h:8.2 DataPathFirmwareErrorFlag
MODULE_STATE_CHANGED_FLAG = 0x01  # 00h:8.0 ModuleStateChangedFlag

# CMIS page 01h Supported Signal Integrity Controls Advertisement (8.4.7). xcvrd
# only stages a given SI control if the module advertises support for it; the
# emulator config advertises none, so the SI-application test writes these bits.
SI_ADV_TX_CDR_OFFSET = 161    # 01h:161 bit0 TxCDRSupported
SI_ADV_RX_CDR_OFFSET = 162    # 01h:162 bit0 RxCDRSupported
TX_CDR_SUPPORTED = 0x01
RX_CDR_SUPPORTED = 0x01

# CMIS page 10h Staged Control Set 0. The SI controls (TX/RX CDR enable, input EQ,
# output EQ/amplitude) live at offsets 153-175 (TX 153-160, RX 161-175); the
# DPConfigLane bytes at 145-152 carry ExplicitControl in bit 0 -- 1 means the lane
# uses the Staged Control Set (i.e. the custom SI values) instead of the
# application defaults.
SCS0_PAGE = 0x10
SCS0_SI_CONTROL_RANGE = range(153, 176)
SCS0_DPCONFIG_RANGE = range(145, 153)
EXPLICIT_CONTROL_BIT = 0x01

# DataPathDeinit (10h:128, CMIS v5.2 8.8.1) -- one bit per host lane; setting a
# lane's bit tears that lane's datapath down. On a reconfiguration event (a port
# going admin-down, or a forced CMIS re-init) xcvrd's CmisManagerTask writes this
# register itself to deinit the active lanes before re-provisioning. The reference
# daemon deinits all eight host lanes (0xff) on admin-down; a correct daemon must
# at least deinit the lanes that were active.
DPDEINIT_OFFSET = 128

# DataPathStateLane (11h:128, CMIS v5.2 8.9.3) -- the module's own report of each
# host lane's datapath state, 4 bits per lane, two lanes per byte (lane 1 = byte
# 128 bits 3:0, lane 2 = byte 128 bits 7:4, ...). xcvrd reads this back to observe
# the datapath, and it tracks DataPathDeinit / re-provision writes deterministically
# (unlike the emulator's GetInfo.dpsms objects, which do not fully follow a raw
# DPDeinit write). State codes: 1=DataPathDeactivated, 4=DataPathActivated.
DP_STATE_PAGE = 0x11
DP_STATE_OFFSET = 128
DP_STATE_ACTIVATED = 0x4
DP_STATE_DEACTIVATED = 0x1


def decode_dp_lane_states(raw, n_lanes=8):
    """Decode page-11h DataPathStateLane bytes into a per-host-lane list of 4-bit
    state codes (host lane 1 first)."""
    states = []
    for i in range(n_lanes):
        states.append((raw[i // 2] >> (4 * (i % 2))) & 0x0F)
    return states


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


# --- CMIS page 02h Module Thresholds (CMIS v5.2 8.11) ------------------------
# Each threshold is a 2-byte big-endian register at an absolute page-02h offset;
# xcvrd decodes value = raw / scale (temp signed 1/256 C; voltage 100 uV -> V;
# tx/rx power raw/10000 mW then -> dBm; tx bias raw/500 mA). The emulator's EEPROM
# serves 0 for any unwritten byte, so a module has NO meaningful thresholds until
# these are written -- write_dom_thresholds gives the TRANSCEIVER_DOM_THRESHOLD
# projection real, discriminating values (otherwise every field is 0.0 / -inf and
# a daemon that publishes zeros would pass the golden).
THRESHOLDS_PAGE = 2

# (offset, raw_register_value, signed). raw = natural_value * cmis_scale.
DOM_THRESHOLD_WRITES = [
    (128,  75 * 256, True),   # temphighalarm      75.0 C
    (130,  -5 * 256, True),   # templowalarm       -5.0 C
    (132,  70 * 256, True),   # temphighwarning    70.0 C
    (134, -10 * 256, True),   # templowwarning    -10.0 C
    (136, 36000, False),      # vcchighalarm        3.6 V
    (138, 30000, False),      # vcclowalarm         3.0 V
    (140, 35000, False),      # vcchighwarning      3.5 V
    (142, 31000, False),      # vcclowwarning       3.1 V
    (176, 40000, False),      # txpowerhighalarm    4.0 mW
    (178, 10000, False),      # txpowerlowalarm     1.0 mW
    (180, 35000, False),      # txpowerhighwarning  3.5 mW
    (182, 12000, False),      # txpowerlowwarning   1.2 mW
    (184,  6500, False),      # txbiashighalarm    13.0 mA
    (186,  3000, False),      # txbiaslowalarm      6.0 mA
    (188,  6000, False),      # txbiashighwarning  12.0 mA
    (190,  3500, False),      # txbiaslowwarning    7.0 mA
    (192, 20000, False),      # rxpowerhighalarm    2.0 mW
    (194,  5000, False),      # rxpowerlowalarm     0.5 mW
    (196, 18000, False),      # rxpowerhighwarning  1.8 mW
    (198,  6000, False),      # rxpowerlowwarning   0.6 mW
]

# Sentinel a scenario can wait on to confirm xcvrd re-read the enriched thresholds.
DOM_THRESHOLD_SENTINEL = ("temphighalarm", "75.0")


def write_dom_thresholds(emu, index):
    """Write the CMIS page-02h Module Thresholds on the emulated module so xcvrd
    reads and publishes real TRANSCEIVER_DOM_THRESHOLD values.

    Thresholds are static EEPROM data cached by xcvrd at module insertion (NOT
    re-read on the periodic DOM poll), so the caller must re-insert the module
    (unplug + plug) after writing for xcvrd to pick them up.
    """
    for offset, raw, signed in DOM_THRESHOLD_WRITES:
        emu.write(index, 0, THRESHOLDS_PAGE, offset,
                  int(raw).to_bytes(2, "big", signed=signed))
