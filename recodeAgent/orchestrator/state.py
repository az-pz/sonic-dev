"""Typed-ish state helpers for the Burr application.

Burr's State is a dict-like, immutable object (state.update(...), state.append(...)).
We keep the schema in one place so actions and transitions agree on key names.

State keys
----------
  milestone_idx  : int   index into milestones.MILESTONES of the milestone in progress
  iter_count     : int   translate->validate repair attempts spent on the CURRENT milestone
  max_iter       : int   repair budget per milestone (paper: 5)
  milestone_passed : bool  did the last validate for this milestone pass?
  report         : dict  last validation report (parsed from pipeline/report.json)
  analysis_done  : bool  analyzer produced pipeline/analysis.md
  plan_done      : bool  planner produced pipeline/plan.json
  history        : list  append-only log of (milestone_id, iter, passed) tuples
  done           : bool  whole pipeline finished (all milestones green or gave up)
"""
from __future__ import annotations

from . import milestones


def initial_state(max_iter: int = 5) -> dict:
    return {
        "milestone_idx": 0,
        "num_milestones": len(milestones.MILESTONES),
        "last_idx": len(milestones.MILESTONES) - 1,
        "iter_count": 0,
        "max_iter": max_iter,
        "milestone_passed": False,
        "report": {},
        "analysis_done": False,
        "plan_done": False,
        "history": [],
        "done": False,
    }


def current_milestone(state) -> milestones.Milestone:
    return milestones.MILESTONES[state["milestone_idx"]]


def is_last_milestone(state) -> bool:
    return state["milestone_idx"] >= len(milestones.MILESTONES) - 1


def more_milestones(state) -> bool:
    return state["milestone_idx"] < len(milestones.MILESTONES) - 1
