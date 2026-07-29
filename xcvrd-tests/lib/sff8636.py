"""SFF-8636 (QSFP28) register/port helpers for the emulated non-CMIS module.

The emulator serves one module (emu_config index for XCVRD_SFF_PORT, default
Ethernet40) as SFF-8636 instead of CMIS. SONiC selects the SFF-8636 api purely
from EEPROM byte 0 (identifier 0x11), which routes the port through xcvrd's
SffManagerTask rather than CmisManagerTask.
"""
import os

# Default SFF-8636 port (emu_config index 10 = Ethernet40). Override with env.
SFF_PORT = os.environ.get("XCVRD_SFF_PORT", "Ethernet40")

# EEPROM fields as (bank, page, offset, length), matching lib.emu.read_field.
IDENTIFIER = (0, 0, 0, 1)      # SFF8024 identifier; 0x11 = QSFP28
QSFP28_ID = 0x11

# 00h:86 per-host-lane TX_DISABLE (bit per lane, lane 1 = bit 0). This is the
# control register xcvrd's SffManagerTask drives in reaction to host_tx_ready /
# admin_status -- the SFF-8636 analogue of the CMIS DataPathDeinit gate.
TX_DISABLE = (0, 0, 86, 1)
TX_DISABLE_PAGE = 0
TX_DISABLE_OFFSET = 86
