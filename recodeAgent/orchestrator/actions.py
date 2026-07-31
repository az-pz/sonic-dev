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
from . import milestones
from .copilot import invoke_agent, transcript_from_events, summary_from_events

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


def _is_mock() -> bool:
    return os.environ.get("RECODE_MOCK") == "1"


def _read_json(name: str) -> dict:
    p = _pipeline() / name
    if not p.exists():
        return {}
    try:
        return json.loads(p.read_text(encoding="utf-8"))
    except (ValueError, OSError):
        return {}


def _log_agent(tracer, *, stage: str, prompt: str, result) -> dict:
    """Surface a Copilot invocation in the Burr UI: log the chat transcript, a
    change summary, and run stats as attributes on the current action (visible in
    the UI's trace/attributes panel). Returns the summary for use in state.
    `tracer` is the injected TracerFactory (None-safe for tests)."""
    transcript = transcript_from_events(result.events)
    summary = summary_from_events(result.events)
    if tracer is not None:
        try:
            tracer.log_attributes(
                stage=stage,
                agent=result.agent,
                copilot_prompt=prompt,
                copilot_chat=transcript or (result.final_text or ""),
                final_text=result.final_text or "",
                files_modified=summary["files_modified"],
                lines_added=summary["lines_added"],
                lines_removed=summary["lines_removed"],
                premium_requests=summary["premium_requests"],
                returncode=result.returncode,
                duration_s=round(result.duration_s, 1),
                transcript_log=result.stdout_path or "",
            )
        except Exception as e:  # never let telemetry break the pipeline
            print(f"[recode] warning: failed to log agent attributes: {e}")
    return summary


@action(reads=[], writes=["analysis_done", "last_agent"])
def analyze(state, __tracer) -> dict:
    """Analyzer Agent: research source/xcvrd (incl. its Python unit tests) and design the Rust target."""
    prompt = (
        f"Research the Python xcvrd source at {SOURCE} (including its behavioral "
        f"unit tests + mocks at {SOURCE_TESTS}) and the provided platform-bridge / "
        f"STATE_DB scaffolding under the IMMUTABLE input crate {CRATE} (read-only "
        f"reference). Produce the three design documents and write them to "
        f"{PIPELINE/'analysis.md'}: (1) source research, (2) Python-dep->Rust-crate "
        "analysis, (3) target design including the PyO3 platform-bridge boundary, the "
        "STATE_DB schema contract, AND the unit-test "
        "strategy (how to translate the Python behavioral unit tests + their platform/"
        "swss mocks into Rust, e.g. trait seams for mockable HAL/DB). Milestone "
        "planning is a SEPARATE downstream stage (the Scoper) -- do NOT define "
        "milestones here. Do not write any Rust."
    )
    res = invoke_agent(
        "analyzer", prompt=prompt, cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)], log_dir=_pipeline() / "logs",
    )
    _log_agent(__tracer, stage="analyze", prompt=prompt, result=res)
    return state.update(
        analysis_done=(_pipeline() / "analysis.md").exists(),
        last_agent="analyzer",
    )


def _xcvrd_test_modules() -> list[str]:
    """The set of xcvrd-tests module stems (test_*.py) — the universe the Scoper's
    milestone partition must cover. Empty if the suite isn't reachable (e.g. an
    offline mock run on a box without it)."""
    tdir = XCVRD_TESTS / "tests"
    if not tdir.is_dir():
        return []
    return sorted(p.stem for p in tdir.glob("test_*.py"))


def _scope_coverage(ms: list) -> dict:
    """Coverage guardrail for the Scoper output: which xcvrd-tests modules are NOT
    claimed by any milestone, and whether a final golden/full-suite milestone exists."""
    universe = set(_xcvrd_test_modules())
    claimed = {mod for m in ms for mod in m.test_modules}
    return {
        "universe": sorted(universe),
        "orphans": sorted(universe - claimed) if universe else [],
        "has_golden_final": bool(ms) and "test_golden" in ms[-1].test_modules,
    }


