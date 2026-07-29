"""remove_stale_transceiver_info at daemon init (xcvrd.py:986-1018,1073).

Cold-start correctness: if a module was unplugged while xcvrd was down, its
TRANSCEIVER_INFO row is stale (STATE_DB survives the daemon). On startup xcvrd
purges the INFO row for any physically-absent port (presence checked via
_wrapper_get_presence). A daemon that trusts stale STATE_DB on boot fails here.

No emulator change: xcvrd stop/start (via supervisorctl) + emulator unplug.
"""
import pytest

from lib.waits import wait_until, T_FAST, T_BASELINE

pytestmark = pytest.mark.slow


@pytest.fixture
def stale_env(module, xcvrd):
    """Guarantee the port + daemon are healthy again after the test, whatever
    happens (this test deliberately stops xcvrd)."""
    yield module, xcvrd
    try:
        module.plug()
        xcvrd.wait_healthy(module.port, timeout=T_BASELINE)
    except Exception:  # noqa: BLE001
        try:
            xcvrd.start()
        except Exception:  # noqa: BLE001
            pass


def test_stale_info_purged_on_init(stale_env):
    """A TRANSCEIVER_INFO row for a module unplugged while xcvrd was down is
    purged when xcvrd restarts."""
    module, xcvrd = stale_env
    # 1) start from a present module with a real INFO row.
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    assert module.info_populated()

    # 2) take xcvrd down, THEN unplug -> STATE_DB keeps the now-stale INFO row.
    xcvrd.stop()
    assert not xcvrd.is_running()
    module.unplug()
    assert module.db.exists(f"TRANSCEIVER_INFO|{module.port}"), \
        "precondition: stale INFO row should survive xcvrd being stopped"

    # 3) bring xcvrd back up (module still absent) -> init purges the stale row.
    xcvrd.start()
    wait_until(lambda: not module.db.exists(f"TRANSCEIVER_INFO|{module.port}"),
               timeout=T_BASELINE,
               msg=f"{module.port} stale TRANSCEIVER_INFO purged on xcvrd init")
