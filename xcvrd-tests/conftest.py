"""Pytest fixtures for the xcvrd black-box harness.

Everything runs ON the DUT (admin@vlab-01): the emulator gRPC, sonic-db-cli and
the pmon supervisor are all local. Fixtures wire the stimulus (emulator),
observation (STATE_DB + Monitor trace) and daemon control together and enforce
per-test isolation (restore presence + any mutated DOM bytes).
"""
import os
import sys

import pytest

sys.path.insert(0, os.path.dirname(__file__))

from lib.emu import EmulatorClient, port_to_index, index_to_port  # noqa: E402
from lib.monitor import MonitorRecorder  # noqa: E402
from lib.statedb import StateDB  # noqa: E402
from lib.xcvrd_ctl import XcvrdControl  # noqa: E402
from lib import cmis  # noqa: E402
from lib.waits import wait_until, eventually, stays  # noqa: E402

TEST_PORT = os.environ.get("XCVRD_TEST_PORT", "Ethernet100")


class Module:
    """Test-facing view of one emulated transceiver + its STATE_DB rows."""

    def __init__(self, emu, statedb, index, port):
        self.emu = emu
        self.db = statedb
        self.index = index
        self.port = port

    # STATE_DB projections -------------------------------------------------
    def info(self):
        return self.db.hgetall(f"TRANSCEIVER_INFO|{self.port}")

    def dom(self):
        return self.db.hgetall(f"TRANSCEIVER_DOM_SENSOR|{self.port}")

    def status(self):
        """Rich CMIS status (module_state, DPxState, tx/rx status)."""
        return self.db.hgetall(f"TRANSCEIVER_STATUS|{self.port}")

    def status_sw(self):
        """SW status table: plug status ('1'/'0'), error, cmis_state (HLD 1.1.3)."""
        return self.db.hgetall(f"TRANSCEIVER_STATUS_SW|{self.port}")

    def info_manufacturer(self):
        return self.db.hget(f"TRANSCEIVER_INFO|{self.port}", "manufacturer")

    def info_populated(self):
        """True iff xcvrd currently advertises a real optic identity."""
        return bool(self.info_manufacturer())

    # stimulus -------------------------------------------------------------
    def unplug(self):
        self.emu.unplug(self.index)

    def plug(self):
        self.emu.plug(self.index)

    # waits ----------------------------------------------------------------
    def wait_info_populated(self, timeout=60):
        return wait_until(self.info_populated, timeout=timeout,
                          msg=f"{self.port} TRANSCEIVER_INFO populated")

    def wait_info_cleared(self, timeout=60):
        return wait_until(lambda: not self.info_populated(), timeout=timeout,
                          msg=f"{self.port} TRANSCEIVER_INFO cleared")


# --- session-scoped services -------------------------------------------------
@pytest.fixture(scope="session")
def emu():
    c = EmulatorClient()
    yield c
    c.close()


@pytest.fixture(scope="session")
def statedb():
    return StateDB("STATE_DB")


@pytest.fixture(scope="session")
def configdb():
    return StateDB("CONFIG_DB")


@pytest.fixture(scope="session")
def xcvrd(statedb):
    return XcvrdControl(statedb=statedb)


@pytest.fixture(scope="session")
def monitor():
    m = MonitorRecorder().start()
    yield m
    m.stop()


@pytest.fixture(scope="session")
def test_index(emu):
    idx = port_to_index(TEST_PORT)
    known = emu.list()
    if idx not in known:
        pytest.skip(f"emulator has no module {idx} ({TEST_PORT}); "
                    f"known indices: {sorted(known)}")
    return idx


@pytest.fixture(scope="session", autouse=True)
def _session_ready(emu, statedb, xcvrd, test_index):
    """Bring the testbed to a known-good baseline before any test runs."""
    emu.plug(test_index)
    if not xcvrd.is_running():
        xcvrd.start()
    port = index_to_port(test_index)
    wait_until(lambda: statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=90,
               msg=f"{port} TRANSCEIVER_INFO populated at session start "
                   "(emulator-backed xcvrd healthy)")
    yield


# --- function-scoped helpers -------------------------------------------------
@pytest.fixture
def module(emu, statedb, test_index):
    """Default test module with snapshot/restore isolation."""
    port = index_to_port(test_index)
    snap_temp = emu.read_field(test_index, cmis.TEMP)
    snap_vcc = emu.read_field(test_index, cmis.VCC)
    m = Module(emu, statedb, test_index, port)
    yield m
    # Restore presence + any DOM bytes a test mutated so tests don't leak.
    try:
        emu.plug(test_index)
        emu.write_field(test_index, cmis.TEMP, snap_temp)
        emu.write_field(test_index, cmis.VCC, snap_vcc)
    except Exception:  # noqa: BLE001
        pass


@pytest.fixture(autouse=True)
def _scope_monitor(monitor):
    """Clear the interaction trace before each test so windows are isolated."""
    monitor.clear()
    yield
