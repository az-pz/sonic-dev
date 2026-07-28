# xcvrd black-box tests

Structured Python (pytest) tests that exercise **xcvrd** as a black box against
the `xcvr-emu` emulator. They run **on the DUT** (`admin@vlab-01`), where the
emulator gRPC (`localhost:50051`), `sonic-db-cli` and the pmon supervisor are all
local. The current upstream xcvrd is treated as the reference implementation.

> See **[DESIGN.md](DESIGN.md)** for the architecture, diagrams, the classes
> changed vs. created, and how the suite sits on the DUT.

## What it observes

| Channel | How | Used for |
|---------|-----|----------|
| STATE_DB | `sonic-db-cli` (`lib/statedb.py`) | xcvrd's declared outputs: `TRANSCEIVER_INFO/DOM_SENSOR/STATUS/...` |
| Emulator Monitor stream | gRPC (`lib/monitor.py`) | the xcvrd↔emulator interaction trace — every EEPROM read/write, zero-touch |
| Emulator gRPC | `lib/emu.py` | stimulus: plug/unplug, raw EEPROM read/write |
| xcvrd lifecycle | `supervisorctl` in pmon (`lib/xcvrd_ctl.py`) | restart / flush tables |

## Setup / teardown (why results are trustworthy)

`TRANSCEIVER_*` rows live in Redis STATE_DB and **survive xcvrd being stopped**,
so a naive read-only test would PASS on stale residue even when the daemon is
dead. The harness prevents that:

- **Clean baseline (session setup)** — flush `TRANSCEIVER_*`, restart xcvrd, and
  require it to repopulate. If it can't, the whole suite fails fast with a clear
  message instead of asserting against stale rows. If xcvrd wasn't running it is
  started (with a warning), so tests always run against a fresh, live daemon.
- **Per-test health guard** — every test first checks xcvrd is RUNNING, so a
  mid-suite crash fails fast rather than false-passing on residue.
- **Per-test isolation** — the `module` fixture snapshots/restores presence and
  any mutated DOM bytes; teardown replugs all modules and leaves xcvrd running.

Validated: healthy → all pass; xcvrd stopped → self-heals + warns, all pass on
fresh data; emulator down → fail-fast with zero false passes.

### Timeouts & the fast dev loop

Waits use named tiers from `lib/waits.py`, calibrated against the reference
xcvrd (measured with `tools/measure_timeouts.py`): `T_FAST=15s` for STATE_DB
reactions (presence/info/status/cmis/error — real max ~3.3s) and `T_DOM=80s` for
the DOM sensor + steady-state EEPROM reads, which are paced by xcvrd's ~60s poll
cadence. So a broken daemon fails most tests in ~15s instead of 60-120s.

Because the DOM-cadence tests take ~60s even when passing, they are marked
**`slow`** and skipped by the default dev loop:

```bash
./run.sh -m "not slow"   # ~19 fast tests, ~1-1.5 min  (inner dev loop)
./run.sh                 # full 29 tests, ~6 min       (includes DOM cadence)
```

The `slow` set is all of `test_dom.py` + `test_golden.py` + `test_status_error.py`
plus the two steady-read `test_interaction_trace.py` tests. Re-run
`tools/measure_timeouts.py` and adjust the tiers if the reference xcvrd changes.

### Negative control (does the suite really catch a broken xcvrd?)

`tools/inject_dummy_xcvrd.sh` swaps the real xcvrd for a **fake-healthy** dummy:
it writes *bogus* `TRANSCEIVER_INFO` for every port (just enough to satisfy the
clean-baseline gate, which only checks the probe port's `manufacturer` is
non-empty) and then does nothing else — no emulator reads, no presence handling,
no DOM, no CMIS state, no Monitor traffic. So the real test bodies actually RUN
and then FAIL on their content/behaviour assertions:

```bash
tools/inject_dummy_xcvrd.sh inject     # install the fake-healthy xcvrd (on the DUT)
./run.sh -m "not slow"                 # -> 22 failed, 4 passed (see below)
tools/inject_dummy_xcvrd.sh restore    # put the real xcvrd back -> suite green again
```

Result: **22 failed, 4 passed**. The 4 passes are expected and informative:
- `test_xcvrd_running` — the dummy really is RUNNING (a liveness check, not a
  functional one).