@action(
    reads=["scope_done", "num_milestones", "gaps"],
    writes=["scope_done", "num_milestones", "last_idx", "milestone_idx",
            "milestone_passed", "milestone_concluded", "last_agent"],
)
def scope(state, __tracer) -> dict:
    """Scoper Agent: turn analysis.md + the source + the xcvrd-tests suite into the
    milestone set (pipeline/milestones.json). First pass PARTITIONS every xcvrd-tests
    module across dependency-ordered milestones, ending on a golden/full-suite
    milestone. On re-scope (parity feedback) it APPENDS unit-only milestones for the
    untranslated-source gaps -- new ids, never renumbering the ones already done."""
    is_rescope = bool(state["scope_done"]) and state["num_milestones"] > 0
    old_count = state["num_milestones"]
    modules = _xcvrd_test_modules()
    if not is_rescope:
        prompt = (
            f"Read {PIPELINE/'analysis.md'} and research the Python xcvrd source at "
            f"{SOURCE}. Partition ALL of the daemon's functionality into a "
            "dependency-ordered set of translation milestones and write them to "
            f"{milestones.artifact_path()} (schema: id, title, goal, test_modules, "
            "marker, origin='scoper', unit_only=false, source_refs, deps).\n"
            "HARD REQUIREMENTS:\n"
            f"  * Every xcvrd-tests module must be claimed by exactly one milestone. "
            f"The full module universe is: {json.dumps(modules)}.\n"
            "  * Milestones must be dependency-ordered (bootstrap before features); "
            "M0 is the deploy-smoke skeleton (test_modules=[]).\n"
            "  * Each milestone is a REASONABLE chunk -- not one giant milestone, not "
            "dozens of trivial ones. Group tests by the daemon functionality they exercise.\n"
            "  * The FINAL milestone is golden conformance / full suite (test_modules "
            "include 'test_golden'; it re-runs everything).\n"
            "Do NOT invent tests, modify tests, or write Rust -- you only produce the "
            "milestone plan mapped onto the EXISTING suite."
        )
    else:
        prompt = (
            f"Re-scope. Read the untranslated-source gaps in "
            f"{PIPELINE/'parity_report.json'} and the existing milestone set at "
            f"{milestones.artifact_path()}. For each gap, APPEND one new milestone to "
            "that file (do NOT modify or renumber existing milestones). New milestones: "
            "origin='parity', unit_only=true, test_modules=[] (no new e2e test exists for "
            "untested source -- they inherit the full prior e2e gate and are verified by "
            "Rust unit tests), with source_refs naming the exact source symbols to cover "
            f"and a goal describing what to translate. Use fresh ids continuing after "
            f"the current highest (currently {old_count} milestones)."
        )
    res = invoke_agent(
        "scoper", prompt=prompt, cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)], log_dir=_pipeline() / "logs",
    )
    _log_agent(__tracer, stage="scope" if not is_rescope else "scope:rescope",
               prompt=prompt, result=res)

    ms = milestones.load()
    cov = _scope_coverage(ms)
    if __tracer is not None:
        try:
            __tracer.log_attributes(scope_rescope=is_rescope, num_milestones=len(ms),
                                    orphan_test_modules=cov["orphans"],
                                    has_golden_final=cov["has_golden_final"])
        except Exception:
            pass
    # Guardrail: on a REAL run the partition must cover every xcvrd-tests module and
    # end on a golden/full-suite milestone. Offline mock runs skip the hard assert
    # (the mock scoper writes the bootstrap set, not a full partition of ~30 modules).
    if not is_rescope and not _is_mock():
        if cov["orphans"]:
            raise RuntimeError(
                f"scoper left xcvrd-tests modules unclaimed by any milestone: {cov['orphans']}")
        if cov["universe"] and not cov["has_golden_final"]:
            raise RuntimeError("scoper's final milestone is not the golden/full-suite gate")

    new_count = len(ms)
    first_pending = old_count if is_rescope else 0
    if first_pending >= new_count:               # defensive clamp
        first_pending = max(0, new_count - 1)
    return state.update(
        scope_done=True,
        num_milestones=new_count,
        last_idx=new_count - 1,
        milestone_idx=first_pending,
        milestone_passed=False,
        milestone_concluded=False,   # fresh milestone set: don't let select_milestone advance past the first pending one
        last_agent="scoper",
    )


