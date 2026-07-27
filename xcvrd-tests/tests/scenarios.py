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

from lib import cmis, golden
from lib.waits import wait_until, wait_stable, POLL, T_FAST, T_DOM

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
    # Optional cleanup run after capture/compare (e.g. clear a raised flag) so a
    # scenario's stimulus doesn't leak into subsequent tests / the next user.
    teardown: Optional[Callable[[ScenarioCtx], None]] = None


# Scenarios are plain module-level constants (below); each gets its OWN test
# function in test_golden.py so they are selectable by pytest function name.


# --- steady_state ------------------------------------------------------------
# The baseline: a present module at rest. Its golden is the stable identity + SW
# status + DOM thresholds projection (the original golden/<port>.json contract).
def _prepare_steady_state(ctx: ScenarioCtx) -> None:
    db, port = ctx.statedb, ctx.port
    # Enrich the module's page-02h DOM thresholds so the DOM_THRESHOLD projection
    # is discriminating: the emulator serves 0 for unwritten threshold bytes, so
    # without this every threshold reads 0.0 / -inf and a daemon that publishes
    # zeros would pass. Thresholds are cached by xcvrd at insertion (not re-read on
    # the DOM poll), so re-insert the module after writing to force a fresh read.
    cmis.write_dom_thresholds(ctx.emu, ctx.index)
    ctx.emu.unplug(ctx.index)
    # Wait for xcvrd to actually SEE the removal (INFO cleared) before re-plugging:
    # a back-to-back unplug/plug is too fast for its presence poll, so the module
    # never re-inserts and the cached (stale) thresholds are never re-read.
    wait_until(lambda: not db.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} removal detected before re-insert")
    ctx.emu.plug(ctx.index)
    wait_until(lambda: db.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} TRANSCEIVER_INFO populated before snapshot")
    wait_until(lambda: db.hgetall(f"TRANSCEIVER_STATUS_SW|{port}").get("cmis_state") == "READY",
               timeout=T_FAST, msg=f"{port} cmis_state READY before golden snapshot")
    # Rich TRANSCEIVER_STATUS (module + per-lane datapath state) is published by the
    # DomInfoUpdateTask; for an admin-down port it reads the deactivated baseline
    # (ModuleLowPwr / DataPathDeactivated). Wait for it before snapshotting.
    wait_until(lambda: db.hgetall(f"TRANSCEIVER_STATUS|{port}").get("module_state"),
               timeout=T_DOM, msg=f"{port} TRANSCEIVER_STATUS populated before golden snapshot")
    # DOM_THRESHOLD lands on xcvrd's ~60s DOM poll after the re-insertion; wait
    # specifically for the ENRICHED value (the sentinel) so we snapshot the real
    # thresholds, not the transient all-zero read. Allow ~2 DOM cycles' headroom.
    _sentinel_field, _sentinel_value = cmis.DOM_THRESHOLD_SENTINEL
    wait_until(lambda: db.hget(f"TRANSCEIVER_DOM_THRESHOLD|{port}", _sentinel_field) == _sentinel_value,
               timeout=2 * T_DOM,
               msg=f"{port} enriched DOM_THRESHOLD ({_sentinel_field}={_sentinel_value}) before golden snapshot")


