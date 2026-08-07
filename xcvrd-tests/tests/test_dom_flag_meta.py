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
from lib.waits import wait_until, stays, POLL, T_FAST, T_DOM

pytestmark = pytest.mark.slow

DOM_FLAG = "TRANSCEIVER_DOM_FLAG"
CHANGE_COUNT = "TRANSCEIVER_DOM_FLAG_CHANGE_COUNT"
SET_TIME = "TRANSCEIVER_DOM_FLAG_SET_TIME"
CLEAR_TIME = "TRANSCEIVER_DOM_FLAG_CLEAR_TIME"
NEVER = "never"


@pytest.fixture
def dom_flags(module):
    """``module`` whose byte-9 temp/vcc flag register is CLEARED on teardown so a
    raised alarm can't leak into later tests / the next user.

    We clear to 0x00 rather than restoring a snapshot: 0x00 is the only valid
    resting state (the emulator never sets a flag on its own), and restoring a
    snapshot would *perpetuate* a stuck flag -- if the byte were already raised at
    fixture setup, teardown would keep re-raising it, permanently breaking the
    temp+vcc baseline check.
    """
    yield module
    try:
        module.emu.write_field(module.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
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
    # Establish a KNOWN-cleared baseline first (like the other tests in this file).
    # The emulator holds whatever was last written to 00h:9 with no clear-on-read and
    # its state outlives a pytest run, so a flag raised by an earlier test (or an
    # earlier session) would otherwise leave this waiting for a 'False' that can never
    # arrive -- an 80s timeout that looks like a daemon bug but is stale stimulus.
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
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


def test_dom_flag_change_count_not_bumped_on_noop(dom_flags):
    """A no-op (writing the SAME already-raised value) must NOT bump the change
    count nor restamp SET_TIME -- the count tracks TRANSITIONS, not polls."""
    m = dom_flags
    m.plug()
    field = "tempHAlarm"

    # Establish a cleared baseline that xcvrd has actually PUBLISHED, so the raise
    # below is a real False->True transition (not a no-op against residual state).
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    wait_until(lambda: _flag(m, field) == "False", timeout=T_DOM,
               msg=f"{m.port} {field} cleared baseline")

    # Raise the flag (a real transition) and let the metadata settle.
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([cmis.TEMP_HIGH_ALARM_FLAG]))
    wait_until(lambda: _flag(m, field) == "True", timeout=T_DOM,
               msg=f"{m.port} {field} raised")
    wait_until(lambda: _meta(m, SET_TIME, field) not in (None, NEVER), timeout=T_DOM,
               msg=f"{m.port} {field} SET_TIME stamped")
    count_after_raise = _count(m, CHANGE_COUNT, field)
    set_time_after_raise = _meta(m, SET_TIME, field)

    # Re-assert the SAME raised value: the module still reads the flag set, so
    # xcvrd sees 1 -> 1 (no transition). Hold it across several DOM polls.
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([cmis.TEMP_HIGH_ALARM_FLAG]))
    assert stays(lambda: _count(m, CHANGE_COUNT, field) == count_after_raise
                 and _meta(m, SET_TIME, field) == set_time_after_raise,
                 duration=2 * POLL + T_DOM), (
        f"{m.port} {field} change count/set-time changed on a no-op "
        f"(count {count_after_raise} -> {_count(m, CHANGE_COUNT, field)}, "
        f"set_time {set_time_after_raise!r} -> {_meta(m, SET_TIME, field)!r})")

    # Leave the emulator flag register clean so a raised flag can't leak into the
    # next test (the fixture also restores the snapshot, but be explicit).
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    wait_until(lambda: _flag(m, field) == "False", timeout=T_DOM,
               msg=f"{m.port} {field} cleared after no-op test")


def test_dom_flag_metadata_initialized_on_first_publish(dom_flags):
    """After a re-plug (which deletes the metadata tables) the first DOM publish
    RE-INITIALIZES every flag's change count to 0 and its set/clear times to
    'never' -- the clean-slate seed for a flag that has not transitioned."""
    m = dom_flags
    field = "tempHAlarm"
    # Clear the flag byte so no flag is raised on the fresh insertion.
    m.emu.write_field(m.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    # Re-insert so xcvrd deletes + re-initializes the metadata tables.
    m.unplug()
    m.wait_info_cleared(timeout=T_FAST)
    m.plug()
    m.wait_info_populated(timeout=T_FAST)
    # First DOM publish initializes the metadata: count '0', times 'never'.
    wait_until(lambda: _meta(m, CHANGE_COUNT, field) is not None, timeout=2 * T_DOM,
               msg=f"{m.port} DOM flag metadata initialized on first publish")
    assert _meta(m, CHANGE_COUNT, field) == "0", \
        f"{m.port} {field} initial change count must be '0' (got {_meta(m, CHANGE_COUNT, field)!r})"
    assert _meta(m, SET_TIME, field) == NEVER, \
        f"{m.port} {field} initial SET_TIME must be 'never' (got {_meta(m, SET_TIME, field)!r})"
    assert _meta(m, CLEAR_TIME, field) == NEVER, \
        f"{m.port} {field} initial CLEAR_TIME must be 'never' (got {_meta(m, CLEAR_TIME, field)!r})"
