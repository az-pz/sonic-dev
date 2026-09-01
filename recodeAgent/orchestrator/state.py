"""Typed-ish state helpers for the Burr application.

Burr's State is a dict-like, immutable object (state.update(...), state.append(...)).
We keep the schema in one place so actions and transitions agree on key names.

State keys
----------
  milestone_idx    : int   index into the CURRENT milestone set of the milestone in progress
  num_milestones   : int   size of the current milestone set (0 until `scope` runs)
  last_idx         : int   num_milestones - 1 (-1 until `scope` runs)
  iter_count       : int   translate->validate repair attempts spent on the CURRENT milestone
  max_iter         : int   repair budget per milestone (default 10)
  milestone_passed : bool  did the last validate for this milestone pass?
  milestone_concluded : bool  did the current milestone conclude (passed OR gave up = budget spent)?
                            select_milestone advances to the next milestone when this is set.
  report           : dict  last validation report (parsed from pipeline/report.json)
  analysis_done    : bool  analyzer produced pipeline/analysis.md
  scope_done       : bool  scoper produced pipeline/milestones.json
  plan_done        : bool  planner produced pipeline/plan.json
  history          : list  append-only log of (milestone_id, iter, passed, gave_up) tuples
  skipped          : list  milestone ids skipped after exhausting the repair budget
  parity_round     : int   completed parity passes (outer-loop counter)
  max_parity_rounds: int   outer-loop budget: max parity re-scope rounds before failing
  parity_complete  : bool  did the last parity pass find full source coverage?
  gaps             : list  untranslated-source gaps from the last parity report
  retry_pending    : bool  parity appended a retry milestone for deferred skips -> re-run the loop
  done             : bool  whole pipeline finished SUCCESSFULLY (parity complete)

Optimize phase (runs AFTER parity completes; see app.py's graph)
  opt_round        : int   1-based benchmark->optimize round in progress
  max_opt_rounds   : int   how many optimisation rounds to attempt (0 disables the phase)
  bench            : dict  last bench.json the Benchmarker produced
  bench_history    : list  per-round benchmark summary, so the trend is visible in the UI
  optimize         : dict  last optimize.json the Optimizer produced
  opt_repairing    : bool  the appended optimize-repair milestone is the one in flight
  opt_done         : bool  the optimize phase has run; prevents re-entering it from parity
"""
from __future__ import annotations

from . import milestones


def initial_state(max_iter: int = 10, max_parity_rounds: int = 3,
                  max_opt_rounds: int = 0) -> dict:
    # Milestone count is unknown until `scope` runs (it generates the set), so it
    # starts at 0 / last_idx=-1; the scope action fills these in.
    return {
        "milestone_idx": 0,
        "num_milestones": 0,
        "last_idx": -1,
        "iter_count": 0,
        "max_iter": max_iter,
        "milestone_passed": False,
        "milestone_concluded": False,
        "report": {},
        "analysis_done": False,
        "scope_done": False,
        "plan_done": False,
        "history": [],
        "skipped": [],
        "parity_round": 0,
        "max_parity_rounds": max_parity_rounds,
        "parity_complete": False,
        "gaps": [],
        "retry_pending": False,
        "done": False,
        "last_agent": "",
        # --- optimize phase (skipped entirely when max_opt_rounds is 0) ---
        "opt_round": 1,
        "max_opt_rounds": max_opt_rounds,
        "bench": {},
        "bench_history": [],
        "optimize": {},
        "opt_repairing": False,
        "opt_done": False,
    }


def current_milestone(state) -> milestones.Milestone:
    return milestones.load()[state["milestone_idx"]]


def is_last_milestone(state) -> bool:
    return state["milestone_idx"] >= state["last_idx"]


def more_milestones(state) -> bool:
    return state["milestone_idx"] < state["last_idx"]
