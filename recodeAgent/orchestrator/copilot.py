"""invoke_agent(): the boundary between the deterministic orchestrator and the
GitHub Copilot CLI agent runtime.

Every Burr action that needs LLM work shells out to `copilot -p ... --agent NAME`.
Copilot owns the entire agent loop (reasoning, tools, MCP, LSP, file edits);
this wrapper only launches it, captures structured JSONL output, and returns a
result. Real inter-stage STATE is passed via files in pipeline/ (see actions.py),
not by parsing agent chatter -- the parsed output is used for
success/failure detection and logging.

Verified against GitHub Copilot CLI 1.0.72 (`copilot --help`):
  -p/--prompt, --agent, --model, --effort/--reasoning-effort {none,minimal,low,
  medium,high,xhigh,max}, --allow-all (= --allow-all-tools --allow-all-paths
  --allow-all-urls; required for non-interactive autonomy incl. web fetch),
  --no-ask-user, --output-format {text,json(JSONL)}, --add-dir, --log-dir, --share.
Custom agents are discovered from ~/.copilot/agents/ (user level); ensure_agents_installed()
mirrors agents/*.agent.md there before each run.
"""
from __future__ import annotations

import json
import os
import platform
import shutil
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

# Canonical agent profiles live in agents/; the CLI discovers custom agents from
# ~/.copilot/agents/ (or COPILOT_HOME/agents). Mirror them there before running.
AGENTS_SRC = Path(__file__).resolve().parent.parent / "agents"


def ensure_agents_installed() -> list[str]:
    """Copy agents/*.agent.md into the Copilot user-level agents dir so
    `--agent NAME` resolves. Idempotent; returns the installed profile names."""
    dest = Path(os.environ.get("COPILOT_HOME", Path.home() / ".copilot")) / "agents"
    dest.mkdir(parents=True, exist_ok=True)
    installed = []
    for src in sorted(AGENTS_SRC.glob("*.agent.md")):
        (dest / src.name).write_text(src.read_text(encoding="utf-8"), encoding="utf-8")
        installed.append(src.stem.replace(".agent", ""))
    return installed


def _truncate(s: str, limit: int) -> str:
    s = s or ""
    return s if len(s) <= limit else s[:limit] + f"\n…[truncated {len(s) - limit} chars]"


def transcript_from_events(events: list, max_chars: int = 60000) -> str:
    """Render Copilot's JSONL events into a readable chat transcript for the UI.

    Captures, in order: the user prompt, each assistant message (reasoning +
    text), every tool call (name + intent + command) and its result, and a final
    usage line. Matches the CLI 1.0.72 event shapes (assistant.message /
    tool.execution_start / tool.execution_complete / result)."""
    lines: list[str] = []
    results: dict = {}
    # index tool results by call id so we can print them under their call
    for ev in events:
        if isinstance(ev, dict) and ev.get("type") == "tool.execution_complete":
            d = ev.get("data", {}) or {}
            res = (d.get("result") or {}).get("content") or ""
            results[d.get("toolCallId")] = (bool(d.get("success")), res)

    for ev in events:
        if not isinstance(ev, dict):
            continue
        etype = ev.get("type")
        d = ev.get("data", {}) or {}
        if etype == "user.message":
            txt = d.get("content") or d.get("text") or ""
            if txt.strip():
                lines.append(f"### 👤 user\n{txt.strip()}")
        elif etype == "assistant.message":
            reasoning = (d.get("reasoningText") or "").strip()
            content = (d.get("content") or "").strip()
            if reasoning:
                lines.append(f"### 🤖 assistant (thinking)\n{reasoning}")
            if content:
                lines.append(f"### 🤖 assistant\n{content}")
            for tr in d.get("toolRequests") or []:
                name = tr.get("name", "tool")
                intent = tr.get("intentionSummary") or ""
                args = tr.get("arguments") or {}
                cmd = args.get("command") or args.get("prompt") or args.get("path") or ""
                head = f"  ↳ 🔧 {name}" + (f": {intent}" if intent else "")
                lines.append(head)
                if cmd:
                    lines.append(f"     $ {_truncate(str(cmd), 800)}")
                ok, res = results.get(tr.get("toolCallId"), (None, ""))
                if res:
                    tag = "ok" if ok else "ERR"
                    lines.append(f"     ⤷ ({tag}) {_truncate(str(res), 800)}")
        elif etype == "result":
            u = ev.get("usage", {}) or {}
            cc = u.get("codeChanges", {}) or {}
            fm = cc.get("filesModified") or []
            lines.append(
                f"### ✅ result  exit={ev.get('exitCode')}  "
                f"files_modified={len(fm)} (+{cc.get('linesAdded', 0)}/-{cc.get('linesRemoved', 0)})  "
                f"premium_requests={u.get('premiumRequests')}  "
                f"duration={round((u.get('sessionDurationMs') or 0) / 1000, 1)}s"
            )
            if fm:
                lines.append("  changed: " + ", ".join(map(str, fm[:50])))
    return _truncate("\n\n".join(lines), max_chars)


