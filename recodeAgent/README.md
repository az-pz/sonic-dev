# recodeAgent — multi-agent Python→Rust translation of xcvrd

A ReCodeAgent-style (arXiv:2604.07341) multi-agent pipeline that translates the
SONiC **xcvrd** transceiver daemon from Python to Rust, validating every step as
a **black box** against the existing `xcvrd-tests` suite on the `sonic-dev`
testbed.

The LLM work is done by **GitHub Copilot CLI custom agents** (Analyzer, Planner,
Translator, Validator). A small **Apache Burr** state machine is the only
deterministic code: it sequences the agents, owns the milestone × repair loop,
persists state (crash-resume), and renders the live graph UI. Burr never calls an
LLM — Copilot is the agent runtime.

> **Scope boundary:** everything here lives under `dev/recodeAgent/`. The pipeline
> *calls* `xcvrd-tests/run.sh` and drives the DUT over SSH, but never edits
> `xcvrd-tests/`, `platform/`, `emu-deploy/`, `xcvr-emu/`, or
> `setup-sonic-testbed.sh`.

---

## 1. Architecture

```
Apache Burr  (deterministic state machine + telemetry UI + SQLite resume)
  analyze ─▶ plan ─▶ select_milestone ─▶ translate ─▶ validate
                          ▲                   ▲            │
                          │ next milestone    │ repair     │ parse results.xml
                          │ (passed &         │ (failed &  ▼
                          │  more left)       │  iter<max) verdict
                          └───────────────────┴────────────┤
                                       all green ─▶ terminal
        │ every action = subprocess ▼                 │ validate action ▼
   GitHub Copilot CLI custom agents            tools/validate_on_dut.sh
   (Opus 4.8, high reasoning)                    build Rust (Debian-13 container)
   analyzer / planner / translator               ▶ inject into pmon (reversible)
   do ALL real work via their tools              ▶ xcvrd-tests/run.sh  (UNCHANGED)
   (read/edit/shell/LSP/MCP)                     ▶ parse results.xml ▶ restore py xcvrd
```

**Division of labor**

| Layer | Owner | Responsibility |
|-------|-------|----------------|
| Sequencing, milestone loop, repair loop, typed state, resume, UI | **Burr** (this repo) | ~200 lines of deterministic Python |
| Analysis, planning, translation, repair diagnosis | **Copilot agents** | all the LLM reasoning + code writing |
| Black-box verdict | **`xcvrd-tests/run.sh`** (unchanged) | trusted oracle; `results.xml` is the structured result |
| HAL + STATE_DB + build/deploy | **environment scaffolding** (this repo) | provided so agents don't fight interop (see §3) |

---

## 2. How we adapt the paper (deliberately)

Faithful to ReCodeAgent's four agents and Algorithm 1 loop, with two adaptations
for our environment:

1. **Validator = black-box test runner, not test translator.** The paper's
   Validator translates the developer tests, runs them, does coverage-gap
   analysis, and *generates* tests. We already have a trusted, human-validated
   suite (`xcvrd-tests`), so our Validator instead **deploys the candidate Rust
   daemon to the DUT and runs that suite unchanged.** This sidesteps the paper's
   biggest risk (assertion relaxation / test-translation coupling): the oracle
   cannot be gamed by the Translator.

2. **Plan = prioritized functionality milestones.** Instead of "Part A (source) +
   Part B (tests)", the plan is an ordered list of xcvrd functionality slices,
   each gated by a specific `xcvrd-tests` subset (§5). "Coverage" = which test
   groups pass. A milestone must go green before the next begins.

Everything else — Analyzer, Planner, skeleton-first, name mapping, the
translate→validate→repair loop with `maxIter` — is the paper's design.

---

## 3. Environment scaffolding (the key design decision)

The paper's Analyzer picks "idiomatic target-language counterparts" for each
source dependency. We **pin** two of them and pre-build them so the agents never
have to reinvent fragile interop:

