"""Presence / hot-plug lifecycle (HLD 1.1, 1.3, 1.4.2).

When a transceiver is unplugged in the emulator, xcvrd must remove its static
info from STATE_DB; on re-plug it must be restored. TRANSCEIVER_STATUS reflects
the plug state ('1' inserted, '0' removed).
"""
from lib.waits import wait_until, stays, T_FAST


def test_present_module_populates_info(module):
    """A present module advertises a real optic identity in TRANSCEIVER_INFO."""
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    assert module.info_manufacturer() == "xcvr-emu"


def test_unplug_clears_info(module):
    """Unplugging clears TRANSCEIVER_INFO (data wiped)."""
    module.wait_info_populated(timeout=T_FAST)
    module.unplug()
    module.wait_info_cleared(timeout=T_FAST)
    assert not module.info_populated()


def test_unplug_stays_cleared(module):
    """After unplug the port must not be silently re-added (no flapping)."""
    module.unplug()
    module.wait_info_cleared(timeout=T_FAST)
    assert stays(lambda: not module.info_populated(), duration=8.0), \
        f"{module.port} reappeared in TRANSCEIVER_INFO after unplug"


def test_replug_restores_info(module):
    """Re-plugging restores the identity xcvrd advertises."""
    module.unplug()
    module.wait_info_cleared(timeout=T_FAST)
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    assert module.info_manufacturer() == "xcvr-emu"


def test_status_reflects_plug_state(module):
    """TRANSCEIVER_STATUS_SW.status is '1' inserted / '0' removed (HLD 1.1.3).

    Note: the plug status/error flags live in the *SW* status table; the plain
    TRANSCEIVER_STATUS table carries the rich CMIS module/datapath state.
    """
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    wait_until(lambda: module.status_sw().get("status") == "1", timeout=T_FAST,
               msg=f"{module.port} STATUS_SW.status == '1' when inserted")

    module.unplug()
    module.wait_info_cleared(timeout=T_FAST)
    wait_until(lambda: module.status_sw().get("status") == "0", timeout=T_FAST,
               msg=f"{module.port} STATUS_SW.status == '0' when removed")


def test_present_module_reaches_cmis_ready(module):
    """A present CMIS module is driven to cmis_state READY by xcvrd."""
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    wait_until(lambda: module.status_sw().get("cmis_state") == "READY", timeout=T_FAST,
               msg=f"{module.port} cmis_state == READY")