def summary_from_events(events: list) -> dict:
    """Structured summary of a run for UI attributes: exit code, files changed,
    line deltas, premium requests, duration."""
    out = {"exit_code": None, "files_modified": [], "lines_added": 0,
           "lines_removed": 0, "premium_requests": None, "duration_s": None}
    for ev in events:
        if isinstance(ev, dict) and ev.get("type") == "result":
            u = ev.get("usage", {}) or {}
            cc = u.get("codeChanges", {}) or {}
            out.update(
                exit_code=ev.get("exitCode"),
                files_modified=cc.get("filesModified") or [],
                lines_added=cc.get("linesAdded", 0),
                lines_removed=cc.get("linesRemoved", 0),
                premium_requests=u.get("premiumRequests"),
                duration_s=round((u.get("sessionDurationMs") or 0) / 1000, 1),
            )
    return out


def _git_bash_dirs() -> list[str]:
    """On Windows, the DUT tools (validate_on_dut.sh, build_check.sh, unit_test.sh)
    must run under **Git Bash** + the Git/Windows ssh/scp/tar (which read the
    Windows ~/.ssh/config) -- NOT the WSL `bash` that usually wins on PATH. Return
    Git-for-Windows bin dirs to prepend to the agent's PATH so `bash tools/...`
    resolves to Git Bash. Empty on non-Windows (bash is already correct)."""
    if platform.system() != "Windows":
        return []
    roots: list[Path] = []
    git = shutil.which("git")
    if git:
        # ...\Git\cmd\git.exe -> ...\Git
        roots.append(Path(git).resolve().parent.parent)
    roots.append(Path(r"C:\Program Files\Git"))
    roots.append(Path(r"C:\Program Files (x86)\Git"))
    dirs: list[str] = []
    for root in roots:
        for sub in ("bin", os.path.join("usr", "bin")):
            d = root / sub
            if (d / "bash.exe").exists() and str(d) not in dirs:
                dirs.append(str(d))
    return dirs


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

    Copilot CLI 1.0.72 emits one JSON object per line shaped like
    {"type": "...", "data": {...}, "id", "timestamp", ...}. The assistant's text
    is `data.content` on a `type == "assistant.message"` event (streamed via
    `assistant.message_delta` -> `data.deltaContent`); the run ends with a
    `type == "result"` event carrying `exitCode`. We tolerate non-JSON lines and
    also fall back to legacy top-level {result,text,content,message} shapes.
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
        if not isinstance(obj, dict):
            continue
        etype = obj.get("type")
        data = obj.get("data") if isinstance(obj.get("data"), dict) else {}
        if etype == "assistant.message":
            content = data.get("content")
            if isinstance(content, str) and content.strip():
                final = content
        elif etype is None:
            # Legacy/alternate shapes: capture the most recent human-readable text.
            for key in ("result", "text", "content", "message"):
                val = obj.get(key)
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
    extra_env: dict | None = None,
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

    ensure_agents_installed()   # make sure agents/<name>.agent.md is discoverable
    cwd = str(cwd)
    cmd = [
        "copilot", "-p", prompt,
        "--agent", agent_name,
        "--model", model,
        "--reasoning-effort", effort,
        "--allow-all",                # tools + paths + urls: full non-interactive autonomy
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
    # Make the agent's shell use Git Bash (not the broken/foreign WSL bash) so the
    # DUT tool scripts run in the environment they were built for.
    gb = _git_bash_dirs()
    if gb:
        env["PATH"] = os.pathsep.join(gb) + os.pathsep + env.get("PATH", "")
    if extra_env:
        # e.g. RECODE_CRATE_DIR -> the pipeline working copy the tools should build.
        env.update({k: str(v) for k, v in extra_env.items()})

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
