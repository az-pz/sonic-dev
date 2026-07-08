# emu-deploy — run xcvrd against emulated CMIS optics on the KVM DUT

This runs the [xcvr-emu](https://github.com/ishidawataru/xcvr-emu) CMIS
transceiver emulator as a **standalone Docker container on the DUT** and installs
a `sonic_platform` gRPC bridge into the DUT's `pmon` container, so `xcvrd`
populates `TRANSCEIVER_INFO` and `TRANSCEIVER_DOM_SENSOR` in `STATE_DB` — exactly
what transceiver tests such as `platform_tests/test_xcvr_info_in_db.py` require.

Nothing here modifies the cloned SONiC repos, and nothing is written into pmon's
system `dist-packages`. The bridge is placed in a side directory
(`/opt/xcvr-emu-bridge`) inside pmon and loaded via `PYTHONPATH` — fully
reversible and rebuilt whenever pmon is recreated.

## Why the emulator is a separate container

sonic-mgmt tests frequently trigger a SONiC `config reload`, which restarts every
SONiC **feature** container (pmon/swss/syncd/…). A plain `docker run` container is
**not** a feature, so the emulator container is left untouched by a reload — the
emulated optics stay up across reloads. (Previously the emulator ran inside pmon
and was killed on every reload.) Note that `xcvrd` itself is currently launched as
a plain process (no supervisord), so it does not auto-survive a reload; re-run the
deploy to bring it back. See the note under *Architecture* below.

## Architecture

```
DUT vlab-01 (docker host)
 ├─ xcvr-emu  container   docker run --network host --restart unless-stopped
 │     xcvr-emud -c /emu_config.yaml   (gRPC :50051, 33 QSFP-DD modules)
 │        ▲  gRPC localhost:50051 (shared host netns)
 │        │
 └─ pmon   container   (--network host)
       ├─ /opt/xcvr-emu-bridge/     (our python, loaded via PYTHONPATH — NOT dist-packages)
       │    ├─ sonic_platform/      (bridge: SfpOptoeBase -> gRPC Read/Write/GetInfo)
       │    └─ xcvr_emu/proto/      (gRPC client stubs the bridge imports)
       └─ xcvrd  (launched via `docker exec -d` with
                  PYTHONPATH=/opt/xcvr-emu-bridge, XCVR_EMU_ADDR=localhost:50051
                  -> reads bridge -> writes TRANSCEIVER_INFO / DOM to STATE_DB)
```

`xcvrd` is launched **directly** by `deploy_on_dut.sh` via `docker exec -d` with
`PYTHONPATH=/opt/xcvr-emu-bridge` and `XCVR_EMU_ADDR=localhost:50051` exported
into its environment (so it imports our `sonic_platform` bridge without touching
pmon's dist-packages). **No supervisord program is installed.**

> ⚠️ Because xcvrd is a plain process (not supervised), it does **not** survive a
> `config reload` / pmon restart — those kill it and it stays down until you
> re-run the deploy. The emulator *container* still survives (it's a separate
> `--restart unless-stopped` container). A full pmon *recreation* also wipes
> `/opt`. Re-run the `emulator` deploy to bring xcvrd back after any of these.

Key bridge detail: `Chassis.get_change_event()` is implemented to report a
stable plant (no hotplug). Without it xcvrd falls back to
`platform_sfputil.get_transceiver_change_event()` which is `None` on this
emulated platform, crashing every xcvrd thread (so DOM never populates).

## Files

| File | Purpose |
|------|---------|
| `gen_emu_config.py` | generate `emu_config.yaml` with N present QSFP-DD modules |
| `emu_config.yaml`   | 33 present modules (indices 0..32) |
| `build_emu_image.sh`| build `xcvr-emu:local` from the repo Dockerfile, `docker save|gzip` → `xcvr-emu-image.tar.gz` (cached) |
| `build_bundle.sh`   | assemble `emu-bundle.tar.gz` (bridge `sonic_platform` + `xcvr_emu` proto stubs + config) |
| `deploy_on_dut.sh`  | (runs on DUT) `docker load` + run the emulator container, install the bridge into pmon, launch xcvrd via `docker exec -d` with the env exported, verify |
| `ship_and_deploy.sh`| (runs on VM) ship the image + bundle to the DUT and run deploy_on_dut.sh |

## Usage

```bash
# 1) locally (has the xcvr-emu repo + bridge):
cd dev/emu-deploy
./build_emu_image.sh          # -> xcvr-emu-image.tar.gz  (cached; EMU_REBUILD_IMAGE=1 to force)
./build_bundle.sh             # -> emu-bundle.tar.gz

# 2) on the VM (testbed host):
./ship_and_deploy.sh          # docker load emulator container + install bridge + start xcvrd

# 3) run the target test (from the mgmt container, conn graph injected):
#    see setup-sonic-testbed.sh -> inject_conn_graph, then pytest
```

Or in one shot via the top-level helper:

```bash
cd dev && ./setup-sonic-testbed.sh emulator_e2e
```

Result: `platform_tests/test_xcvr_info_in_db.py::test_xcvr_info_in_db` PASSES, and
the emulator survives the `config reload` that sonic-mgmt tests trigger.
