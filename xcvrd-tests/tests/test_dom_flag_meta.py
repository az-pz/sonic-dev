"""TRANSCEIVER_DOM_FLAG breadth + flag change-tracking metadata.

Two gaps beyond the existing dom_flag golden (which only snapshots a single raised
temperature flag):

  * flag GROUPS -- xcvrd decodes every module DOM flag group, not just the
    temp-high alarm. We raise the Vcc-high-alarm flag (00h:9.4) alongside the
    temp-high alarm (00h:9.0) and assert BOTH surface in TRANSCEIVER_DOM_FLAG.

  * flag CHANGE-TRACKING metadata -- on every flag transition xcvrd maintains
    three side tables (CMIS HLD "flag change count / set time / clear time"):
      TRANSCEIVER_DOM_FLAG_CHANGE_COUNT|<port>
      TRANSCEIVER_DOM_FLAG_SET_TIME|<port>
      TRANSCEIVER_DOM_FLAG_CLEAR_TIME|<port>
    Raising a flag bumps its change count and stamps SET_TIME; clearing it bumps
    the count again and stamps CLEAR_TIME. A reduced daemon that publishes the
    flag value but not the change-tracking metadata fails here.

The change count is CUMULATIVE in STATE_DB (it survives across runs), so we assert
a DELTA around a raise/clear rather than an absolute value. No emulator change: the
emulator holds the written flag byte with no clear-on-read, like the dom_flag gate.
"""
import pytest

from lib import cmis
from lib.waits import wait_until, T_DOM

pytestmark = pytest.mark.slow

DOM_FLAG = "TRANSCEIVER_DOM_FLAG"
CHANGE_COUNT = "TRANSCEIVER_DOM_FLAG_CHANGE_COUNT"
SET_TIME = "TRANSCEIVER_DOM_FLAG_SET_TIME"
CLEAR_TIME = "TRANSCEIVER_DOM_FLAG_CLEAR_TIME"
NEVER = "never"


@pytest.fixture
def dom_flags(module):
    """``module`` whose byte-9 temp/vcc flag register is restored on teardown so a
    raised alarm can't leak into later tests / the next user."""
    snap = module.emu.read_field(module.index, cmis.MODULE_FLAGS_TEMP_VCC)
    yield module
    try:
        module.emu.write_field(module.index, cmis.MODULE_FLAGS_TEMP_VCC, snap)
    except Exception:  # noqa: BLE001
        pass


def _flag(m, field):
    return m.db.hget(f"{DOM_FLAG}|{m.port}", field)


def _count(m, table, field):
    v = m.db.hget(f"{table}|{m.port}", field)
    try:
        return int(v)
    except (TypeError, ValueError):
        return 0


def _meta(m, table, field):
    return m.db.hget(f"{table}|{m.port}", field)


def test_dom_flag_groups_temp_and_vcc(dom_flags):
    """xcvrd decodes multiple DOM flag groups: raising temp-high (00h:9.0) AND
    vcc-high (00h:9.4) surfaces both tempHAlarm and vccHAlarm; clearing drops both."""
    m = dom_flags
    m.plug()
    # baseline: table published, neither alarm raised.
    wait_until(lambda: _flag(m, "tempHAlarm") == "False" and _flag(m, "vccHAlarm") == "False",
               timeout=T_DOM, msg=f"{m.port} DOM flag baseline (temp+vcc False)")

    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC,
                      bytes([cmis.TEMP_HIGH_ALARM_FLAG | cmis.VCC_HIGH_ALARM_FLAG]))
    wait_until(lambda: _flag(m, "tempHAlarm") == "True" and _flag(m, "vccHAlarm") == "True",
               timeout=T_DOM,
               msg=f"{m.port} both tempHAlarm and vccHAlarm set after raising 00h:9 bits 0+4")

    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    wait_until(lambda: _flag(m, "tempHAlarm") == "False" and _flag(m, "vccHAlarm") == "False",
               timeout=T_DOM, msg=f"{m.port} both flags cleared")


def test_dom_flag_change_count_and_times(dom_flags):
    """A flag transition bumps TRANSCEIVER_DOM_FLAG_CHANGE_COUNT and stamps
    SET_TIME (on raise) / CLEAR_TIME (on clear) for that flag."""
    m = dom_flags
    m.plug()
    field = "tempHAlarm"

    # Start from a known-cleared baseline so the raise is a real transition.
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    wait_until(lambda: _flag(m, field) == "False", timeout=T_DOM,
               msg=f"{m.port} {field} cleared baseline")
    base = _count(m, CHANGE_COUNT, field)

    # Raise -> change count increments by 1, SET_TIME is stamped (not 'never').
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([cmis.TEMP_HIGH_ALARM_FLAG]))
    wait_until(lambda: _flag(m, field) == "True", timeout=T_DOM,
               msg=f"{m.port} {field} raised")
    wait_until(lambda: _count(m, CHANGE_COUNT, field) == base + 1, timeout=T_DOM,
               msg=f"{m.port} {field} change count {base} -> {base + 1} on raise")
    assert _meta(m, SET_TIME, field) not in (None, NEVER), \
        f"{m.port} {field} SET_TIME not stamped on raise (got {_meta(m, SET_TIME, field)!r})"

    # Clear -> change count increments again, CLEAR_TIME is stamped.
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    wait_until(lambda: _flag(m, field) == "False", timeout=T_DOM,
               msg=f"{m.port} {field} cleared")
    wait_until(lambda: _count(m, CHANGE_COUNT, field) == base + 2, timeout=T_DOM,
               msg=f"{m.port} {field} change count {base + 1} -> {base + 2} on clear")
    assert _meta(m, CLEAR_TIME, field) not in (None, NEVER), \
        f"{m.port} {field} CLEAR_TIME not stamped on clear (got {_meta(m, CLEAR_TIME, field)!r})"
