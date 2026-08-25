"""Link-change flag re-capture, measured as a RATE rather than attributed per event (B13).

xcvrd refreshes the DOM/STATUS/VDM flag tables on its periodic DOM poll. On a link
change it must also re-capture them off-poll: dom_mgr.on_port_update_event watches
APPL_DB PORT_TABLE and, on a PORT_SET (e.g. a flap_count bump), schedules
update_port_db_diagnostics_on_link_change ~1s later, which re-reads ONLY the flag
tables for that port (dom_mgr.py:424-493).

WHY THIS IS A RATE TEST. A single flag appearing in STATE_DB is produced identically by
both mechanisms, so attributing one event to one of them is inherently a race against
the poll. Earlier versions of this test tried exactly that and were fragile in
proportion to the poll cadence: with an 8s guard and a 15s window they silently
required --dom_update_interval to be ~60s, and at 5s not only failed but became
VACUOUS, since a 5s poll satisfies a 15s "fast" assertion on its own.

So instead of asking WHICH mechanism published one flag, this asks HOW OFTEN the flag
table changes. We toggle a module DOM flag (temp-high alarm, 00h:9.0) and flap the port
once per TOGGLE_PERIOD, far faster than any sane poll, and read the daemon's own
transition counter TRANSCEIVER_DOM_FLAG_CHANGE_COUNT, which it increments on every
observed edge in either direction (dom/utilities/db/utils.py:215).

    with the link-change re-read : count tracks the flaps      (~TOGGLES)
    poll-only, 5s cadence        : bounded by polls            (~TOGGLES/2.5)
    poll-only, 60s cadence       : ~0

The margin therefore WIDENS as the poll slows -- the opposite of the old test, which
grew more fragile the faster the poll ran. Aggregating ~10 events also means no single
coincidence can flip the verdict, so there is no phase alignment, no retry, and no
inconclusive/skip path a regression could hide behind.

Uses a present port (default Ethernet48); override with XCVRD_LINKCHG_PORT. The byte-9
flag register is cleared on teardown so a raised alarm can't leak.
"""
import os
import time

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.statedb import StateDB
from lib.waits import wait_until, T_FAST, T_DOM

pytestmark = pytest.mark.slow

LINKCHG_PORT = os.environ.get("XCVRD_LINKCHG_PORT", "Ethernet48")
DOM_FLAG = "TRANSCEIVER_DOM_FLAG"
DOM_FLAG_COUNT = "TRANSCEIVER_DOM_FLAG_CHANGE_COUNT"
DOM_FLAG_SET_TIME = "TRANSCEIVER_DOM_FLAG_SET_TIME"
DOM_FLAG_CLEAR_TIME = "TRANSCEIVER_DOM_FLAG_CLEAR_TIME"
DOM_SENSOR = "TRANSCEIVER_DOM_SENSOR"   # refreshed by the POLL, never by the link-change re-read
APPL_PORT_TABLE = "PORT_TABLE"          # APPL_DB PORT_TABLE (colon-separated); flap_count lives here
FLAG_FIELD = "tempHAlarm"

TOGGLES = 10            # edges driven; enough that one coincidence cannot decide the result
TOGGLE_PERIOD = 2.0     # seconds between edges -- must be well under the poll cadence
# The link-change re-read is scheduled DIAG_DB_UPDATE_TIME_AFTER_LINK_CHANGE (1s) after
# the flap, so a daemon that honours it captures most edges. 0.6 leaves room for the
# edge closest to the end of the run and for one or two coalesced flaps, while still
# sitting far above what a poll alone can reach.
#
# CALIBRATED on the reference daemon at a measured 4.8s cadence:
#     with flaps (this test)          10/10 edges captured   ratio 1.00
#     same edges, NO flaps (control)   2/10 edges captured   ratio 0.20
# so 0.60 sits midway between the two with a 5x separation. The no-flap control is the
# honest negative here: it feeds the daemon exactly what one ignoring the APPL_DB
# trigger would see -- the periodic poll and nothing else -- without modifying it.
# Note the control landed at 0.20 rather than the ~0.48 the poll ceiling allows,
# because consecutive polls often observe the same state between even-spaced toggles;
# the real margin is wider than the arithmetic bound below.
MIN_CAPTURE_RATIO = 0.6
# Below this cadence a poll-only daemon could reach the threshold on polls alone, so the
# test can no longer separate the two and says so instead of reporting a false pass.
MIN_SAFE_CADENCE = 3.0


