"""Prioritized functionality milestones — now a Scoper-owned, persisted ARTIFACT.

Milestones used to be a static Python list here. With the Scoper agent, the
milestone set is generated from the analysis + source and written to
``pipeline/milestones.json``; the Parity Verifier can append more (origin="parity")
when it finds untranslated source. This module is now a **loader + helpers** over
that artifact, with ``DEFAULT_MILESTONES`` kept only as (a) the bootstrap the
mock/real Scoper starts from and (b) a fallback when no artifact exists yet.

Contract (one object per milestone in ``pipeline/milestones.json``):
    id            "M0".. — stable; never renumbered. Parity appends new ids.
    title         short label
    goal          what the Rust daemon must do for this slice
    test_modules  xcvrd-tests module stems this milestone ADDS to the gate.
                  [] for M0 (deploy-smoke) and for unit-only parity milestones.
    marker        pytest -m marker for the (cumulative) run ("" = none)
    origin        "scoper" (first pass, e2e-mapped) | "parity" (feedback, unit-only)
    unit_only     True => no NEW e2e tests; gated by the cumulative e2e set + unit tests
    source_refs   source symbols/files this milestone covers (for traceability)
    deps          milestone ids that must precede it

Each milestone's gate is CUMULATIVE: it must pass its OWN new e2e tests AND every
earlier milestone's (regression safety), so a unit-only parity milestone still runs
the full prior e2e suite. Selection uses pytest ``-k`` module selectors, NOT file
paths (the black-box ``xcvrd-tests/run.sh`` always runs ``pytest <dir> <args>``).
``cumulative_args(mid)`` builds the pytest args; ``python -m orchestrator.milestones
--args M3`` exposes them to the shell harness.
"""
from __future__ import annotations

import json
import os
from dataclasses import dataclass, field, asdict
from pathlib import Path

DEFAULT_MARKER = ""   # no pytest -m filter: run slow tests at every milestone
MILESTONES_FILE = "milestones.json"

_ROOT = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class Milestone:
    id: str
    title: str
    goal: str                                               # what the Rust daemon must do
    test_modules: list[str] = field(default_factory=list)   # pytest module stems this milestone ADDS
    marker: str = DEFAULT_MARKER                            # pytest -m marker for the (cumulative) run
    origin: str = "scoper"                                  # "scoper" | "parity"
    unit_only: bool = False                                 # parity milestone with no new e2e tests
    source_refs: list[str] = field(default_factory=list)   # source symbols/files covered
    deps: list[str] = field(default_factory=list)          # milestone ids that must precede
    # Gate on the ENTIRE xcvrd-tests suite instead of the cumulative -k selection.
    # Set by the optimize-repair milestone: a performance change can regress any
    # behaviour, including the T-series parity tests that no milestone lists, so the
    # cumulative selection is not a sufficient gate for it.
    full_suite: bool = False


# The bootstrap milestone set: the Scoper starts from this and the loader falls
# back to it when no pipeline/milestones.json exists yet. It ALSO defines the final
# golden / full-suite conformance milestone that every run must end on.
DEFAULT_MILESTONES: list[Milestone] = [
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
        test_modules=["test_presence", "test_info_content"], deps=["M0"],
    ),
    Milestone(
        "M2", "DOM",
        "Periodically poll module monitors via the platform and publish "
        "TRANSCEIVER_DOM_SENSOR; the emulator Monitor trace shows real reads.",
        test_modules=["test_dom", "test_interaction_trace"], deps=["M1"],
    ),
    Milestone(
        "M3", "Status / CMIS state / errors",
        "Publish TRANSCEIVER_STATUS_SW (plug status, cmis_state=READY) and decode "
        "injected error events (blocking removes DOM, non-blocking keeps it).",
        test_modules=["test_status_error"], deps=["M2"],
    ),
    Milestone(
        "M4", "lpmode / reset",
        "Handle sfputil lpmode/reset: drive the CMIS ModuleGlobalControls writes "
        "and reflect lpmode state.",
        test_modules=["test_lpmode_reset"], deps=["M3"],
    ),
    Milestone(
        "M5", "Multiport concurrency",
        "Handle concurrent presence/DOM across many ports with per-module "
        "isolation (no cross-talk).",
        test_modules=["test_multiport"], deps=["M4"],
    ),
    Milestone(
        "M6", "Golden conformance (full suite)",
        "Reproduce the reference STATE_DB projection and pass the ENTIRE suite, "
        "including slow tests (no marker filter).",
        test_modules=["test_golden"], marker="", deps=["M5"],
    ),
]