### 3a. HAL = the existing Python platform via PyO3 (`crate/platform-bridge`)

The Rust xcvrd talks to `xcvr-emu` through the **exact Python `sonic_platform`
plugin we run today** (`dev/platform/sonic_platform/`), via **PyO3**. We do NOT
re-implement the CMIS/SFF decode stack in Rust.

Why: `sonic_platform.Sfp` subclasses `SfpOptoeBase`, so the Python platform
already provides the *entire* transceiver API on top of three raw hooks:

```
Sfp(SfpOptoeBase)
  raw hooks     : read_eeprom / write_eeprom / get_presence   (→ emulator gRPC)
  derived (free): get_transceiver_info(), get_transceiver_dom_real_value(),
                  get_transceiver_status(), get_xcvr_api() [CMIS mgmt], ...
Chassis(ChassisBase)
                : get_num_sfps(), get_sfp(i), get_change_event(timeout)
```

**`platform-bridge` is a PyO3 crate (built by us) that exposes this high-level
API to Rust as clean typed structs.** So:

- **What stays in Python (behind the bridge):** all CMIS/SFF parsing — "the exact
  platform we have now."
- **What the agents translate into Rust:** the xcvrd **daemon logic** — the task
  loops (`SfpStateUpdateTask`, `DomInfoUpdateTask`, `CmisManagerTask`
  orchestration), polling cadence, state-update decisions, and the STATE_DB
  schema writes.

> ⚠️ **Decision to confirm:** this is a *thick* HAL boundary (Rust calls
> `get_transceiver_info()` etc. via PyO3). The alternative "thin" boundary (Rust
> re-implements CMIS decode on top of only read/write/presence) is a far larger,
> riskier translation surface. We chose thick to match "use the exact platform we
> have now" and to keep the translation scoped to the *daemon*.

### 3b. STATE_DB = official `swss-common` Rust crate

