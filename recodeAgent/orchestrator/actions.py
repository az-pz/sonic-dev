"""Burr actions: the ReCodeAgent stages as deterministic nodes.

Each action either (a) invokes a Copilot custom agent via invoke_agent() and
reads the artifact it wrote to pipeline/, or (b) does pure bookkeeping
(select_milestone). No LLM logic lives here -- the agents own that.

Paper mapping (Algorithm 1):
  analyze           -> line 22 (Analyzer Agent, once)
  plan              -> line 23 (Planning Agent, once; copies crate/ -> pipeline/crate)
  select_milestone  -> our milestone loop head (advance / init)
  translate         -> lines 1-13 (Translator Agent; Part A source + Part B unit tests;
                       repair mode when report != {})
  validate          -> lines 14-21 (Validator Agent: e2e xcvrd-tests black-box oracle
                       PLUS the translated/new Rust unit tests via cargo test)
"""
from __future__ import annotations

import json
import os
from pathlib import Path

from burr.core import action

from . import state as S
from .copilot import invoke_agent

ROOT = Path(__file__).resolve().parent.parent
PIPELINE = Path(os.environ.get("RECODE_PIPELINE_DIR", ROOT / "pipeline"))
SOURCE = ROOT / "source" / "xcvrd"
SOURCE_TESTS = SOURCE / "tests"          # Python behavioral unit tests + mocks (Part B input)
CRATE = ROOT / "crate"                   # IMMUTABLE input: bootstrap xcvrd-rs + scaffolding (never edited)
PIPELINE_CRATE = PIPELINE / "crate"      # the working copy the agents translate into
# xcvrd-tests lives outside recodeAgent; agents get read access via --add-dir.
XCVRD_TESTS = ROOT.parent / "xcvrd-tests"


def _crate_env() -> dict:
    """Point the DUT tools (validate_on_dut.sh / build_check.sh / unit_test.sh) at the
    pipeline working copy so the immutable crate/ is never built or modified."""
    return {"RECODE_CRATE_DIR": str(PIPELINE_CRATE)}


def _pipeline() -> Path:
    PIPELINE.mkdir(parents=True, exist_ok=True)
    return PIPELINE


def _read_json(name: str) -> dict:
    p = _pipeline() / name
    if not p.exists():
        return {}
    try:
        return json.loads(p.read_text(encoding="utf-8"))
    except (ValueError, OSError):
        return {}


@action(reads=[], writes=["analysis_done"])
def analyze(state) -> dict:
    """Analyzer Agent: research source/xcvrd (incl. its Python unit tests) and design the Rust target."""
    invoke_agent(
        "analyzer",
        prompt=(
            f"Research the Python xcvrd source at {SOURCE} (including its behavioral "
            f"unit tests + mocks at {SOURCE_TESTS}) and the provided platform-bridge / "
            f"STATE_DB scaffolding under the IMMUTABLE input crate {CRATE} (read-only "
            f"reference). Produce the three design documents and write them to "
            f"{PIPELINE/'analysis.md'}: (1) source research, (2) Python-dep->Rust-crate "
            "analysis, (3) target design including the PyO3 platform-bridge boundary, the "
            "STATE_DB schema contract, the M0..M6 milestone mapping, AND the unit-test "
            "strategy (how to translate the Python behavioral unit tests + their platform/"
            "swss mocks into Rust, e.g. trait seams for mockable HAL/DB). Do not write any Rust."
        ),
        cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)],
        log_dir=_pipeline() / "logs",
    )
    return state.update(analysis_done=(_pipeline() / "analysis.md").exists())


@action(reads=["analysis_done"], writes=["plan_done"])
def plan(state) -> dict:
    """Planning Agent: copy crate/->pipeline/crate, fragment extraction (Part A + Part B), name mapping, skeleton, milestone plan."""
    invoke_agent(
        "planner",
        prompt=(
            f"Read {PIPELINE/'analysis.md'}. First COPY the immutable input crate "
            f"{CRATE} to the working copy {PIPELINE_CRATE} (idempotent -- do NOT clobber "
            "existing translation work if it already exists; never modify the original "
            f"{CRATE}). Then, working ONLY in {PIPELINE_CRATE}: extract translation units "
            "from the xcvrd source AND from its Python unit tests (Part A source + Part B "
            f"tests at {SOURCE_TESTS}), build a one-to-one name mapping, generate the Rust "
            f"skeleton under {PIPELINE_CRATE/'xcvrd-rs'} (compilable stubs + test/mock module "
            "layout on top of the provided platform-bridge + swss-common), and write a "
            f"dependency-aware milestone plan to {PIPELINE/'plan.json'} aligned to M0..M6 "
            "(each milestone lists both its daemon steps and its unit-test steps). Verify the "
            "skeleton compiles with `bash tools/build_check.sh`."
        ),
        cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)],
        log_dir=_pipeline() / "logs",
        extra_env=_crate_env(),
    )
    return state.update(plan_done=(_pipeline() / "plan.json").exists())


