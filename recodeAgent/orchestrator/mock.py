"""Offline mock for invoke_agent(): drive the Burr graph + crash-resume WITHOUT
Copilot. Enabled with RECODE_MOCK=1.

The mock writes the same pipeline/ artifacts a real agent would, so the
file-based state hand-off (actions.py) works unchanged. Validator outcomes are
scriptable so the repair loop and milestone advancement can be exercised:

  RECODE_MOCK_FAIL   comma-separated "milestone:attempts" pairs that should FAIL
                     the first N validate attempts, e.g. "M1:1,M3:2" -> M1 fails
                     once then passes, M3 fails twice then passes. Default: all pass.
"""
from __future__ import annotations

import json
import os
from pathlib import Path


def _pipeline_dir() -> Path:
    d = Path(os.environ.get("RECODE_PIPELINE_DIR", "pipeline"))
    d.mkdir(parents=True, exist_ok=True)
    return d


def _fail_budget() -> dict[str, int]:
    out: dict[str, int] = {}
    for pair in os.environ.get("RECODE_MOCK_FAIL", "").split(","):
        pair = pair.strip()
        if not pair or ":" not in pair:
            continue
        mid, n = pair.split(":", 1)
        try:
            out[mid.strip()] = int(n)
        except ValueError:
            continue
    return out


def respond(agent_name: str, prompt: str) -> str:
    pdir = _pipeline_dir()

    if agent_name == "analyzer":
        (pdir / "analysis.md").write_text(
            "# (mock) source research\n\n- xcvrd tasks, platform bridge, STATE_DB schema\n"
            "\n## module inventory\n- xcvrd.py\n- dom/dom_mgr.py\n- cmis/cmis_manager_task.py\n",
            encoding="utf-8")
        return "mock analyzer: wrote analysis.md"

    if agent_name == "scoper":
        from . import milestones as M
        if os.environ.get("RECODE_CRASH_AT") == "SCOPE":
            raise RuntimeError("(mock) simulated crash at SCOPE")
        if not M.artifact_path().exists():
            # first pass: write the bootstrap milestone set (M0..M6)
            M.save(list(M.DEFAULT_MILESTONES))
            return "mock scoper: wrote milestones.json (bootstrap set)"
        # re-scope: append one unit-only parity milestone per gap
        rep = {}
        prep = pdir / "parity_report.json"
        if prep.exists():
            try:
                rep = json.loads(prep.read_text(encoding="utf-8"))
            except ValueError:
                rep = {}
        gaps = rep.get("gaps") or [{"source_ref": "(mock)", "functionality": "gap"}]
        ms = M.load()
        base = len(ms)
        for i, g in enumerate(gaps):
            nid = f"M{base + i}"
            ms.append(M.Milestone(
                nid, f"(parity) {g.get('functionality', 'gap')}",
                f"Translate {g.get('source_ref', '?')}",
                test_modules=[], origin="parity", unit_only=True,
                source_refs=[g.get("source_ref", "?")],
            ))
        M.save(ms)
        return f"mock scoper: appended {len(ms) - base} parity milestone(s)"

    if agent_name == "planner":
        (pdir / "plan.json").write_text(json.dumps({
            "name_mapping": {},
            "skeleton": "crate/xcvrd-rs",
            "milestones": ["M0", "M1", "M2", "M3", "M4", "M5", "M6"],
        }, indent=2), encoding="utf-8")
        return "mock planner: wrote plan.json"

    if agent_name == "translator":
        (pdir / "translate.marker").write_text("mock translated\n", encoding="utf-8")
        return "mock translator: filled skeleton"

    if agent_name == "validator":
        # Crash hook (resume testing): abort before writing the verdict so the
        # persisted state is "mid-milestone", exercising crash-resume.
        mid = _extract_milestone(prompt)
        if os.environ.get("RECODE_CRASH_AT") == mid:
            raise RuntimeError(f"(mock) simulated crash at {mid}")
        # Is this a "retry deferred tests" milestone? (origin=retry in milestones.json)
        retry_refs: list[str] = []
        is_retry = False
        try:
            from . import milestones as M
            mobj = M.by_id(mid)
            is_retry = (mobj.origin == "retry")
            retry_refs = list(mobj.source_refs)
        except Exception:
            pass
        # Decide pass/fail from RECODE_MOCK_FAIL + a per-milestone attempt counter.
        # RECODE_MOCK_RETRY_FAIL=1 forces retry milestones to always fail (permanent skip).
        budget = _fail_budget().get(mid, 0)
        if is_retry and os.environ.get("RECODE_MOCK_RETRY_FAIL") == "1":
            budget = 10 ** 9
        cnt_file = pdir / f".mock_attempts_{mid}"
        attempts = int(cnt_file.read_text()) if cnt_file.exists() else 0
        attempts += 1
        cnt_file.write_text(str(attempts))
        passed = attempts > budget
        # On a retry milestone, the failing tests are the re-enabled ones (source_refs)
        # -- mirroring the validator reporting the actual deferred tests, not a synthetic id.
        if passed:
            failures = []
        elif is_retry and retry_refs:
            failures = [{"layer": "e2e", "test": t,
                         "symptom": f"(mock) retry {mid} attempt {attempts} still failing",
                         "repair_hint": "mock"} for t in retry_refs]
        else:
            failures = [{"layer": "e2e",
                         "test": f"tests/test_mock_{mid}.py::test_mock_{mid}_behavior",
                         "symptom": f"(mock) {mid} attempt {attempts} <= budget {budget}",
                         "repair_hint": "mock"}]
        report = {
            "milestone": mid,
            "passed": passed,
            "tests": {"total": 3, "passed": 3 if passed else 1, "failed": 0 if passed else 2},
            "failures": failures,
        }
        (pdir / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        return f"mock validator: {mid} {'PASS' if passed else 'FAIL'} (attempt {attempts})"

    if agent_name == "parity_verifier":
        # Crash hook (outer-loop resume testing).
        if os.environ.get("RECODE_CRASH_AT") == "PARITY":
            raise RuntimeError("(mock) simulated crash at PARITY")
        # Report gaps for the first RECODE_MOCK_PARITY_GAPS rounds, then COMPLETE.
        gaps_budget = int(os.environ.get("RECODE_MOCK_PARITY_GAPS", "0") or 0)
        cnt_file = pdir / ".mock_parity_attempts"
        attempts = int(cnt_file.read_text()) if cnt_file.exists() else 0
        attempts += 1
        cnt_file.write_text(str(attempts))
        complete = attempts > gaps_budget
        report = {
            "coverage_matrix": [
                {"module": "xcvrd", "covered": complete,
                 "missing": [] if complete else [f"mock_fn_{attempts}"]},
            ],
            "gaps": [] if complete else [{
                "source_ref": f"xcvrd.py:mock_fn_{attempts}",
                "functionality": f"mock gap {attempts}",
                "suggested_milestone": "unit-only",
            }],
            "complete": complete,
        }
        (pdir / "parity_report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        return f"mock parity: {'COMPLETE' if complete else 'GAPS'} (attempt {attempts})"

    return f"mock {agent_name}: ok"


def _extract_milestone(prompt: str) -> str:
    """Pull the milestone id (M0, M1, ... M12, ...) out of the prompt the action passed."""
    import re
    m = re.search(r"\bM(\d+)\b", prompt or "")
    return f"M{m.group(1)}" if m else "M0"
