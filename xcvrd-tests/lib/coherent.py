"""Coherent (C-CMIS / ZR) laser-tuning capability provisioning + register consts (B16).

For a coherent module xcvrd's CmisManagerTask drives two control-plane tuning
writes during bring-up (cmis_manager_task.py):
  * Tx output power  -- DP_PRE_INIT_CHECK: configure_tx_output_power -> set_tx_power,
    writes TX_CONFIG_POWER (12h:200). Guarded by tx_power != get_tx_config_power().
  * laser frequency  -- AP_CONF: configure_laser_frequency -> set_laser_freq, writes
    GRID_SPACING (12h:128) + LASER_CONFIG_CHANNEL (12h:136). Guarded by
    freq != get_laser_config_freq() AND validate_frequency_and_grid().

validate_frequency_and_grid reads the module's tuning CAPABILITY from page 04h
(supported grid spacings + the low/high channel range + the min/max programmable
power). The emulator's coherent module does not advertise these, so we provision
them here (pure harness stimulus on raw pages the emulator serves) and pick a
75GHz-grid frequency inside the range. The tuning values themselves are computed
and written by xcvrd; the test asserts the register WRITES on the Monitor trace.
"""
import struct

# --- page 04h Laser Capabilities Advertisement (non-banked) -----------------
CAP_PAGE = 0x04
SUPPORT_GRID_OFFSET = 128        # 04h:128 supported grid spacings (bit7 = 75GHz)
LOW_CHANNEL_OFFSET = 158         # 04h:158 >h lowest supported channel number
HIGH_CHANNEL_OFFSET = 160        # 04h:160 >h highest supported channel number
MIN_PROG_POWER_OFFSET = 198      # 04h:198 >h min programmable Tx power (scale 100 -> dBm)
MAX_PROG_POWER_OFFSET = 200      # 04h:200 >h max programmable Tx power (scale 100 -> dBm)
GRID_75GHZ_BIT = 0x80            # SUPPORT_GRID bit7 -> 75GHz grid supported

# --- page 12h Media Lane Provisioning: the tuning register writes (banked) ---
TUNE_PAGE = 0x12
GRID_SPACING_OFFSET = 128        # 12h:128 grid spacing selection (set_laser_freq)
LASER_CONFIG_CHANNEL_OFFSET = 136  # 12h:136 >h channel number (set_laser_freq)
TX_CONFIG_POWER_OFFSET = 200     # 12h:200 >h configured Tx power, scale 100 (set_tx_power)

# A 75GHz-grid frequency in the provisioned range: channel = (freq-193100)/25 = 3,
# and 3 % 3 == 0 so it is a valid 75GHz-grid channel. Distinct from the module's
# default (193100) so the freq != get_laser_config_freq() guard trips.
LASER_FREQ_GHZ = 193175
TX_POWER_DBM = -10.0


def provision_tuning_capability(emu, index, low_ch=-100, high_ch=100,
                                min_power_dbm=-20.0, max_power_dbm=0.0):
    """Advertise laser-tuning capability on page 04h so validate_frequency_and_grid
    and get_supported_power_config accept LASER_FREQ_GHZ / TX_POWER_DBM."""
    emu.write(index, 0, CAP_PAGE, SUPPORT_GRID_OFFSET, bytes([GRID_75GHZ_BIT]))
    emu.write(index, 0, CAP_PAGE, LOW_CHANNEL_OFFSET, struct.pack(">h", low_ch))
    emu.write(index, 0, CAP_PAGE, HIGH_CHANNEL_OFFSET, struct.pack(">h", high_ch))
    emu.write(index, 0, CAP_PAGE, MIN_PROG_POWER_OFFSET,
              struct.pack(">h", int(round(min_power_dbm * 100))))
    emu.write(index, 0, CAP_PAGE, MAX_PROG_POWER_OFFSET,
              struct.pack(">h", int(round(max_power_dbm * 100))))


def clear_tuning_registers(emu, index):
    """Zero the page-12h tuning registers so the next configure guard
    (tx_power != get_tx_config_power(), freq != get_laser_config_freq()) trips and
    xcvrd re-issues the writes."""
    for off in (GRID_SPACING_OFFSET, LASER_CONFIG_CHANNEL_OFFSET, TX_CONFIG_POWER_OFFSET):
        emu.write(index, 0, TUNE_PAGE, off, b"\x00\x00")


def deprovision(emu, index):
    """Undo provision_tuning_capability() + clear the tuning registers."""
    for off in (SUPPORT_GRID_OFFSET, LOW_CHANNEL_OFFSET, HIGH_CHANNEL_OFFSET,
                MIN_PROG_POWER_OFFSET, MAX_PROG_POWER_OFFSET):
        emu.write(index, 0, CAP_PAGE, off, b"\x00\x00")
    clear_tuning_registers(emu, index)
