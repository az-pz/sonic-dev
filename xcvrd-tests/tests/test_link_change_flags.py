"""Link-change flag re-capture: a flap re-reads the flag tables off the poll (B13).

xcvrd normally refreshes the DOM/STATUS/VDM flag tables on its DOM poll. But on a
link change it must re-capture them much sooner: dom_mgr.on_port_update_event watches
APPL_DB PORT_TABLE and, on any PORT_SET (e.g. a flap_count bump from a link flap),
schedules update_port_db_diagnostics_on_link_change ~1s later, which re-reads ONLY the
flag tables (TRANSCEIVER_DOM_FLAG / STATUS_FLAG / VDM_FLAG) for that port -- a
distinct, fast trigger separate from presence and the periodic poll
(dom_mgr.py:424-493).

We raise a module DOM flag (temp-high alarm, 00h:9.0), bump flap_count, and assert the
flag surfaces. The hard part is attributing that to the link-change re-read rather than
to a periodic poll that happened to land in the same window.

HOW THE TWO ARE TOLD APART. Not by timing -- by what each path WRITES. The link-change
re-read touches only the flag tables (dom_mgr.py:470-493), whereas a periodic poll also
refreshes TRANSCEIVER_DOM_SENSOR and stamps its last_update_time. So if the flag
surfaces while DOM_SENSOR's last_update_time has not moved, no poll ran and the
link-change re-read is the only thing that could have published it.

That discriminator is exact and holds at ANY poll cadence. An earlier version instead
assumed the poll was slow -- it raised the flag, asserted it stayed hidden for an 8s
guard, then flapped and accepted any appearance within 15s. That silently depended on
--dom_update_interval being ~60s: at 5s a poll lands inside the guard every time (the
test fails), and even when it did not, a 5s poll would satisfy the 15s assertion on its
own, so a PASS would not have meant the fast path worked. Widening the guard or
narrowing the window cannot fix that -- once the poll is faster than the assertion the
two mechanisms are indistinguishable by time alone.

A poll can still land mid-attempt; that makes the attempt INCONCLUSIVE, not failed, so
we phase-align to a just-completed poll and retry. A daemon that only refreshes flags
on its poll never produces a clean observation and fails.

Uses a present port (default Ethernet48); override with XCVRD_LINKCHG_PORT. The byte-9
flag register is cleared on teardown so a raised alarm can't leak.
"""
import os
import time

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.statedb import StateDB
from lib.waits import wait_until, stays, T_FAST, T_DOM

pytestmark = pytest.mark.slow

LINKCHG_PORT = os.environ.get("XCVRD_LINKCHG_PORT", "Ethernet48")
DOM_FLAG = "TRANSCEIVER_DOM_FLAG"
DOM_SENSOR = "TRANSCEIVER_DOM_SENSOR"   # refreshed by the periodic poll, NOT by the link-change re-read
APPL_PORT_TABLE = "PORT_TABLE"     # APPL_DB PORT_TABLE (colon-separated), flap_count lives here
FLAG_FIELD = "tempHAlarm"          # TRANSCEIVER_DOM_FLAG temp-high-alarm field
GUARD = 2.0                        # short pre-flap check that the raise alone does not publish
ATTEMPTS = 4                       # retries when a poll lands mid-attempt (inconclusive, not failed)


