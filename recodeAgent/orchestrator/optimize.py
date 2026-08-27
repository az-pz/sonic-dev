"""Burr actions for the OPTIMIZE stage: benchmark -> optimize -> validate.

A separate stage from the translation pipeline (actions.py), run on its own. It
starts from a crate that is already correct -- every milestone translated and the
e2e oracle green -- and makes it faster without changing what it does:

  benchmark   Benchmarker agent runs benchmark/bench.sh against the working copy
              and writes pipeline/bench.json                    (measures only)
  optimize    Optimizer agent makes ONE small focused change set to the crate,
              guided by bench.json, and proves unit tests still pass
  validate    the SAME Validator agent the translation stage uses: mocked unit
              tests plus the e2e xcvrd-tests oracle on the DUT

Reusing the existing Validator is deliberate. A performance stage that graded
itself with a weaker gate than the translation stage would be able to "optimise"
by regressing behaviour the translation stage worked to establish. The optimizer
must clear exactly the bar the translation had to clear.

A round that fails validation is REVERTED, not repaired: the change set is small
by construction, and a broken optimisation has no value to preserve. The revert
is recorded in pipeline/optimize_history.json so the next round does not retry it.
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
# Snapshot of the crate taken before each optimize round, so a failed round can be
# rolled back to a known-good tree rather than relying on the agent to undo itself.
SNAPSHOT = "crate_snapshot"


def _crate() -> Path:
    """The working copy being optimised.

    Resolved through the module rather than from-imported: --pipeline-dir rebinds
    actions.PIPELINE_CRATE after this module is already imported, and a from-import
    would have captured the pre-rebind value.
    """
    return _actions.PIPELINE_CRATE


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

@action(reads=["round"], writes=["bench", "bench_history", "last_agent"])
def benchmark(state, __tracer) -> dict:
    """Benchmarker Agent: run benchmark/bench.sh against the working copy and read
    back the JSON it wrote. Measures only -- it has no edit tool."""
    round_no = state["round"]
    out_path = _pipeline() / BENCH_JSON

    if _is_mock():
        bench = _mock_bench(round_no)
        summary = _bench_summary(bench)
        hist = list(state["bench_history"]) + [{"round": round_no, **summary}]
        return state.update(bench=bench, bench_history=hist, last_agent="benchmarker")

    scenarios = os.environ.get("RECODE_BENCH_SCENARIOS", "").strip()
    scen_arg = f" --scenario {scenarios}" if scenarios else ""
    reps = os.environ.get("RECODE_BENCH_REPS", "1")
    cmd = (f"bash {BENCH_DIR}/bench.sh {_crate()} --reps {reps}{scen_arg} "
           f"--out {out_path}")
    prompt = (
        f"Benchmark round {round_no} of the working copy at {_crate()}.\n\n"
        f"Run EXACTLY this command and let it finish:\n    {cmd}\n\n"
        f"Then read {out_path} and verify before reporting: provenance.crate names the "
        "crate you measured and built_this_run is true; every scenario produced records "
        "for BOTH the rust and python variants; and no scenario reports null, skipped, or "
        "an error field. Report the per-scenario rust vs python figures with ratios, and "
        "state whether the run is usable evidence. If B9 shows the two daemons doing "
        "materially different amounts of EEPROM work, say so FIRST -- the timings below it "
        "are then comparing two different programs.\n\n"
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


@action(reads=["round", "bench"], writes=["optimize", "last_agent"])
def optimize(state, __tracer) -> dict:
    """Optimizer Agent: ONE small focused change set to the working copy, guided by
    the measured results, without altering observable behaviour."""
    round_no = state["round"]
    # Snapshot BEFORE the agent touches anything, so a failed validate can roll back
    # to a known-good tree rather than trusting the agent to undo its own edits.
    _snapshot_crate()

    if _is_mock():
        doc = _mock_optimize(round_no)
        return state.update(optimize=doc, last_agent="optimizer")

    prompt = (
        f"Optimisation round {round_no}. Improve the PERFORMANCE of the working copy at "
        f"{_crate()} (the daemon xcvrd-rs AND the Rust platform-bridge) without "
        "changing observable behaviour.\n\n"
        f"Evidence: {_pipeline() / BENCH_JSON} holds this round's measurements. "
        f"{_pipeline() / HISTORY_JSON} holds every previous round including the ones that "
        "were REVERTED and why -- read it and do not retry a failed idea.\n\n"
        "Make ONE small, focused change set: one coherent idea, small enough that if "
        "validation fails you know exactly what caused it. Prefer removing redundant work, "
        "then reducing I/O round trips, then avoiding needless copies; leave concurrency "
        "and build-profile changes until last.\n\n"
        "The daemon is graded as a black box on what it writes to STATE_DB. Do not change "
        "which rows/fields/values are published or when. A change that is faster because it "
        "does less observable work is a behaviour change -- reject it yourself.\n\n"
        "Before finishing, run `bash tools/unit_test.sh` and make sure it passes. If your "
        "change breaks a unit test, fix the change or revert it -- do NOT weaken the test.\n\n"
        f"Then write {_pipeline() / OPTIMIZE_JSON} with: round, title, files, rationale "
        "(citing the numbers that motivated it), expected_effect (a prediction the next "
        "benchmark will check), behaviour_risk, unit_tests, measured_before.\n\n"
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
    return state.update(optimize=doc, last_agent="optimizer")


@action(reads=["round", "optimize", "bench_history"],
        writes=["round", "validated", "report", "history", "reverted", "last_agent"])
def validate_optimization(state, __tracer) -> dict:
    """Validator Agent (the same one the translation stage uses): mocked unit tests
    plus the full e2e xcvrd-tests oracle on the DUT.

    On failure the round is REVERTED rather than repaired. The change set is small by
    construction, so there is nothing worth salvaging, and repairing here would let a
    behaviour regression survive several rounds of edits before anyone noticed.
    """
    round_no = state["round"]
    doc = state["optimize"] or {}

    if _is_mock():
        passed = os.environ.get("RECODE_MOCK_OPT_FAIL", "") != str(round_no)
        report = {"round": round_no, "passed": passed, "tests": 105,
                  "failures": [] if passed else [{"test": "mock", "why": "scripted failure"}]}
        (_pipeline() / REPORT_JSON).write_text(json.dumps(report, indent=2), encoding="utf-8")
    else:
        # Remove the previous round's verdict first. If the Validator dies without
        # writing one, the read below must come back empty and fail the round --
        # inheriting last round's "passed": true would silently keep a regression.
        (_pipeline() / REPORT_JSON).unlink(missing_ok=True)
        prompt = (
            f"Validate optimisation round {round_no} of the working copy at {_crate()}.\n\n"
            f"The Optimizer made a performance change described in {_pipeline() / OPTIMIZE_JSON}; "
            "read it so you know what to scrutinise. This is a PERFORMANCE change that must not "
            "have altered behaviour, so run the FULL gate, not a subset:\n"
            "1. `bash tools/unit_test.sh` -- the mocked Rust unit tests.\n"
            "2. `bash tools/validate_on_dut.sh --all` -- the entire xcvrd-tests black-box suite "
            "on the DUT (no -k gate: a performance change can regress any behaviour, so the "
            "cumulative-milestone selection used during translation is not sufficient here).\n\n"
            f"Write the verdict to {_pipeline() / REPORT_JSON} as "
            '{"round","passed","tests","failures"}, where passed requires BOTH layers to pass. '
            "Each failure needs the test id and an actionable description of what regressed."
        )
        result = invoke_agent(
            "validator", prompt, cwd=ROOT,
            add_dirs=[str(XCVRD_TESTS)],
            log_dir=_pipeline() / "logs", extra_env=_crate_env(),
        )
        _log_agent(__tracer, stage=f"validate-opt-{round_no}", prompt=prompt, result=result)
        report = _read_json(REPORT_JSON)
        if not report:
            print(f"[optimize] round {round_no}: validator wrote no {REPORT_JSON} "
                  "-- treating the round as FAILED")

    passed = bool(report.get("passed"))
    reverted = False
    if not passed:
        reverted = _restore_crate()
        print(f"[optimize] round {round_no} FAILED validation; "
              f"{'reverted to the pre-round snapshot' if reverted else 'NO SNAPSHOT TO REVERT TO'}")

    entry = {
        "round": round_no,
        "title": doc.get("title", "?"),
        "files": doc.get("files", []),
        "rationale": doc.get("rationale", ""),
        "expected_effect": doc.get("expected_effect", ""),
        "passed": passed,
        "reverted": reverted,
        "failures": report.get("failures", [])[:5],
        "bench_before": (state["bench_history"] or [{}])[-1],
        "ts": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }
    hist = _append_history(entry)

    return state.update(round=round_no + 1, validated=passed, report=report,
                        history=hist, reverted=reverted, last_agent="validator")
