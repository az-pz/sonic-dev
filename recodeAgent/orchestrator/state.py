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
  done             : bool  whole pipeline finished SUCCESSFULLY (parity complete)
"""
from __future__ import annotations

from . import milestones


def initial_state(max_iter: int = 10, max_parity_rounds: int = 3) -> dict:
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
        "done": False,
        "last_agent": "",
    }


def current_milestone(state) -> milestones.Milestone:
    return milestones.load()[state["milestone_idx"]]


def is_last_milestone(state) -> bool:
    return state["milestone_idx"] >= state["last_idx"]


def more_milestones(state) -> bool:
    return state["milestone_idx"] < state["last_idx"]
