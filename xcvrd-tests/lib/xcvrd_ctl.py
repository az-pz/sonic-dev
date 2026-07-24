"""XcvrdControl: drive the xcvrd daemon lifecycle on the DUT.

xcvrd runs supervised inside the pmon container. We control it exactly the way
an operator would -- via ``docker exec pmon supervisorctl`` -- and provide the
STATE_DB flush that the black-box tests rely on (a plain stop leaves stale
TRANSCEIVER_* rows behind, so "keys present" never proves the daemon ran).
"""
import subprocess

from .statedb import StateDB
from .waits import T_BASELINE

PMON = "pmon"
TRANSCEIVER_TABLES = [
    "TRANSCEIVER_INFO", "TRANSCEIVER_DOM_SENSOR", "TRANSCEIVER_DOM_THRESHOLD",
    "TRANSCEIVER_STATUS", "TRANSCEIVER_PM", "TRANSCEIVER_FIRMWARE_INFO",
]


class XcvrdControl:
    def __init__(self, container=PMON, statedb=None):
        self.container = container
        self.statedb = statedb or StateDB()

    def _sv(self, *args):
        out = subprocess.run(
            ["docker", "exec", self.container, "supervisorctl", *args],
            capture_output=True, text=True, timeout=60)
        return out.stdout.strip() + out.stderr.strip()

    def status(self):
        """Return the supervisor status word for xcvrd (RUNNING/STOPPED/...)."""
        line = self._sv("status", "xcvrd")
        parts = line.split()
        return parts[1] if len(parts) > 1 else line

    def is_running(self):
        return self.status() == "RUNNING"

    def start(self):
        return self._sv("start", "xcvrd")

    def stop(self):
        return self._sv("stop", "xcvrd")

    def restart(self):
        return self._sv("restart", "xcvrd")

    def flush_transceiver_tables(self):
        """Delete every TRANSCEIVER_* row so a restart repopulates from scratch.

        A single glob (rather than the per-table list) so it also clears
        STATUS_SW / *_FLAG / VDM / PM rows that scenarios now golden -- stale rows
        in ANY transceiver table could otherwise mask a dead daemon.
        """
        return self.statedb.delete_pattern("TRANSCEIVER_*")

    def is_reference_python(self):
        """True iff the deployed /usr/local/bin/xcvrd is the stock **Python**
        daemon and NOT a Rust-injected shim (which execs /usr/local/bin/xcvrd-rs).

        Guards golden capture: the golden is the oracle, so it must be baselined
        from the reference Python xcvrd, never from the Rust candidate under test.
        """
        out = subprocess.run(
            ["docker", "exec", self.container, "sh", "-c",
             "grep -q xcvrd-rs /usr/local/bin/xcvrd 2>/dev/null && echo rust || echo python"],
            capture_output=True, text=True, timeout=30)
        return out.stdout.strip() == "python"

    def wait_healthy(self, probe_port, timeout=T_BASELINE, poll=1.0):
        """Force a fresh, verified-live baseline and return True iff healthy.

        Flush the transceiver tables (remove any stale rows that could mask a
        dead daemon), restart xcvrd, then require it to repopulate the probe
        port's TRANSCEIVER_INFO. This proves the daemon is actually running and
        emulator-backed, rather than STATE_DB merely holding residue from a
        previous run.
        """
        import time
        self.flush_transceiver_tables()
        self.restart()
        if not self.is_running():
            return False
        deadline = time.time() + timeout
        key = f"TRANSCEIVER_INFO|{probe_port}"
        while time.time() < deadline:
            if self.statedb.hget(key, "manufacturer"):
                return True
            time.sleep(poll)
        return False
