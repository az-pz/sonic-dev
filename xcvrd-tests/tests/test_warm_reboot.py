"""Warm / fast-reboot lifecycle: xcvrd must not disrupt live datapaths (T4-C).

On a normal shutdown xcvrd's deinit() deletes the TRANSCEIVER_STATUS /
TRANSCEIVER_STATUS_SW tables. During a warm or fast reboot it must instead LEAVE
them in place, so the datapath state persists across the xcvrd restart and the
data plane is not disrupted (xcvrd.py:1132 -- the tables are only deleted
`if not is_warm_fast_reboot`). Fast reboot is signalled by
FAST_RESTART_ENABLE_TABLE|system.enable == 'true', which xcvrd's deinit reads
fresh (no caching), so we can toggle it around a controlled stop.

Two gates:
  * fast reboot: with the flag set, stopping xcvrd PRESERVES TRANSCEIVER_STATUS
    (module_state / DP{n}State survive) -- the daemon adopts the existing state.
  * normal (control): with the flag clear, stopping xcvrd DELETES it.

A reduced daemon that always flushes STATUS on shutdown (ignoring warm/fast
reboot) fails the first gate. Uses an admin-up, datapath-activated port (default
Ethernet8; override XCVRD_REBOOT_PORT).

These tests drive the xcvrd lifecycle (stop/start) and a system flag, so the
fixture ALWAYS clears the flag and restores a healthy, live xcvrd on teardown --
even if the test fails mid-way -- so nothing leaks into the rest of the suite.
"""
import os

import pytest

from lib.emu import port_to_index
from lib.waits import wait_until, T_FAST, T_DOM, T_BASELINE

REBOOT_PORT = os.environ.get("XCVRD_REBOOT_PORT", "Ethernet8")
FAST_RESTART_FLAG = "FAST_RESTART_ENABLE_TABLE|system"

pytestmark = pytest.mark.slow


def _status_present(statedb, port):
    return statedb.exists(f"TRANSCEIVER_STATUS|{port}")


@pytest.fixture
def reboot_ctl(xcvrd, statedb, configdb, emu):
    """Yield an admin-up port whose TRANSCEIVER_STATUS is populated, with a
    bulletproof teardown that clears the fast-reboot flag and restores a healthy
    xcvrd (flush + restart + repopulate) no matter how the test exits."""
    idx = port_to_index(REBOOT_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({REBOOT_PORT})")
    if configdb.hget(f"PORT|{REBOOT_PORT}", "admin_status") != "up":
        pytest.skip(f"{REBOOT_PORT} is not admin-up; STATUS preservation is only meaningful "
                    "for a port with an active datapath. Set XCVRD_REBOOT_PORT to an admin-up port.")
    # Defensive: clear any leftover flag, make sure xcvrd is up.
    statedb.hdel(FAST_RESTART_FLAG, "enable")
    if not xcvrd.is_running():
        xcvrd.start()
    emu.plug(idx)
    # STATUS is published by DomInfoUpdateTask a beat after INFO; wait for it +
    # an activated datapath so there is real state to preserve.
    wait_until(lambda: _status_present(statedb, REBOOT_PORT)
               and statedb.hget(f"TRANSCEIVER_STATUS|{REBOOT_PORT}", "DP1State") == "DataPathActivated",
               timeout=T_DOM, msg=f"{REBOOT_PORT} TRANSCEIVER_STATUS populated + activated")
    try:
        yield REBOOT_PORT
    finally:
        statedb.hdel(FAST_RESTART_FLAG, "enable")
        if not xcvrd.is_running():
            xcvrd.start()
        xcvrd.wait_healthy(REBOOT_PORT, timeout=T_BASELINE)


def test_fast_reboot_preserves_transceiver_status(reboot_ctl, xcvrd, statedb):
    """With the fast-reboot flag set, stopping xcvrd leaves TRANSCEIVER_STATUS in
    place (module_state / DP1State survive) -- the datapath is not disrupted."""
    port = reboot_ctl
    assert _status_present(statedb, port), f"{port} STATUS not present at start"

    statedb.hset(FAST_RESTART_FLAG, "enable", "true")
    xcvrd.stop()
    assert not xcvrd.is_running(), "xcvrd did not stop"

    assert _status_present(statedb, port), (
        f"{port}: TRANSCEIVER_STATUS was deleted on a FAST-REBOOT shutdown -- xcvrd must "
        "preserve it so the datapath is not disrupted across the restart")
    st = statedb.hgetall(f"TRANSCEIVER_STATUS|{port}")
    assert st.get("module_state") == "ModuleReady", \
        f"{port}: preserved module_state={st.get('module_state')!r} (expected ModuleReady)"
    assert st.get("DP1State") == "DataPathActivated", \
        f"{port}: preserved DP1State={st.get('DP1State')!r} (expected DataPathActivated)"


def test_normal_shutdown_clears_transceiver_status(reboot_ctl, xcvrd, statedb):
    """Control: with no fast-reboot flag, a normal xcvrd shutdown deletes
    TRANSCEIVER_STATUS (so the preservation above is a real fast-reboot behaviour,
    not a daemon that never cleans up)."""
    port = reboot_ctl
    statedb.hdel(FAST_RESTART_FLAG, "enable")
    assert _status_present(statedb, port), f"{port} STATUS not present at start"

    xcvrd.stop()
    assert not xcvrd.is_running(), "xcvrd did not stop"

    wait_until(lambda: not _status_present(statedb, port), timeout=T_FAST,
               msg=f"{port} TRANSCEIVER_STATUS deleted on a normal shutdown")
