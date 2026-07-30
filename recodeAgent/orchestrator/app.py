r"""The Burr application: wire the ReCodeAgent stages into a persisted, resumable
state machine with two nested loops -- the per-milestone repair loop (inner,
correctness) and the parity coverage loop (outer, completeness).

    analyze -> scope -> plan -> select_milestone -> translate -> validate
                 ^                      ^                            |
                 |        (passed & more milestones) ---------------+
                 |                      |                           | repair (failed & budget)
                 |                      +--------- validate <-------+
                 |                                    |
                 |            (all milestones passed) v
                 |                              parity_verify
                 |   (gaps & rounds<budget)  /        |        \  (complete)
                 +--------------------------+         |         +--> terminal (success)
                                            (gaps & budget exhausted) --> terminal (FAIL)

Scope owns the milestone set (pipeline/milestones.json); parity_verify owns "done"
(success only when source coverage is complete). No deferral: budget-exhausted-with-gaps
is a hard failure.

Run:
    python -m orchestrator.app --app-id recode-001 --max-iter 5 --max-parity-rounds 3
    RECODE_MOCK=1 python -m orchestrator.app --app-id smoke   # offline graph/resume test
Resume (same app-id): re-run the same command; SQLite persister continues.
UI:  burr   (then open the printed URL; project "recodeagent-xcvrd")
"""
from __future__ import annotations

import argparse
import os
import uuid
from pathlib import Path

import burr.core
from burr.core import ApplicationBuilder, default, expr
from burr.core.persistence import SQLLitePersister

from . import actions, state as S

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_DB = str(ROOT / "pipeline" / "burr.db")
PROJECT = "recodeagent-xcvrd"


def build_application(app_id: str, max_iter: int = 5, max_parity_rounds: int = 3,
                     db_path: str = DEFAULT_DB):
    Path(db_path).parent.mkdir(parents=True, exist_ok=True)
    persister = SQLLitePersister.from_values(db_path=db_path, table_name="recode_state")
    persister.initialize()

    return (
        ApplicationBuilder()
        .with_actions(
            analyze=actions.analyze,
            scope=actions.scope,
            plan=actions.plan,
            select_milestone=actions.select_milestone,
            translate=actions.translate,
            validate=actions.validate,
            parity_verify=actions.parity_verify,
            terminal=burr.core.Result("done", "history", "milestone_idx",
                                      "parity_round", "parity_complete", "gaps", "report"),
        )
        .with_transitions(
            ("analyze", "scope"),
            ("scope", "plan"),
            ("plan", "select_milestone"),
            ("select_milestone", "translate"),
            ("translate", "validate"),
            # inner loop: repair the current milestone while it fails and there's budget
            ("validate", "translate", expr("not milestone_passed and iter_count < max_iter")),
            # advance to the next milestone once this one passes
            ("validate", "select_milestone", expr("milestone_passed and milestone_idx < last_idx")),
            # all milestones passed -> run the parity (source-coverage) gate
            ("validate", "parity_verify", expr("milestone_passed and milestone_idx >= last_idx")),
            # otherwise: inner give-up (budget exhausted) -> terminal (fail)
            ("validate", "terminal", default),
            # outer loop: gaps found + budget left -> re-scope new milestones
            ("parity_verify", "scope", expr("not parity_complete and parity_round < max_parity_rounds")),
            # complete (success) OR gaps + budget exhausted (fail) -> terminal
            ("parity_verify", "terminal", default),
        )
        .initialize_from(
            persister,
            resume_at_next_action=True,      # crash-resume: pick up where we left off
            default_state=S.initial_state(max_iter=max_iter, max_parity_rounds=max_parity_rounds),
            default_entrypoint="analyze",
        )
        .with_state_persister(persister)
        .with_identifiers(app_id=app_id)
        .with_tracker("local", project=PROJECT)   # Burr telemetry UI
        .build()
    )


def main() -> int:
    ap = argparse.ArgumentParser(description="ReCodeAgent xcvrd Python->Rust pipeline (Burr).")
    ap.add_argument("--app-id", default=None,
                    help="run id; reuse the same id to resume a crashed run.")
    ap.add_argument("--max-iter", type=int, default=5, help="repair budget per milestone.")
    ap.add_argument("--max-parity-rounds", type=int, default=3,
                    help="outer-loop budget: max parity re-scope rounds before failing.")
    ap.add_argument("--db", default=DEFAULT_DB, help="SQLite persistence path.")
    ap.add_argument("--mock", action="store_true", help="offline: mock agents (no Copilot).")
    args = ap.parse_args()

    if args.mock:
        os.environ["RECODE_MOCK"] = "1"
    app_id = args.app_id or f"recode-{uuid.uuid4().hex[:8]}"

    app = build_application(app_id, max_iter=args.max_iter,
                            max_parity_rounds=args.max_parity_rounds, db_path=args.db)

    print(f"[recode] app_id={app_id}  mock={os.environ.get('RECODE_MOCK')=='1'}  db={args.db}")
    print(f"[recode] loaded state at startup: milestone_idx={app.state['milestone_idx']} "
          f"history_len={len(app.state['history'])}  (idx>0 or history => resumed, not restarted)")
    last_action, result, final_state = app.run(halt_after=["terminal"])
    print(f"[recode] finished at {last_action}: done={final_state['done']} "
          f"milestone_idx={final_state['milestone_idx']} "
          f"parity_round={final_state['parity_round']} parity_complete={final_state['parity_complete']}")
    for h in final_state["history"]:
        print(f"    {h['milestone']}  iter={h['iter']}  passed={h['passed']}")
    return 0 if final_state["done"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
