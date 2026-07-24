"""Named e2e scenarios for differential golden testing.

A *scenario* pins the system in one reproducible state and declares which
STATE_DB tables make up its golden projection. The reference (upstream **Python**)
xcvrd is the oracle: we capture its projection per scenario into
``golden/<scenario>/<port>.json`` and then assert a candidate xcvrd (e.g. the Rust
reimplementation) reproduces exactly the same rows.

This is the T0 plumbing that lets the suite grow one behavior at a time: each new
feature adds a `Scenario` here (stimulus + the table set it should now cover),
recaptures the golden from Python, and locks it in. See the repo README / plan.

A scenario is:
  * ``name``     — subdir under ``golden/`` and the pytest param id.
  * ``tables``   — STATE_DB tables to snapshot (a subset of ALL_TRANSCEIVER_TABLES).
  * ``prepare``  — ``prepare(ctx)`` drives the module to the scenario state.
  * ``slow``     — mark the golden case ``slow`` (e.g. waits on the ~60s DOM poll).

``prepare`` receives a :class:`ScenarioCtx` (module + statedb + emu + configdb) so
future scenarios can drive CONFIG_DB (breakout, media settings), raw EEPROM
(flags, VDM), or presence (post-error / post-reset) without changing the runner.
"""
from dataclasses import dataclass
from typing import Callable, List, Optional
import os

from lib.waits import wait_until, T_FAST, T_DOM

# Every STATE_DB table xcvrd can populate for a port. A scenario goldens a subset;
# the union grows as features land. Kept here as the single source of truth so the
# flush (xcvrd_ctl.flush_transceiver_tables) and the goldens can't drift.
ALL_TRANSCEIVER_TABLES = [
    "TRANSCEIVER_INFO",
    "TRANSCEIVER_STATUS_SW",
    "TRANSCEIVER_STATUS",
    "TRANSCEIVER_DOM_SENSOR",
    "TRANSCEIVER_DOM_THRESHOLD",
    "TRANSCEIVER_DOM_FLAG",
    "TRANSCEIVER_STATUS_FLAG",
    "TRANSCEIVER_VDM_REAL_VALUE",
    "TRANSCEIVER_PM",
    "TRANSCEIVER_FIRMWARE_INFO",
]


@dataclass
class ScenarioCtx:
    """Everything a scenario's ``prepare`` needs to drive + observe one port."""
    port: str
    index: int
    statedb: object
    emu: object
    configdb: object = None


@dataclass
class Scenario:
    name: str
    tables: List[str]
    prepare: Callable[[ScenarioCtx], None]
    description: str = ""
    # None -> the harness's default TEST_PORT (Ethernet100). A scenario can pin a
    # different port (e.g. an admin-up one whose datapath xcvrd actually drives).
    port: Optional[str] = None


# Scenarios are plain module-level constants (below); each gets its OWN test
# function in test_golden.py so they are selectable by pytest function name.


# --- steady_state ------------------------------------------------------------
# The baseline: a present module at rest. Its golden is the stable identity + SW
# status + DOM thresholds projection (the original golden/<port>.json contract).
def _prepare_steady_state(ctx: ScenarioCtx) -> None:
    db, port = ctx.statedb, ctx.port
    ctx.emu.plug(ctx.index)
    wait_until(lambda: db.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} TRANSCEIVER_INFO populated before snapshot")
    wait_until(lambda: db.hgetall(f"TRANSCEIVER_STATUS_SW|{port}").get("cmis_state") == "READY",
               timeout=T_FAST, msg=f"{port} cmis_state READY before golden snapshot")
    # DOM_THRESHOLD lands on xcvrd's ~60s DOM poll, so wait for it explicitly
    # rather than relying on test order -- this is why the case is `slow`.
    wait_until(lambda: db.hgetall(f"TRANSCEIVER_DOM_THRESHOLD|{port}"),
               timeout=T_DOM,
               msg=f"{port} DOM_THRESHOLD populated before golden snapshot")


STEADY_STATE = Scenario(
    name="steady_state",
    description="present module at rest: identity + SW status + DOM thresholds",
    tables=["TRANSCEIVER_INFO", "TRANSCEIVER_STATUS_SW", "TRANSCEIVER_DOM_THRESHOLD"],
    prepare=_prepare_steady_state,
)


# --- activated_datapath (CMIS bring-up parity gate) --------------------------
# An ADMIN-UP CMIS port whose datapath xcvrd's CmisManagerTask has driven all the
# way up: module in high power (module_state=ModuleReady), every datapath
# DataPathActivated, per-host-lane ConfigSuccess, and a REAL active
# application-select in TRANSCEIVER_INFO (active_apsel_hostlaneN != 'N/A'). The
# admin-down baseline (steady_state) short-circuits to cmis_state=READY with 'N/A'
# apsel and DataPathDeactivated; a reduced daemon that only reproduces THAT will
# fail this scenario. The port must be admin-up in CONFIG_DB (default Ethernet4 on
# the KVM testbed); override with XCVRD_ACTIVATED_PORT.
_ACTIVATED_PORT = os.environ.get("XCVRD_ACTIVATED_PORT", "Ethernet4")


def _prepare_activated_datapath(ctx: ScenarioCtx) -> None:
    db, port = ctx.statedb, ctx.port
    ctx.emu.plug(ctx.index)
    wait_until(lambda: db.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} TRANSCEIVER_INFO populated before snapshot")

    def _activated():
        st = db.hgetall(f"TRANSCEIVER_STATUS|{port}")
        return (st.get("module_state") == "ModuleReady"
                and st.get("DP1State") == "DataPathActivated")
    wait_until(_activated, timeout=T_DOM,
               msg=f"{port} CMIS datapath activated (ModuleReady + DP1State) before snapshot")
    # real active application-select, not the reduced 'N/A'
    wait_until(lambda: db.hget(f"TRANSCEIVER_INFO|{port}", "active_apsel_hostlane1") not in (None, "N/A"),
               timeout=T_FAST, msg=f"{port} active_apsel populated before snapshot")


ACTIVATED_DATAPATH = Scenario(
    name="activated_datapath",
    description="admin-up port with CMIS datapath driven to activated (real active_apsel)",
    tables=["TRANSCEIVER_INFO", "TRANSCEIVER_STATUS", "TRANSCEIVER_STATUS_SW"],
    prepare=_prepare_activated_datapath,
    port=_ACTIVATED_PORT,
)


# Convenience list (e.g. for a capture-all helper). Each scenario still gets its
# own named test function in test_golden.py.
ALL = [STEADY_STATE, ACTIVATED_DATAPATH]
