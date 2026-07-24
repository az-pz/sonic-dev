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
from dataclasses import dataclass, field
from typing import Callable, List, Optional

from .waits import wait_until, T_FAST, T_DOM

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
    """Everything a scenario's ``prepare`` may need to drive the system."""
    module: object
    statedb: object
    emu: object
    configdb: object = None


@dataclass
class Scenario:
    name: str
    tables: List[str]
    prepare: Callable[[ScenarioCtx], None]
    description: str = ""
    slow: bool = False


# --- registry ----------------------------------------------------------------
_REGISTRY: "dict[str, Scenario]" = {}


def register(scenario: Scenario) -> Scenario:
    if scenario.name in _REGISTRY:
        raise ValueError(f"duplicate scenario name: {scenario.name}")
    _REGISTRY[scenario.name] = scenario
    return scenario


def all_scenarios() -> List[Scenario]:
    return list(_REGISTRY.values())


def get(name: str) -> Scenario:
    return _REGISTRY[name]


# --- steady_state ------------------------------------------------------------
# The baseline: a present module at rest. Its golden is the stable identity + SW
# status + DOM thresholds projection (the original golden/<port>.json contract).
def _prepare_steady_state(ctx: ScenarioCtx) -> None:
    m, statedb = ctx.module, ctx.statedb
    m.plug()
    m.wait_info_populated(timeout=T_FAST)
    wait_until(lambda: m.status_sw().get("cmis_state") == "READY", timeout=T_FAST,
               msg=f"{m.port} cmis_state READY before golden snapshot")
    # DOM_THRESHOLD lands on xcvrd's ~60s DOM poll, so wait for it explicitly
    # rather than relying on test order -- this is why the case is `slow`.
    wait_until(lambda: statedb.hgetall(f"TRANSCEIVER_DOM_THRESHOLD|{m.port}"),
               timeout=T_DOM,
               msg=f"{m.port} DOM_THRESHOLD populated before golden snapshot")


register(Scenario(
    name="steady_state",
    description="present module at rest: identity + SW status + DOM thresholds",
    tables=["TRANSCEIVER_INFO", "TRANSCEIVER_STATUS_SW", "TRANSCEIVER_DOM_THRESHOLD"],
    prepare=_prepare_steady_state,
    slow=True,
))


# --- adding a scenario (template for feature work) ---------------------------
# Each e2e feature registers a scenario here, extends the emulator/bridge only if
# the data isn't already observable, recaptures from Python (--capture-golden),
# and commits the new golden. Sketch:
#
#   def _prepare_activated_datapath(ctx):
#       # configure the port speed/app so the CMIS manager drives bring-up, then
#       # wait for the emulator's datapath to reach DPACTIVATED (emu.get_info().dpsms).
#       ...
#   register(Scenario(
#       name="activated_datapath",
#       tables=["TRANSCEIVER_INFO", "TRANSCEIVER_STATUS", "TRANSCEIVER_STATUS_SW"],
#       prepare=_prepare_activated_datapath, slow=True))
