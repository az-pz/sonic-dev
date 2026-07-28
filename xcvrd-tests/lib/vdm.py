"""VDM (Versatile Diagnostics Monitoring) provisioning as harness stimulus.

CMIS VDM is a paged mechanism the sonic_xcvr CMIS API reads over raw EEPROM --
exactly the raw pages the emulator already serves -- so we can drive real VDM
output with no emulator change, the same way the DOM-flag / optics-SI tests do.

Memory layout the API reads (per CMIS v5.2 8.14 + sonic_xcvr cmisVDM.py):

  * advertise      01h:142.6  VDM_SUPPORTED           (module supports VDM at all)
                   2Fh:128    VDM_SUPPORTED_PAGE      (# descriptor pages beyond 0x20)
  * descriptor     page 0x20  2 bytes/observable at 128 + 2*index:
                     even byte = (threshold_set_id << 4) | (lane - 1)
                     odd  byte = VDM type ID (see codes VDM_TYPE)
  * real value     page 0x24 (=desc+4)  2 bytes/observable at 128 + 2*index
  * thresholds     page 0x28 (=desc+8)  8 bytes/threshold-set at 128 + 8*tsid:
                     HighAlarm, LowAlarm, HighWarn, LowWarn (2 bytes each)
  * flags          page 0x2C  4 bits/observable at 128 + index//2,
                     bit 4*(index%2) + {0:HAlarm, 1:LAlarm, 2:HWarn, 3:LWarn}

Values are encoded in the observable's CMIS format: S16/U16 are scaled integers;
F16 is a mantissa*10^(exponent-24) float (used for BER / errored-frame ratios).

Only BASIC (instantaneous) observables are provisioned. Statistic (min/max/avg)
observables would make xcvrd run the VDM freeze/unfreeze + statistic read, which
it only does on a non-low-power (admin-up) port -- and provisioning on an admin-up
port flips its TRANSCEIVER_INFO.vdm_supported and disturbs the datapath goldens.
The basic set covers every field the VDM parity gate checks.
"""
import struct

# --- register locations -----------------------------------------------------
VDM_ADV_FIELD = (0, 1, 142, 1)     # 01h:142, VDM_SUPPORTED is bit 6
VDM_SUPPORTED_BIT = 0x40
VDM_CTRL_PAGE = 0x2F
VDM_SUPPORTED_PAGE_OFFSET = 128    # 2Fh:128 (# descriptor pages beyond 0x20)
VDM_DESC_PAGE = 0x20
VDM_VALUE_PAGE = 0x24              # descriptor page + 4
VDM_THRESH_PAGE = 0x28             # descriptor page + 8
VDM_FLAG_PAGE = 0x2C
PAGE_UPPER = 128                   # upper-page offset (128..255)

# Flag bit positions within an observable's nibble.
FLAG_BITS = {"halarm": 0, "lalarm": 1, "hwarn": 2, "lwarn": 3}


# --- one basic observable per (type_id -> db field). thresholds = (HA, LA, HW, LW).
# (type_id, db_field_prefix, fmt, scale, value, thresholds). type_id + fmt/scale
# come from sonic_xcvr codes VDM_TYPE; db_field from CMIS_VDM_KEY_TO_DB_PREFIX_KEY_MAP.
VDM_SAMPLES = [
    (4,  "laser_temperature_media",             "S16", 1.0 / 256, 45.0,   (80.0, -5.0, 75.0, 0.0)),
    (5,  "esnr_media_input",                    "U16", 1.0 / 256, 18.0,   (35.0, 5.0, 30.0, 7.0)),
    (6,  "esnr_host_input",                     "U16", 1.0 / 256, 20.0,   (35.0, 5.0, 30.0, 7.0)),
    (7,  "pam4_level_transition_media_input",   "U16", 1.0 / 256, 15.0,   (25.0, 3.0, 22.0, 5.0)),
    (8,  "pam4_level_transition_host_input",    "U16", 1.0 / 256, 16.0,   (25.0, 3.0, 22.0, 5.0)),
    (15, "prefec_ber_curr_media_input",         "F16", 1.0,       1.0e-5, (1.0e-3, 0.0, 1.0e-4, 0.0)),
    (16, "prefec_ber_curr_host_input",          "F16", 1.0,       2.0e-6, (1.0e-3, 0.0, 1.0e-4, 0.0)),
    (23, "errored_frames_curr_media_input",     "F16", 1.0,       3.0e-7, (1.0e-4, 0.0, 1.0e-5, 0.0)),
    (24, "errored_frames_curr_host_input",      "F16", 1.0,       4.0e-7, (1.0e-4, 0.0, 1.0e-5, 0.0)),
]

# The STATE_DB field name is the db prefix + lane number (lane 1 here).
EXPECTED_REAL = {f"{prefix}1": val for _, prefix, _, _, val, _ in VDM_SAMPLES}
EXPECTED_THRESH = {  # {threshold_type: {field1: value}}
    "halarm": {f"{p}1": t[0] for _, p, _, _, _, t in VDM_SAMPLES},
    "lalarm": {f"{p}1": t[1] for _, p, _, _, _, t in VDM_SAMPLES},
    "hwarn":  {f"{p}1": t[2] for _, p, _, _, _, t in VDM_SAMPLES},
    "lwarn":  {f"{p}1": t[3] for _, p, _, _, _, t in VDM_SAMPLES},
}