@action(reads=["analysis_done"], writes=["plan_done", "last_agent"])
def plan(state, __tracer) -> dict:
    """Planning Agent: copy crate/->pipeline/crate, fragment extraction (Part A + Part B), name mapping, skeleton, milestone plan."""
    prompt = (
        f"Read {PIPELINE/'analysis.md'}. First COPY the immutable input crate "
        f"{CRATE} to the working copy {PIPELINE_CRATE} (idempotent -- do NOT clobber "
        "existing translation work if it already exists; never modify the original "
        f"{CRATE}). Then, working ONLY in {PIPELINE_CRATE}: extract translation units "
        "from the xcvrd source AND from its Python unit tests (Part A source + Part B "
        f"tests at {SOURCE_TESTS}), build a one-to-one name mapping, generate the Rust "
        f"skeleton under {PIPELINE_CRATE/'xcvrd-rs'} (compilable stubs + test/mock module "
        "layout on top of the provided platform-bridge + swss-common), and write a "
        f"dependency-aware translation plan to {PIPELINE/'plan.json'} that, for EACH "
        f"milestone in the Scoper's set at {milestones.artifact_path()}, lists both its "
        "daemon steps and its unit-test steps. Verify the "
        "skeleton compiles with `bash tools/build_check.sh`."
    )
    res = invoke_agent(
        "planner", prompt=prompt, cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)], log_dir=_pipeline() / "logs",
        extra_env=_crate_env(),
    )
    _log_agent(__tracer, stage="plan", prompt=prompt, result=res)
    return state.update(
        plan_done=(_pipeline() / "plan.json").exists(),
        last_agent="planner",
    )


@action(
    reads=["milestone_idx", "milestone_concluded"],
    writes=["milestone_idx", "iter_count", "milestone_passed", "milestone_concluded"],
)
def select_milestone(state) -> dict:
    """Loop head: initialise (from plan/scope) or advance (after a milestone concludes).

    A milestone "concludes" when validate either passes it OR exhausts the repair
    budget (give-up). In both cases we advance to the next milestone -- a stuck
    milestone is skipped rather than failing the whole run; its untranslated
    functionality is caught later by the Parity Verifier. On the first entry
    (from plan, or after a re-scope) `milestone_concluded` is False, so we start
    the current milestone without advancing. Always reset the per-milestone repair
    counter, the passed flag, and the concluded flag.
    """
    idx = state["milestone_idx"]
    if state["milestone_concluded"]:       # re-entered after this milestone passed or was given up
        idx += 1
    return state.update(milestone_idx=idx, iter_count=0,
                        milestone_passed=False, milestone_concluded=False)