The Rust daemon reads/writes Redis STATE_DB through the upstream Rust bindings at
[`sonic-net/sonic-swss-common` `crates/swss-common`](https://github.com/sonic-net/sonic-swss-common/tree/master/crates/swss-common/src),
not a hand-rolled client. Provided as a pinned dependency in the crate skeleton.

### 3c. Build/deploy targeting pmon

pmon is **Debian 13, glibc 2.41, Python 3.13.5**. Neither the Windows dev box nor
the `sonic-dev` host can produce a matching binary directly. So
`tools/validate_on_dut.sh` builds inside a **Debian-13 container with Rust +
python3.13-dev** (PyO3 links libpython3.13), then `docker cp`s the binary into
pmon and swaps it in via supervisor — **reversible**, restoring the Python xcvrd
after every run (same inject/restore pattern as `xcvrd-tests/tools/inject_dummy_xcvrd.sh`).

---

## 4. Directory layout

```
dev/recodeAgent/
├── README.md                     # this file (living design doc)
├── pyproject.toml                # orchestrator deps (apache-burr)
├── orchestrator/                 # the small deterministic Burr layer
│   ├── app.py                    #   ApplicationBuilder: actions + transitions + persister
│   ├── state.py                  #   typed state helpers
│   ├── actions.py                #   @action: analyze/plan/select_milestone/translate/validate
│   ├── copilot.py                #   invoke_agent(): subprocess wrapper around `copilot`
│   └── milestones.py             #   the M0..M6 matrix (§5)
├── agents/                       # Copilot CLI personas (paper §3.2–3.5), Opus 4.8
│   ├── analyzer.agent.md  planner.agent.md  translator.agent.md  validator.agent.md
├── tools/
│   ├── validate_on_dut.sh        # build (Debian-13) ▶ inject ▶ run.sh ▶ results.xml ▶ restore
│   ├── build_rust.sh             # the containerized build step
│   └── recode_mcp_server.py      # optional PA tools (get_file_structure, ...) over MCP
├── crate/                        # the Rust workspace (build target = pmon)
│   ├── Cargo.toml                #   workspace: xcvrd-rs + platform-bridge
│   ├── xcvrd-rs/                 #   the daemon — agents translate logic into here
│   └── platform-bridge/          #   PyO3 wrappers around sonic_platform (we build this)
├── source/xcvrd/                 # INPUT: the Python xcvrd source (pulled from pmon)
└── pipeline/                     # runtime hand-off: analysis.md, plan.json, report.json (gitignored)
```

Inter-stage state is passed as **files in `pipeline/`** (each agent ends its run
by writing a parseable artifact there — `analysis.md`, `plan.json`,
`report.json`). Copilot CLI 1.0.67 also supports `--output-format json` (JSONL),
which the orchestrator parses for success/failure detection and logging; the
file artifacts remain the authoritative state channel.

---

## 5. Milestone matrix (the incremental plan)

Each milestone is a slice of the daemon. Its gate is **CUMULATIVE**: a milestone
must pass its own new tests **and every earlier milestone's tests** (regression
safety — new work can't break earlier functionality). `orchestrator/milestones.py`
is the single source of truth; `python -m orchestrator.milestones --args M3`
prints the resolved gate, and `tools/validate_on_dut.sh M3` runs it automatically.

Fast-subset-first: M1–M5 run `-m "not slow"`; M6 drops the filter (full suite).
Selection uses a pytest **`-k` module expression** (not file paths): `run.sh`
always runs `pytest <tests-dir> …`, so a `-k "test_presence or test_info_content"`
narrows the already-collected dir to exactly the intended modules.

| #  | Milestone (adds) | Cumulative gate (this + all earlier) |
|----|------------------|--------------------------------------|
| M0 | **Skeleton** (compiles, injects, RUNNING) | *deploy-smoke* — supervisor RUNNING (no pytest) |
| M1 | **Presence + identity** | `-k "test_presence or test_info_content"` `-m "not slow"` |
| M2 | **DOM** | + `test_dom or test_interaction_trace` |
| M3 | **Status / CMIS / errors** | + `test_status_error` |
| M4 | **lpmode / reset** | + `test_lpmode_reset` |
| M5 | **Multiport concurrency** | + `test_multiport` |
| M6 | **Golden conformance** | + `test_golden`, **no marker** (full suite incl. slow) |

So M3's gate is `-k "test_presence or test_info_content or test_dom or
test_interaction_trace or test_status_error" -m "not slow"`; M6 selects all eight
modules with no marker. Verified: M1 → `29 items / 18 deselected / 11 selected`.

---

## 6. Status

**Deterministic core (Burr): proven.** analyze→plan→(translate↔validate)×milestone
with SQLite persistence; happy path, repair loop, budget exhaustion, and
cross-process crash-resume all validated offline via the mock.

**DUT validation harness (`tools/validate_on_dut.sh`): proven end-to-end.** Builds
the crate in a Debian-13 (trixie) container on the sonic-dev host, ships the
binary through the mgmt→vlab-01 chain, **crash-safely** injects it into pmon (a
Python shim `execv`s the Rust binary; backup-verified + atomic shim write so an
ENOSPC/partial write can never truncate xcvrd), runs `xcvrd-tests/run.sh`, parses
`results.xml`→`report.json`, and **always restores** the Python xcvrd.

- **M0** is a *deploy-smoke* gate (inject + supervisor RUNNING; no pytest), since
  the suite's clean-baseline requires `TRANSCEIVER_INFO` repopulation and thus no
  pytest passes on a bare skeleton. Proven: skeleton → `passed: true`.
- **Fail path** proven: a no-op binary against a real pytest gate → `passed: false`.

> **DUT disk note:** vlab-01's 16G root filled to 0 during Phase-0 work. Root
> cause: an unbounded docker **container json-log** (one container had an 8.3GB
> `*-json.log`; docker doesn't rotate these by default and `docker system df`
> doesn't even count them). Fix/lever (reclaimed 8.3GB, testbed unaffected):
> `sudo find /var/lib/docker/containers -name '*-json.log' -exec truncate -s 0 {} +`.
> The crash-safe inject also means a future ENOSPC can no longer corrupt xcvrd.


Remaining Phase 0: the `platform-bridge` (PyO3) + `crate/xcvrd-rs` real skeleton,
the four `.agent.md` profiles, and pulling the Python xcvrd source into `source/`.

---

## 7. Running the checks yourself

Two things are runnable today. Neither needs a Copilot token (the orchestrator
check uses the offline mock; the DUT harness exercises build/inject/test/restore).

### A. Deterministic orchestrator (offline, ~30s, no DUT)

From **Git Bash**, in `dev/recodeAgent/`:

```bash
bash tools/check.sh
```

Runs four scenarios against the mock agent and prints a summary. Expected:

| # | Scenario | Look for |
|---|----------|----------|
| 1 | Happy path | `done=True milestone_idx=6`, M0..M6 all `passed=True` |
| 2 | Repair loop | `M1 iter=1 passed=False` then `M1 iter=2 passed=True` |
| 3 | Budget exhaustion (`--max-iter 3`) | `done=False milestone_idx=2` (gave up on M2) |
| 4 | Crash-resume | process 2 prints `loaded state ... milestone_idx=3` = **resumed, not restarted** |

Run a single scenario manually:

```bash
export RECODE_MOCK=1 RECODE_PIPELINE_DIR="$PWD/pipeline"
python -m orchestrator.app --app-id my-run --mock          # happy path
python -m orchestrator.app --app-id my-run --mock          # re-run SAME id => resumes/continues
```

### B. DUT validation harness (real build+inject+test+restore, ~1-2 min)

Needs `ssh sonic-dev` reachable (it is on this box). From **Git Bash** (the
harness uses bash/ssh/scp/tar — do NOT use `& bash -lc` from PowerShell, it opens
an interactive shell):

```bash
cd /c/Users/t-fhabibi/Desktop/toRust/dev/recodeAgent
bash tools/validate_on_dut.sh M0                       # deploy-smoke: skeleton must be RUNNING
bash tools/validate_on_dut.sh M1 tests/test_presence.py -m "not slow"   # a real pytest gate
```

It prints the verdict and writes `pipeline/report.json`:

```json
{ "milestone": "M0", "passed": true, "tests": {"total":1,"passed":1,"failed":0}, "failures": [] }
```

M0 passes on the current no-op skeleton (proves the harness). M1+ will fail until
the daemon logic is translated — that's expected. The harness ALWAYS restores the
Python xcvrd afterward; confirm the testbed is healthy any time with:

```powershell
ssh sonic-dev "docker exec mgmt bash -lc 'sshpass -p password ssh -o StrictHostKeyChecking=no admin@10.250.0.101 \"docker exec pmon supervisorctl status xcvrd\"'"
```

### The state machine (what the Burr graph encodes)

```
        analyze ─▶ plan ─▶ select_milestone ─▶ translate ─▶ validate
                               ▲                                 │
   (milestone_passed &         │                                 │
    milestone_idx < last_idx)  └──────────────── select_milestone┤ pass -> advance
                                                                  │
             translate ◀──────────────────────────────────────── ┤ (not passed &
                 (repair the same milestone)                      │  iter < max_iter)
                                                                  │
                                              terminal ◀───────── ┘ default
                                       (last milestone passed, or budget exhausted)
```

### Burr telemetry UI (optional)

Traces ARE captured to `~/.burr/recodeagent-xcvrd/` on every run. The interactive
UI server (`burr`) needs `apache-burr[start]` (pyarrow), which has **no wheel for
this box's Python 3.12 ARM64** — so the live UI is unavailable here. To use it,
run on a Linux/x64 host: `pip install "apache-burr[start]"` then `burr`, and open
the printed URL (project `recodeagent-xcvrd`).


