"""Link-change flag re-capture: a flap re-reads the flag tables off the poll (B13).

xcvrd normally refreshes the DOM/STATUS/VDM flag tables on its ~60s DOM poll. But
on a link change it must re-capture them much sooner: dom_mgr.on_port_update_event
watches APPL_DB PORT_TABLE and, on any PORT_SET (e.g. a flap_count bump from a
link flap), schedules update_port_db_diagnostics_on_link_change ~1s later, which
re-reads ONLY the flag tables (TRANSCEIVER_DOM_FLAG / STATUS_FLAG / VDM_FLAG) for
that port -- a distinct, fast trigger separate from presence and the periodic poll
(dom_mgr.py:424-493).

We raise a module DOM flag (temp-high alarm, 00h:9.0) WITHOUT waiting for the poll,
confirm it has not yet surfaced, then bump flap_count in APPL_DB PORT_TABLE and
assert the flag appears FAST -- within T_FAST (15s), far under the ~60s DOM-poll
cadence (T_DOM). The pre-flap `stays` check establishes no poll raced in, so the
fast reaction is the link-change re-read, not a coincidental poll. A reduced daemon
that only refreshes flags on its slow poll (ignoring APPL_DB link changes) fails.

Uses a present port (default Ethernet48); override with XCVRD_LINKCHG_PORT. The
byte-9 flag register is cleared on teardown so a raised alarm can't leak.
"""
import os

import pytest

from lib import cmis
from lib.emu import port_to_index
from lib.statedb import StateDB
from lib.waits import wait_until, stays, T_FAST, T_DOM

pytestmark = pytest.mark.slow

LINKCHG_PORT = os.environ.get("XCVRD_LINKCHG_PORT", "Ethernet48")
DOM_FLAG = "TRANSCEIVER_DOM_FLAG"
APPL_PORT_TABLE = "PORT_TABLE"     # APPL_DB PORT_TABLE (colon-separated), flap_count lives here
FLAG_FIELD = "tempHAlarm"          # TRANSCEIVER_DOM_FLAG temp-high-alarm field
GUARD = 8.0                        # seconds to confirm no poll raced in pre-flap


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


def test_link_change_triggers_fast_flag_recapture(linkchg_port, emu, statedb):
    """A flap_count bump makes xcvrd re-read the DOM flag table within seconds --
    far sooner than its ~60s poll -- so a freshly-raised alarm surfaces fast."""
    port, idx = linkchg_port
    appldb = StateDB("APPL_DB")

    # Baseline: a flap re-read publishes the (cleared) flag as False.
    _flap(appldb, port)
    wait_until(lambda: _flag(statedb, port) == "False", timeout=T_FAST,
               msg=f"{port} DOM flag baseline False after a flap re-read")

    # Raise the flag but do NOT flap yet: confirm it has not surfaced (no poll raced
    # in), so the reaction below is attributable to the link-change re-read.
    emu.write_field(idx, cmis.MODULE_FLAGS_TEMP_VCC, bytes([cmis.TEMP_HIGH_ALARM_FLAG]))
    assert stays(lambda: _flag(statedb, port) == "False", duration=GUARD), (
        f"{port}: DOM flag surfaced within {GUARD}s WITHOUT a flap -- a poll raced in; "
        "re-run (the link-change trigger can't be isolated from a coincident poll here)")

    # Flap -> xcvrd re-reads the flag table ~1s later; the alarm appears fast.
    _flap(appldb, port)
    wait_until(lambda: _flag(statedb, port) == "True", timeout=T_FAST,
               msg=f"{port} DOM temp-high alarm re-captured FAST (<{T_FAST}s) after a link "
                   f"flap -- xcvrd's link-change flag re-read, well under the ~{int(T_DOM)}s poll")
