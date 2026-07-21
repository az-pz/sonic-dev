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

Faithful to ReCodeAgent's four agents and Algorithm 1 loop, with these adaptations
for our environment:

1. **Two validation layers — unit tests (Part B) *and* a fixed black-box oracle.**
   We keep the paper's Part B (test translation): the Translator rewrites xcvrd's
   Python **behavioral unit tests** (`source/xcvrd/tests/test_xcvrd.py`) into Rust
   and adds new unit tests for new code, running them against **mocks** of the HAL
   and STATE_DB (mirroring the Python `mock_platform.py` / `mock_swsscommon.py`) via
   `cargo test` — fast, no DUT. On top of that, the **end-to-end `xcvrd-tests`** are
   an *additional, authoritative* oracle: the Validator deploys the candidate Rust
   daemon to the DUT and runs that suite **unchanged** (never translated or
   generated), so the ultimate oracle cannot be gamed. A milestone passes only when
   **both** layers pass.

2. **Plan = prioritized functionality milestones.** The plan is an ordered list of
   xcvrd functionality slices M0–M6, each gated by a specific `xcvrd-tests` subset
   (§5) plus its unit tests. A milestone must go green before the next begins.

3. **Immutable input, mutable working copy.** `crate/` (the M1 bootstrap +
   scaffolding) is a read-only input; the Planner copies it to `pipeline/crate/` and
   all translation happens there. `crate/` is never modified.

Everything else — Analyzer, Planner, skeleton-first, name mapping, the
translate→validate→repair loop with `maxIter` — is the paper's design.

---

## 3. Environment scaffolding (the key design decision)

The paper's Analyzer picks "idiomatic target-language counterparts" for each
source dependency. We **pin** two of them and pre-build them so the agents never
have to reinvent fragile interop:

### 3a. HAL = the existing Python platform via PyO3 (`crate/platform-bridge`)

The Rust xcvrd talks to `xcvr-emu` through the **exact Python `sonic_platform`
plugin we run today** (pulled to `source/sonic_platform/`; lives on pmon at
`/usr/local/lib/python3.13/dist-packages/sonic_platform/`), via **PyO3**. We do
NOT re-implement the CMIS/SFF decode stack in Rust.

Why: `sonic_platform.Sfp` subclasses `SfpOptoeBase`, so the Python platform
already provides the *entire* transceiver API on top of three raw hooks:

```
Sfp(SfpOptoeBase)
  raw hooks     : read_eeprom / write_eeprom / get_presence   (→ emulator gRPC)
  derived (free): get_transceiver_info(), get_transceiver_dom_real_value(),
                  get_transceiver_status(), set_lpmode()/reset() [CMIS mgmt], ...
Chassis(ChassisBase)
                : get_num_sfps(), get_sfp(i), get_change_event(timeout)
```

**`platform-bridge` is a PyO3 crate (built by us) that embeds CPython, imports the
real plugin, and exposes this high-level API to Rust.** So:

- **What stays in Python (behind the bridge):** all CMIS/SFF parsing — "the exact
  platform we have now."
- **What the agents translate into Rust:** the xcvrd **daemon logic** — the task
  loops (`SfpStateUpdateTask`, `DomInfoUpdateTask`, `CmisManagerTask`
  orchestration), polling cadence, state-update decisions, and the STATE_DB
  schema writes.

**Exposed surface** (`platform_bridge::{Platform, Chassis, Sfp, ChangeEvent}`):
`Platform::new()` → `num_sfps()`, `sfp(i)`, `get_change_event(timeout_ms)`; per-SFP
`get_presence()/is_replaceable()/get_reset_status()/sfp_type()/get_error_description()`
(typed scalars), `get_transceiver_info()/_dom_real_value()/_status()/_threshold_info()`
(returned as `serde_json::Value` so the surface is stable as milestones add fields),
`get_lpmode()/set_lpmode()/reset()` [M4], `read_eeprom()/write_eeprom()`, and a
generic `call_json(method, args)` escape hatch. Complex dicts are marshalled via
`json.dumps(…, default=str)`; NUL-padded CMIS strings are returned verbatim (the
daemon logic strips them, exactly like the Python original).

