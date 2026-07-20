"""invoke_agent(): the boundary between the deterministic orchestrator and the
GitHub Copilot CLI agent runtime.

Every Burr action that needs LLM work shells out to `copilot -p ... --agent NAME`.
Copilot owns the entire agent loop (reasoning, tools, MCP, LSP, file edits);
this wrapper only launches it, captures structured JSONL output, and returns a
result. Real inter-stage STATE is passed via files in pipeline/ (see actions.py),
not by parsing agent chatter -- the parsed output is used for
success/failure detection and logging.

Verified against GitHub Copilot CLI 1.0.67 (`copilot --help`):
  -p/--prompt, --agent, --model, --effort/--reasoning-effort {none..max},
  --allow-all-tools (required for non-interactive), --no-ask-user,
  --output-format {text,json(JSONL)}, --add-dir, -C <cwd>, --log-dir, --share.
"""
from __future__ import annotations

import json
import os
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path

# Opus 4.8, high reasoning, per project decision. NOTE: confirm the exact model
# identifier the installed CLI accepts before the first real run (see verify_cli()).
DEFAULT_MODEL = os.environ.get("RECODE_MODEL", "claude-opus-4.8")
DEFAULT_EFFORT = os.environ.get("RECODE_EFFORT", "high")

# When set, skip Copilot entirely and return a canned response so the Burr graph,
# transitions and crash-resume can be exercised offline (Phase-0 harness tests).
MOCK = os.environ.get("RECODE_MOCK") == "1"


@dataclass
class AgentResult:
    agent: str
    ok: bool
    returncode: int
    final_text: str            # last assistant message (best-effort from JSONL)
    duration_s: float
    stdout_path: str | None = None
    events: list = field(default_factory=list)


def _mock(agent_name: str, prompt: str) -> AgentResult:
    from .mock import respond  # lazy: only needed offline
    text = respond(agent_name, prompt)
    return AgentResult(agent=agent_name, ok=True, returncode=0,
                       final_text=text, duration_s=0.0, events=[])


def _parse_jsonl(raw: str) -> tuple[list, str]:
    """Return (events, final_assistant_text) from Copilot's --output-format json.

    JSONL = one JSON object per line. We tolerate non-JSON lines (they are kept
    out of `events`) and pick the last object that looks like an assistant/result
    message for `final_text`.
    """
    events: list = []
    final = ""
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        events.append(obj)
        # Heuristic: capture the most recent human-readable text field.
        for key in ("result", "text", "content", "message"):
            val = obj.get(key) if isinstance(obj, dict) else None
            if isinstance(val, str) and val.strip():
                final = val
    return events, final


def invoke_agent(
    agent_name: str,
    prompt: str,
    *,
    cwd: str | os.PathLike,
    model: str = DEFAULT_MODEL,
    effort: str = DEFAULT_EFFORT,
    add_dirs: list[str] | None = None,
    timeout: float | None = None,
    log_dir: str | os.PathLike | None = None,
    share_path: str | os.PathLike | None = None,
) -> AgentResult:
    """Run one Copilot custom agent non-interactively to completion.

    Args:
        agent_name: file name (without `.agent.md`) of a profile in agents/ or
            ~/.copilot/agents/ or .github/agents/.
        prompt: the task. Keep durable instructions in the .agent.md profile and
            per-run inputs here (paths to source/, plan.json, report.json, ...).
        cwd: working directory Copilot runs in (the agent sees files under it).
        add_dirs: extra directories to grant file access to (e.g. the xcvrd-tests
            path for read-only reference).
    Returns:
        AgentResult with ok/returncode/final_text and the parsed JSONL events.
    """
    if MOCK:
        return _mock(agent_name, prompt)

    cwd = str(cwd)
    cmd = [
        "copilot", "-p", prompt,
        "--agent", agent_name,
        "--model", model,
        "--reasoning-effort", effort,
        "--allow-all-tools",          # required for non-interactive autonomy
        "--no-ask-user",              # never block on a question
        "--output-format", "json",    # JSONL: machine-parseable
        "--no-color",
    ]
    for d in (add_dirs or []):
        cmd += ["--add-dir", str(d)]
    if log_dir:
        cmd += ["--log-dir", str(log_dir)]
    if share_path:
        cmd += ["--share", str(share_path)]

    env = dict(os.environ)
    # Auth precedence documented by the CLI: COPILOT_GITHUB_TOKEN > GH_TOKEN > GITHUB_TOKEN.
    # We do not inject a token here; the caller's environment must provide one.

    t0 = time.monotonic()
    proc = subprocess.run(
        cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout,
    )
    dt = time.monotonic() - t0

    events, final = _parse_jsonl(proc.stdout)
    if not final:
        final = (proc.stdout or proc.stderr or "").strip()

    stdout_path = None
    if log_dir:
        stdout_path = str(Path(log_dir) / f"{agent_name}.stdout.jsonl")
        try:
            Path(stdout_path).write_text(proc.stdout, encoding="utf-8")
        except OSError:
            stdout_path = None

    return AgentResult(
        agent=agent_name,
        ok=(proc.returncode == 0),
        returncode=proc.returncode,
        final_text=final,
        duration_s=dt,
        stdout_path=stdout_path,
        events=events,
    )


def verify_cli() -> dict:
    """Best-effort preflight: confirm the CLI exists and report its version.

    Does NOT verify the model id (that needs an authenticated call); the first
    real run will surface an invalid --model. Kept cheap for Phase-0 checks.
    """
    try:
        out = subprocess.run(["copilot", "--version"], capture_output=True,
                             text=True, timeout=30)
        return {"ok": out.returncode == 0, "version": out.stdout.strip()}
    except (OSError, subprocess.SubprocessError) as e:
        return {"ok": False, "error": repr(e)}
