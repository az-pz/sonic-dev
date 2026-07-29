"""CMIS intermediate cmis_state progression (cmis/cmis_manager_task.py state table).

Existing tests only observe the terminal `cmis_state == READY`. The CmisManagerTask
actually drives the port through an ordered, timer-paced sequence of states on
bring-up, each published to `TRANSCEIVER_STATUS_SW.cmis_state`:

  INSERTED -> DP_PRE_INIT_CHECK -> DP_DEINIT -> AP_CONFIGURED -> DP_INIT
           -> DP_TXON -> DP_ACTIVATION -> READY

Each intermediate state persists ~1s (an armed timer between states), so a re-plug
followed by rapid polling captures the sequence. This gates that the state machine
is genuinely traversed -- multiple distinct intermediate states are published and a
late datapath state (DP_TXON/DP_ACTIVATION) is reached before READY -- so a reduced
daemon that jumps straight to READY (or publishes no intermediate states) fails.
The machine may legitimately revisit an early state (e.g. a second DP_PRE_INIT_CHECK
pass), so the assertion is on the SET of states seen, not a strict order. No
emulator change: a re-plug + fast STATE_DB polling.

Uses an admin-up spare port (default Ethernet28) so the bring-up actually runs to
DP_ACTIVATION/READY and the golden ports stay undisturbed; override with
XCVRD_CMIS_PROGRESS_PORT.
"""
import os
import time

import pytest

from lib.emu import port_to_index
from lib.waits import wait_until, T_FAST, T_DOM

pytestmark = pytest.mark.slow

PROGRESS_PORT = os.environ.get("XCVRD_CMIS_PROGRESS_PORT", "Ethernet28")

# Canonical CMIS bring-up order (cmis_manager_task state table). Index is used to
# assert the observed states are published in a non-decreasing order.
CMIS_ORDER = [
    "INSERTED",
    "DP_PRE_INIT_CHECK",
    "DP_DEINIT",
    "AP_CONFIGURED",
    "DP_INIT",
    "DP_TXON",
    "DP_ACTIVATION",
    "READY",
]
CMIS_INDEX = {s: i for i, s in enumerate(CMIS_ORDER)}
# Non-terminal bring-up states (everything before READY).
INTERMEDIATE = set(CMIS_ORDER[:-1])


@pytest.fixture
def progress_port(emu, statedb, configdb):
    idx = port_to_index(PROGRESS_PORT)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({PROGRESS_PORT})")
    if configdb.hget(f"PORT|{PROGRESS_PORT}", "admin_status") != "up":
        pytest.skip(f"{PROGRESS_PORT} is not admin-up; the CMIS bring-up only runs the full "
                    "datapath progression on an admin-up port. Set XCVRD_CMIS_PROGRESS_PORT.")
    emu.plug(idx)
    _wait_cmis(statedb, PROGRESS_PORT, "READY", timeout=T_DOM)
    yield PROGRESS_PORT, idx
    # Leave the port healthy (present + READY) for the next test/user.
    try:
        emu.plug(idx)
        _wait_cmis(statedb, PROGRESS_PORT, "READY", timeout=T_DOM)
    except Exception:  # noqa: BLE001
        pass


def _cmis(statedb, port):
    return statedb.hget(f"TRANSCEIVER_STATUS_SW|{port}", "cmis_state")


def _wait_cmis(statedb, port, target, timeout):
    wait_until(lambda: _cmis(statedb, port) == target, timeout=timeout,
               msg=f"{port} cmis_state == {target}")


def _capture_progression(statedb, port, timeout):
    """Rapid-poll cmis_state, returning the ordered list of distinct values seen
    (from now until READY or timeout)."""
    seq = []
    last = None
    deadline = time.time() + timeout
    while time.time() < deadline:
        v = _cmis(statedb, port)
        if v != last:
            seq.append(v)
            last = v
        if v == "READY":
            break
        time.sleep(0.05)
    return seq


def test_cmis_state_progression(progress_port, emu, statedb):
    """A re-plug drives cmis_state through the ordered CMIS bring-up sequence, not
    straight to READY."""
    port, idx = progress_port

    # Force a fresh bring-up: unplug -> REMOVED, then plug and capture the states.
    emu.unplug(idx)
    wait_until(lambda: _cmis(statedb, port) in ("REMOVED", None)
               or not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} removed before bring-up capture")

    emu.plug(idx)
    seq = _capture_progression(statedb, port, timeout=T_DOM)

    # Reached READY.
    assert seq and seq[-1] == "READY", \
        f"{port} did not reach cmis_state READY (captured {seq})"

    # Every captured bring-up state is a known CMIS state (ignore the pre-plug
    # REMOVED / None and the terminal READY).
    unknown = [s for s in seq if s not in CMIS_INDEX and s not in ("REMOVED", None)]
    assert not unknown, f"{port} published unrecognized cmis_state(s) {unknown} (seq={seq})"

    # At least four distinct INTERMEDIATE states observed -- proves the machine is
    # actually traversed, tolerant to occasionally missing the fastest state. (The
    # machine can legitimately revisit an early state, e.g. a second
    # DP_PRE_INIT_CHECK pass before READY, so we assert on the SET of states seen
    # rather than a strict monotonic order.)
    seen_intermediate = {s for s in seq if s in INTERMEDIATE}
    assert len(seen_intermediate) >= 4, (
        f"{port} observed too few intermediate cmis_states {sorted(seen_intermediate)} "
        f"(seq={seq}); the daemon may be jumping to READY without traversing the machine")

    # A LATE datapath state (DP_TXON / DP_ACTIVATION) must be reached before READY,
    # proving the full provisioning path ran -- not just the early setup states.
    assert seen_intermediate & {"DP_TXON", "DP_ACTIVATION"}, (
        f"{port} never reached a late datapath state before READY "
        f"(intermediates {sorted(seen_intermediate)}, seq={seq})")