> ✅ **Confirmed & proven on the DUT (2026-07-20).** This *thick* boundary (Rust
> calls `get_transceiver_info()` etc. via PyO3) beats a "thin" one (Rust
> re-implements CMIS decode on read/write/presence) — far smaller, safer
> translation surface, and it matches "use the exact platform we have now."
> `bridge-smoke` runs inside pmon: PyO3 **0.22.6** links `libpython3.13.so.1.0`,
> imports `sonic_platform`, discovers **33 SFPs** over gRPC, and CMIS-decodes real
> identity (`type=QSFP-DD…`, `manufacturer=xcvr-emu`, `model=EMU-40G-LR4`,
> `cmis_rev=5.2`). Reproduce end to end (build in trixie → run in pmon → clean up)
> with **`bash tools/bridge_smoke.sh`**.
>
> The `TRANSCEIVER_INFO` contract M1 must reproduce (from `get_transceiver_info()`):
> `type, type_abbrv_name, hardware_rev, serial, manufacturer, model, connector,
> encoding, ext_identifier, ext_rateselect_compliance, cable_length,
> nominal_bit_rate, vendor_date, vendor_oui, active_apsel_hostlane{1..8},
> application_advertisement, host_lane_count, media_lane_count, cable_type,
> media_interface_technology, vendor_rev, cmis_rev, specification_compliance,
> vdm_supported`.

### 3b. STATE_DB = official `swss-common` Rust crate

The Rust daemon reads/writes Redis STATE_DB through the upstream Rust bindings at
[`sonic-net/sonic-swss-common` `crates/swss-common`](https://github.com/sonic-net/sonic-swss-common/tree/master/crates/swss-common/src),
not a hand-rolled client. It's a **pinned git dependency** (`xcvrd-rs/Cargo.toml`,
rev `7faca59`) exposing `DbConnector`, `Table`, `SonicV2Connector`,
`ProducerStateTable`, `SubscriberStateTable`, etc. Agents write STATE_DB from Rust
with e.g. `DbConnector::new_unix(6, "/var/run/redis/redis.sock", 0)?.hset(key, field, &CxxString::from(v))`.

**Native-lib wiring (the tricky part, solved once):** `swss-common`'s `build.rs`
runs **bindgen** over the C-API headers and links `dylib=swsscommon` (the C++
`libswsscommon`). pmon ships `libswsscommon.so.0` **with the C-API compiled in**
(verified by symbol probe: `SWSSTable_*`, `SWSSDBConnector_new_unix`, …), so a
Rust binary loads and runs there. The build container (`Dockerfile.build`) bakes
what bindgen needs: `clang`/`libclang-dev`, the pinned c-api **headers**
(`SWSS_COMMON_REPO`), and `BINDGEN_EXTRA_CLANG_ARGS=-x c` so bindgen parses only
the C boundary (no boost/hiredis/C++ headers). The link library itself is
pmon-specific, so it's pulled from the live pmon into `~/recode/swsslib` by
`tools/dut/ensure_swsslib.sh` and mounted at build time (`-L native=/swsslib`).
Keep the `Dockerfile.build` `SWSS_COMMON_REV` arg in sync with the Cargo rev.

> ✅ **Proven on the DUT (2026-07-20).** The `statedb_probe` example links
> `libswsscommon.so.0` and round-trips a STATE_DB hash; the `hal_to_statedb`
> example composes **both** libraries — reads
> transceiver 0 via the bridge and publishes 6 CMIS fields to STATE_DB — the exact
> `SfpStateUpdateTask` pattern. Both run green in pmon via **`bash tools/env_check.sh`**.

### 3c. Build/deploy targeting pmon

