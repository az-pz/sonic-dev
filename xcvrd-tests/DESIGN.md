# xcvrd Black-Box Test Suite — Design

> Keep this document in sync with the code. When you change the harness, the
> bridge hook, the emulator contract, the fixtures, or add/remove tests, update
> the relevant section here (especially §4 "Pre-existing classes changed",
> §5 "Harness classes", and §7 "Suite at a glance").

## 1. Philosophy

The suite treats **xcvrd as an opaque daemon** and the current upstream xcvrd as
the **reference/oracle**. It never imports or patches xcvrd. It interacts only
through xcvrd's real boundaries:

- **Stimulus (inputs):** the `xcvr-emu` emulator (plug/unplug, raw EEPROM
  writes), a STATE_DB error-injection row, and `sfputil` control commands.
- **Observation (outputs):** the STATE_DB tables xcvrd writes, **plus** the
  emulator's `Monitor` stream — a live trace of every EEPROM read/write xcvrd
  performs.

Everything runs **on the DUT** (`admin@vlab-01`), so the emulator gRPC,
`sonic-db-cli`, and the pmon supervisor are all local — no SSH hops inside test
logic.

## 2. How it works (architecture)

```
                          DUT host (vlab-01) — pytest runs here
    +-------------------------------------------------------------------------+
    |  pytest + conftest.py   (clean baseline, per-test health guards)        |
    |                                                                         |
    |  lib/  EmulatorClient   MonitorRecorder   StateDB   XcvrdControl        |
    |        (emu.py)         (monitor.py)     (statedb) (xcvrd_ctl)          |
    |        ErrorInjector    sfputil helper                                  |
    |        (inject.py)      (sfputil.py)                                    |
    +-------------------------------------------------------------------------+
         |                 |                    |                 |
         | gRPC :50051     | HSET XCVR_EMU_     | HGETALL         | supervisorctl
         | plug/unplug,    |  INJECT (sonic-    | (sonic-db-cli)  | restart/flush
         | raw EEPROM w/r  |  db-cli)           |                 |
         v                 v                    v                 v
   +--------------+   +---------------------------------+   +------------------+
   | xcvr-emu     |   |            redis                |   |      pmon        |
   | container    |   |  STATE_DB:                      |   |                  |
   | (--network   |   |   TRANSCEIVER_INFO/DOM/         |   |  xcvrd  <-- SUT  |
   |   host,      |   |   STATUS_SW/...                 |   |   |  (untouched) |
   |   :50051)    |   |   XCVR_EMU_INJECT hash (gated)  |   |   v              |
   |              |   +---------------------------------+   |  sonic_platform  |
   | xcvr-emud    |            ^          ^                 |  bridge          |
   |  Read/Write  |            |          |                 |  (Chassis/Sfp)   |
   |  UpdateInfo  |            |          |                 +------------------+
   |  List        |            |          |                        |    |
   |  Monitor     |<-----------|----------|--- gRPC Read/Write -----+    |
   |    stream    |            |          +--- reads XCVR_EMU_INJECT -----+
   +--------------+            |
         |                     +--- xcvrd publishes TRANSCEIVER_* --- (from pmon)
         |
         +--- Monitor stream (every read/write) --> MonitorRecorder (in lib/)
```

**The three stimulus -> observation loops**

1. **Presence / DOM / identity** — `EmulatorClient` plugs/unplugs or writes raw
   EEPROM -> the bridge feeds xcvrd -> xcvrd updates STATE_DB -> `StateDB`
   asserts.
2. **Errors** — `ErrorInjector` writes `XCVR_EMU_INJECT` -> the bridge's
   `get_change_event` surfaces the error bitmap -> xcvrd sets
   `STATUS_SW.error` / removes DOM -> `StateDB` asserts.
3. **Interaction trace + control** — `MonitorRecorder` captures the exact EEPROM
   reads/writes xcvrd and sfputil issue (e.g. reset -> `00h:26 = 0x08`).

## 3. Where it sits on the DUT

```
Windows (dev) --ssh--> bf3-host3 --docker exec mgmt--> ssh admin@10.250.0.101
                                                                |
     +----------------------------------------------- DUT host (vlab-01) ------+
     |  pytest + lib/   (grpc, sonic-db-cli, sudo sfputil)   <-- TESTS RUN HERE|
     |      | gRPC :50051        | sonic-db-cli            | supervisorctl      |
     |  +---v--------+     +------v-------+          +------v------+            |
     |  | xcvr-emu   |     |  redis       |          |   pmon      |            |
     |  | container  |<--->|  STATE_DB    |<-------->| xcvrd+bridge|            |
     |  +------------+ gRPC+--------------+   reads  +-------------+            |
     +------------------------------------------------------------------------+
```

The test process is a **peer of the system under test on the same host**: it
pokes the emulator and STATE_DB, and watches STATE_DB + the Monitor stream —
exactly the surfaces a real operator / monitoring system would use.