@action(reads=["milestone_idx", "iter_count", "report"], writes=["last_agent"])
def translate(state, __tracer) -> dict:
    """Translator Agent: implement the current milestone (Part A + Part B); repair mode if report != {}."""
    m = S.current_milestone(state)
    report = state["report"] or {}
    mode = "REPAIR" if report.get("failures") else "IMPLEMENT"
    prompt = (
        f"Milestone {m.id} ({m.title}). Goal: {m.goal}\n"
        f"Mode: {mode}. "
        + (f"The Validator reported these failures (unit and/or e2e): "
           f"{json.dumps(report.get('failures', []))}. "
           "Do NOT patch blindly: first investigate WHY each fails and what the "
           "feedback is telling you, re-read the failing tests to see what behaviour "
           "and STATE_DB fields they require, and decide whether each is a bug in "
           "translated code OR functionality that was never translated (add it by "
           "porting the Python logic). Also confirm ALL functionality the milestone's "
           "tests need is translated, then fix the root cause faithfully. "
           if mode == "REPAIR" else "")
        + f"Work ONLY in the working copy {PIPELINE_CRATE/'xcvrd-rs'} (never the "
          f"immutable {CRATE}). Implement this milestone's daemon logic (Part A) on "
          "the provided platform-bridge (PyO3) + swss-common, AND translate the "
          "matching Python behavioral unit tests + add new Rust unit tests for the "
          "new code (Part B), using the crate's mock HAL/DB seams (mirroring "
          "mock_platform.py / mock_swsscommon.py). Do not modify the e2e tests or the "
          "platform. Compile with `bash tools/build_check.sh` and run the unit tests "
          "with `bash tools/unit_test.sh` until both are clean."
    )
    res = invoke_agent(
        "translator", prompt=prompt, cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)], log_dir=_pipeline() / "logs",
        extra_env=_crate_env(),
    )
    _log_agent(__tracer, stage=f"translate:{m.id}:{mode}", prompt=prompt, result=res)
    return state.update(last_agent="translator")


@action(
    reads=["milestone_idx", "iter_count", "max_iter"],
    writes=["milestone_passed", "milestone_concluded", "iter_count", "report",
            "history", "skipped", "done", "last_agent"],
)
def validate(state, __tracer) -> dict:
    """Validator Agent (adapted): run the mocked unit tests AND the e2e xcvrd-tests
    on the DUT for the working copy; write the authoritative combined report.json."""
    m = S.current_milestone(state)
    gate = milestones.cumulative_args(m.id)   # CUMULATIVE: this milestone + all earlier ones
    # Build the exact explicit harness invocation, passing -k with the cumulative
    # test selection (M0 has no e2e tests -> deploy-smoke, no -k).
    if gate:
        gate_expr = gate[1] if len(gate) >= 2 else ""
        e2e_cmd = f'bash tools/validate_on_dut.sh {m.id} -k "{gate_expr}"'
    else:
        e2e_cmd = f"bash tools/validate_on_dut.sh {m.id}"
    prompt = (
        f"Validate milestone {m.id} ({m.title}). Run BOTH validation layers on the "
        f"working copy {PIPELINE_CRATE}:\n"
        f"1. Unit tests (Part B, mocked, fast): `bash tools/unit_test.sh` -- builds + "
        "runs the crate's Rust unit tests (cargo test) in the container; no DUT needed.\n"
        f"2. E2E black-box oracle (authoritative): run `{e2e_cmd}` -- pass the CUMULATIVE "
        "gate EXPLICITLY as a pytest -k selection (this milestone's tests PLUS every "
        "earlier milestone's; resolve it yourself with `python -m orchestrator.milestones "
        f"--args {m.id}`). It builds the Rust crate for pmon, injects it (reversibly), runs "
        "exactly that -k subset of xcvrd-tests/run.sh, restores the Python xcvrd, and parses "
        "results.xml into pipeline/report.json.\n"
        f"Then write the authoritative verdict to {PIPELINE/'report.json'} as "
        '{"milestone","passed","tests","failures"}, where `passed` requires BOTH the unit '
        "tests AND the e2e suite to pass, and `failures` gives actionable, structured repair "
        "guidance for each failing unit or e2e test. Never edit the daemon, tests, or platform."
    )
    res = invoke_agent(
        "validator", prompt=prompt, cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)], log_dir=_pipeline() / "logs",
        extra_env=_crate_env(),
    )
    report = _read_json("report.json")
    passed = bool(report.get("passed"))
    it = state["iter_count"] + 1
    # A milestone "concludes" when it passes OR the repair budget is exhausted.
    # On give-up we do NOT fail the run: select_milestone advances to the next
    # milestone (the stuck one is skipped and recorded; its untranslated behaviour
    # is caught later by the Parity Verifier). Success is decided by parity, not here.
    gave_up = (not passed) and (it >= state["max_iter"])
    concluded = passed or gave_up
    done = False
    entry = {"milestone": m.id, "iter": it, "passed": passed,
             "gave_up": gave_up}
    summary = _log_agent(__tracer, stage=f"validate:{m.id}", prompt=prompt, result=res)
    if __tracer is not None:
        try:
            __tracer.log_attributes(milestone=m.id, milestone_passed=passed,
                                    gave_up=gave_up,
                                    report_tests=report.get("tests", {}),
                                    report_failures=report.get("failures", []))
        except Exception:
            pass
    new = state.update(
        milestone_passed=passed,
        milestone_concluded=concluded,
        iter_count=it,
        report={} if passed else report,   # clear on pass so next milestone starts fresh
        done=done,
        last_agent="validator",
    ).append(history=entry)
    if gave_up:
        new = new.append(skipped=m.id)     # record the un-fixable milestone for the final verdict
    return new


