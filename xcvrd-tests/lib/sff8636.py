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

# 00h:93 Power Control byte (SFF-8636 6.2.6). SffManagerTask drives two controls
# here on module insert / admin-up: set_lpmode(False) takes the module OUT of low
# power (Power_override bit0 = 1, Power_set bit1 = 0), and enable_high_power_class
# sets the High Power Class Enable bit for a class >= 5 module.
POWER_CTRL = (0, 0, 93, 1)
POWER_CTRL_PAGE = 0
POWER_CTRL_OFFSET = 93
POWER_OVERRIDE_BIT = 0x01           # 93.0 Power_override
POWER_SET_BIT = 0x02                # 93.1 Power_set (1 => low power)
HIGH_POWER_CLASS_5_7_BIT = 0x04     # 93.2 High Power Class Enable (classes 5-7)
HIGH_POWER_CLASS_8_BIT = 0x08       # 93.3 High Power Class Enable (class 8)

# 00h:129 Power Class byte. enable_high_power_class only writes when the module
# advertises power class >= 5; the emulator SFF module ships class 4, so the
# high-power-class gate provisions this to a class-5 code first.
POWER_CLASS = (0, 0, 129, 1)
POWER_CLASS_OFFSET = 129
POWER_CLASS_5_VALUE = 193           # Sff8636Codes.POWER_CLASSES: 193 = Power Class 5


def sff_mgr_enabled():
    """True iff the running xcvrd was launched with --enable_sff_mgr.

    SffManagerTask (which drives the SFF-8636 TX_DISABLE / power-control registers)
    is OFF by default, so we read the live xcvrd cmdline out of the pmon container.
    Matches both the Python daemon (`python3 /usr/local/bin/xcvrd`) and an injected
    Rust build (`xcvrd-rs`).

    NOTE: the Rust injection shim (`/usr/local/bin/xcvrd` -> `os.execv(".../xcvrd-rs",
    ["xcvrd-rs"])`) drops argv, so flags set in supervisor do NOT reach the Rust
    daemon. That is why this probes the *process*, not the supervisor config: it
    reports what the daemon actually received.
    """
    import subprocess
    try:
        out = subprocess.run(
            ["docker", "exec", "pmon", "bash", "-lc",
             "for p in $(pgrep -f 'xcvrd' 2>/dev/null); do "
             "tr '\\0' ' ' < /proc/$p/cmdline 2>/dev/null; echo; done"],
            capture_output=True, text=True, timeout=20)
        return "--enable_sff_mgr" in out.stdout
    except Exception:  # noqa: BLE001
        return False


def require_sff_mgr():
    """FAIL (do not skip) when xcvrd is running without --enable_sff_mgr.

    These are parity gates for real daemon behaviour, so a missing SffManagerTask
    is a genuine coverage hole, not an "n/a" environment. Skipping it silently
    hides the fact that an entire xcvrd control path went unverified, so we fail
    loudly with instructions instead.
    """
    import pytest
    if sff_mgr_enabled():
        return
    pytest.fail(
        "xcvrd is running WITHOUT --enable_sff_mgr, so SffManagerTask never runs and "
        "no SFF-8636 control behaviour can be observed.\n"
        "This is a FAILURE, not a skip: the SFF control path is part of the parity "
        "surface and must be exercised.\n"
        "To enable it on the DUT:\n"
        "  1. edit pmon's supervisor entry so xcvrd starts with the flag, e.g.\n"
        "       command=python3 /usr/local/bin/xcvrd --enable_sff_mgr\n"
        "     (docker exec pmon vi /etc/supervisor/conf.d/supervisord.conf)\n"
        "  2. docker exec pmon supervisorctl update && "
        "docker exec pmon supervisorctl restart xcvrd\n"
        "If a Rust xcvrd is injected, note the shim execs the binary with NO argv, "
        "so it must pass --enable_sff_mgr itself (or default the SFF manager on).",
        pytrace=False)