@action(
    reads=["milestone_idx", "milestone_passed"],
    writes=["milestone_idx", "iter_count", "milestone_passed"],
)
def select_milestone(state) -> dict:
    """Loop head: initialise (from plan) or advance (after a pass).

    On re-entry after a pass, advance to the next milestone; always reset the
    per-milestone repair counter and the passed flag.
    """
    idx = state["milestone_idx"]
    if state["milestone_passed"]:          # re-entered because the last one passed
        idx += 1
    return state.update(milestone_idx=idx, iter_count=0, milestone_passed=False)


@action(reads=["milestone_idx", "iter_count", "report"], writes=[])
def translate(state) -> dict:
    """Translator Agent: implement the current milestone; repair mode if report != {}."""
    m = S.current_milestone(state)
    report = state["report"] or {}
    mode = "REPAIR" if report.get("failures") else "IMPLEMENT"
    invoke_agent(
        "translator",
        prompt=(
            f"Milestone {m.id} ({m.title}). Goal: {m.goal}\n"
            f"Mode: {mode}. "
            + (f"Fix exactly these validation failures (unit and/or e2e): "
               f"{json.dumps(report.get('failures', []))}. "
               if mode == "REPAIR" else "")
            + f"Work ONLY in the working copy {PIPELINE_CRATE/'xcvrd-rs'} (never the "
              f"immutable {CRATE}). Implement this milestone's daemon logic (Part A) on "
              "the provided platform-bridge (PyO3) + swss-common, AND translate the "
              "matching Python behavioral unit tests + add new Rust unit tests for the "
              "new code (Part B), using the crate's mock HAL/DB seams (mirroring "
              "mock_platform.py / mock_swsscommon.py). Do not modify the e2e tests or the "
              "platform. Compile with `bash tools/build_check.sh` and run the unit tests "
              "with `bash tools/unit_test.sh` until both are clean."
        ),
        cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)],
        log_dir=_pipeline() / "logs",
        extra_env=_crate_env(),
    )
    return state


@action(
    reads=["milestone_idx", "iter_count"],
    writes=["milestone_passed", "iter_count", "report", "history", "done"],
)
def validate(state) -> dict:
    """Validator Agent (adapted): build+inject the Rust daemon on the DUT and run
    the milestone's xcvrd-tests subset; the harness writes the authoritative
    report.json (passed flag derived from results.xml)."""
    m = S.current_milestone(state)
    from . import milestones
    gate = milestones.cumulative_args(m.id)   # CUMULATIVE: this milestone + all earlier ones
    invoke_agent(
        "validator",
        prompt=(
            f"Validate milestone {m.id} ({m.title}). Run BOTH validation layers on the "
            f"working copy {PIPELINE_CRATE}:\n"
            f"1. Unit tests (Part B, mocked, fast): `bash tools/unit_test.sh` -- builds + "
            "runs the crate's Rust unit tests (cargo test) in the container; no DUT needed.\n"
            f"2. E2E black-box oracle (authoritative): `bash tools/validate_on_dut.sh {m.id}` "
            "-- resolves the CUMULATIVE gate itself (this milestone's tests PLUS every earlier "
            f"milestone's = {json.dumps(gate) or '(deploy-smoke)'}), builds the Rust crate for "
            "pmon, injects it (reversibly), runs xcvrd-tests/run.sh, restores the Python xcvrd, "
            "and parses results.xml into pipeline/report.json.\n"
            f"Then write the authoritative verdict to {PIPELINE/'report.json'} as "
            '{"milestone","passed","tests","failures"}, where `passed` requires BOTH the unit '
            "tests AND the e2e suite to pass, and `failures` gives actionable, structured repair "
            "guidance for each failing unit or e2e test. Never edit the daemon, tests, or platform."
        ),
        cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)],
        log_dir=_pipeline() / "logs",
        extra_env=_crate_env(),
    )
    report = _read_json("report.json")
    passed = bool(report.get("passed"))
    it = state["iter_count"] + 1
    done = passed and S.is_last_milestone(state)
    entry = {"milestone": m.id, "iter": it, "passed": passed}
    return state.update(
        milestone_passed=passed,
        iter_count=it,
        report={} if passed else report,   # clear on pass so next milestone starts fresh
        done=done,
    ).append(history=entry)
