"""XcvrdControl: drive the xcvrd daemon lifecycle on the DUT.

xcvrd runs supervised inside the pmon container. We control it exactly the way
an operator would -- via ``docker exec pmon supervisorctl`` -- and provide the
STATE_DB flush that the black-box tests rely on (a plain stop leaves stale
TRANSCEIVER_* rows behind, so "keys present" never proves the daemon ran).
"""
import subprocess

from .statedb import StateDB

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
        """Delete every TRANSCEIVER_* row so a restart repopulates from scratch."""
        total = 0
        for tbl in TRANSCEIVER_TABLES:
            total += self.statedb.delete_pattern(f"{tbl}|*")
        return total
