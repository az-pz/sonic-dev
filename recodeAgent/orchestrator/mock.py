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
            "# (mock) source research\n\n- xcvrd tasks, platform bridge, STATE_DB schema\n",
            encoding="utf-8")
        return "mock analyzer: wrote analysis.md"

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
        # Decide pass/fail from RECODE_MOCK_FAIL + a per-milestone attempt counter.
        budget = _fail_budget().get(mid, 0)
        cnt_file = pdir / f".mock_attempts_{mid}"
        attempts = int(cnt_file.read_text()) if cnt_file.exists() else 0
        attempts += 1
        cnt_file.write_text(str(attempts))
        passed = attempts > budget
        report = {
            "milestone": mid,
            "passed": passed,
            "tests": {"total": 3, "passed": 3 if passed else 1, "failed": 0 if passed else 2},
            "failures": [] if passed else [f"(mock) {mid} attempt {attempts} <= budget {budget}"],
        }
        (pdir / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        return f"mock validator: {mid} {'PASS' if passed else 'FAIL'} (attempt {attempts})"

    return f"mock {agent_name}: ok"


def _extract_milestone(prompt: str) -> str:
    """Pull the milestone id (M0..M9) out of the prompt the action passed."""
    import re
    m = re.search(r"\bM([0-9])\b", prompt or "")
    return f"M{m.group(1)}" if m else "M0"