- `test_lpmode_reset` ×3 — `sfputil reset`/`lpmode` drive the module through the
  **platform/sfputil path, not xcvrd**, so a dummy xcvrd doesn't affect them.
  (Good to know: these three are platform-control tests, not xcvrd tests.)

Everything that actually depends on xcvrd — identity content, presence, DOM,
interaction trace, multi-port, error injection, golden — FAILS, proving those
assertions genuinely validate the daemon rather than passing on residue.

## v1 scope

- `tests/test_presence.py` — plug/unplug clears+restores `TRANSCEIVER_INFO`; `TRANSCEIVER_STATUS_SW` plug state + `cmis_state` READY.
- `tests/test_info_content.py` — static identity matches the emulator (vendor, PN, OUI, serial, type, power class); all admin-up present ports populated.
- `tests/test_dom.py` — DOM table present; raw temperature/voltage writes propagate after refresh (`slow`).
- `tests/test_interaction_trace.py` — xcvrd really polls the module and re-reads on plug (Monitor trace).
- `tests/test_multiport.py` — concurrent multi-port presence/change/DOM: simultaneous unplug/replug, partial-unplug isolation, and distinct per-port DOM writes with no cross-talk (per-module isolation).
- `tests/test_golden.py` — STATE_DB projection matches the committed golden baseline (conformance gate), **per scenario** (see below).
- `tests/test_cmis_datapath.py` — **CMIS datapath bring-up** parity gate (`slow`): an admin-up port (default `Ethernet4`, override `XCVRD_ACTIVATED_PORT`) must be driven to `module_state=ModuleReady`, every configured `DP{n}State=DataPathActivated`, `config_state=ConfigSuccess`, and a **real** `active_apsel_hostlane{n}` (not `N/A`) with real `host_lane_count`. Cross-checks the emulator's own **datapath** state machine via `GetInfo.dpsms` **and its module state machine via `GetInfo.msm`** (`MODULE_READY`), so it proves xcvrd drove the module — not just wrote STATE_DB. A reduced daemon that only marks `cmis_state=READY` with `N/A` apsel fails it. The `msm` cross-check needs an emulator exposing `GetInfo.msm` (branch `feature/getinfo-msm`); older emulators leave it unset and that one test skips. The datapath SM itself needed **no emulator change** — the emulator already models it (`dpsm.py`) and Python drives it for admin-up ports.
- `tests/test_transceiver_status.py` — rich `TRANSCEIVER_STATUS` coverage (`slow`): asserts xcvrd publishes the module + all 8 per-lane datapath/config fields, and that the admin-down baseline reads the **deactivated** state (`ModuleLowPwr` / `DataPathDeactivated`). A daemon that only publishes `TRANSCEIVER_STATUS_SW` fails it. (The activated case is `test_cmis_datapath.py`.)
- `tests/test_status_flag.py` — `TRANSCEIVER_STATUS_FLAG` coverage (`slow`, no golden): asserts xcvrd publishes the module-level fault/state fields, then raises `ModuleFirmwareErrorFlag` (CMIS byte 8 bit1) on the emulator and asserts it surfaces as `module_firmware_fault` and clears again. The reduced Rust publishes no flag table at all, so it fails. **No emulator change** — module flags are observable via byte 8 (no clear-on-read), like the DOM flags. (Per-lane tx-fault/rx-los read `N/A` until the module advertises lane-flag support in 01h:157/158.)
- `tests/test_firmware_info.py` — `TRANSCEIVER_FIRMWARE_INFO` published with `active_firmware`/`inactive_firmware` (`slow`, no golden). The reduced Rust publishes no firmware table. Values read `N/A` here because they come from a CDB *GetFirmwareInfo* command the emulator doesn't serve (not the plain byte 39-40 registers), so the gate covers table publication + fields.
- `tests/test_vdm.py` — the four `TRANSCEIVER_VDM_*_THRESHOLD` tables published with the CMIS VDM observables (laser temp / eSNR / PAM4 level transition / pre-FEC BER / errored frames, per host+media lane) (`slow`, no golden). The reduced Rust publishes no VDM tables. Values read `N/A` (module advertises `vdm_supported=False`, emulator serves no VDM pages 20h-2Fh), so the gate covers publication + observable field structure.

No changes to xcvrd or the emulator. (Error-injection + lpmode/reset are v2.)

No changes to xcvrd. The only bridge addition is a test-only error-injection hook
(a STATE_DB table the bridge reads); the emulator is unchanged.

## v2 scope

