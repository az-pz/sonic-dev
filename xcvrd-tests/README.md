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

## v1 scope

- `tests/test_presence.py` — plug/unplug clears+restores `TRANSCEIVER_INFO`; `TRANSCEIVER_STATUS_SW` plug state + `cmis_state` READY.
- `tests/test_info_content.py` — static identity matches the emulator (vendor, PN, OUI, serial, type, power class); all admin-up present ports populated.
- `tests/test_dom.py` — DOM table present; raw temperature/voltage writes propagate after refresh (`slow`).
- `tests/test_interaction_trace.py` — xcvrd really polls the module and re-reads on plug (Monitor trace).
- `tests/test_golden.py` — STATE_DB projection matches the committed golden baseline (conformance gate).

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

Error injection uses a **gated** bridge hook: only when the deploy drops a
`.test_hooks` marker next to the bridge does `chassis.get_change_event` read a
single STATE_DB hash `XCVR_EMU_INJECT` (field = physical index, value = SfpBase
error bitmap) with one `HGETALL` and surface that event as a real platform would.
Without the marker the hook is fully inert — **no STATE_DB access at all** — so a
production/virtual platform pays zero cost and carries no test backdoor. The
testbed enables it via `EMU_TEST_HOOKS=1` (default in `setup-sonic-testbed.sh`);
a clean platform deploy leaves it off.

## Golden baseline (conformance mode)

`golden/<port>.json` is the reference xcvrd's normalized STATE_DB projection
(`TRANSCEIVER_INFO` + `TRANSCEIVER_STATUS_SW` + `TRANSCEIVER_DOM_THRESHOLD`, with
volatile timestamps stripped). `test_golden.py` asserts a candidate xcvrd
reproduces it — the gate for a future (e.g. Rust) reimplementation.

```bash
# capture/refresh the golden from the current reference xcvrd, then commit golden/*.json
./run.sh --capture-golden -k test_state_matches_golden
# normal run compares against the committed golden
./run.sh -k test_state_matches_golden
```

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