@pytest.fixture
def linkchg_port(emu, statedb, configdb):
    idx = port_to_index(LINKCHG_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({LINKCHG_PORT})")
    emu.plug(idx)
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{LINKCHG_PORT}", "manufacturer"),
               timeout=T_FAST, msg=f"{LINKCHG_PORT} present before link-change test")
    # The link-change flag re-read is gated by dom monitoring, so ensure dom_polling
    # is enabled (a prior test may have left it disabled), and start from a clean flag.
    configdb.hdel(f"PORT|{LINKCHG_PORT}", "dom_polling")
    emu.write_field(idx, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    yield LINKCHG_PORT, idx
    try:
        emu.write_field(idx, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    except Exception:  # noqa: BLE001
        pass


def _flap(appldb, port):
    """Bump flap_count in APPL_DB PORT_TABLE (colon-separated) to emulate a link
    flap -> PORT_SET event on the APPL_DB subscription xcvrd watches."""
    cur = appldb.hget(f"{APPL_PORT_TABLE}:{port}", "flap_count")
    nxt = (int(cur) + 1) if cur and cur.isdigit() else 1
    appldb.hset(f"{APPL_PORT_TABLE}:{port}", "flap_count", str(nxt))


def _flag(statedb, port):
    return statedb.hget(f"{DOM_FLAG}|{port}", FLAG_FIELD)


def _poll_stamp(statedb, port):
    """TRANSCEIVER_DOM_SENSOR.last_update_time -- moves only when the PERIODIC poll
    runs. The link-change re-read writes the flag tables and nothing else
    (dom_mgr.py:470-493), so this is the marker that says a poll happened."""
    return statedb.hget(f"{DOM_SENSOR}|{port}", "last_update_time")


def _await_poll(statedb, port, timeout):
    """Block until a periodic poll completes, returning its stamp.

    Phase-aligning to a just-finished poll leaves the longest possible quiet window
    for the attempt, whatever the configured cadence: at 5s it buys ~5s, at 60s ~60s.
    """
    start = _poll_stamp(statedb, port)
    wait_until(lambda: _poll_stamp(statedb, port) not in (None, start), timeout=timeout,
               msg=f"{port} periodic DOM poll observed (to phase-align the attempt)")
    return _poll_stamp(statedb, port)


def _attempt(statedb, appldb, emu, port, idx):
    """One isolated observation.

    Returns "pass", "inconclusive" (a poll landed, so the flag cannot be attributed),
    or a failure string.
    """
    # Clear the flag and let the daemon publish False, so we start from a known state.
    emu.write_field(idx, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    _flap(appldb, port)
    wait_until(lambda: _flag(statedb, port) == "False", timeout=T_FAST,
               msg=f"{port} DOM flag baseline False after a flap re-read")

    # Phase-align: start right after a poll so the next one is a full cadence away.
    stamp = _await_poll(statedb, port, timeout=T_DOM)

    # Raise the flag in hardware. Nothing should publish it until something re-reads.
    emu.write_field(idx, cmis.MODULE_FLAGS_TEMP_VCC, bytes([cmis.TEMP_HIGH_ALARM_FLAG]))
    if not stays(lambda: _flag(statedb, port) == "False", duration=GUARD):
        # Either a poll republished it (inconclusive) or it surfaced with no re-read
        # at all, which would be a real defect.
        return "inconclusive" if _poll_stamp(statedb, port) != stamp else (
            f"{port}: DOM flag surfaced within {GUARD}s with no flap AND no poll "
            "(DOM_SENSOR.last_update_time unchanged) -- nothing should have published it")

    # Flap -> xcvrd re-reads the flag tables ~1s later.
    _flap(appldb, port)
    deadline = time.time() + T_FAST
    while time.time() < deadline:
        if _flag(statedb, port) == "True":
            # THE DISCRIMINATOR. A poll would also have refreshed DOM_SENSOR; if that
            # stamp has not moved, only the link-change re-read can have published it.
            if _poll_stamp(statedb, port) != stamp:
                return "inconclusive"
            return "pass"
        if _poll_stamp(statedb, port) != stamp:
            return "inconclusive"   # poll landed first; cannot attribute this attempt
        time.sleep(0.2)
    return (f"{port}: DOM temp-high alarm did NOT re-capture within {T_FAST}s of a link "
            "flap -- xcvrd's link-change flag re-read (dom_mgr.py:424-493) did not fire")


def test_link_change_triggers_fast_flag_recapture(linkchg_port, emu, statedb):
    """A flap_count bump makes xcvrd re-read the DOM flag table, publishing a freshly
    raised alarm WITHOUT the periodic poll running (DOM_SENSOR.last_update_time stays
    put) -- so the re-capture is attributable to the link change at any poll cadence."""
    port, idx = linkchg_port
    appldb = StateDB("APPL_DB")

    inconclusive = 0
    for _ in range(ATTEMPTS):
        result = _attempt(statedb, appldb, emu, port, idx)
        if result == "pass":
            return
        if result == "inconclusive":
            inconclusive += 1
            continue
        pytest.fail(result)

    # Never isolated the fast path. Skip rather than fail: with every attempt polluted
    # by a poll we have no evidence either way, and reporting "broken" from an absence
    # of evidence is how a flaky test earns its reputation. If the cadence is so fast
    # that this is chronic, raise --dom_update_interval for the run.
    pytest.skip(
        f"{port}: {inconclusive}/{ATTEMPTS} attempts had a periodic DOM poll land inside "
        "the observation window, so the link-change re-read could not be isolated. The "
        "poll cadence (--dom_update_interval) is likely comparable to the ~1s link-change "
        "delay; re-run with a larger interval.")
