"""The prioritized functionality milestones (README §5).

Each milestone is a slice of the xcvrd daemon. Its gate is CUMULATIVE: a milestone
must pass its OWN new tests **and every earlier milestone's tests** (regression
safety -- new work must not break earlier functionality). The matrix here is the
single source of truth; `cumulative_args(mid)` builds the pytest args, and the CLI
(`python -m orchestrator.milestones --args M3`) exposes them to the shell harness.

Fast-subset-first: M1..M5 run with `-m "not slow"` so the inner translate->validate
loop stays quick; M6 drops the filter to run the full suite (incl. slow tests).
"""
from __future__ import annotations

from dataclasses import dataclass, field

DEFAULT_MARKER = "not slow"


@dataclass(frozen=True)
class Milestone:
    id: str
    title: str
    goal: str                                   # what the Rust daemon must do
    test_paths: list[str] = field(default_factory=list)  # NEW test files this milestone adds
    marker: str = DEFAULT_MARKER                # pytest -m marker for the (cumulative) run


MILESTONES: list[Milestone] = [
    Milestone(
        "M0", "Skeleton (deploy smoke)",
        "Rust crate compiles; the binary is injected into pmon and stays RUNNING "
        "under supervisor. Deploy-smoke gate only: the suite's clean-baseline "
        "requires TRANSCEIVER_INFO repopulation, so no pytest passes on a bare "
        "skeleton -- the harness special-cases M0 to check supervisor RUNNING.",
        test_paths=[], marker="",
    ),
    Milestone(
        "M1", "Presence + identity",
        "On insertion, read identity via the platform bridge and publish "
        "TRANSCEIVER_INFO; on removal, clear it; restore on re-plug.",
        test_paths=["tests/test_presence.py", "tests/test_info_content.py"],
    ),
    Milestone(
        "M2", "DOM",
        "Periodically poll module monitors via the platform and publish "
        "TRANSCEIVER_DOM_SENSOR; the emulator Monitor trace shows real reads.",
        test_paths=["tests/test_dom.py", "tests/test_interaction_trace.py"],
    ),
    Milestone(
        "M3", "Status / CMIS state / errors",
        "Publish TRANSCEIVER_STATUS_SW (plug status, cmis_state=READY) and decode "
        "injected error events (blocking removes DOM, non-blocking keeps it).",
        test_paths=["tests/test_status_error.py"],
    ),
    Milestone(
        "M4", "lpmode / reset",
        "Handle sfputil lpmode/reset: drive the CMIS ModuleGlobalControls writes "
        "and reflect lpmode state.",
        test_paths=["tests/test_lpmode_reset.py"],
    ),
    Milestone(
        "M5", "Multiport concurrency",
        "Handle concurrent presence/DOM across many ports with per-module "
        "isolation (no cross-talk).",
        test_paths=["tests/test_multiport.py"],
    ),
    Milestone(
        "M6", "Golden conformance (full suite)",
        "Reproduce the reference STATE_DB projection and pass the ENTIRE suite, "
        "including slow tests (no marker filter).",
        test_paths=["tests/test_golden.py"], marker="",   # full suite incl. slow
    ),
]


def index_of(mid: str) -> int:
    for i, m in enumerate(MILESTONES):
        if m.id == mid:
            return i
    raise KeyError(mid)


def by_id(mid: str) -> Milestone:
    return MILESTONES[index_of(mid)]


def cumulative_args(mid: str) -> list[str]:
    """pytest args for a milestone's CUMULATIVE gate: the union of test_paths for
    M1..mid (in order, de-duplicated) plus the current milestone's `-m` marker.
    M0 returns [] (deploy-smoke; no pytest)."""
    idx = index_of(mid)
    if idx == 0:
        return []
    paths: list[str] = []
    for m in MILESTONES[1:idx + 1]:            # M1..current
        for p in m.test_paths:
            if p not in paths:
                paths.append(p)
    args = list(paths)
    marker = MILESTONES[idx].marker
    if marker:
        args += ["-m", marker]
    return args


def _cli() -> int:
    """`python -m orchestrator.milestones --args M3` -> one pytest arg per line
    (so a shell can read them into an array, preserving 'not slow')."""
    import sys
    argv = sys.argv[1:]
    if len(argv) >= 2 and argv[0] == "--args":
        for a in cumulative_args(argv[1]):
            print(a)
        return 0
    for m in MILESTONES:                        # default: print the matrix
        print(f"{m.id}  {m.title}")
        print(f"     gate: {' '.join(cumulative_args(m.id)) or '(deploy-smoke)'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
