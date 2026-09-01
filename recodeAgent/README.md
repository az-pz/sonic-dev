# recodeAgent — multi-agent Python→Rust translation of xcvrd

A ReCodeAgent-style (arXiv:2604.07341) multi-agent pipeline that translates the
SONiC **xcvrd** transceiver daemon from Python to Rust, validating every step as a
**black box** against the existing `xcvrd-tests` suite on the `sonic-dev` testbed,
then optionally optimising the result.

The LLM work is done by **GitHub Copilot CLI custom agents**. A small **Apache
Burr** state machine is the only deterministic code: it sequences the agents, owns
the milestone × repair loop, persists state (crash-resume), and renders a live
graph UI. Burr never calls an LLM — Copilot is the agent runtime.

> **Scope boundary:** everything here lives under `dev/recodeAgent/`. The pipeline
> *calls* `xcvrd-tests/run.sh` and drives the DUT over SSH, but never edits
> `xcvrd-tests/`, `platform/`, `emu-deploy/`, `xcvr-emu/`, or
> `setup-sonic-testbed.sh`.

---

## 0. Quickstart

From **Git Bash** in `dev/recodeAgent/` (Python ≥ 3.11):

```bash
# 1. Install the orchestrator (Apache Burr) — once
pip install -e .            # '.[tracking]' for telemetry, '.[ui]' for the UI
                            # (without [tracking] the run still works; the tracker auto-disables)

# 2. Install the Copilot custom-agent profiles into $COPILOT_HOME/agents
bash tools/install_agents.sh        # copilot.py also auto-installs before each run

# 3a. Offline dry-run — mock agents, no Copilot/DUT (proves the graph wiring)
python -m orchestrator.app --app-id demo --mock

# 3b. Real run — drives the actual LLM agents (needs `copilot login` + AI credits)
python -m orchestrator.app --app-id run1

# 3c. ...and then make it faster (see §6)
python -m orchestrator.app --app-id run1 --optimize
python -m orchestrator.app --app-id run1 --optimize --benchmarks B4,B9   # focused
```

Also installed as a console script: `recode --app-id run1`.

`--app-id` is the resume key: re-running the **same** id continues from the last
persisted node; use a fresh id to start over.

| Flag | Meaning |
|---|---|
| `--max-iter N` | per-milestone repair budget (default 10) |
| `--max-parity-rounds N` | outer parity budget (default 3) |
| `--optimize` | run the optimize phase after parity (default 5 rounds; §6) |
| `--max-opt-rounds N` | optimisation round count; implies `--optimize`, `0` disables |
| `--benchmarks IDS` | focus the phase on these scenarios only, e.g. `B4,B9` (default: all) |
| `--mock` | offline, no Copilot or DUT |
| `--pipeline-dir PATH` | artifact directory (default `./pipeline`) |
| `--db PATH` | state file (default `<pipeline-dir>/burr.db`) |
| `--start-milestone Mx` / `--start-parity` / `--start-benchmark` | enter partway through |

### Start partway through, from an existing pipeline folder

For when `analysis.md`, `milestones.json`, `plan.json`, and the working copy
`crate/xcvrd-rs/` already exist and you want a **new** run to begin later in the
graph. All three flags are mutually exclusive, validate those artifacts up front,
and fail fast if any are missing.

```bash
# at a chosen milestone — skips analyze/scope/plan
python -m orchestrator.app --pipeline-dir PATH --start-milestone M3 --app-id from-m3

# at the Parity Verifier — also skips the whole milestone loop
python -m orchestrator.app --pipeline-dir PATH --start-parity --app-id parity-only

# at the optimize phase — also skips parity
python -m orchestrator.app --pipeline-dir PATH --start-benchmark --max-opt-rounds 5 \
    --app-id optimise-only
```

`--start-parity` grades the translation as it stands; the outer loop still works
from there, so it is the quick way to re-check coverage after a manual fix.

`--start-benchmark` **asserts** the translation is already complete and correct —
`parity_complete` is set from the flag, not re-derived. Point it at an unfinished
translation and it will optimise code that is still going to change; only the
appended full-suite milestone would eventually catch that.

**Use a fresh `--app-id` to force the requested start.** If the id already exists
in the selected DB, Burr's crash-resume state wins and these flags do not rewind
it. `RECODE_PIPELINE_DIR` works instead of `--pipeline-dir`.

---

## 1. Architecture

