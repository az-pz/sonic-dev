"""Pytest fixtures for the xcvrd black-box harness.

Everything runs ON the DUT (admin@vlab-01): the emulator gRPC, sonic-db-cli and
the pmon supervisor are all local. Fixtures wire the stimulus (emulator),
observation (STATE_DB + Monitor trace) and daemon control together and enforce
per-test isolation (restore presence + any mutated DOM bytes).
"""
import os
import sys
import warnings

import pytest

sys.path.insert(0, os.path.dirname(__file__))

from lib.emu import EmulatorClient, port_to_index, index_to_port  # noqa: E402
from lib.monitor import MonitorRecorder  # noqa: E402
from lib.statedb import StateDB  # noqa: E402
from lib.xcvrd_ctl import XcvrdControl  # noqa: E402
from lib.inject import ErrorInjector  # noqa: E402
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
def injector(statedb):
    return ErrorInjector(statedb)


@pytest.fixture(scope="session")
def monitor():
    m = MonitorRecorder().start()
    yield m
    m.stop()


@pytest.fixture(scope="session")
def test_index(emu):
    try:
        known = emu.list()
    except Exception as e:  # noqa: BLE001
        pytest.fail(f"emulator not reachable at :50051 ({e}). Is the xcvr-emu "
                    "container running on the DUT?")
    idx = port_to_index(TEST_PORT)
    if idx not in known:
        pytest.skip(f"emulator has no module {idx} ({TEST_PORT}); "
                    f"known indices: {sorted(known)}")
    return idx


@pytest.fixture(scope="session", autouse=True)
def _clean_baseline(emu, statedb, xcvrd, injector, test_index):
    """Establish a fresh, verified-live baseline before any test runs.

    CRITICAL: TRANSCEIVER_* rows live in Redis STATE_DB and survive xcvrd being
    stopped, so read-only tests would otherwise PASS on stale residue even when
    the daemon is dead. We flush those tables, restart xcvrd, and require it to
    repopulate -- proving it is alive and emulator-backed. If it can't, we fail
    the whole suite loudly instead of letting stale data mask a broken daemon.
    """
    # 0) clear any leftover error injections from a previous run.
    injector.clear_all()

    # 1) emulator must be reachable and every module plugged in.
    try:
        for idx in emu.indices():
            emu.plug(idx)
    except Exception as e:  # noqa: BLE001
        pytest.fail(f"emulator not reachable at session start: {e}")

    # 2) flush stale rows + restart xcvrd + require repopulation.
    port = index_to_port(test_index)
    was_running = xcvrd.is_running()
    if not xcvrd.wait_healthy(port, timeout=90):
        pytest.fail(
            "xcvrd is not healthy: after flushing TRANSCEIVER_* and restarting, "
            f"it did not repopulate TRANSCEIVER_INFO|{port} (status="
            f"{xcvrd.status()!r}). Aborting so stale STATE_DB cannot mask a dead "
            "daemon. Start/repair xcvrd and the emulator, then re-run.")
    if not was_running:
        warnings.warn(UserWarning(
            "xcvrd was NOT running at session start; the clean baseline started "
            "it. Tests ran against the freshly-started daemon."))
    yield

    # 3) cleanup: leave the testbed clean and live for the next user.
    try:
        for idx in emu.indices():
            emu.plug(idx)
        if not xcvrd.is_running():
            xcvrd.start()
    except Exception:  # noqa: BLE001
        pass


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


@pytest.fixture
def inject(injector, statedb, emu, test_index):
    """Error-injection handle; clears all injections on teardown so a leftover
    error can't leak into later tests."""
    injector.clear_all()
    yield injector
    injector.clear_all()


@pytest.fixture
def sfp_control(emu, statedb, test_index):
    """Handle for lpmode/reset control tests; restores lpmode off + a healthy
    module on teardown so control-plane side effects don't leak."""
    from lib import sfputil
    port = index_to_port(test_index)
    yield sfputil
    try:
        sfputil.lpmode(port, on=False)
        emu.plug(test_index)
    except Exception:  # noqa: BLE001
        pass


@pytest.fixture(autouse=True)
def _pretest(monitor, xcvrd):
    """Before each test: fail fast if xcvrd died mid-suite (so later read-only
    tests can't false-pass on stale STATE_DB), and scope the interaction trace."""
    if not xcvrd.is_running():
        pytest.fail(f"xcvrd is not running (status={xcvrd.status()!r}); "
                    "refusing to assert against stale STATE_DB")
    monitor.clear()
    yield
