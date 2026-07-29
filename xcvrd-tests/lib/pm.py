"""C-CMIS PM (performance monitoring) provisioning as harness stimulus.

TRANSCEIVER_PM is published only for a COHERENT (C-CMIS) module, and only inside
xcvrd's VDM statistic-freeze path (dom_mgr.py post_port_pm_info_to_db). It requires:
  * a coherent module      -> CCmisApi (is_coherent_module: 'ZR' in media interface)
  * VDM supported + a VDM *statistic* observable advertised (is_vdm_statistic_supported)
  * a successful VDM freeze/unfreeze (VDM_FREEZE_DONE / VDM_UNFREEZE_DONE)
  * an admin-up (non-lpmode) port

The coherent classification is an emulator CONFIG property: the module must advertise
a coherent media interface (400GBASE-ZR, SM media interface code 77 at 00h:87), which
the emulator resets from config on every plug -- so it comes from the emulator config,
not the harness. Everything else here is pure harness stimulus on raw pages the emulator
already serves (like the VDM gate):
  * VDM statistic advertisement -> descriptor page 0x20 + 01h:142.6 + 2Fh:128
  * pre-set freeze/unfreeze done -> 2Fh:145 bits 7/6, so xcvrd's freeze poll passes
    (the emulator has no VDM-freeze logic; the done bits are just raw memory)
  * PM statistic values          -> page 34h (FEC PM) + 35h (link PM), decoded by CCmisApi
"""
import struct

# --- VDM advertisement / freeze (page 01h + 2Fh) ----------------------------
VDM_ADV_FIELD = (0, 1, 142, 1)       # 01h:142, VDM_SUPPORTED is bit 6
VDM_SUPPORTED_BIT = 0x40
VDM_CTRL_PAGE = 0x2F
VDM_SUPPORTED_PAGE_OFFSET = 128      # 2Fh:128 (# descriptor pages beyond 0x20)
VDM_STATUS_OFFSET = 145              # 2Fh:145: UnfreezeDone bit6, FreezeDone bit7
VDM_FREEZE_DONE = 0x80
VDM_UNFREEZE_DONE = 0x40
VDM_DESC_PAGE = 0x20
PAGE_UPPER = 128
# A VDM statistic observable type (Pre-FEC BER Average Media Input, classified 'S').
VDM_STAT_TYPE_ID = 13

# --- PM register pages (C-CMIS) ---------------------------------------------
FEC_PM_PAGE = 0x34    # Pre-FEC BER bit/frame counters (avg from counters, min/max from sub-interval)
LINK_PM_PAGE = 0x35   # CD / DGD / OSNR / power etc.


def _u64(v):
    return struct.pack(">Q", int(v))


def _u16(v):
    return struct.pack(">H", int(v))


def _i16(v):
    return struct.pack(">h", int(v))


def _i32(v):
    return struct.pack(">i", int(v))


# Expected published TRANSCEIVER_PM values, derived from the provisioned registers
# (CCmisApi.get_pm_all: preFEC BER = corr_bits/bits, sub-interval for min/max; the
# rest are scaled register reads).
EXPECTED_PM = {
    "prefec_ber_avg": 1e-6,    # RX_CORR_BITS_PM / RX_BITS_PM = 1e6 / 1e12
    "prefec_ber_min": 5e-6,    # RX_MIN_CORR..SUBINT / RX_BITS_SUB_INTERVAL = 5e4 / 1e10
    "prefec_ber_max": 2e-5,    # RX_MAX_CORR..SUBINT / RX_BITS_SUB_INTERVAL = 2e5 / 1e10
    "cd_avg": 1500.0,          # RX_AVG_CD_PM (ps/nm), scale 1
    "dgd_avg": 5.0,            # RX_AVG_DGD_PM, raw 500 / scale 100
    "osnr_avg": 30.0,          # RX_AVG_OSNR_PM, raw 300 / scale 10
    "tx_power_avg": -1.5,      # TX_AVG_POWER_PM, raw -150 / scale 100
    "rx_tot_power_avg": -2.5,  # RX_AVG_POWER_PM, raw -250 / scale 100
}