```
Apache Burr  (deterministic state machine + telemetry UI + SQLite resume)

  analyze ─▶ scope ─▶ plan ─▶ select_milestone ─▶ translate ─▶ validate
              ▲                    ▲                              │
   re-scope   │      next milestone│      repair (failed &        │ parse results.xml
   (parity    │      (concluded &  │      iter < max_iter)        ▼
    gaps)     │       more left)   └──────────────────────────  verdict
              │                                                   │
              │                        all milestones concluded ─▶ parity_verify
              └───────────── gaps + budget left ──────────────────┤
                                                                  │ complete
                                          ┌───────────────────────┤
                        (no --optimize) ──┴──▶ terminal           │
                                                                  ▼
                                        benchmark ⇄ optimize  (N rounds)
                                                     │
                                                     ▼
                                                 opt_repair
                                          (appends a full-suite milestone)
                                                     │
                                                     ▼
                                    select_milestone ▶ translate ⇄ validate ▶ terminal

        │ every action = subprocess ▼                 │ validate action ▼
   GitHub Copilot CLI custom agents            tools/validate_on_dut.sh
   (claude-opus-4.8, high/max effort)            build Rust (Debian-13 container)
   analyzer / scoper / planner / translator      ▶ inject into pmon (reversible)
   validator / parity_verifier                   ▶ xcvrd-tests/run.sh  (UNCHANGED)
   benchmarker / optimizer                       ▶ parse results.xml ▶ restore py xcvrd
```

Three loops:

- **inner** — milestone × repair (correctness, gated by the e2e oracle);
- **outer** — parity (completeness: is anything untranslated?);
- **optimize** — benchmark ⇄ optimize, closed by one full-suite milestone (§6).

The **Scoper** derives the milestone set from the analysis + the `xcvrd-tests`
suite; the **Parity Verifier** compares the finished Rust against the Python
source per module and feeds gaps back to the Scoper as new unit-only milestones.
There is **no deferral at the outer level** — the run succeeds only when parity is
complete; exhausting the outer budget with gaps open is a hard failure.

**Division of labour**

| Layer | Owner | Responsibility |
|-------|-------|----------------|
| Sequencing, the three loops, typed state, resume, UI | **Burr** (this repo) | deterministic Python, no LLM calls |
| Analysis, scoping, planning, translation, repair, parity, optimisation | **Copilot agents** | all reasoning + code writing |
| Black-box verdict | **`xcvrd-tests/run.sh`** (unchanged) | trusted oracle; `results.xml` is the structured result |
| HAL + STATE_DB + build/deploy | **environment scaffolding** (this repo) | provided so agents don't fight interop (§3) |

---

## 2. How we adapt the paper (deliberately)

Faithful to ReCodeAgent's agents and Algorithm 1 loop, extended with agents of our
own (**Scoper**, **Parity Verifier**, and the optimize pair) plus these
adaptations:

1. **Two validation layers — unit tests (Part B) *and* a fixed black-box oracle.**
   We keep the paper's Part B: the Translator rewrites xcvrd's Python **behavioral
   unit tests** (`source/xcvrd/tests/test_xcvrd.py`) into Rust and adds new ones,
   running them against **mocks** of the HAL and STATE_DB (mirroring the Python
   `mock_platform.py` / `mock_swsscommon.py`) via `cargo test` — fast, no DUT. On
   top of that, the end-to-end `xcvrd-tests` are an *additional, authoritative*
   oracle, run **unchanged** (never translated or generated) so it cannot be gamed.
   A milestone passes only when **both** layers pass.

2. **Plan = prioritized functionality milestones, owned by the Scoper.** The
   milestone set is not hand-authored: the **Scoper** (after `analyze`, before
   `plan`) partitions the daemon's functionality into an ordered set and writes
   `pipeline/milestones.json`, mapping **every** `xcvrd-tests` module onto exactly
   one milestone and ending on a golden/full-suite gate.

3. **Immutable input, mutable working copy.** `crate/` (the M1 bootstrap +
   scaffolding) is read-only input; the Planner copies it to `pipeline/crate/` and
   all translation happens there.

### 2a. Parity loop (completeness, not just correctness)

Passing every milestone proves the translation is correct *against the tests* —
but tests can miss behaviour. After all milestones are green the **Parity
Verifier** compares the Python source against the final Rust per module and writes
`pipeline/parity_report.json` (`coverage_matrix`, `gaps`, `complete`):

