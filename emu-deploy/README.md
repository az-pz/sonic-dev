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
**not** a feature, so the emulator container is left untouched by a reload. This
means the emulated optics stay up across reloads; when pmon restarts, supervisord
brings `xcvrd` back and it simply reconnects to the still-running emulator over
gRPC. (Previously the emulator ran inside pmon and was killed on every reload.)

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
       └─ xcvrd  (supervised; PYTHONPATH=/opt/xcvr-emu-bridge,
                  XCVR_EMU_ADDR=localhost:50051 -> reads bridge ->
                  writes TRANSCEIVER_INFO / DOM to STATE_DB)
```

`xcvrd` runs inside pmon **under pmon's supervisord** (`autorestart=true`),
launched with `PYTHONPATH=/opt/xcvr-emu-bridge` and `XCVR_EMU_ADDR=localhost:50051`
baked into the program's `environment=`. The supervisor drop-in lives at
`/etc/supervisor/conf.d/xcvr-emu.conf`; pmon's main supervisord includes
`conf.d/*.conf` and only regenerates its own `supervisord.conf`, so the drop-in
(and `/opt`) **survive a pmon restart**. (A full pmon *recreation* — reboot /
image change — still wipes `/opt` + the drop-in; re-run the `emulator` deploy
after that. The emulator container itself survives via `--restart unless-stopped`.)

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
| `supervisor/xcvr-emu.conf`  | supervisord program for **xcvrd** (autorestart); vanilla `/usr/local/bin/xcvrd` with `PYTHONPATH=/opt/xcvr-emu-bridge` exported |
| `build_bundle.sh`   | assemble `emu-bundle.tar.gz` (bridge `sonic_platform` + `xcvr_emu` proto stubs + supervisor + config) |
| `deploy_on_dut.sh`  | (runs on DUT) `docker load` + run the emulator container, install the bridge into pmon, register the xcvrd supervisord program, verify |
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
