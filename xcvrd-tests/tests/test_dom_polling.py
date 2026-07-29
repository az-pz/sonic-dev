"""dom_polling=disabled CONFIG_DB knob halts DOM updates (dom_mgr.py:76-113,199).

An operator can set ``dom_polling`` = ``disabled`` on a PORT in CONFIG_DB to stop
xcvrd polling that port's DOM. xcvrd reads the field live from CONFIG_DB on every
DOM cycle (get_dom_polling_from_config_db), so disabling it makes the DOM tables
stop refreshing / repopulating for that port; the default (field absent) is
``enabled``. A real operator knob with zero coverage. No emulator change: a
CONFIG_DB write + STATE_DB observation.
"""
import pytest

from lib.waits import wait_until, eventually, stays, T_FAST, T_DOM

pytestmark = pytest.mark.slow

DOM_SENSOR = "TRANSCEIVER_DOM_SENSOR"


@pytest.fixture
def dom_polling(module, configdb):
    """Restore dom_polling to enabled (field removed) + a healthy port on teardown."""
    yield module, configdb
    try:
        configdb.hdel(f"PORT|{module.port}", "dom_polling")
        module.plug()
    except Exception:  # noqa: BLE001
        pass


def test_dom_polling_disabled_halts_dom(dom_polling):
    """Setting dom_polling=disabled stops DOM_SENSOR from being (re)published; the
    default restores it."""
    module, configdb = dom_polling
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    # DOM present at baseline (default enabled).
    eventually(lambda: module.db.exists(f"{DOM_SENSOR}|{module.port}") or None,
               timeout=2 * T_DOM, msg=f"{module.port} DOM_SENSOR present at baseline")

    # Disable polling, then clear the row: xcvrd must NOT repopulate it while
    # disabled. Poll is read live from CONFIG_DB, so no port event is needed.
    configdb.hset(f"PORT|{module.port}", "dom_polling", "disabled")
    module.db.delete(f"{DOM_SENSOR}|{module.port}")
    assert stays(lambda: not module.db.exists(f"{DOM_SENSOR}|{module.port}"),
                 duration=T_DOM + 20), \
        f"{module.port} DOM_SENSOR was republished despite dom_polling=disabled"

    # Re-enable -> DOM polling resumes and the row comes back.
    configdb.hdel(f"PORT|{module.port}", "dom_polling")
    wait_until(lambda: module.db.exists(f"{DOM_SENSOR}|{module.port}"), timeout=2 * T_DOM,
               msg=f"{module.port} DOM_SENSOR repopulated after re-enabling dom_polling")