- `complete: true` → the pipeline succeeds (or proceeds to optimize, §6).
- `complete: false` with budget left → gaps flow back to the **Scoper**, which
  **appends** `origin="parity", unit_only=true` milestones (fresh ids, never
  renumbering passed ones). They add no new e2e test but inherit the full
  cumulative gate and are verified by new Rust unit tests.
- budget exhausted with gaps open → **hard failure** (`done=False`).

**Inner give-up (skip, don't fail).** Within a milestone the repair loop runs up to
`--max-iter` times. If it still can't go green the run does **not** stop: the
milestone is skipped (recorded in `skipped[]`, flagged `gave_up`), its still-failing
e2e tests are recorded in `pipeline/skips.json` and deselected from every later
milestone's cumulative gate — so they can't drag each one back to `max_iter` — and
the loop advances.

**Deferred-test retry (one shot, then permanent).** The Parity Verifier revisits
`skips.json`. For any skipped test that hasn't had a retry it appends **one**
dedicated retry milestone (`origin="retry"`) that re-enables those tests:

- retry **passes** → the tests stay un-deferred; the fix stuck.
- retry **gives up** → they return to `tests_to_skip`, are marked `retried`, and are
  **skipped permanently**.

So a stubborn test gets exactly one focused second chance, then stops burning
budget. Its untranslated source still shows up as a parity gap until coverage is
complete or the outer budget is spent.

---

## 3. Environment scaffolding (the key design decision)

The paper's Analyzer picks "idiomatic target-language counterparts" for each source
dependency. We **pin** two of them and pre-build them so the agents never have to
reinvent fragile interop.

### 3a. HAL = the existing Python platform via PyO3 (`crate/platform-bridge`)

The Rust xcvrd talks to `xcvr-emu` through the **exact Python `sonic_platform`
plugin we run today** (pulled to `source/sonic_platform/`; on pmon at
`/usr/local/lib/python3.13/dist-packages/sonic_platform/`), via **PyO3**. We do NOT
re-implement the CMIS/SFF decode stack in Rust.

Why: `sonic_platform.Sfp` subclasses `SfpOptoeBase`, so the Python platform already
provides the *entire* transceiver API on top of three raw hooks:

```
Sfp(SfpOptoeBase)
  raw hooks     : read_eeprom / write_eeprom / get_presence   (→ emulator gRPC)
  derived (free): get_transceiver_info(), get_transceiver_dom_real_value(),
                  get_transceiver_status(), set_lpmode()/reset() [CMIS mgmt], ...
Chassis(ChassisBase)
                : get_num_sfps(), get_sfp(i), get_change_event(timeout)
```

**`platform-bridge` is a PyO3 crate that embeds CPython, imports the real plugin,
and exposes this high-level API to Rust.** So:

- **Stays in Python (behind the bridge):** all CMIS/SFF parsing.
- **Translated into Rust by the agents:** the xcvrd **daemon logic** — the task
  loops (`SfpStateUpdateTask`, `DomInfoUpdateTask`, `CmisManagerTask`), polling
  cadence, state-update decisions, and the STATE_DB schema writes.

This *thick* boundary beats a thin one (Rust re-implementing CMIS decode on
read/write/presence): a far smaller, safer translation surface, and it matches "use
the exact platform we have now".

**Exposed surface** (`platform_bridge::{Platform, Chassis, Sfp, ChangeEvent}`):
`Platform::new()` → `num_sfps()`, `sfp(i)`, `get_change_event(timeout_ms)`; per-SFP
`get_presence()/is_replaceable()/get_reset_status()/sfp_type()/get_error_description()`
(typed scalars), `get_transceiver_info()/_dom_real_value()/_status()/_threshold_info()`
(as `serde_json::Value`, so the surface is stable as milestones add fields),
`get_lpmode()/set_lpmode()/reset()`, `read_eeprom()/write_eeprom()`, and a generic
`call_json(method, args)` escape hatch. Complex dicts are marshalled via
`json.dumps(…, default=str)`; NUL-padded CMIS strings are returned verbatim (the
daemon logic strips them, exactly like the Python original).

Verify the boundary independently of the daemon with `bash tools/bridge_smoke.sh`
(builds in the trixie container, runs inside pmon against the live emulator, cleans
up): expect `num_sfps = 33`, CMIS-decoded identity per module, `bridge-smoke: OK`.

### 3b. STATE_DB = official `swss-common` Rust crate

The daemon reads/writes Redis STATE_DB through the upstream Rust bindings at
[`sonic-net/sonic-swss-common` `crates/swss-common`](https://github.com/sonic-net/sonic-swss-common/tree/master/crates/swss-common/src),
not a hand-rolled client — a **pinned git dependency** exposing `DbConnector`,
`Table`, `SonicV2Connector`, `ProducerStateTable`, `SubscriberStateTable`, etc.

**Native-lib wiring (the tricky part, solved once):** `swss-common`'s `build.rs` runs
**bindgen** over the C-API headers and links `dylib=swsscommon`. pmon ships
`libswsscommon.so.0` with the C-API compiled in, so a Rust binary loads and runs
there. The build container (`Dockerfile.build`) bakes what bindgen needs —
`clang`/`libclang-dev`, the pinned c-api headers, and `BINDGEN_EXTRA_CLANG_ARGS=-x c`
so bindgen parses only the C boundary (no boost/hiredis/C++ headers). The link
library itself is pmon-specific, so `tools/dut/ensure_swsslib.sh` pulls it from the
live pmon and mounts it at build time. **Keep `Dockerfile.build`'s `SWSS_COMMON_REV`
in sync with the Cargo rev.**

`bash tools/env_check.sh` proves both libraries compile, link and run together: a
`statedb_probe` example round-trips a STATE_DB hash, and `hal_to_statedb` reads a
transceiver via the bridge and publishes it — the exact `SfpStateUpdateTask`
read→publish pattern.

### 3c. Build/deploy targeting pmon

pmon is **Debian 13, glibc 2.41, Python 3.13.5**. Neither the Windows dev box nor
the `sonic-dev` host can produce a matching binary directly, so
`tools/validate_on_dut.sh` builds inside a **Debian-13 container with Rust +
python3.13-dev + clang + the swss c-api headers**, then `docker cp`s the binary
into pmon and swaps it in via supervisor — **reversibly**, restoring the Python
xcvrd after every run.

The inject is crash-safe: the Python xcvrd is backed up and the backup verified
first, and the shim is staged to a temp file and moved atomically, so an
ENOSPC/partial write can never truncate `xcvrd`.

**Where the orchestrator runs.** The host-side wrappers stage the crate + `dut/*.sh`
and invoke them through a transport shim, `tools/lib_remote.sh`, selected by
**`RECODE_RUN_MODE`**:

- **`remote`** (default) — ssh/scp to `RECODE_SSH_HOST` (default `sonic-dev`); the
  from-your-laptop path.
- **`local`** — no ssh, operate directly on the box, for when the whole pipeline
  runs **on sonic-dev itself**. Auto-selected when `RECODE_SSH_HOST` is
  `localhost`/`127.0.0.1`.

Only the *outer* hop changes; the inner DUT chain (sonic-dev → `mgmt` →
`admin@10.250.0.101` → `pmon`) is identical in both modes.

---

## 4. Repository layout

```
dev/recodeAgent/
├── README.md                     # this file (living design doc)
├── pyproject.toml                # orchestrator deps (apache-burr)
├── orchestrator/                 # the small deterministic Burr layer
│   ├── app.py                    #   ApplicationBuilder: actions + transitions + persister
│   ├── state.py                  #   typed state helpers
│   ├── actions.py                #   analyze/scope/plan/select_milestone/translate/validate/parity_verify
│   ├── optimize.py               #   optimize phase: benchmark / optimize / opt_repair (§6)
│   ├── milestones.py             #   Scoper-owned milestone ARTIFACT loader (§5)
│   ├── copilot.py                #   invoke_agent(): subprocess wrapper around `copilot`
│   └── mock.py                   #   offline mock agents (RECODE_MOCK=1)
├── agents/                       # Copilot CLI custom-agent profiles (§4a)
│   ├── analyzer.agent.md  scoper.agent.md  planner.agent.md
│   ├── translator.agent.md  validator.agent.md  parity_verifier.agent.md
│   └── benchmarker.agent.md  optimizer.agent.md          # optimize phase only
├── tools/
│   ├── validate_on_dut.sh        # build ▶ inject ▶ run.sh ▶ results.xml ▶ restore
│   ├── build_check.sh            # compile-only check (no inject/tests)
│   ├── unit_test.sh              # cargo test (Part-B unit tests, mocked) in the container
│   ├── install_agents.sh         # install the .agent.md profiles where the CLI discovers them
│   ├── lib_remote.sh             # transport shim: RECODE_RUN_MODE=remote | local
│   ├── bridge_smoke.sh           # PyO3 spine proof, in pmon
│   ├── env_check.sh              # bridge + swss-common proof, in pmon
│   ├── check.sh                  # offline orchestrator mock checks (§7)
│   ├── burr_ui.sh                # local Burr telemetry UI (§7a)
│   ├── tests/                    # host-side unit tests for the harness itself
│   └── dut/                      # scripts that run on the sonic-dev host / vlab / pmon
├── crate/                        # IMMUTABLE input: the Rust workspace (build target = pmon)
│   ├── xcvrd-rs/                 #   bootstrap daemon wiring BOTH bindings
│   └── platform-bridge/          #   PyO3 wrappers around sonic_platform
├── source/                       # INPUT (gitignored, re-pullable)
│   ├── xcvrd/                    #   the Python xcvrd source (+ tests/ = Part-B input)
│   └── sonic_platform/           #   the emulator HAL — bridge-design reference
├── results/                      # recorded pipeline outputs (result_N/), immutable
└── pipeline/                     # runtime hand-off (gitignored) — see below
```

Inter-stage state is passed as **files in `pipeline/`**; each agent ends its run by
writing a parseable artifact there:

| Artifact | Written by | Contents |
|---|---|---|
| `analysis.md` | analyzer | source research, dep mapping, target design |
| `milestones.json` | scoper | the ordered milestone set (§5) |
| `plan.json` + `crate/` | planner | per-milestone plan; the mutable working copy |
| `report.json` | validator | combined unit + e2e verdict |
| `skips.json` | orchestrator | deferred e2e tests + their one-shot retry record |
| `parity_report.json` | parity_verifier | `coverage_matrix`, `gaps`, `complete` |
| `bench.json`, `optimize.json`, `optimize_history.json` | benchmarker / optimizer | §6 |
| `burr.db` | Burr | persisted state for crash-resume |

Copilot CLI's `--output-format json` (JSONL) is parsed for success/failure detection
and logging, but the file artifacts remain the authoritative state channel.

### 4a. The agents (`agents/*.agent.md`)

Each stage is a **Copilot CLI custom agent** — a Markdown profile with YAML
frontmatter (`name`, `description`, scoped `tools`) plus a system prompt. The CLI
discovers them from `~/.copilot/agents/`; ours are version-controlled in `agents/`
and mirrored there by `tools/install_agents.sh` (and automatically by `copilot.py`
before every run, honouring `COPILOT_HOME`).

| Agent | Paper | Reads → Writes | Role here |
|-------|-------|----------------|-----------|
| **analyzer** | §3.2 | `source/xcvrd/` (+ `tests/`) → `analysis.md` | Source research, Py-dep→Rust mapping, target design, and a source-cited behaviour inventory for scoping. Bakes in the thick-HAL / swss-common / two-layer-validation / immutable-input constraints and designs the mockable HAL/DB seams. Defines no milestones; writes no Rust. |
| **scoper** | *(ours)* | `analysis.md` + source + `../xcvrd-tests/` (+ `parity_report.json`) → `milestones.json` | Owns the milestone set. First pass **partitions every `xcvrd-tests` module** across dependency-ordered milestones, exactly one milestone per module, ending on a full-suite gate. On parity feedback, appends unit-only milestones. Writes no Rust/tests. |
| **planner** | §3.3 | `analysis.md` + `milestones.json` → `pipeline/crate/`, `plan.json` | Fragment extraction (Part A daemon **+ Part B unit tests**), name mapping, compilable skeleton with mock/test seams, dependency-aware plan. |
| **translator** | §3.4 | `plan.json` / `report.json` → edits `pipeline/crate/xcvrd-rs/` | Implements the milestone's daemon logic **and** rewrites the matching Python unit tests + adds Rust unit tests with mocks. Never touches `crate/`. |
| **validator** | §3.5 | runs `unit_test.sh` + `validate_on_dut.sh` → `report.json` | **Two layers**: mocked Rust unit tests **and** the fixed e2e suite on the DUT. `passed` iff both. Never edits daemon/tests/platform. |
| **parity_verifier** | *(ours)* | source + `pipeline/crate/` → `parity_report.json` | Per-module source-vs-Rust completeness check once all milestones pass. Read-only. |
| **benchmarker** | *(ours)* | runs `benchmark/bench.sh` → `bench.json` | Measures only — **no edit tool**, deliberately (§6). |
| **optimizer** | *(ours)* | `bench.json` + `optimize_history.json` → edits `pipeline/crate/` | One small focused change set per round; must leave `unit_test.sh` green. |

`tools` are scoped per role and all omit the `agent` alias, so an agent cannot
delegate to another — the Burr graph is the only sequencer. The orchestrator runs
each with `--model claude-opus-4.8 --allow-all --no-ask-user --output-format json`
and a **per-agent reasoning effort**: heavy reasoning stages at `max`, the validator
(mostly tool execution) at `high` (see `AGENT_EFFORT` in `copilot.py`). Override
with `RECODE_MODEL` / `RECODE_EFFORT`.

---

## 5. Milestone matrix

Each milestone is a slice of the daemon, and its gate is **CUMULATIVE**: it must
pass its own new tests **and every earlier milestone's** (regression safety). The
set is a Scoper-generated artifact at `pipeline/milestones.json`, with
`DEFAULT_MILESTONES` in `orchestrator/milestones.py` as the bootstrap the Scoper
starts from and the loader's fallback.

```bash
python -m orchestrator.milestones --args M3   # print the resolved gate
bash tools/validate_on_dut.sh M3              # ...and run it
```

Selection uses a pytest **`-k` module expression**, not file paths: `run.sh` always
runs `pytest <tests-dir> …`, so `-k "test_presence or test_info_content"` narrows
the already-collected dir to exactly the intended modules while slow tests within
them still run. **No `-m` marker filter** — every milestone runs its full cumulative
set including slow tests.

The **bootstrap** set (the real first-pass partition maps *every* module and may
differ):

| #  | Milestone (adds) | Cumulative gate |
|----|------------------|-----------------|
| M0 | **Skeleton** (compiles, injects, RUNNING) | *deploy-smoke* — supervisor RUNNING, no pytest |
| M1 | **Presence + identity** | `-k "test_presence or test_info_content"` |
| M2 | **DOM** | + `test_dom or test_interaction_trace` |
| M3 | **Status / CMIS / errors** | + `test_status_error` |
| M4 | **lpmode / reset** | + `test_lpmode_reset` |
| M5 | **Multiport concurrency** | + `test_multiport` |
| M6 | **Golden conformance** | + `test_golden` (all eight modules) |

M0 is a deploy-smoke gate because the suite's clean-baseline fixture requires
`TRANSCEIVER_INFO` repopulation, so no pytest can pass on a bare skeleton.

Milestones appended later carry an `origin` (`parity`, `retry`, `optimize`) and may
set `unit_only` (no new e2e module) or `full_suite` (gate on the entire suite, §6).

---

## 6. Optimize phase (in-pipeline, after parity)

Everything above answers **"is it correct?"**. This phase answers **"is it fast?"** —
in the same graph, after parity confirms the translation is complete. It is gated on
`parity_complete`: tuning a translation with known gaps tunes code that is still
going to change.

**Off by default** (it costs DUT time). Turn it on with `--optimize`, or set the
count directly with `--max-opt-rounds N`; `--max-opt-rounds` wins when both are
given, so `--optimize --max-opt-rounds 0` turns it back off.

**Focus it on specific benchmarks** with `--benchmarks B4,B9` (comma- or
space-separated, case-insensitive). This scopes **both** halves of the loop from one
flag: the Benchmarker runs only those scenarios, and the Optimizer is told they are
the only evidence it has and the only thing its change is judged on. Scoping just
one half would be worse than not scoping at all — optimising for a scenario nobody
measured, or measuring one nobody is optimising, so both read the same state key.

The Optimizer is also told the two consequences explicitly: a change helping
something outside the set is unmeasured here and cannot be claimed as a win, and a
change that speeds these up while plausibly slowing something outside the set is
still a regression that *nothing in this run would catch*.

A focused run is much cheaper — a full sweep drives the live DUT through plug
storms, soaks and shutdowns for every round — so it is the practical way to iterate
on one hot path. Unknown ids are rejected up front by `bench.sh`, which is
deliberate: an unrecognised id used to just produce a shorter result set that still
looked complete, which is how B7 stayed silently unmeasured for several runs.

```
parity_verify ──▶ benchmark ──▶ optimize ──┐
                      ▲                    │ rounds remain
                      └────────────────────┘
                                           │ budget spent
                                           ▼
                                      opt_repair   (appends one milestone)
                                           ▼
                      select_milestone ──▶ translate ⇄ validate ──▶ terminal
```

| action | agent | does |
|---|---|---|
| `benchmark` | **Benchmarker** | runs `benchmark/bench.sh <crate>`, reports `bench.json`. No edit tool — it measures, it does not fix. |
| `optimize` | **Optimizer** | one small focused change set to `pipeline/crate`, guided by the numbers. Must leave `tools/unit_test.sh` passing. |
| `opt_repair` | — | appends a final `full_suite` milestone and hands it to the normal repair loop |

### Why the e2e gate is at the END, not per round

The first design validated **every** round against the full e2e suite and reverted
on failure. Measured over 20 real rounds on `result_4`, that was actively harmful:

- one flaky test (`test_dom_gating`) failed in **14 of 20** rounds,
- **including 7 rounds where the Optimizer changed nothing at all** (`files: []`) —
  an empty change set cannot cause a regression, so those reverts discarded work for
  a failure the round did not produce,
- **16 of 20 rounds were thrown away**, and the second half of the run kept nothing.

So rounds now **accumulate**. The Optimizer still runs the mocked unit tests every
round — cheap, deterministic, catches real breakage immediately — and the expensive
e2e gate runs **once**, as a normal milestone, where a failure is **repaired** by the
Translator over `--max-iter` attempts instead of discarding the round. A flaky
failure now costs one repair attempt that finds nothing, rather than an entire
optimisation.

### Other decisions worth knowing

- **Reuses the milestone machinery.** `opt_repair` appends a milestone exactly as the
  Parity Verifier appends its retry milestone, so the repair loop, `skips.json`
  handling, budget and failure-report format all come for free.
- **`full_suite` gate.** `Milestone.full_suite` makes `validate` run
  `validate_on_dut.sh <M> --all --dom-interval 5` instead of the cumulative `-k`:
  that selection covers only modules some milestone listed, but a performance change
  can regress anything — including the T-series parity tests no milestone claims.
  The DOM interval matches what the benchmarks measured; the harness falls back to no
  flag (logging `DOM_INTERVAL_FALLBACK`) if the crate does not accept it.
- **Terminates.** `validate → terminal` is ordered **before** `select_milestone` and
  `parity_verify`, so the repair milestone cannot fall back into parity and re-enter
  optimization; `opt_done` is a second guard.
- **Fails loudly.** If the repair milestone exhausts its budget, `done=False` and the
  run exits non-zero — an unrepaired regression is never reported as success.
- **One snapshot, not per round.** `crate_snapshot/` is taken once before round 1, so
  the pre-optimisation tree stays recoverable; re-snapshotting per round would
  overwrite the only pristine copy with an already-optimised one.

`optimize_history.json` is append-only, one entry per round; the Optimizer reads it
so it does not repeat an idea.

---

## 7. Running the checks yourself

| Check | Needs | Command |
|---|---|---|
| **A.** Orchestrator, offline | nothing | `bash tools/check.sh` |
| **B.** DUT validation harness | `ssh sonic-dev` | `bash tools/validate_on_dut.sh M1` |
| **C.** platform-bridge spine | `ssh sonic-dev` | `bash tools/bridge_smoke.sh` |
| **D.** bridge + swss-common | `ssh sonic-dev` | `bash tools/env_check.sh` |
| **E.** The real agents | Copilot login + credits | `python -m orchestrator.app --app-id run1` |

Only **E** spends tokens. Run the shell ones from **Git Bash**, not PowerShell.

### A. Deterministic orchestrator (offline, no DUT)

`bash tools/check.sh` runs 16 scenarios against the mock agents:

| # | Scenario | Look for |
|---|---|---|
| 1 | Happy path | `done=True`, M0..M6 `passed=True`, parity complete |
| 2 | Repair loop | `M1 iter=1 passed=False` then `iter=2 passed=True` |
| 3 / 3b | Give-up → retry passes / retry also fails | retry milestone appended; then permanent skip |
| 3c | Skips extraction is shape-tolerant | validator omits `layer` / uses prose |
| 4, 7, 8 | Crash-resume (inner / scope / parity) | process 2 prints `loaded state … milestone_idx=N` |
| 5 | Parity feedback loop | Scoper appends `M7` (origin parity), `parity_round=2` |
| 6 | Outer budget exhaustion | `done=False parity_complete=False` |
| 9 / 10 | Start at M3 / at parity | history begins there; earlier stages skipped |
| 11 | Optimize phase after parity | `OPTIMIZE` then a full-suite `M7`, `done=True` |
| 11b | `--start-benchmark` | history is **only** `OPTIMIZE` + `M7` |
| 11c | Optimize repair | `M7` fails twice then passes — repaired, not reverted |
| 11d | Scenario focus | `scenarios=B4 B9`; a bad id and a no-phase use are both rejected |

### B. DUT validation harness

```bash
bash tools/validate_on_dut.sh M0             # deploy-smoke: skeleton must be RUNNING
bash tools/validate_on_dut.sh M1             # first real pytest gate (auto-resolves -k)
bash tools/validate_on_dut.sh --all          # the ENTIRE suite, incl. T-series parity tests
bash tools/validate_on_dut.sh --all -m "not slow" --dom-interval 5
```

`--all` (aliases `-a`, `all`) drops the milestone `-k` gate under the report label
`ALL`. `--dom-interval N` passes `--dom_update_interval N` to the injected daemon —
**omitted by default**, since a partially translated crate may not implement it; when
requested, the DUT side retries without it if the daemon will not start. Set
`RECODE_PRINT_GATE=1` to print the resolved gate and exit before any DUT work.

It prints the verdict and writes `pipeline/report.json`:

```json
{ "milestone": "M0", "passed": true, "tests": {"total":1,"passed":1,"failed":0}, "failures": [] }
```

The harness **always** restores the Python xcvrd, including on failure. Confirm the
testbed any time with `../setup-sonic-testbed.sh xcvrd_status` (read-only: reports
`PYTHON (stock)` vs `RUST (xcvrd-rs)`, supervisor state, and the inject markers).

### E. Driving the real agents

Authenticate first (`copilot login`, or set `COPILOT_GITHUB_TOKEN`). To exercise a
single agent by hand:

```bash
copilot -p "Analyze source/xcvrd and write pipeline/analysis.md" \
  --agent analyzer --model claude-opus-4.8 --reasoning-effort high \
  --allow-all --no-ask-user --add-dir ../xcvrd-tests
```

### Grading against the real sonic-mgmt suites

Beyond this repo's `xcvrd-tests` gate, the sibling `../setup-sonic-testbed.sh` can run
the **official sonic-mgmt** transceiver suites against a Rust xcvrd built from any
pipeline-run folder (`transceiver_tests_rust` / `transceiver_tests_all_rust`), plus a
`transceiver_tests_noop` negative control that proves those tests have teeth. See the
root `README.md`.

> **Test-validity note:** the sonic-mgmt suites split into STATE_DB-backed
> (`test_sfpshow`, `test_xcvr_info_in_db`), which genuinely exercise xcvrd, and
> platform-API-direct (`test_sfputil`, `api/test_sfp`), which read `sonic_platform`
> and pass even if xcvrd is dead. Count only the former as xcvrd evidence — and only
> after a STATE_DB flush, since rows survive an xcvrd restart and would otherwise let
> a broken daemon false-pass.

### 7a. Burr telemetry UI (local, live)

Traces are captured to `~/.burr/recodeagent-xcvrd/` on every run. The UI server needs
`apache-burr[start]` (pyarrow), which has no wheel for Python 3.12 ARM64 — so the
helper runs it in a local Docker container and **bind-mounts your real `~/.burr`**,
making it live with no syncing:

```bash
bash tools/burr_ui.sh          # build (once) + start on :7241  (BURR_PORT overrides)
bash tools/burr_ui.sh --stop
bash tools/burr_ui.sh --logs
```

**Per-step Copilot chat in the UI.** Every stage logs its invocation as attributes on
that action node: `copilot_chat` (the readable transcript, reconstructed from the
JSONL), plus `copilot_prompt`, `final_text`, `files_modified`,
`lines_added`/`lines_removed`, `premium_requests`, `duration_s`, `returncode`, and —
for validate — `milestone_passed` and `report_tests`/`report_failures`. So clicking a
`translate:M2` node shows exactly what the Translator did. Raw JSONL is also on disk
at `pipeline/logs/<agent>.stdout.jsonl`.