def provision(emu, index):
    """Provision PM on a COHERENT emulated module so xcvrd publishes TRANSCEIVER_PM.

    The caller must re-insert the module afterwards (the VDM advertisement is cached
    by xcvrd at insertion). Does NOT set the coherent media interface -- that is an
    emulator-config property (see module docstring).
    """
    # 1) advertise one VDM statistic observable -> is_vdm_statistic_supported True
    desc = bytearray(PAGE_UPPER)
    desc[0] = (0 << 4) | 0            # threshold-set 0, lane 1
    desc[1] = VDM_STAT_TYPE_ID
    emu.write(index, 0, VDM_DESC_PAGE, PAGE_UPPER, bytes(desc))
    emu.write(index, 0, VDM_CTRL_PAGE, VDM_SUPPORTED_PAGE_OFFSET, bytes([0x00]))  # 1 descriptor page

    # 2) pre-set freeze/unfreeze DONE so xcvrd's freeze poll confirms (raw memory;
    #    the emulator has no freeze logic to set these bits itself).
    emu.write(index, 0, VDM_CTRL_PAGE, VDM_STATUS_OFFSET,
              bytes([VDM_FREEZE_DONE | VDM_UNFREEZE_DONE]))

    # 3) advertise VDM supported (01h:142.6), cached at insertion.
    adv = emu.read_field(index, VDM_ADV_FIELD)
    emu.write_field(index, VDM_ADV_FIELD, bytes([adv[0] | VDM_SUPPORTED_BIT]))

    # 4) real PM values. Page 34h FEC counters -> Pre-FEC BER; page 35h link metrics.
    fec = bytearray(PAGE_UPPER)
    fec[0:8] = _u64(1_000_000_000_000)    # 34h:128 RX_BITS_PM
    fec[8:16] = _u64(10_000_000_000)      # 34h:136 RX_BITS_SUB_INTERVAL_PM
    fec[16:24] = _u64(1_000_000)          # 34h:144 RX_CORR_BITS_PM               -> avg 1e-6
    fec[24:32] = _u64(50_000)             # 34h:152 RX_MIN_CORR_BITS_SUB_INTERVAL -> min 5e-6
    fec[32:40] = _u64(200_000)            # 34h:160 RX_MAX_CORR_BITS_SUB_INTERVAL -> max 2e-5
    emu.write(index, 0, FEC_PM_PAGE, PAGE_UPPER, bytes(fec))

    link = bytearray(PAGE_UPPER)
    link[0:4] = _i32(1500)                # 35h:128 RX_AVG_CD_PM  (ps/nm, scale 1)
    link[12:14] = _u16(500)               # 35h:140 RX_AVG_DGD_PM (scale 100 -> 5.0)
    link[30:32] = _u16(300)               # 35h:158 RX_AVG_OSNR_PM (scale 10 -> 30.0)
    link[54:56] = _i16(-150)              # 35h:182 TX_AVG_POWER_PM (scale 100 -> -1.5)
    link[60:62] = _i16(-250)              # 35h:188 RX_AVG_POWER_PM (scale 100 -> -2.5)
    emu.write(index, 0, LINK_PM_PAGE, PAGE_UPPER, bytes(link))


def deprovision(emu, index):
    """Undo provision(): clear the VDM advertisement + PM pages. Re-insert afterwards
    so xcvrd re-reads vdm_supported=False and stops publishing PM."""
    adv = emu.read_field(index, VDM_ADV_FIELD)
    emu.write_field(index, VDM_ADV_FIELD, bytes([adv[0] & ~VDM_SUPPORTED_BIT]))
    for page in (VDM_DESC_PAGE, FEC_PM_PAGE, LINK_PM_PAGE):
        emu.write(index, 0, page, PAGE_UPPER, bytes(PAGE_UPPER))
    emu.write(index, 0, VDM_CTRL_PAGE, VDM_STATUS_OFFSET, bytes([0x00]))
    emu.write(index, 0, VDM_CTRL_PAGE, VDM_SUPPORTED_PAGE_OFFSET, bytes([0x00]))