## 4. Pre-existing classes changed (to make black-box testing possible)

| Class / file | Change | Why |
|---|---|---|
| **`Chassis`** — `platform/sonic_platform/chassis.py` (the bridge) | `get_change_event()` rewritten from a **no-op stub** -> real presence-diff (v1) -> **unified event path with error injection** (v2). Added `_desired_events()`, `_read_injections()`, `_get_statedb()`; `_presence_cache` -> `_event_cache`. | The stub never told xcvrd about unplug/error events, so hot-plug and error tests were impossible. The bridge now reports insert/remove/error events (errors read from the `XCVR_EMU_INJECT` STATE_DB hook). |
| **`EmulatorServer.Read`** — `xcvr-emu/src/xcvr_emu/server.py` | `Read` now honors module **presence / `force`** (returns `UNAVAILABLE` for an absent module). | Without it, xcvrd re-read a still-served EEPROM after unplug and re-added the port, so removals never "stuck." (On branch `fix/read-honor-presence`, merged into `sonic-dev`.) |

> **xcvrd itself is unchanged** — it is the reference implementation. The bridge
> error-injection read is **gated off by default**: it runs only when the deploy
> drops a `.test_hooks` marker next to the bridge (`EMU_TEST_HOOKS=1`). Without
> the marker, `get_change_event` performs **no STATE_DB access at all**, so a
> production/virtual platform pays zero cost and carries no backdoor. When
> enabled, it reads a single `XCVR_EMU_INJECT` hash with one `HGETALL` (never a
> `KEYS` scan).

## 5. Harness classes (created for the suite)

| Class / module | Role |
|---|---|
| `EmulatorClient` (`lib/emu.py`) | gRPC stimulus: plug/unplug, raw EEPROM read/write, module state |
| `MonitorRecorder` / `Event` (`lib/monitor.py`) | Background subscriber to the emulator `Monitor` stream = the xcvrd<->hardware trace |
| `StateDB` (`lib/statedb.py`) | Observation via `sonic-db-cli` (NUL-padding-safe parsing) + `hset`/`delete` for setup |
| `XcvrdControl` (`lib/xcvrd_ctl.py`) | Daemon lifecycle + `wait_healthy()` (flush + restart + verify repopulation) |
| `ErrorInjector` (`lib/inject.py`) | Writes / clears the `XCVR_EMU_INJECT` hook table |
| `errors.py` | SfpBase error-bit model (bitmaps + descriptions) |
| `cmis.py` | CMIS field offsets (temp/vcc DOM, ModuleGlobalControls 00h:26) + codecs |
| `sfputil.py` | `sfputil` reset / lpmode control stimulus (via sudo) |
| `waits.py` | `eventually()` / `wait_until()` / `stays()` polling helpers |
| `golden.py` | Capture / diff of the reference STATE_DB projection |
| `Module` + fixtures (`conftest.py`) | Per-module view + clean-baseline / health-guard / isolation wiring |

## 6. Test lifecycle (setup -> act -> observe -> teardown)

```
Session setup (autouse, once)
  clear injects  ->  plug all modules  ->  FLUSH TRANSCEIVER_*  ->
  restart xcvrd  ->  REQUIRE repopulation (else fail the whole suite)

Per test
  guard:   assert xcvrd RUNNING (fail fast if it died) + clear Monitor window
  act:     emulator plug/unplug/write | ErrorInjector | sfputil
  observe: eventually(...) over STATE_DB + Monitor trace
  teardown: replug + restore DOM bytes + clear injects (+ lpmode off)
```

The **flush-and-verify baseline** is the key robustness property. Because
`TRANSCEIVER_*` rows persist in Redis across an xcvrd stop, a naive read-only
test would false-pass on stale residue. The baseline removes that residue and
proves the daemon is live, so a broken pipeline **fails fast with zero false
passes**. Validated:

- healthy -> all pass
- xcvrd stopped -> self-heals (flush + restart) + warns, all pass on fresh data
- emulator down -> fail fast (errors) with **zero false passes**

### Calibrated timeouts

Every wait uses a named tier from `lib/waits.py`, calibrated against the
reference xcvrd on the KVM testbed (measured with `tools/measure_timeouts.py`). Two
regimes dominate: STATE_DB reactions to presence/info/status/cmis/error settle
in **<4s**, while the DOM sensor and steady-state EEPROM reads are paced by
xcvrd's **~60s poll cadence**. Each tier sits a few x above the observed max so a
correct-but-slow xcvrd still passes, but a broken one fails quickly instead of
burning 60-120s in the dev loop.

