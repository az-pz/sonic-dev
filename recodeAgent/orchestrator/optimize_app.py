"""Burr application for the OPTIMIZE stage: benchmark -> optimize -> validate.

Run SEPARATELY from the translation pipeline (orchestrator.app). That pipeline
answers "is it correct?"; this one answers "is it fast?", and only makes sense once
the first has finished -- there is nothing to optimise about a crate that does not
yet pass its oracle.

    benchmark ──> optimize ──> validate ──┐
        ^                                 │  rounds remain
        └─────────────────────────────────┘
                                          │  budget spent
                                          └──> terminal

Every round is measured before it is changed, so each optimisation is justified by
the state of the crate it is actually editing rather than by a stale reading. A
round that fails validation is reverted by the validate action itself; the loop
continues to the next round rather than repairing, because the change set is small
enough that repairing it is worth less than trying a different idea.

    python -m orchestrator.optimize_app --app-id opt1 --rounds 5
    python -m orchestrator.optimize_app --app-id demo --mock      # offline wiring check
"""
from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

import burr.core
from burr.core import ApplicationBuilder, default, expr
from burr.core.persistence import SQLLitePersister

from . import actions
from . import optimize as O

PROJECT = "recodeagent-xcvrd-optimize"


def initial_state(max_rounds: int = 5) -> dict:
    return {
        "round": 1,
        "max_rounds": max_rounds,
        "bench": {},            # last bench.json
        "bench_history": [],    # per-round summary, so the trend is visible in the UI
        "optimize": {},         # last optimize.json
        "report": {},           # last validation report
        "validated": False,
        "reverted": False,
        "history": [],          # append-only round log (mirrors optimize_history.json)
        "last_agent": "",
    }


def _tracker_enabled() -> bool:
    if os.environ.get("RECODE_NO_TRACKER") == "1":
        return False
    try:
        import burr.tracking  # noqa: F401
        return True
    except ImportError:
        return False


def build_application(app_id: str, max_rounds: int = 5, db_path: str | None = None,
                      bootstrap_state: dict | None = None):
    # Default the state next to the artifacts it describes: two runs against
    # different pipeline directories are different runs and must not share a store.
    db_path = db_path or str(actions._pipeline() / "optimize_state.db")
    Path(db_path).parent.mkdir(parents=True, exist_ok=True)
    # Its own table: the optimize stage has a different state shape from the
    # translation stage, and sharing one would make a resume ambiguous.
    persister = SQLLitePersister.from_values(db_path=db_path, table_name="optimize_state")
    persister.initialize()

    builder = (
        ApplicationBuilder()
        .with_actions(
            benchmark=O.benchmark,
            optimize=O.optimize,
            validate=O.validate_optimization,
            terminal=burr.core.Result("round", "bench_history", "history",
                                      "validated", "report"),
        )
        .with_transitions(
            ("benchmark", "optimize"),
            ("optimize", "validate"),
            # Re-benchmark every round, pass or fail. After a PASS the numbers have
            # moved and the next round must be guided by the new ones; after a REVERT
            # the crate is back to its previous state and re-measuring confirms the
            # rollback actually restored it rather than assuming so.
            ("validate", "benchmark", expr("round <= max_rounds")),
            ("validate", "terminal", default),
        )
        .initialize_from(
            persister,
            resume_at_next_action=True,
            default_state=bootstrap_state or initial_state(max_rounds=max_rounds),
            default_entrypoint="benchmark",
        )
        .with_state_persister(persister)
        .with_identifiers(app_id=app_id)
    )
    if _tracker_enabled():
        builder = builder.with_tracker("local", project=PROJECT)
    return builder.build()


def main() -> int:
    ap = argparse.ArgumentParser(
        description="ReCodeAgent OPTIMIZE stage: benchmark -> optimize -> validate.")
    ap.add_argument("--app-id", default=None,
                    help="run id; reuse the same id to resume a crashed run.")
    ap.add_argument("--rounds", type=int, default=5,
                    help="optimisation rounds to attempt (default 5).")
    ap.add_argument("--pipeline-dir", default=None,
                    help="pipeline artifact directory (default: RECODE_PIPELINE_DIR or ./pipeline).")
    ap.add_argument("--db", default=None,
                    help="state file (default: <pipeline-dir>/optimize_state.db).")
    ap.add_argument("--mock", action="store_true",
                    help="offline: fake agents, no Copilot or DUT. Proves the graph wiring.")
    ap.add_argument("--scenarios", default=None,
                    help="limit benchmarking to one scenario id (e.g. B9) for a faster loop.")
    ap.add_argument("--reps", default=None, help="benchmark reps per scenario (default 1).")
    args = ap.parse_args()

    if args.pipeline_dir:
        # Must rebind the constants, not just the env: actions.py froze them at import.
        actions.set_pipeline_dir(args.pipeline_dir)
    if args.mock:
        os.environ["RECODE_MOCK"] = "1"
    if args.scenarios:
        os.environ["RECODE_BENCH_SCENARIOS"] = args.scenarios
    if args.reps:
        os.environ["RECODE_BENCH_REPS"] = args.reps

    app_id = args.app_id or f"opt-{os.getpid()}"
    pipeline = actions._pipeline()
    db_path = args.db or str(pipeline / "optimize_state.db")
    crate = actions.PIPELINE_CRATE
    if not args.mock and not crate.exists():
        # This stage optimises an ALREADY-TRANSLATED crate. Failing here with the reason
        # beats an agent being asked to optimise a directory that does not exist.
        print(f"[optimize] no working copy at {crate}.\n"
              "  The optimize stage runs AFTER the translation pipeline has produced a\n"
              "  validated crate. Run orchestrator.app first, or point --pipeline-dir at\n"
              "  an existing pipeline folder.", file=sys.stderr)
        return 2

    app = build_application(app_id, max_rounds=args.rounds, db_path=db_path)
    print(f"[optimize] app-id={app_id} rounds={args.rounds} pipeline={pipeline}"
          f"{' (MOCK)' if args.mock else ''}")

    action, result, state = app.run(halt_after=["terminal"])

    print("\n[optimize] === trend ===")
    for row in state["bench_history"]:
        print("  " + json.dumps(row, sort_keys=True))
    print("\n[optimize] === rounds ===")
    for row in state["history"]:
        flag = "ok " if row.get("passed") else ("REVERTED" if row.get("reverted") else "FAILED")
        print(f"  {row.get('round')}: [{flag}] {row.get('title', '?')}")
    kept = sum(1 for r in state["history"] if r.get("passed"))
    print(f"\n[optimize] {kept}/{len(state['history'])} round(s) kept")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
