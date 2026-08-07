r"""The Burr application: wire the ReCodeAgent stages into a persisted, resumable
state machine with two nested loops -- the per-milestone repair loop (inner,
correctness) and the parity coverage loop (outer, completeness).

    analyze -> scope -> plan -> select_milestone -> translate -> validate
                 ^                      ^                            |
                 |    (concluded: passed OR gave up) & more --------+
                 |                      |                           | repair (failed & budget left)
                 |                      +--------- validate <-------+
                 |                                    |
                 |      (last milestone concluded)    v
                 |                              parity_verify
                 |   (gaps & rounds<budget)  /   |    |    \  (complete & no retry)
                 +--------------------------+    |    |     +--> terminal (success)
                        (retry_pending) select_milestone   (gaps & budget spent) --> terminal (FAIL)

Scope owns the milestone set (pipeline/milestones.json); parity_verify owns "done"
(success only when source coverage is complete). Inner give-up (a milestone whose
repair budget is exhausted) does NOT fail the run -- the milestone is SKIPPED, its
still-failing e2e tests are recorded in pipeline/skips.json and deselected from later
milestones, and the loop advances. parity_verify then revisits skips.json: any test
not yet retried gets ONE dedicated retry milestone (re-enabled); if it still fails it
is skipped permanently. Untranslated behaviour surfaces as parity gaps -> re-scope.
No deferral at the OUTER level: parity-budget-exhausted-with-gaps is a hard failure.

Run:
    python -m orchestrator.app --app-id recode-001 --max-iter 10 --max-parity-rounds 3
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

from . import actions, milestones, state as S

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_DB = str(ROOT / "pipeline" / "burr.db")
PROJECT = "recodeagent-xcvrd"


def _configure_pipeline_dir(path: str | os.PathLike) -> Path:
    """Point every file-based stage at an existing pipeline directory.

    actions.py resolves its pipeline constants at import time, so a CLI-provided
    path must update both the environment (used dynamically by milestones/mock)
    and those two constants.
    """
    p = Path(path).expanduser().resolve()
    if not p.is_dir():
        raise ValueError(f"pipeline directory does not exist: {p}")
    os.environ["RECODE_PIPELINE_DIR"] = str(p)
    actions.PIPELINE = p
    actions.PIPELINE_CRATE = p / "crate"
    return p


def _state_from_existing_pipeline(
    pipeline_dir: Path,
    milestone_id: str | None,
    max_iter: int,
    max_parity_rounds: int,
) -> dict:
    """Bootstrap a NEW Burr run from existing pipeline artifacts.

    ``milestone_id`` selects the entry milestone; pass None to jump straight to the
    Parity Verifier (every milestone is treated as already concluded, so parity runs
    against the translation as it stands).
    """
    required = [
        pipeline_dir / "analysis.md",
        pipeline_dir / "milestones.json",
        pipeline_dir / "plan.json",
        pipeline_dir / "crate" / "xcvrd-rs",
    ]
    missing = [str(p) for p in required if not p.exists()]
    if missing:
        raise ValueError(
            "cannot start from an existing pipeline; missing required artifact(s): "
            + ", ".join(missing)
        )

    ms = milestones.load()
    if milestone_id is None:
        # Parity entry: sit on the LAST milestone so any loop-back (a parity retry
        # milestone, or a re-scope) appends and advances from a consistent position.
        idx = len(ms) - 1
    else:
        try:
            idx = milestones.index_of(milestone_id, ms)
        except KeyError as e:
            ids = ", ".join(m.id for m in ms)
            raise ValueError(
                f"milestone {milestone_id!r} is not in {pipeline_dir / 'milestones.json'} "
                f"(available: {ids})"
            ) from e

    state = S.initial_state(
        max_iter=max_iter, max_parity_rounds=max_parity_rounds)
    state.update({
        "milestone_idx": idx,
        "num_milestones": len(ms),
        "last_idx": len(ms) - 1,
        "analysis_done": True,
        "scope_done": True,
        "plan_done": True,
        "last_agent": "pipeline-bootstrap",
    })
    if milestone_id is None:
        # The milestone loop is behind us; mark the current one concluded/passed so
        # a retry milestone appended by parity is picked up cleanly by select_milestone.
        state.update({"milestone_passed": True, "milestone_concluded": True})
    return state


def _tracker_enabled() -> bool:
    """The Burr telemetry tracker needs the optional 'tracking' extra (pydantic).
    Enable it when available; otherwise run without telemetry rather than crash.
    Force off with RECODE_NO_TRACKER=1."""
    if os.environ.get("RECODE_NO_TRACKER") == "1":
        return False
    try:
        import burr.tracking.client  # noqa: F401  (triggers the extra's import chain)
        return True
    except Exception as e:  # ImportError from require_plugin, or anything else
        print("[recode] telemetry tracker disabled (install 'apache-burr[tracking]' "
              f"to enable the Burr UI): {type(e).__name__}: {e}")
        return False


def build_application(app_id: str, max_iter: int = 10, max_parity_rounds: int = 3,
                     db_path: str = DEFAULT_DB, bootstrap_state: dict | None = None,
                     default_entrypoint: str = "analyze"):
    Path(db_path).parent.mkdir(parents=True, exist_ok=True)
    persister = SQLLitePersister.from_values(db_path=db_path, table_name="recode_state")
    persister.initialize()

    builder = (
        ApplicationBuilder()
        .with_actions(
            analyze=actions.analyze,
            scope=actions.scope,
            plan=actions.plan,
            select_milestone=actions.select_milestone,
            translate=actions.translate,
            validate=actions.validate,
            parity_verify=actions.parity_verify,
            terminal=burr.core.Result("done", "history", "milestone_idx", "skipped",
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
            # milestone concluded (passed OR budget exhausted) and more remain -> next milestone.
            # A give-up here SKIPS the stuck milestone instead of failing the run; the
            # untranslated behaviour is caught later by the Parity Verifier.
            ("validate", "select_milestone", expr("milestone_idx < last_idx")),
            # concluded on the LAST milestone -> run the parity (source-coverage) gate
            ("validate", "parity_verify", default),
            # parity revisited skips.json and appended a retry milestone -> run it
            ("parity_verify", "select_milestone", expr("retry_pending")),
            # outer loop: gaps found + budget left -> re-scope new milestones
            ("parity_verify", "scope", expr("not parity_complete and parity_round < max_parity_rounds")),
            # complete (success) OR gaps + budget exhausted (fail) -> terminal
            ("parity_verify", "terminal", default),
        )
        .initialize_from(
            persister,
            resume_at_next_action=True,      # crash-resume: pick up where we left off
            default_state=bootstrap_state or S.initial_state(
                max_iter=max_iter, max_parity_rounds=max_parity_rounds),
            default_entrypoint=default_entrypoint,
        )
        .with_state_persister(persister)
        .with_identifiers(app_id=app_id)
    )
    if _tracker_enabled():
        builder = builder.with_tracker("local", project=PROJECT)   # Burr telemetry UI
    return builder.build()


def main() -> int:
    ap = argparse.ArgumentParser(description="ReCodeAgent xcvrd Python->Rust pipeline (Burr).")
    ap.add_argument("--app-id", default=None,
                    help="run id; reuse the same id to resume a crashed run.")
    ap.add_argument("--max-iter", type=int, default=10, help="repair budget per milestone (default 10).")
    ap.add_argument("--max-parity-rounds", type=int, default=3,
                    help="outer-loop budget: max parity re-scope rounds before failing.")
    ap.add_argument(
        "--pipeline-dir", default=None,
        help="pipeline artifact directory (default: RECODE_PIPELINE_DIR or ./pipeline).")
    ap.add_argument(
        "--start-milestone", default=None, metavar="Mx",
        help="start a NEW app-id from an existing pipeline folder at this milestone; "
             "requires analysis.md, milestones.json, plan.json, and crate/xcvrd-rs.")
    ap.add_argument(
        "--start-parity", action="store_true",
        help="start a NEW app-id from an existing pipeline folder directly at the "
             "Parity Verifier, skipping analyze/scope/plan and the whole milestone "
             "loop. Same artifact requirements as --start-milestone.")
    ap.add_argument(
        "--db", default=None,
        help="SQLite persistence path (default: <pipeline-dir>/burr.db).")
    ap.add_argument("--mock", action="store_true", help="offline: mock agents (no Copilot).")
    args = ap.parse_args()

    if args.mock:
        os.environ["RECODE_MOCK"] = "1"
    app_id = args.app_id or f"recode-{uuid.uuid4().hex[:8]}"

    try:
        pipeline_dir = _configure_pipeline_dir(
            args.pipeline_dir
            or os.environ.get("RECODE_PIPELINE_DIR")
            or (ROOT / "pipeline"))
    except ValueError as e:
        ap.error(str(e))
    db_path = args.db or str(pipeline_dir / "burr.db")
    bootstrap_state = None
    entrypoint = "analyze"
    if args.start_milestone and args.start_parity:
        ap.error("--start-milestone and --start-parity are mutually exclusive")
    if args.start_milestone or args.start_parity:
        try:
            bootstrap_state = _state_from_existing_pipeline(
                pipeline_dir,
                None if args.start_parity else args.start_milestone,
                max_iter=args.max_iter,
                max_parity_rounds=args.max_parity_rounds)
        except ValueError as e:
            ap.error(str(e))
        entrypoint = "parity_verify" if args.start_parity else "select_milestone"

    app = build_application(app_id, max_iter=args.max_iter,
                            max_parity_rounds=args.max_parity_rounds,
                            db_path=db_path,
                            bootstrap_state=bootstrap_state,
                            default_entrypoint=entrypoint)

    print(f"[recode] app_id={app_id}  mock={os.environ.get('RECODE_MOCK')=='1'}  "
          f"pipeline={pipeline_dir}  db={db_path}")
    if args.start_milestone or args.start_parity:
        where = "the Parity Verifier" if args.start_parity else args.start_milestone
        flag = "--start-parity" if args.start_parity else f"--start-milestone {args.start_milestone}"
        if app.state["last_agent"] == "pipeline-bootstrap":
            skipped = ("analyze/scope/plan and the milestone loop are skipped"
                       if args.start_parity else "analyze/scope/plan are skipped")
            print(f"[recode] starting from existing artifacts at {where}; {skipped}")
        else:
            print(f"[recode] persisted state exists for app_id={app_id}; resuming it "
                  f"({flag} only initializes a NEW app-id)")
    print(f"[recode] loaded state at startup: milestone_idx={app.state['milestone_idx']} "
          f"history_len={len(app.state['history'])}  (idx>0 or history => resumed, not restarted)")
    last_action, result, final_state = app.run(halt_after=["terminal"])
    skipped = final_state["skipped"]
    print(f"[recode] finished at {last_action}: done={final_state['done']} "
          f"milestone_idx={final_state['milestone_idx']} "
          f"parity_round={final_state['parity_round']} parity_complete={final_state['parity_complete']} "
          f"skipped={skipped or '[]'}")
    for h in final_state["history"]:
        flag = "  GAVE-UP/SKIPPED" if h.get("gave_up") else ""
        if h.get("retry_for"):
            flag += f"  [retry for {h['retry_for']}]"
        print(f"    {h['milestone']}  iter={h['iter']}  passed={h['passed']}{flag}")
    if skipped:
        print(f"[recode] WARNING: {len(skipped)} milestone(s) skipped after exhausting the repair "
              f"budget: {skipped}. The Parity Verifier gave deferred tests one retry milestone.")
    perm = actions._permanent_skips()
    if perm:
        print(f"[recode] PERMANENTLY SKIPPED (failed even after a retry milestone): {perm}")
    return 0 if final_state["done"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