pmon is **Debian 13, glibc 2.41, Python 3.13.5**. Neither the Windows dev box nor
the `sonic-dev` host can produce a matching binary directly. So
`tools/validate_on_dut.sh` builds inside a **Debian-13 container with Rust +
python3.13-dev + clang + the swss c-api headers** (PyO3 links libpython3.13;
swss-common links libswsscommon), then `docker cp`s the binary into pmon and swaps
it in via supervisor — **reversible**, restoring the Python xcvrd after every run
(same inject/restore pattern as `xcvrd-tests/tools/inject_dummy_xcvrd.sh`).

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
├── agents/                       # Copilot CLI custom-agent profiles (paper §3.2–3.5)
│   ├── analyzer.agent.md  planner.agent.md  translator.agent.md  validator.agent.md
│   │                             #   installed to ~/.copilot/agents/ by tools/install_agents.sh
├── tools/
│   ├── validate_on_dut.sh        # build (Debian-13) ▶ inject ▶ run.sh ▶ results.xml ▶ restore
│   ├── build_check.sh            # compile-only check (no inject/tests) for planner/translator
│   ├── unit_test.sh              # cargo test (Part-B unit tests, mocked) in the trixie container
│   ├── install_agents.sh         # install the .agent.md profiles where the CLI discovers them
│   ├── bridge_smoke.sh           # build+run platform-bridge smoke in pmon (proves PyO3 spine)
│   ├── env_check.sh              # build+run xcvrd-rs binding examples in pmon (bridge+swss proof)
│   ├── check.sh                  # offline orchestrator mock checks (happy/repair/budget/resume)
│   └── dut/                      # scripts that run on sonic-dev host / vlab / pmon
│       ├── Dockerfile.build  build_crate.sh  run_validate.sh  dut_validate.sh
│       ├── bridge_smoke.sh   env_check.sh   ensure_swsslib.sh   # (ensure_swsslib pulls libswsscommon.so)
├── crate/                        # the Rust workspace (build target = pmon)
│   ├── Cargo.toml                #   workspace: xcvrd-rs + platform-bridge
│   ├── xcvrd-rs/                 #   BOOTSTRAP: daemon bin + lib wiring BOTH bindings
│   │   ├── src/main.rs           #     thin entrypoint -> xcvrd_rs::daemon::run()
│   │   ├── src/lib.rs  src/env.rs  src/daemon.rs  # M1 bootstrap: presence + identity
│   │   └── examples/{statedb_probe,hal_to_statedb}.rs  # binding demos (cargo examples, not deployed)
│   └── platform-bridge/          #   PyO3 wrappers around sonic_platform (BUILT + PROVEN)
│       ├── src/lib.rs            #     Platform/Chassis/Sfp/ChangeEvent
│       └── src/bin/bridge_smoke.rs  #  spine smoke test (run in pmon)
├── source/                       # INPUT (gitignored, re-pullable)
│   ├── xcvrd/                    #   the Python xcvrd source the agents translate
│   │   └── tests/               #     Python behavioral unit tests + mocks (Part-B input, from upstream)
│   └── sonic_platform/           #   the emulator HAL — bridge-design reference
└── pipeline/                     # runtime hand-off (gitignored): analysis.md, plan.json, report.json,
                                  #   and crate/ = the mutable working copy (crate/ stays immutable)
