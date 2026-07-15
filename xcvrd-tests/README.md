# xcvrd black-box tests

Structured Python (pytest) tests that exercise **xcvrd** as a black box against
the `xcvr-emu` emulator. They run **on the DUT** (`admin@vlab-01`), where the
emulator gRPC (`localhost:50051`), `sonic-db-cli` and the pmon supervisor are all
local. The current upstream xcvrd is treated as the reference implementation.

## What it observes

| Channel | How | Used for |
|---------|-----|----------|
| STATE_DB | `sonic-db-cli` (`lib/statedb.py`) | xcvrd's declared outputs: `TRANSCEIVER_INFO/DOM_SENSOR/STATUS/...` |
| Emulator Monitor stream | gRPC (`lib/monitor.py`) | the xcvrd↔emulator interaction trace — every EEPROM read/write, zero-touch |
| Emulator gRPC | `lib/emu.py` | stimulus: plug/unplug, raw EEPROM read/write |
| xcvrd lifecycle | `supervisorctl` in pmon (`lib/xcvrd_ctl.py`) | restart / flush tables |

## v1 scope

- `tests/test_presence.py` — plug/unplug clears+restores `TRANSCEIVER_INFO`; `TRANSCEIVER_STATUS` plug state.
- `tests/test_info_content.py` — static identity matches the emulator (vendor, PN, OUI, type, power class).
- `tests/test_dom.py` — DOM table present; raw temperature/voltage writes propagate after refresh (`slow`).
- `tests/test_interaction_trace.py` — xcvrd really polls the module and re-reads on plug (Monitor trace).

No changes to xcvrd or the emulator. (Error-injection + lpmode/reset are v2; a
golden-baseline compare mode is planned next.)

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