STEADY_STATE = Scenario(
    name="steady_state",
    description="admin-down port at rest: identity + SW status + rich STATUS + DOM thresholds",
    tables=["TRANSCEIVER_INFO", "TRANSCEIVER_STATUS_SW", "TRANSCEIVER_STATUS",
            "TRANSCEIVER_DOM_THRESHOLD"],
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
    # xcvrd TX-disables the module's UNUSED host lanes (those beyond
    # host_lane_count) a beat AFTER the datapath activates -- a 0 -> mask
    # transition on tx_disabled_channel. A snapshot taken between activation and
    # that write catches the transient 0 (this bistability made the golden flaky:
    # the settled value here is host_lane_count=4 -> lanes 5-8 disabled -> 0xF0 =
    # 240, but a premature capture recorded 0). Wait for the unused-lane disable to
    # actually apply (non-zero) before snapshotting; if the active app used all 8
    # host lanes there would be none to disable, so only require this when the
    # module has unused lanes.
    n = int(db.hget(f"TRANSCEIVER_INFO|{port}", "host_lane_count") or 8)
    if n < 8:
        wait_until(lambda: db.hget(f"TRANSCEIVER_STATUS|{port}", "tx_disabled_channel") not in (None, "", "0"),
                   timeout=T_DOM,
                   msg=f"{port} unused-lane TX-disable applied (tx_disabled_channel!=0) before snapshot")
    # Finally, require the whole STATUS projection to have stopped changing so the
    # capture is deterministic (belt-and-suspenders over the explicit wait above).
    wait_stable(lambda: golden.project(db, port, ["TRANSCEIVER_STATUS"]),
                stable_polls=6, interval=POLL, timeout=T_DOM,
                msg=f"{port} TRANSCEIVER_STATUS settled (tx-disable) before snapshot")


ACTIVATED_DATAPATH = Scenario(
    name="activated_datapath",
    description="admin-up port with CMIS datapath driven to activated (real active_apsel)",
    tables=["TRANSCEIVER_INFO", "TRANSCEIVER_STATUS", "TRANSCEIVER_STATUS_SW"],
    prepare=_prepare_activated_datapath,
    port=_ACTIVATED_PORT,
)


# --- dom_flag (T2: DOM flag reporting) ---------------------------------------
# xcvrd's DomInfoUpdateTask reads the module's latched monitor flags (CMIS v5.2
# 8.9, lower page byte 9) and publishes TRANSCEIVER_DOM_FLAG. The reduced Rust
# daemon publishes no flag tables at all, so any raised flag is a hard parity
# gate. We raise TempMonHighAlarm directly on the emulator register -- it holds
# the value with no clear-on-read, so the projection is stable across xcvrd's
# ~60s DOM poll and the golden is deterministic. The teardown clears the flag so
# the module returns to the unflagged baseline for later tests.
_FLAG_PORT = os.environ.get("XCVRD_FLAG_PORT", "Ethernet4")


def _prepare_dom_flag(ctx: ScenarioCtx) -> None:
    db, port = ctx.statedb, ctx.port
    ctx.emu.plug(ctx.index)
    wait_until(lambda: db.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST,
               msg=f"{port} TRANSCEIVER_INFO populated before flag stimulus")
    ctx.emu.write_field(ctx.index, cmis.MODULE_FLAGS_TEMP_VCC,
                        bytes([cmis.TEMP_HIGH_ALARM_FLAG]))
    # xcvrd latches the raised flag on its next DOM poll -> DOM_FLAG.tempHAlarm.
    wait_until(lambda: db.hgetall(f"TRANSCEIVER_DOM_FLAG|{port}").get("tempHAlarm") == "True",
               timeout=T_DOM,
               msg=f"{port} DOM_FLAG tempHAlarm raised before golden snapshot")


def _teardown_dom_flag(ctx: ScenarioCtx) -> None:
    # Clear the raised flag so the emulator register (and, on xcvrd's next poll,
    # STATE_DB) return to the unflagged baseline for subsequent tests.
    try:
        ctx.emu.write_field(ctx.index, cmis.MODULE_FLAGS_TEMP_VCC, bytes([0x00]))
    except Exception:  # noqa: BLE001
        pass


DOM_FLAG = Scenario(
    name="dom_flag",
    description="module with TempMonHighAlarm raised: TRANSCEIVER_DOM_FLAG projection",
    tables=["TRANSCEIVER_DOM_FLAG"],
    prepare=_prepare_dom_flag,
    teardown=_teardown_dom_flag,
    port=_FLAG_PORT,
)


# Convenience list (e.g. for a capture-all helper). Each scenario still gets its
# own named test function in test_golden.py.
ALL = [STEADY_STATE, ACTIVATED_DATAPATH, DOM_FLAG]