| Tier | Value | Real max | Used for |
|---|---|---|---|
| `T_FAST` | 15s | ~3.3s | info populate/clear, status 0/1, cmis READY, error set/clear, DOM removal |
| `T_MULTI` | 25s | ~3.3s x N | the same fast reaction aggregated across several ports |
| `T_BURST` | 25s | ~3s | plug-triggered identity re-read burst; sfputil reset/lpmode monitor capture |
| `T_DOM` | 80s | ~59s | DOM sensor appear/refresh/restore + steady-state read cadence (~60s poll) |
| `T_BASELINE` | 30s | ~3s + restart | flush TRANSCEIVER_* + restart xcvrd + repopulate INFO |

`T_DOM` is the floor set by xcvrd's DOM poll interval, not test slack: a DOM
value cannot be confirmed wrong until one full cadence elapses, so DOM-gated
tests (and their failures) are inherently ~60-80s. Everything else fails in ~15s.
Re-run `tools/measure_timeouts.py` and adjust the tiers if the reference xcvrd's timing
changes.

### Negative control (mutation test)

`tools/inject_dummy_xcvrd.sh` is the end-to-end proof that a green board means
"xcvrd works," not "stale rows survived." It backs up the real `xcvrd` and swaps
in a **fake-healthy** dummy, then restarts it:

- `inject`  — install the fake-healthy xcvrd and restart it
- `restore` — put the real xcvrd back
- `status`  — show which one is active

The dummy writes *bogus* `TRANSCEIVER_INFO` for every port (via `sonic-db-cli`)
so the flush-and-verify baseline accepts it as live and lets the real test bodies
RUN — but it does nothing else (no emulator reads, no presence/DOM/CMIS/Monitor
activity). Result on the DUT: **22 failed, 4 passed**. Every xcvrd-dependent test
FAILS on its real assertion; the 4 passes are expected — `test_xcvrd_running`
(the dummy is genuinely RUNNING) and `test_lpmode_reset` ×3, which drive the
module through the sfputil/platform path rather than xcvrd. Restoring makes the
suite green again.

## 7. Suite at a glance

| File | Tests | Asserts |
|---|---|---|
| `test_health.py` | 2 | xcvrd running; baseline populated by a live daemon |
| `test_presence.py` | 6 | plug/unplug clears+restores INFO; `STATUS_SW.status` 1/0; `cmis_state` READY |
| `test_info_content.py` | 5 | identity matches emulator (vendor/PN/OUI/serial/type/power class); all admin-up ports populated |
| `test_dom.py` | 3 | DOM table present; raw temp/voltage writes propagate after refresh (`slow`) |
| `test_interaction_trace.py` | 3 | xcvrd polls the module + re-reads on plug (Monitor stream) |
| `test_status_error.py` | 3 | inject I2C-stuck / bad-EEPROM (blocking) / high-temp (non-blocking): `STATUS_SW.error` set, DOM removed for blocking (INFO kept), DOM kept for non-blocking, recovery clears |
| `test_lpmode_reset.py` | 3 | `sfputil reset` -> SoftwareReset `00h:26=0x08`; `lpmode on` -> LowPwr `0x10` on the Monitor trace; lpmode show On/Off |
| `test_multiport.py` | 3 | concurrent multi-port: simultaneous unplug/replug all clear+restore; partial unplug leaves other ports intact; distinct DOM writes per port show no cross-talk (per-module isolation) |
| `test_golden.py` | 1 | STATE_DB projection matches the committed golden baseline (conformance gate) |
| **Total** | **29** | all green on the DUT |

## 8. Running

On the DUT (`admin@vlab-01`), from a copy of this folder:

```bash
./run.sh                     # full suite
./run.sh -m "not slow"       # skip the ~60s DOM refresh tests
./run.sh -k presence -q      # subset
./run.sh --capture-golden -k test_state_matches_golden   # refresh golden/*.json
```

From a full `sonic-develop` checkout on the VM host (ships to the DUT + runs):

```bash
cd dev && ./setup-sonic-testbed.sh xcvrd_tests
./setup-sonic-testbed.sh xcvrd_tests -- -m "not slow"
```

## 9. Environment notes

- DUT host already has `grpcio` + `protobuf` + `pyyaml` (system). It is
  **offline** and has **no venv** (`ensurepip` missing), so `pytest` ships as
  universal wheels in `wheels/` and installs into a local `.pydeps` dir.
- Emulator proto stubs are vendored in `lib/proto/`.
- Default test module is `Ethernet100` (emulator index 25); override with
  `XCVRD_TEST_PORT=EthernetN`. Port<->index mapping is `Ethernet{index*4}`.
- Presence-removal + error-recovery require the emulator's presence-aware `Read`
  (branch `fix/read-honor-presence`, merged to `sonic-dev`) in the deployed
  image. Build with `XCVR_EMU_BRANCH=sonic-dev` (default) or the fix branch.

## 10. Not covered yet / future

- Emulator NUL-pads CMIS vendor strings (real modules space-pad); the harness
  strips NULs. Changing the emulator's string encoding is a **behavior change to
  discuss** before implementing.
- Port config-change handling (CONFIG_DB add/remove logical port).
- Wiring `xcvrd_tests` into an e2e / CI gate.