@action(
    reads=["parity_round", "max_parity_rounds"],
    writes=["parity_round", "parity_complete", "gaps", "done", "history", "last_agent"],
)
def parity_verify(state, __tracer) -> dict:
    """Parity Verifier Agent: after every milestone passes its tests, comprehensively
    compare the Python source against the final Rust translation, PER MODULE, to verify
    nothing was left untranslated. Writes pipeline/parity_report.json
    {coverage_matrix, gaps, complete}. If complete -> the pipeline is done (success).
    If gaps remain and there's outer-loop budget -> they feed back to the Scoper as new
    milestones; if the budget is exhausted with gaps still open, the run FAILS (there is
    no deferral -- everything must translate)."""
    prompt = (
        f"Comprehensively verify translation completeness. Compare the Python xcvrd "
        f"source at {SOURCE} against the final Rust translation at "
        f"{PIPELINE_CRATE/'xcvrd-rs'}, working PER MODULE over the module inventory in "
        f"{PIPELINE/'analysis.md'}. For each source module, determine whether every "
        "function / behavior / branch has a corresponding Rust implementation. Write "
        f"{PIPELINE/'parity_report.json'} as {{\"coverage_matrix\":[{{\"module\",\"covered\","
        "\"missing\":[...]}], \"gaps\":[{\"source_ref\",\"functionality\",\"suggested_milestone\"}], "
        "\"complete\": true|false}. `complete` is true ONLY when every module is fully "
        "covered (gaps empty). Do NOT modify any code, tests, or the platform -- you only "
        "produce the completeness report."
    )
    res = invoke_agent(
        "parity_verifier", prompt=prompt, cwd=ROOT,
        add_dirs=[str(XCVRD_TESTS)], log_dir=_pipeline() / "logs",
        extra_env=_crate_env(),
    )
    rep = _read_json("parity_report.json")
    complete = bool(rep.get("complete"))
    gaps = rep.get("gaps", []) or []
    rnd = state["parity_round"] + 1
    _log_agent(__tracer, stage=f"parity_verify:round{rnd}", prompt=prompt, result=res)
    if __tracer is not None:
        try:
            __tracer.log_attributes(parity_round=rnd, parity_complete=complete,
                                    gap_count=len(gaps), gaps=gaps[:50],
                                    coverage_matrix=rep.get("coverage_matrix", []))
        except Exception:
            pass
    entry = {"milestone": "PARITY", "iter": rnd, "passed": complete}
    return state.update(
        parity_round=rnd,
        parity_complete=complete,
        gaps=gaps,
        done=complete,                # success iff full source coverage
        last_agent="parity_verifier",
    ).append(history=entry)