def encode_f16(target):
    """Encode a positive float as CMIS F16 (value = (exp<<11)|mantissa, decoding to
    mantissa*10^(exp-24)). Picks the exponent whose mantissa lands nearest 1000 for
    precision. Returns a 16-bit int."""
    best = None
    for exp in range(0, 32):
        mant = int(round(target / (10.0 ** (exp - 24))))
        if 1 <= mant <= 0x7FF:
            score = abs(mant - 1000)
            if best is None or score < best[0]:
                best = (score, (exp << 11) | mant)
    return best[1] if best else 0


def decode_f16(value):
    """Inverse of encode_f16 (matches sonic_xcvr CmisVdmApi.get_F16)."""
    exp = (value >> 11) & 0x1F
    mant = value & 0x7FF
    return mant * 10.0 ** (exp - 24)


def _encode(fmt, scale, value):
    if fmt == "S16":
        return struct.pack(">h", int(round(value / scale)))
    if fmt == "U16":
        return struct.pack(">H", int(round(value / scale)))
    if fmt == "F16":
        return struct.pack(">H", encode_f16(value))
    raise ValueError(f"unknown VDM format {fmt!r}")


def _field_index(db_field):
    """Descriptor index of the observable publishing ``db_field`` (e.g.
    'laser_temperature_media1' -> 0)."""
    for i, (_, prefix, _, _, _, _) in enumerate(VDM_SAMPLES):
        if db_field == f"{prefix}1":
            return i
    raise KeyError(db_field)


def provision(emu, index, samples=VDM_SAMPLES):
    """Provision VDM on an emulated module: advertise support and write the
    descriptor / value / threshold pages so xcvrd publishes real VDM output.

    The advertisement is cached by xcvrd at module insertion, so the caller must
    re-insert the module (unplug + plug) after provisioning for it to take effect.
    """
    desc = bytearray(PAGE_UPPER)
    val = bytearray(PAGE_UPPER)
    thr = bytearray(PAGE_UPPER)
    for i, (type_id, _prefix, fmt, scale, value, thresholds) in enumerate(samples):
        tsid = i
        desc[2 * i] = (tsid << 4) | 0        # threshold-set id + lane (lane 1 = 0)
        desc[2 * i + 1] = type_id
        val[2 * i:2 * i + 2] = _encode(fmt, scale, value)
        base = 8 * tsid
        for j, t in enumerate(thresholds):   # HA, LA, HW, LW
            thr[base + 2 * j:base + 2 * j + 2] = _encode(fmt, scale, t)

    emu.write(index, 0, VDM_DESC_PAGE, PAGE_UPPER, bytes(desc))
    emu.write(index, 0, VDM_VALUE_PAGE, PAGE_UPPER, bytes(val))
    emu.write(index, 0, VDM_THRESH_PAGE, PAGE_UPPER, bytes(thr))
    emu.write(index, 0, VDM_FLAG_PAGE, PAGE_UPPER, bytes(PAGE_UPPER))  # flags start clear
    # advertise: one descriptor page (0x20) -> VDM_SUPPORTED_PAGE = 0
    emu.write(index, 0, VDM_CTRL_PAGE, VDM_SUPPORTED_PAGE_OFFSET, bytes([0x00]))
    adv = emu.read_field(index, VDM_ADV_FIELD)
    emu.write_field(index, VDM_ADV_FIELD, bytes([adv[0] | VDM_SUPPORTED_BIT]))


def raise_flag(emu, index, db_field, flag_type="halarm"):
    """Raise a VDM flag (high/low alarm/warn) for one observable on the flag page."""
    i = _field_index(db_field)
    off = PAGE_UPPER + i // 2
    bit = 4 * (i % 2) + FLAG_BITS[flag_type]
    cur = emu.read(index, 0, VDM_FLAG_PAGE, off, 1, force=True)
    emu.write(index, 0, VDM_FLAG_PAGE, off, bytes([cur[0] | (1 << bit)]))


def clear_flags(emu, index):
    emu.write(index, 0, VDM_FLAG_PAGE, PAGE_UPPER, bytes(PAGE_UPPER))


def deprovision(emu, index):
    """Undo provision(): clear the VDM_SUPPORTED advertisement and zero the VDM
    pages. Re-insert the module afterwards so xcvrd re-reads vdm_supported=False."""
    adv = emu.read_field(index, VDM_ADV_FIELD)
    emu.write_field(index, VDM_ADV_FIELD, bytes([adv[0] & ~VDM_SUPPORTED_BIT]))
    for page in (VDM_DESC_PAGE, VDM_VALUE_PAGE, VDM_THRESH_PAGE, VDM_FLAG_PAGE):
        emu.write(index, 0, page, PAGE_UPPER, bytes(PAGE_UPPER))
    emu.write(index, 0, VDM_CTRL_PAGE, VDM_SUPPORTED_PAGE_OFFSET, bytes([0x00]))
