"""Burr actions for the OPTIMIZE phase: benchmark <-> optimize, then one repair milestone.

This phase runs INSIDE the translation pipeline (see app.py's graph), after parity
completes. It starts from a crate that is already correct -- every milestone
translated, the e2e oracle green, full source coverage -- and makes it faster:

  parity_verify -> benchmark -> optimize -> benchmark -> ... (max_opt_rounds)
                -> opt_repair -> select_milestone -> translate <-> validate -> terminal

  benchmark    Benchmarker agent runs benchmark/bench.sh against the working copy
               and writes pipeline/bench.json                     (measures only)
  optimize     Optimizer agent makes ONE small focused change set to the crate,
               guided by bench.json, and proves the UNIT tests still pass
  opt_repair   appends a final milestone that re-runs the ENTIRE e2e suite through
               the normal translate <-> validate repair loop

Why the e2e gate moved to the end
---------------------------------
Each round used to be validated against the full e2e suite and REVERTED on failure.
Measured over 20 real rounds that was actively harmful: one flaky test
(test_dom_gating) failed in 14 of them, INCLUDING 7 rounds where the Optimizer
changed nothing at all -- an empty change set cannot cause a regression, so those
reverts discarded work for a failure the round did not produce. 16 of 20 rounds
were thrown away and the second half of the run kept nothing.

So rounds are no longer individually reverted. The Optimizer still runs the mocked
unit tests itself every round (cheap, deterministic, catches real breakage
immediately), and the expensive e2e gate runs ONCE at the end as a normal
milestone -- where a failure is REPAIRED by the Translator over max_iter attempts
instead of throwing away the whole round. A flaky failure costs a repair attempt
that finds nothing, not an entire optimisation.

The crate is snapshotted once before the phase begins, so the pre-optimisation
tree is always recoverable if the repair milestone cannot converge.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from pathlib import Path

from burr.core import action

from .copilot import invoke_agent
from . import actions as _actions
from . import milestones
from .actions import (
    ROOT, XCVRD_TESTS,
    _crate_env, _pipeline, _is_mock, _read_json, _log_agent,
)

BENCH_DIR = ROOT.parent / "benchmark"
BENCH_JSON = "bench.json"
OPTIMIZE_JSON = "optimize.json"
HISTORY_JSON = "optimize_history.json"
# NOT "report.json": that is the translation stage's validator artifact, and
# sharing a pipeline directory would have each stage clobber the other's verdict.
REPORT_JSON = "optimize_report.json"
# Snapshot of the crate taken ONCE before the phase begins, so the pre-optimisation
# tree stays recoverable if the repair milestone cannot converge.
SNAPSHOT = "crate_snapshot"


def _crate() -> Path:
    """The working copy being optimised.

    Resolved through the module rather than from-imported: --pipeline-dir rebinds
    actions.PIPELINE_CRATE after this module is already imported, and a from-import
    would have captured the pre-rebind value.
    """
    return _actions.PIPELINE_CRATE


def _scenarios(state) -> list[str]:
    """The scenario ids this run is focused on, or [] for the whole suite.

    Read from state, with RECODE_BENCH_SCENARIOS as an override for one-off runs.
    Both the Benchmarker and the Optimizer resolve it through here so they cannot
    disagree about the target -- optimising for a scenario nobody measured, or
    measuring one nobody is optimising, is worse than not scoping at all.
    """
    raw = os.environ.get("RECODE_BENCH_SCENARIOS", "") or (state["bench_scenarios"] or "")
    return [s for s in raw.replace(",", " ").upper().split() if s]


# --------------------------------------------------------------------------- io

def _history() -> list:
    data = _read_json(HISTORY_JSON)
    return data.get("rounds", []) if isinstance(data, dict) else []


def _append_history(entry: dict) -> list:
    rounds = _history()
    rounds.append(entry)
    (_pipeline() / HISTORY_JSON).write_text(
        json.dumps({"rounds": rounds}, indent=2), encoding="utf-8")
    return rounds


def _bench_summary(bench: dict) -> dict:
    """Pull the few figures the loop reasons about out of a bench.json.

    Deliberately shallow: the Optimizer reads the full artifact itself. This exists
    so the orchestrator can record a per-round trend without interpreting results,
    which is the agents' job.
    """
    if not bench:
        return {}
    out = {"crate": bench.get("provenance", {}).get("crate", "?"),
           "sha": bench.get("provenance", {}).get("sha256_16", "?")}
    for rec in bench.get("records", []):
        scen, var, res = rec.get("scenario"), rec.get("variant"), rec.get("result", {})
        if not scen or not isinstance(res, dict):
            continue
        if scen == "B9" and "total_events" in res:
            out[f"b9_events_{var}"] = res["total_events"]
        elif scen == "B5" and "cpu_pct" in res:
            out[f"b5_cpu_{var}"] = res["cpu_pct"]
            out[f"b5_rss_kb_{var}"] = res.get("rss_kb_median")
        elif scen == "B4" and "p50_ns" in res:
            out[f"b4_sweep_ms_{var}"] = round(res["p50_ns"] / 1e6, 2)
    return out


# --------------------------------------------------------------- crate snapshot

def _snapshot_crate() -> None:
    """Copy the working crate aside so a failed round can be rolled back.

    Excludes target/ -- it is build output, it is large, and restoring a stale one
    would be worse than rebuilding.
    """
    dest = _pipeline() / SNAPSHOT
    if dest.exists():
        shutil.rmtree(dest, ignore_errors=True)
    if _crate().exists():
        shutil.copytree(_crate(), dest,
                        ignore=shutil.ignore_patterns("target", "*.tmp"))


def _restore_crate() -> bool:
    src = _pipeline() / SNAPSHOT
    if not src.exists():
        return False
    # Keep target/ in place: it is not in the snapshot and deleting it would force a
    # full rebuild for no benefit -- cargo will rebuild exactly what changed back.
    for item in _crate().iterdir():
        if item.name == "target":
            continue
        shutil.rmtree(item, ignore_errors=True) if item.is_dir() else item.unlink(missing_ok=True)
    for item in src.iterdir():
        dst = _crate() / item.name
        shutil.copytree(item, dst) if item.is_dir() else shutil.copy2(item, dst)
    return True


# ------------------------------------------------------------------ mock helpers

def _mock_bench(round_no: int) -> dict:
    """Synthetic bench.json for --mock runs: improves slightly each round so the
    trend logic and the graph wiring can be exercised without a DUT."""
    scale = max(0.55, 1.0 - 0.12 * round_no)
    doc = {
        "run": {"id": f"mock-{round_no}", "dom_update_interval_s": 5},
        "provenance": {"crate": "pipeline", "sha256_16": f"mock{round_no:012d}",
                       "built_this_run": True},
        "records": [
            {"scenario": "B9", "variant": "rust",
             "result": {"total_events": int(28000 * scale)}},
            {"scenario": "B9", "variant": "python", "result": {"total_events": 12900}},
            {"scenario": "B5", "variant": "rust",
             "result": {"cpu_pct": round(42.0 * scale, 2), "rss_kb_median": 83000}},
            {"scenario": "B5", "variant": "python",
             "result": {"cpu_pct": 18.1, "rss_kb_median": 71000}},
        ],
    }
    (_pipeline() / BENCH_JSON).write_text(json.dumps(doc, indent=2), encoding="utf-8")
    return doc


def _mock_optimize(round_no: int) -> dict:
    doc = {"round": round_no, "title": f"mock optimisation {round_no}",
           "files": ["xcvrd-rs/src/db.rs"], "rationale": "mock",
           "expected_effect": "mock", "behaviour_risk": "none", "unit_tests": "passed"}
    (_pipeline() / OPTIMIZE_JSON).write_text(json.dumps(doc, indent=2), encoding="utf-8")
    return doc


# ----------------------------------------------------------------------- actions

@action(reads=["opt_round", "bench_scenarios"],
        writes=["bench", "bench_history", "last_agent"])
def benchmark(state, __tracer) -> dict:
    """Benchmarker Agent: run benchmark/bench.sh against the working copy and read
    back the JSON it wrote. Measures only -- it has no edit tool."""
    round_no = state["opt_round"]
    out_path = _pipeline() / BENCH_JSON

    if _is_mock():
        bench = _mock_bench(round_no)
        summary = _bench_summary(bench)
        hist = list(state["bench_history"]) + [{"round": round_no, **summary}]
        return state.update(bench=bench, bench_history=hist, last_agent="benchmarker")

    scenarios = _scenarios(state)
    scen_arg = f" --scenario {','.join(scenarios)}" if scenarios else ""
    reps = os.environ.get("RECODE_BENCH_REPS", "1")
    cmd = (f"bash {BENCH_DIR}/bench.sh {_crate()} --reps {reps}{scen_arg} "
           f"--out {out_path}")
    focus = (f"This run is scoped to {', '.join(scenarios)} ONLY -- the command already "
             "says so. Do not add scenarios that were not asked for, and do not report "
             "the others as missing: they were deliberately not run.\n\n"
             if scenarios else "")
    prompt = (
        f"Benchmark round {round_no} of the working copy at {_crate()}.\n\n"
        f"Run EXACTLY this command and let it finish:\n    {cmd}\n\n"
        + focus +
        f"Then read {out_path} and verify before reporting: provenance.crate names the "
        "crate you measured and built_this_run is true; every scenario THAT WAS REQUESTED "
        "produced records for BOTH the rust and python variants; and none of them reports "
        "null, skipped, or an error field. Report the per-scenario rust vs python figures "
        "with ratios, and state whether the run is usable evidence. Pass through any note "
        "or caveat the JSON carries on a scenario verbatim rather than deciding yourself "
        "whether it matters.\n\n"
        "Do not edit anything. Do not re-run to get a nicer number. Do not fill in a "
        "missing value with an estimate."
    )
    result = invoke_agent(
        "benchmarker", prompt, cwd=ROOT,
        add_dirs=[str(BENCH_DIR), str(XCVRD_TESTS)],
        log_dir=_pipeline() / "logs", extra_env=_crate_env(),
    )
    _log_agent(__tracer, stage=f"benchmark-{round_no}", prompt=prompt, result=result)

    bench = _read_json(BENCH_JSON)
    summary = _bench_summary(bench)
    hist = list(state["bench_history"]) + [{"round": round_no, **summary}]
    if not bench:
        print(f"[optimize] round {round_no}: benchmarker produced no {BENCH_JSON}")
    return state.update(bench=bench, bench_history=hist, last_agent="benchmarker")


@action(reads=["opt_round", "bench", "max_opt_rounds", "bench_scenarios"],
        writes=["optimize", "opt_round", "last_agent"])
def optimize(state, __tracer) -> dict:
    """Optimizer Agent: ONE small focused change set to the working copy, guided by
    the measured results, without altering observable behaviour."""
    round_no = state["opt_round"]
    # Snapshot ONCE, before the first round touches anything, so the pre-optimisation
    # tree stays recoverable if the repair milestone cannot converge. NOT per round:
    # rounds accumulate deliberately (see the module docstring), so re-snapshotting
    # would overwrite the only pristine copy with an already-optimised one.
    if round_no <= 1:
        _snapshot_crate()

    if _is_mock():
        doc = _mock_optimize(round_no)
        return state.update(optimize=doc, opt_round=round_no + 1, last_agent="optimizer")

    scenarios = _scenarios(state)
    if scenarios:
        focus = (
            f"FOCUS: this run targets {', '.join(scenarios)} ONLY. Those are the only "
            "scenarios being measured, so they are the only evidence you have and the only "
            "thing your change will be judged on. Optimise for them specifically rather "
            "than for general tidiness.\n"
            "Two consequences worth being explicit about. A change that helps something "
            "NOT in that set is unmeasured here -- you cannot claim it as a win, so do not "
            "spend a round on it. And a change that speeds these up while plausibly slowing "
            "something outside the set is still a REGRESSION; nothing in this run would "
            "catch it, which is exactly why you must not make it.\n\n"
        )
    else:
        focus = ""

    prompt = (
        f"Optimisation round {round_no} of {state['max_opt_rounds']}. Improve the "
        f"PERFORMANCE of the working copy at {_crate()} (the daemon xcvrd-rs AND the "
        "Rust platform-bridge) without changing observable behaviour.\n\n"
        + focus +
        f"Evidence: {_pipeline() / BENCH_JSON} holds this round's measurements. "
        f"{_pipeline() / HISTORY_JSON} holds every previous round -- read it, and do not "
        "repeat an idea that was already tried.\n\n"
        "Make ONE small, focused change set: one coherent idea, small enough that a later "
        "failure can be traced to it. Prefer removing redundant work, then reducing I/O "
        "round trips, then avoiding needless copies; leave concurrency and build-profile "
        "changes until last.\n\n"
        "The daemon is graded as a black box on what it writes to STATE_DB. Do not change "
        "which rows/fields/values are published or when. A change that is faster because it "
        "does less observable work is a behaviour change -- reject it yourself.\n\n"
        "Rounds ACCUMULATE: your change stays in the crate and the next round builds on it. "
        "The full e2e suite runs once after the final round, and anything it catches is "
        "REPAIRED there rather than thrown away -- so do not gamble on a change you cannot "
        "justify, but do not refuse a well-evidenced one for fear of a single revert.\n\n"
        "Before finishing, run `bash tools/unit_test.sh` and make sure it passes. This is "
        "the only automatic gate between rounds, so a broken crate here compounds into "
        "every later round. If your change breaks a unit test, fix the change or undo it -- "
        "do NOT weaken the test.\n\n"
        f"Then write {_pipeline() / OPTIMIZE_JSON} with: round, title, files (EVERY file you "
        "touched -- the pipeline records this as the change set, so an incomplete list makes "
        "a later regression untraceable), rationale (citing the numbers that motivated it), "
        "expected_effect (a prediction the next benchmark will check), behaviour_risk, "
        "unit_tests, measured_before.\n\n"
        "If there is no meaningful and safe change left, say so: write optimize.json with "
        '"title": "no further safe optimisation identified" and an honest explanation. '
        "Inventing a marginal change carries regression risk for no measured gain."
    )
    result = invoke_agent(
        "optimizer", prompt, cwd=ROOT,
        add_dirs=[str(BENCH_DIR), str(XCVRD_TESTS)],
        log_dir=_pipeline() / "logs", extra_env=_crate_env(),
    )
    _log_agent(__tracer, stage=f"optimize-{round_no}", prompt=prompt, result=result)

    doc = _read_json(OPTIMIZE_JSON)
    _append_history({
        "round": round_no,
        "title": doc.get("title", "?"),
        "files": doc.get("files", []),
        "rationale": doc.get("rationale", ""),
        "expected_effect": doc.get("expected_effect", ""),
        "bench_before": (state["bench_history"] or [{}])[-1],
        "ts": time.strftime("%Y-%m-%dT%H:%M:%S"),
    })
    return state.update(optimize=doc, opt_round=round_no + 1, last_agent="optimizer")


@action(reads=["opt_round", "max_opt_rounds", "bench_history", "num_milestones"],
        writes=["num_milestones", "last_idx", "milestone_idx", "milestone_passed",
                "milestone_concluded", "iter_count", "opt_repairing", "opt_done",
                "done", "history"])
def opt_repair(state, __tracer) -> dict:
    """Close the optimize phase by appending ONE final milestone that re-proves the
    optimised crate against the ENTIRE e2e suite.

    The rounds before this one were gated only by the mocked unit tests, so this is
    the first time the accumulated optimisations meet the black-box oracle. Handing
    that to the normal translate <-> validate loop (rather than a bespoke check) buys
    the repair budget, the skips.json handling and the failure-reporting format that
    the translation stage already has -- and means a regression is FIXED rather than
    discarded, which is the whole reason the per-round revert was removed.

    full_suite=True: the cumulative -k selection covers only modules some milestone
    listed, but a performance change can regress anything, including the T-series
    parity tests that no milestone claims.
    """
    ms = milestones.load()
    base = len(ms)
    nid = f"M{base}"
    rounds_done = max(0, state["opt_round"] - 1)
    trend = state["bench_history"] or []
    ms.append(milestones.Milestone(
        nid, "Post-optimisation conformance",
        f"{rounds_done} optimisation round(s) changed the crate for performance while "
        "only the mocked unit tests were gating. Re-prove the ENTIRE e2e suite against "
        "the optimised crate and fix anything that regressed.\n\n"
        "Repair by correcting the daemon so the original behaviour is restored, keeping "
        "the performance work wherever that is possible. Where an optimisation cannot be "
        "made correct, undo THAT optimisation rather than weakening a test -- "
        f"{_pipeline() / HISTORY_JSON} records what each round changed and why. Never "
        "edit the tests: they are the oracle the optimisation has to survive.",
        test_modules=[], marker="", origin="optimize", unit_only=False,
        source_refs=[], deps=[ms[-1].id] if ms else [], full_suite=True,
    ))
    milestones.save(ms)

    print(f"[optimize] {rounds_done} round(s) done; appended {nid} "
          "(post-optimisation conformance, full e2e suite)")
    if trend:
        print(f"[optimize] first: {json.dumps(trend[0], sort_keys=True)}")
        print(f"[optimize] last : {json.dumps(trend[-1], sort_keys=True)}")

    if __tracer is not None:
        try:
            __tracer.log_attributes(opt_rounds_done=rounds_done, repair_milestone=nid,
                                    bench_history=trend)
        except Exception:
            pass

    return state.update(
        num_milestones=base + 1,
        last_idx=base,           # the repair milestone is now last
        milestone_idx=base,      # ...and is the one to run next
        milestone_passed=False,
        milestone_concluded=False,
        iter_count=0,
        opt_repairing=True,      # routes validate -> terminal instead of back to parity
        opt_done=True,           # parity must not re-enter the optimize phase
        done=False,              # not finished until the repair milestone concludes
    ).append(history={"milestone": "OPTIMIZE", "iter": rounds_done, "passed": True})