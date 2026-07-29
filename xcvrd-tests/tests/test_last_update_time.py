"""last_update_time stamping on DOM / STATUS rows (dom/utilities/db/utils.py:53,171).

Every diagnostic row xcvrd posts carries a ``last_update_time`` field, the UTC time
of the post formatted with ``"%a %b %d %H:%M:%S %Y"`` (e.g. "Wed Jul 29 18:34:54
2026"). It is currently unchecked. A reduced daemon that omits or mis-formats the
stamp fails here. No emulator change: pure STATE_DB observation.
"""
from datetime import datetime

import pytest

from lib.waits import wait_until, eventually, T_FAST, T_DOM

pytestmark = pytest.mark.slow

TIME_FORMAT = "%a %b %d %H:%M:%S %Y"


def _parse(value):
    return datetime.strptime(value, TIME_FORMAT)


def test_dom_sensor_last_update_time(module):
    """TRANSCEIVER_DOM_SENSOR carries a well-formed last_update_time."""
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    row = eventually(lambda: module.db.hgetall(f"TRANSCEIVER_DOM_SENSOR|{module.port}") or None,
                     timeout=2 * T_DOM, msg=f"{module.port} DOM_SENSOR populated")
    lut = row.get("last_update_time")
    assert lut, f"{module.port} DOM_SENSOR missing last_update_time (row={sorted(row)})"
    _parse(lut)  # raises ValueError if the format is wrong


def test_status_last_update_time(module):
    """TRANSCEIVER_STATUS carries a well-formed last_update_time."""
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    wait_until(lambda: module.db.hgetall(f"TRANSCEIVER_STATUS|{module.port}").get("last_update_time"),
               timeout=2 * T_DOM, msg=f"{module.port} STATUS last_update_time stamped")
    lut = module.db.hget(f"TRANSCEIVER_STATUS|{module.port}", "last_update_time")
    _parse(lut)


def test_last_update_time_advances_on_refresh(module):
    """The stamp advances across DOM polls (the row is genuinely re-posted)."""
    module.plug()
    module.wait_info_populated(timeout=T_FAST)
    first = eventually(lambda: module.db.hget(f"TRANSCEIVER_DOM_SENSOR|{module.port}", "last_update_time"),
                       timeout=2 * T_DOM, msg=f"{module.port} initial DOM last_update_time")
    t0 = _parse(first)
    # Wait for a later post: the stamp must move forward within ~2 DOM cycles.
    wait_until(lambda: _parse(module.db.hget(f"TRANSCEIVER_DOM_SENSOR|{module.port}",
                                             "last_update_time")) > t0,
               timeout=2 * T_DOM, msg=f"{module.port} DOM last_update_time advances on refresh")