```

Inter-stage state is passed as **files in `pipeline/`** (each agent ends its run
by writing a parseable artifact there — `analysis.md`, `plan.json`,
`report.json`). Copilot CLI 1.0.72 also supports `--output-format json` (JSONL),
which the orchestrator parses for success/failure detection and logging; the
file artifacts remain the authoritative state channel.

### 4a. The four agents (`agents/*.agent.md`)

Each stage is a **GitHub Copilot CLI custom agent** — a Markdown profile with YAML
frontmatter (`name`, `description`, scoped `tools`) plus a system prompt. The CLI
discovers custom agents from `~/.copilot/agents/` (user level) or a repo's
`.github/agents/`; since our profiles must live under `dev/recodeAgent/`, they are
version-controlled in `agents/` and mirrored to `~/.copilot/agents/` by
`tools/install_agents.sh` (and automatically by `copilot.py`'s
`ensure_agents_installed()` before every run — honoring `COPILOT_HOME`).

| Agent | Paper | Reads → Writes | Adaptation for this project |
|-------|-------|----------------|------------------------------|
| **analyzer** | §3.2 | `source/xcvrd/` (+ `tests/`) → `pipeline/analysis.md` | 3 design docs (source research, Py-dep→Rust analysis, target design). Bakes in the thick-HAL, swss-common, two-layer-validation, immutable-input, and M0–M6 constraints; designs the mockable HAL/DB seams. Writes no Rust. |
| **planner** | §3.3 | `analysis.md` → copies `crate/`→`pipeline/crate/`, writes `pipeline/plan.json` + skeleton | Fragment extraction (Part A daemon **+ Part B unit tests**, validated), name mapping, compilable skeleton with mock/test seams, dependency-aware **M0–M6** plan. |
| **translator** | §3.4 | `plan.json`/`report.json` → edits `pipeline/crate/xcvrd-rs/` | Implements the milestone's daemon logic (Part A) **and** rewrites the matching Python unit tests + adds new Rust unit tests with mocks (Part B); `build_check.sh` + `unit_test.sh`. Never touches `crate/`. |
| **validator** | §3.5 | runs `unit_test.sh` + `validate_on_dut.sh` → `pipeline/report.json` | **Two layers**: Rust unit tests (mocked, `cargo test`) **and** the fixed e2e `xcvrd-tests` on the DUT. Combined verdict (`passed` iff both), actionable per-failure hints. Never edits daemon/tests/platform. |

`tools` are scoped per role (all omit the `agent` alias, so an agent can't
delegate to another — the Burr graph is the only sequencer). The orchestrator runs
each with `--model claude-opus-4.8 --reasoning-effort high --allow-all
--no-ask-user --output-format json` (model/effort overridable via `RECODE_MODEL` /
`RECODE_EFFORT`). Verified on CLI 1.0.72: the profiles are discovered, the model +
flags are accepted, and JSONL parsing extracts the agent's final message.

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

**platform-bridge (PyO3 thick HAL): proven on the DUT.** The `Platform/Chassis/Sfp`
wrappers embed CPython, import the real `sonic_platform` plugin, and return its
high-level results to Rust (see §3a). Built with PyO3 0.22.6 in the trixie
container (links `libpython3.13.so.1.0`) and run inside pmon via `bridge-smoke`:
discovers 33 SFPs over gRPC and CMIS-decodes real identity. Re-runnable any time
with `bash tools/bridge_smoke.sh` (builds → runs in pmon → cleans up; leaves xcvrd
untouched).

**swss-common wiring + bootstrap: proven on the DUT.** The official `swss-common`
crate (pinned git rev) is wired directly into **`xcvrd-rs`** alongside
`platform-bridge`, so the crate agents start from already has both bindings. The
daemon (`src/daemon.rs`) is a minimal **M1 bootstrap** (presence + identity):
`src/env.rs` exposes `open_platform()` / `open_state_db()` / `open_config_db()`,
and the daemon reads identity via the HAL and publishes `TRANSCEIVER_INFO` +
`TRANSCEIVER_STATUS_SW`, reacting to plug/unplug via `get_change_event`. Two
`examples/` also demonstrate the bindings — `statedb_probe` round-trips a STATE_DB
hash; `hal_to_statedb` reads a transceiver and publishes it (the exact
`SfpStateUpdateTask` read→publish pattern), both green via `bash tools/env_check.sh`.
The build container bakes the swss build prereqs (clang, pinned c-api headers,
bindgen `-x c`) and `ensure_swsslib.sh` supplies `libswsscommon.so` from the live
pmon, so agent builds "just work" (see §3b).

> ✅ **M1 green on the DUT (2026-07-21).** `validate_on_dut.sh M1` → **11 passed,
> 0 failed** (`test_info_content` ×5 + `test_presence` ×6): identity fields,
> plug/unplug clear+restore, `STATUS_SW.status` 1/0, and `cmis_state=READY`. The
> bootstrap gets the suite past the clean-baseline fixture so the real tests run
> and pass, giving the agents a working M1 starting point instead of a no-op.

**The four Copilot agents: implemented + wired.** `agents/{analyzer,planner,
translator,validator}.agent.md` encode the paper's §3.2–3.5 roles with this
project's adaptations (thick HAL, fixed black-box oracle, milestone-incremental).
`copilot.py` auto-installs them and invokes each with `--model claude-opus-4.8
--reasoning-effort high --allow-all --output-format json`. Verified on CLI 1.0.72:
profiles discovered, model + flags accepted, JSONL parsing fixed for the 1.0.72
event shape. This completes Phase 0 — the pipeline can now be driven end to end
(`python -m orchestrator.app --app-id run1`), authentication permitting.

**Phase 0 complete.** *(Done: deterministic Burr core, DUT validation harness,
platform-bridge, swss-common wiring, the M1 bootstrap daemon, the source pull, and
the four agent profiles.)* Next is Phase 1 — actually running the pipeline so the
agents extend `crate/xcvrd-rs` beyond M1 (DOM, CMIS state, errors, …) on the proven
scaffolding.

---

## 7. Running the checks yourself

Checks **A–D** need no Copilot token (the orchestrator check uses the offline
mock; the DUT harness + smokes exercise build/inject/test/restore). Check **E**
drives the real Copilot agents and needs a login + AI credits.

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
bash tools/validate_on_dut.sh M0        # deploy-smoke: skeleton must be RUNNING
bash tools/validate_on_dut.sh M1        # first real pytest gate (auto-resolves the -k selection)
```

It prints the verdict and writes `pipeline/report.json`:

```json
{ "milestone": "M0", "passed": true, "tests": {"total":1,"passed":1,"failed":0}, "failures": [] }
```

M0 passes (deploy-smoke) and **M1 passes 11/11** on the current bootstrap daemon
(presence + identity). M2+ (DOM, CMIS state, errors, …) will fail until that logic
is added — that's expected. The harness ALWAYS restores the Python xcvrd
afterward; confirm the testbed is healthy any time with:

```powershell
ssh sonic-dev "docker exec mgmt bash -lc 'sshpass -p password ssh -o StrictHostKeyChecking=no admin@10.250.0.101 \"docker exec pmon supervisorctl status xcvrd\"'"
```

### C. platform-bridge smoke (real PyO3 → sonic_platform in pmon, ~1 min)

Proves the HAL boundary independently of the daemon. From **Git Bash**:

```bash
cd /c/Users/t-fhabibi/Desktop/toRust/dev/recodeAgent
bash tools/bridge_smoke.sh
```

Builds `bridge-smoke` in the trixie container, runs it inside pmon against the live
`xcvr-emu`, and cleans up. Expected: `num_sfps = 33`, every present module prints
`type=QSFP-DD… manufacturer=xcvr-emu model=EMU-40G-LR4`, and `bridge-smoke: OK`
(`rc=0`). It leaves the Python xcvrd untouched.

### D. Agent scaffolding: bridge + swss-common (real STATE_DB in pmon, ~1 min)

Proves the two libraries agents build `xcvrd-rs` on compile, link, and run
together. From **Git Bash**:

```bash
cd /c/Users/t-fhabibi/Desktop/toRust/dev/recodeAgent
bash tools/env_check.sh
```

Builds the `statedb_probe` + `hal_to_statedb` examples in the trixie container
(pulling `libswsscommon.so` from pmon first via `ensure_swsslib.sh`), runs both
inside pmon, and cleans up. Expected: `statedb_probe: OK` (STATE_DB round-trip) and
`hal_to_statedb: OK` — `bridge -> swss: wrote 6 fields to TRANSCEIVER_INFO|RECODE_HAL2DB_0`
(`manufacturer=xcvr-emu`, `cmis_rev=5.2`, …). Uses throwaway STATE_DB keys it
deletes; leaves xcvrd untouched.

### E. Driving the real agents (needs a Copilot login + AI credits)

Unlike A–D, this runs the actual LLM pipeline. Install the profiles and drive the
orchestrator against Copilot (authenticate first with `copilot login`, or set
`COPILOT_GITHUB_TOKEN`):

```bash
cd /c/Users/t-fhabibi/Desktop/toRust/dev/recodeAgent
bash tools/install_agents.sh                     # -> ~/.copilot/agents/*.agent.md
python -m orchestrator.app --app-id run1          # analyze ▶ plan ▶ (translate ▶ validate)*
```

`copilot.py` also auto-installs the profiles before each call. To exercise a single
agent by hand:

```bash
copilot -p "Analyze source/xcvrd and write pipeline/analysis.md" \
  --agent analyzer --model claude-opus-4.8 --reasoning-effort high \
  --allow-all --no-ask-user --add-dir ../xcvrd-tests
```

Smoke-verified on CLI 1.0.72: all four agents are discovered, `claude-opus-4.8` +
`--allow-all` + `--reasoning-effort high` are accepted, and the JSONL result is
parsed. The agents edit only `crate/xcvrd-rs/`; the Validator runs the fixed
`xcvrd-tests` and always restores the Python xcvrd.

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

### Burr telemetry UI (local, live)

Traces are captured to `~/.burr/recodeagent-xcvrd/` on every run. The interactive
UI server (`burr`) needs `apache-burr[start]` (pyarrow), which has **no wheel for
this box's Python 3.12 ARM64** — so it can't run *natively* here. The helper runs
it in a local Docker container (Docker Desktop's Linux/arm64 engine, where pyarrow
*does* have wheels) and **bind-mounts your real `~/.burr`**, so the UI reads the
same files the pipeline writes — it's **live**, no syncing:

```bash
bash tools/burr_ui.sh          # build (once) + start the UI locally on :7241
#  -> open http://localhost:7241   (project: recodeagent-xcvrd)
bash tools/burr_ui.sh --stop   # stop the UI container
bash tools/burr_ui.sh --logs   # tail the UI server logs
```

Because the trace dir is mounted directly, the graph, per-step timings, and each
node's state update **as a run progresses** — just refresh the browser (the UI
also polls on its own). Requires Docker Desktop running. Override the port with
`BURR_PORT`.

**Per-step Copilot chat in the UI.** Every stage logs its Copilot invocation as
**attributes** on that action node (visible in the UI's action/attributes panel):
- `copilot_chat` — the readable transcript (assistant messages + each tool call
  and its result), reconstructed from the JSONL by `copilot.transcript_from_events`;
- `copilot_prompt`, `final_text`, `files_modified`, `lines_added`/`lines_removed`,
  `premium_requests`, `duration_s`, `returncode`, and (for validate) `milestone_passed`
  + `report_tests`/`report_failures`.
So clicking a `translate:M2` node shows exactly what the Translator did — the chat,
the shell/edit tool calls, and which files it changed. (Full raw JSONL is also on
disk at `pipeline/logs/<agent>.stdout.jsonl`.)