# --- artifact I/O ------------------------------------------------------------
def _pipeline_dir() -> Path:
    return Path(os.environ.get("RECODE_PIPELINE_DIR", _ROOT / "pipeline"))


def artifact_path() -> Path:
    return _pipeline_dir() / MILESTONES_FILE


def _from_dict(d: dict) -> Milestone:
    return Milestone(
        id=d["id"],
        title=d.get("title", d["id"]),
        goal=d.get("goal", ""),
        test_modules=list(d.get("test_modules", []) or []),
        marker=d.get("marker", DEFAULT_MARKER) or "",
        origin=d.get("origin", "scoper"),
        unit_only=bool(d.get("unit_only", False)),
        source_refs=list(d.get("source_refs", []) or []),
        deps=list(d.get("deps", []) or []),
        full_suite=bool(d.get("full_suite", False)),
    )


def load() -> list[Milestone]:
    """Load the milestone set from pipeline/milestones.json, falling back to
    DEFAULT_MILESTONES when it does not exist or cannot be parsed. Reading fresh
    on every call keeps cross-process crash-resume consistent with the artifact."""
    p = artifact_path()
    if not p.exists():
        return list(DEFAULT_MILESTONES)
    try:
        raw = json.loads(p.read_text(encoding="utf-8"))
    except (ValueError, OSError):
        return list(DEFAULT_MILESTONES)
    items = raw.get("milestones", raw) if isinstance(raw, dict) else raw
    out: list[Milestone] = []
    for d in items:
        try:
            out.append(_from_dict(d))
        except (KeyError, TypeError):
            continue
    return out or list(DEFAULT_MILESTONES)


def save(milestones: list[Milestone]) -> Path:
    """Persist the milestone set (Scoper-owned). Returns the artifact path."""
    p = artifact_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(
        json.dumps({"milestones": [asdict(m) for m in milestones]}, indent=2),
        encoding="utf-8",
    )
    return p


# --- helpers over the CURRENT (loaded) milestone set -------------------------
def index_of(mid: str, ms: list[Milestone] | None = None) -> int:
    ms = ms if ms is not None else load()
    for i, m in enumerate(ms):
        if m.id == mid:
            return i
    raise KeyError(mid)


def by_id(mid: str, ms: list[Milestone] | None = None) -> Milestone:
    ms = ms if ms is not None else load()
    return ms[index_of(mid, ms)]


def cumulative_modules(mid: str, ms: list[Milestone] | None = None) -> list[str]:
    """The test module stems for a milestone's cumulative gate (M1..mid), in order.
    Unit-only (parity) milestones add nothing new but still inherit the full prior
    e2e set -- 'must pass all previous e2e tests'."""
    ms = ms if ms is not None else load()
    idx = index_of(mid, ms)
    mods: list[str] = []
    for m in ms[1:idx + 1]:                      # M1..current (M0 has none)
        for mod in m.test_modules:
            if mod not in mods:
                mods.append(mod)
    return mods


def cumulative_args(mid: str, ms: list[Milestone] | None = None) -> list[str]:
    """pytest args for a milestone's CUMULATIVE gate: a `-k` expression selecting
    this milestone's modules plus every earlier milestone's, plus the current
    milestone's `-m` marker. Returns [] when there are no e2e modules yet (M0
    deploy-smoke, or an early unit-only milestone before any e2e test exists)."""
    ms = ms if ms is not None else load()
    mods = cumulative_modules(mid, ms)
    if not mods:
        return []
    args = ["-k", " or ".join(mods)]
    marker = ms[index_of(mid, ms)].marker
    if marker:
        args += ["-m", marker]
    return args


def _cli() -> int:
    """`python -m orchestrator.milestones --args M3` -> one pytest arg per line
    (so a shell can read them into an array, preserving multi-word args)."""
    import sys
    argv = sys.argv[1:]
    ms = load()
    if len(argv) >= 2 and argv[0] == "--args":
        for a in cumulative_args(argv[1], ms):
            print(a)
        return 0
    for m in ms:                                 # default: print the matrix
        tag = f"  [{m.origin}{'/unit-only' if m.unit_only else ''}]"
        print(f"{m.id}  {m.title}{tag}")
        print(f"     gate: {' '.join(cumulative_args(m.id, ms)) or '(deploy-smoke)'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