- `tests/test_status_error.py` — inject error events (I2C-stuck / bad-EEPROM =
  blocking, high-temp = non-blocking): `TRANSCEIVER_STATUS_SW.error` is set, DOM
  is removed for blocking errors (static INFO kept), DOM is retained for
  non-blocking, and recovery clears the error + repopulates.
- `tests/test_lpmode_reset.py` — `sfputil reset` / `lpmode` drive the module and
  we assert the exact ModuleGlobalControls (00h:26) write on the Monitor stream
  (reset → SoftwareReset 0x08, lpmode on → LowPwrRequestSW 0x10) + lpmode show.
- `tests/test_daemon_control.py` — **daemon-driven** module power control (T3.2,
  `slow`): distinct from the operator `sfputil` commands above, xcvrd ITSELF takes
  an admin-up port out of low power during CMIS bring-up. Asserts xcvrd's own
  ModuleGlobalControls write clearing `LowPwrRequestSW` on the Monitor stream (the
  emulator boots modules in low power, so a cleared write is the daemon driving),
  and cross-checks `module_state=ModuleReady` in STATE_DB + the emulator's
  `GetInfo.msm`. A reduced daemon that never drives module power fails. Ties to 3.1.
- `tests/test_optics_si.py` — **media / optics SI application** (T3.3, `slow`):
  xcvrd applies per-vendor Signal-Integrity settings from `optics_si_settings.json`
  during CMIS bring-up. The test provisions an SI file (vendor `XCVR-EMU`) into the
  platform dir + restarts xcvrd (see `lib/optics_si.py`), advertises SI-control
  support (`01h:161/162`) on the module, forces a fresh bring-up, and asserts xcvrd
  stages the CDR-enable SI controls into the page-10h Staged Control Set
  (offsets 153-175) and sets `ExplicitControl=1` in the DPConfigLane bytes. A
  daemon that stubs media/SI application never writes that region. Pure harness
  stimulus (no emulator image change); the fixture fully restores the no-SI
  baseline on teardown. (The JSON vendor key is a regex because the emulator
  returns a NUL-padded vendor name the SI parser's `.strip()` leaves intact.)
- `tests/test_cmis_reconfig.py` — **datapath teardown + re-provision on
  reconfiguration** (T3.4, `slow`): the teardown counterpart to
  `test_cmis_datapath.py`. Flips a port (default `Ethernet8`, override
  `XCVRD_RECONFIG_PORT`) admin-down then admin-up in CONFIG_DB and asserts xcvrd's
  CmisManagerTask drives the whole cycle: on admin-down its own **DataPathDeinit**
  write (`10h:128`, covering the active host lanes) on the Monitor stream + the
  module's **DataPathStateLane** report (`11h:128`) taking the active lanes to
  `DataPathDeactivated`; on admin-up a **DPConfigLane** re-write (`10h:145-152`) +
  the lanes returning to `DataPathActivated`. Both signals are deterministic and
  module-side (page 11h is what xcvrd reads back); we deliberately avoid the
  emulator's `GetInfo.dpsms` objects (they don't follow a raw DPDeinit write) and
  `TRANSCEIVER_STATUS.DP1State` (non-deterministic refresh on this stimulus). A
  reduced daemon that ignores `admin_status` fails. Pure CONFIG_DB stimulus (no
  emulator change); the fixture always restores admin-up + re-activation, and
  `Ethernet8` is used so the `Ethernet4` `activated_datapath` golden is undisturbed.

Error injection uses a **gated** bridge hook: only when the deploy drops a
`.test_hooks` marker next to the bridge does `chassis.get_change_event` read a
single STATE_DB hash `XCVR_EMU_INJECT` (field = physical index, value = SfpBase
error bitmap) with one `HGETALL` and surface that event as a real platform would.
Without the marker the hook is fully inert — **no STATE_DB access at all** — so a
production/virtual platform pays zero cost and carries no test backdoor. The
testbed enables it via `EMU_TEST_HOOKS=1` (default in `setup-sonic-testbed.sh`);
a clean platform deploy leaves it off.

## Golden baseline (conformance mode) — per-scenario, differential

The golden is captured from the reference **Python** xcvrd (it is the oracle) and
a candidate xcvrd (e.g. the Rust reimplementation) must reproduce it exactly. As
of T0 the golden is organized **per scenario** so coverage can grow one behavior
at a time:

- `tests/scenarios.py` registers named scenarios — each pins the module in one
  reproducible state (`prepare`) and declares the STATE_DB tables that form its
  golden. Registered scenarios:
  - `steady_state` — admin-down port at rest on `Ethernet100`:
    `TRANSCEIVER_INFO` + `TRANSCEIVER_STATUS_SW` + the full rich `TRANSCEIVER_STATUS`
    (ModuleLowPwr, every `DP{n}State=DataPathDeactivated`, `active_apsel='N/A'`) +
    the **enriched** `TRANSCEIVER_DOM_THRESHOLD`. The prepare writes the CMIS
    page-02h Module Thresholds (temp/vcc/tx-power/rx-power/tx-bias) on the emulator
    and re-inserts the module so xcvrd reads them — the emulator serves 0 for
    unwritten threshold bytes, so without this every threshold is `0.0`/`-inf` and a
    daemon that publishes zeros would pass. **No emulator change** (the harness
    writes the raw page-02h bytes via the emulator's table-agnostic EEPROM store).
    The deactivated complement of `activated_datapath`.
  - `activated_datapath` — admin-up port (`Ethernet4`) whose CMIS datapath xcvrd
    drove to activated: `TRANSCEIVER_INFO` (real `active_apsel`/lane counts) +
    the full rich `TRANSCEIVER_STATUS` (ModuleReady, `DP{n}State=DataPathActivated`,
    ConfigSuccess) + `TRANSCEIVER_STATUS_SW`. The CMIS bring-up parity gate.
  - `dom_flag` — a module with a **raised DOM alarm flag** (`Ethernet4`, override
    `XCVRD_FLAG_PORT`): the scenario raises `TempMonHighAlarm` on the emulator
    (lower page byte 9) and goldens `TRANSCEIVER_DOM_FLAG` (`tempHAlarm=True`, every
    other flag `False`). A daemon that doesn't read + publish the module's latched
    monitor flags — e.g. the reduced Rust, which emits no flag tables — fails it.
    **No emulator change** was needed: the emulator holds the flag register with no
    clear-on-read, so xcvrd reads it on the next DOM poll and the projection is
    stable; a scenario `teardown` clears the flag back to the unflagged baseline.
- Goldens live at `golden/<scenario>/<port>.json` (volatile timestamps stripped).
- Each scenario has its **own test function** in `tests/test_golden.py`
  (`test_steady_state`, `test_activated_datapath`, …) so it is selectable by
  pytest function name. Scenario definitions live in `tests/scenarios.py`.
- Each e2e feature adds a scenario (+ the emulator/bridge work only if the data
  isn't already observable), recaptures from Python, and commits the new golden.

```bash
# (re)capture EVERY scenario's golden from the reference Python xcvrd, then commit golden/**
./run.sh --capture-golden tests/test_golden.py
# compare the live daemon against the committed goldens
./run.sh tests/test_golden.py
# just one scenario (by test function name)
./run.sh -k test_activated_datapath
```

Capture **refuses to run against a non-reference (Rust-injected) xcvrd**
(`XcvrdControl.is_reference_python`), so a golden can never be accidentally
baselined from the very candidate it is meant to grade.

## Running

From a full `sonic-develop` checkout on the VM host:

```bash
cd dev && ./setup-sonic-testbed.sh xcvrd_tests            # ship to DUT + run
./setup-sonic-testbed.sh xcvrd_tests -- -m "not slow"     # skip the ~60s DOM tests
```

Or directly on the DUT (`admin@vlab-01`), from a copy of this folder:

```bash
./run.sh                     # full suite
./run.sh -m "not slow"       # fast subset
./run.sh -k unplug -q
```

## Requirements / environment

The DUT host already has `grpcio`, `protobuf` and `pyyaml` (system packages). It
is **offline** and has **no venv** (`ensurepip` missing), so `pytest` is shipped
as universal wheels in `wheels/` and installed into a local `.pydeps` dir by
`run.sh` (no venv, no system pollution). Emulator proto stubs are vendored in
`lib/proto/`.

The default test module is `Ethernet100` (emulator index 25); override with
`XCVRD_TEST_PORT=EthernetN`.

> Note: presence-removal and re-plug behavior require the emulator's
> presence-aware `Read` (branch `fix/read-honor-presence`) to be built into the
> deployed image; otherwise xcvrd re-reads a still-served EEPROM and the removal
> won't stick. Deploy with `XCVR_EMU_BRANCH=fix/read-honor-presence`.
