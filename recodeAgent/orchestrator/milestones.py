"""The prioritized functionality milestones (README §5).

Each milestone is a slice of the xcvrd daemon. Its gate is CUMULATIVE: a milestone
must pass its OWN new tests **and every earlier milestone's tests** (regression
safety). The matrix here is the single source of truth; `cumulative_args(mid)`
builds the pytest args, and the CLI (`python -m orchestrator.milestones --args M3`)
exposes them to the shell harness.

Selection uses pytest **`-k` module selectors**, NOT file paths: the black-box
`xcvrd-tests/run.sh` always runs `pytest <tests-dir> <extra args>`, so passing file
paths would either collect the whole dir or error on a relative path. A `-k
"test_presence or test_info_content"` expression narrows the already-collected dir
to exactly the intended modules (matched by the test module stem in each node id).

Fast-subset-first: M1..M5 run with `-m "not slow"`; M6 drops the filter (full suite).
"""
from __future__ import annotations

from dataclasses import dataclass, field

DEFAULT_MARKER = "not slow"


@dataclass(frozen=True)
class Milestone:
    id: str
    title: str
    goal: str                                    # what the Rust daemon must do
    test_modules: list[str] = field(default_factory=list)  # pytest module stems this milestone ADDS
    marker: str = DEFAULT_MARKER                 # pytest -m marker for the (cumulative) run


MILESTONES: list[Milestone] = [
    Milestone(
        "M0", "Skeleton (deploy smoke)",
        "Rust crate compiles; the binary is injected into pmon and stays RUNNING "
        "under supervisor. Deploy-smoke gate only: the suite's clean-baseline "
        "requires TRANSCEIVER_INFO repopulation, so no pytest passes on a bare "
        "skeleton -- the harness special-cases M0 to check supervisor RUNNING.",
        test_modules=[], marker="",
    ),
    Milestone(
        "M1", "Presence + identity",
        "On insertion, read identity via the platform bridge and publish "
        "TRANSCEIVER_INFO; on removal, clear it; restore on re-plug.",
        test_modules=["test_presence", "test_info_content"],
    ),
    Milestone(
        "M2", "DOM",
        "Periodically poll module monitors via the platform and publish "
        "TRANSCEIVER_DOM_SENSOR; the emulator Monitor trace shows real reads.",
        test_modules=["test_dom", "test_interaction_trace"],
    ),
    Milestone(
        "M3", "Status / CMIS state / errors",
        "Publish TRANSCEIVER_STATUS_SW (plug status, cmis_state=READY) and decode "
        "injected error events (blocking removes DOM, non-blocking keeps it).",
        test_modules=["test_status_error"],
    ),
    Milestone(
        "M4", "lpmode / reset",
        "Handle sfputil lpmode/reset: drive the CMIS ModuleGlobalControls writes "
        "and reflect lpmode state.",
        test_modules=["test_lpmode_reset"],
    ),
    Milestone(
        "M5", "Multiport concurrency",
        "Handle concurrent presence/DOM across many ports with per-module "
        "isolation (no cross-talk).",
        test_modules=["test_multiport"],
    ),
    Milestone(
        "M6", "Golden conformance (full suite)",
        "Reproduce the reference STATE_DB projection and pass the ENTIRE suite, "
        "including slow tests (no marker filter).",
        test_modules=["test_golden"], marker="",   # full suite incl. slow
    ),
]


def index_of(mid: str) -> int:
    for i, m in enumerate(MILESTONES):
        if m.id == mid:
            return i
    raise KeyError(mid)


def by_id(mid: str) -> Milestone:
    return MILESTONES[index_of(mid)]


def cumulative_modules(mid: str) -> list[str]:
    """The test module stems for a milestone's cumulative gate (M1..mid), in order."""
    idx = index_of(mid)
    mods: list[str] = []
    for m in MILESTONES[1:idx + 1]:              # M1..current
        for mod in m.test_modules:
            if mod not in mods:
                mods.append(mod)
    return mods


def cumulative_args(mid: str) -> list[str]:
    """pytest args for a milestone's CUMULATIVE gate: a `-k` expression selecting
    this milestone's modules plus every earlier milestone's, plus the current
    milestone's `-m` marker. M0 returns [] (deploy-smoke; no pytest)."""
    mods = cumulative_modules(mid)
    if not mods:
        return []
    args = ["-k", " or ".join(mods)]
    marker = MILESTONES[index_of(mid)].marker
    if marker:
        args += ["-m", marker]
    return args


def _cli() -> int:
    """`python -m orchestrator.milestones --args M3` -> one pytest arg per line
    (so a shell can read them into an array, preserving multi-word args)."""
    import sys
    argv = sys.argv[1:]
    if len(argv) >= 2 and argv[0] == "--args":
        for a in cumulative_args(argv[1]):
            print(a)
        return 0
    for m in MILESTONES:                         # default: print the matrix
        print(f"{m.id}  {m.title}")
        print(f"     gate: {' '.join(cumulative_args(m.id)) or '(deploy-smoke)'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