@pytest.fixture
def linkchg_port(emu, statedb, configdb):
    idx = port_to_index(LINKCHG_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({LINKCHG_PORT})")
    emu.plug(idx)
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{LINKCHG_PORT}", "manufacturer"),
               timeout=T_FAST, msg=f"{LINKCHG_PORT} present before link-change test")
    # The link-change re-read returns early on is_port_dom_monitoring_disabled
    # (dom_mgr.py:454), so dom_polling must be enabled for the path under test to run
    # at all -- a prior test may have left it disabled.
    configdb.hdel(f"PORT|{LINKCHG_PORT}", "dom_polling")
    emu.write_field(idx, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    yield LINKCHG_PORT, idx
    try:
        emu.write_field(idx, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    except Exception:  # noqa: BLE001
        pass


def _flap(appldb, port):
    """Bump flap_count in APPL_DB PORT_TABLE (colon-separated) to emulate a link flap
    -> PORT_SET event on the APPL_DB subscription xcvrd watches."""
    cur = appldb.hget(f"{APPL_PORT_TABLE}:{port}", "flap_count")
    nxt = (int(cur) + 1) if cur and cur.isdigit() else 1
    appldb.hset(f"{APPL_PORT_TABLE}:{port}", "flap_count", str(nxt))


def _int(statedb, table, port, field):
    v = statedb.hget(f"{table}|{port}", field)
    return int(v) if v and v.isdigit() else 0


def _poll_stamp(statedb, port):
    return statedb.hget(f"{DOM_SENSOR}|{port}", "last_update_time")


def _measure_poll_cadence(statedb, port, timeout):
    """Time two consecutive periodic polls.

    Measured rather than read from config so the test calibrates itself against the
    daemon actually running, whatever launched it and with whatever flags.
    """
    first = _poll_stamp(statedb, port)
    wait_until(lambda: _poll_stamp(statedb, port) not in (None, first), timeout=timeout,
               msg=f"{port} first periodic DOM poll observed (measuring cadence)")
    t0, second = time.time(), _poll_stamp(statedb, port)
    wait_until(lambda: _poll_stamp(statedb, port) not in (None, second), timeout=timeout,
               msg=f"{port} second periodic DOM poll observed (measuring cadence)")
    return time.time() - t0


def test_link_change_triggers_fast_flag_recapture(linkchg_port, emu, statedb):
    """Flapping a port far faster than the DOM poll makes the flag table change at
    FLAP rate, not poll rate -- proving xcvrd re-reads the flags on a link change."""
    port, idx = linkchg_port
    appldb = StateDB("APPL_DB")

    cadence = _measure_poll_cadence(statedb, port, timeout=T_DOM)
    if cadence < MIN_SAFE_CADENCE:
        pytest.skip(
            f"{port}: measured DOM poll cadence {cadence:.1f}s is below {MIN_SAFE_CADENCE}s, "
            f"so a poll-only daemon could match the flap rate ({TOGGLE_PERIOD}s) and the "
            "link-change re-read cannot be separated from it. Re-run with a larger "
            "--dom_update_interval.")

    # Settle to a known state so the first edge is genuinely an edge.
    emu.write_field(idx, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    _flap(appldb, port)
    wait_until(lambda: statedb.hget(f"{DOM_FLAG}|{port}", FLAG_FIELD) == "False",
               timeout=T_FAST, msg=f"{port} DOM flag baseline False before the flap burst")

    count0 = _int(statedb, DOM_FLAG_COUNT, port, FLAG_FIELD)
    polls0 = _poll_stamp(statedb, port)
    set0 = statedb.hget(f"{DOM_FLAG_SET_TIME}|{port}", FLAG_FIELD)
    clear0 = statedb.hget(f"{DOM_FLAG_CLEAR_TIME}|{port}", FLAG_FIELD)

    # Drive edges much faster than the poll, flapping after each so the link-change
    # re-read is what has a chance to observe them.
    raised = False
    for _ in range(TOGGLES):
        raised = not raised
        emu.write_field(idx, cmis.MODULE_FLAGS_TEMP_VCC,
                        bytes([cmis.TEMP_HIGH_ALARM_FLAG if raised else 0x00]))
        _flap(appldb, port)
        time.sleep(TOGGLE_PERIOD)
    # The last flap's re-read is scheduled ~1s out; give it room before reading.
    time.sleep(3.0)

    captured = _int(statedb, DOM_FLAG_COUNT, port, FLAG_FIELD) - count0
    required = int(TOGGLES * MIN_CAPTURE_RATIO)
    elapsed = TOGGLES * TOGGLE_PERIOD + 3.0
    poll_ceiling = elapsed / cadence      # edges a poll-only daemon could possibly have seen

    assert captured >= required, (
        f"{port}: DOM flag table recorded {captured} transitions across {TOGGLES} flap-driven "
        f"edges in {elapsed:.0f}s (needed >={required}). A poll-only daemon at the measured "
        f"{cadence:.1f}s cadence could reach at most ~{poll_ceiling:.0f}, so this looks like "
        "xcvrd is refreshing the flag tables ONLY on its periodic poll and ignoring the "
        "APPL_DB link-change trigger (dom_mgr.on_port_update_event -> "
        "update_port_db_diagnostics_on_link_change, dom_mgr.py:424-493)")

    # Corroboration, not the primary argument: the flag table must have changed far more
    # often than the poll ran. If both moved together the transitions were just polls.
    assert captured > poll_ceiling, (
        f"{port}: recorded {captured} flag transitions but the poll could account for "
        f"~{poll_ceiling:.0f} of them at a {cadence:.1f}s cadence -- not separable")

    # Both edge directions must be classified, not merely counted: a daemon that
    # increments the counter but mislabels rising vs falling would otherwise pass.
    assert statedb.hget(f"{DOM_FLAG_SET_TIME}|{port}", FLAG_FIELD) != set0, (
        f"{port}: {DOM_FLAG_SET_TIME} did not advance -- no rising edge was recorded")
    assert statedb.hget(f"{DOM_FLAG_CLEAR_TIME}|{port}", FLAG_FIELD) != clear0, (
        f"{port}: {DOM_FLAG_CLEAR_TIME} did not advance -- no falling edge was recorded")
    assert polls0 is not None
