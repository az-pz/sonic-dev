# emu-deploy — run xcvrd against emulated CMIS optics on the KVM DUT

This deploys the [xcvr-emu](https://github.com/ishidawataru/xcvr-emu) CMIS
transceiver emulator plus a `sonic_platform` gRPC bridge into the DUT's `pmon`
container, then launches `xcvrd` so it populates `TRANSCEIVER_INFO` and
`TRANSCEIVER_DOM_SENSOR` in `STATE_DB` — exactly what transceiver tests such as
`platform_tests/test_xcvr_info_in_db.py` require.

Nothing here modifies the cloned SONiC repos, and nothing is written into pmon's
system `dist-packages`. The bridge + emulator are placed in a side directory
(`/opt/xcvr-emu-bridge`) inside pmon and loaded via `PYTHONPATH` — fully
reversible and rebuilt whenever pmon is recreated.

## Architecture

```
pmon container (on DUT vlab-01)
 ├─ /opt/xcvr-emu-bridge/     (our python, loaded via PYTHONPATH — NOT dist-packages)
 │    ├─ xcvr_emu/ + cmis/    (the emulator packages)
 │    └─ sonic_platform/      (bridge: SfpOptoeBase -> gRPC Read/Write/GetInfo)
 │
 ├─ xcvr-emud                 (python3 -m xcvr_emu.xcvr_emud, gRPC :50051, 33 QSFP-DD)
 │    Sfp(i)  <->  emulator module i  <->  Ethernet(i*4)
 └─ xcvrd                     (PYTHONPATH=/opt/xcvr-emu-bridge; reads bridge ->
                               writes TRANSCEIVER_INFO / DOM to STATE_DB)
```

Both `xcvr-emud` and `xcvrd` run inside pmon **under pmon's supervisord**
(`autorestart=true`), launched with `PYTHONPATH=/opt/xcvr-emu-bridge` and
`XCVR_EMU_ADDR=localhost:50051` baked into each program's `environment=`. The
supervisor drop-in lives at `/etc/supervisor/conf.d/xcvr-emu.conf`; pmon's main
supervisord includes `conf.d/*.conf` and only regenerates its own
`supervisord.conf`, so the drop-in (and `/opt`) **survive a pmon restart** — which
is exactly what a SONiC `config reload` triggers. sonic-mgmt tests reload config
frequently; without supervisord the manually-started daemons would be killed on
every reload. With it, they auto-restart and `TRANSCEIVER_INFO`/`DOM` repopulate
on their own. (A full container *recreation* — reboot / image change — still
wipes `/opt` + the drop-in; re-run the `emulator` deploy after that.)

Key bridge detail: `Chassis.get_change_event()` is implemented to report a
stable plant (no hotplug). Without it xcvrd falls back to
`platform_sfputil.get_transceiver_change_event()` which is `None` on this
emulated platform, crashing every xcvrd thread (so DOM never populates).

## Files

| File | Purpose |
|------|---------|
| `gen_emu_config.py` | generate `emu_config.yaml` with N present QSFP-DD modules |
| `emu_config.yaml`   | 33 present modules (indices 0..32) |
| `supervisor/xcvr-emu.conf`  | supervisord programs for emud + xcvrd (autorestart) |
| `supervisor/start-xcvrd.sh` | xcvrd wrapper that waits for the emulator before launching |
| `build_bundle.sh`   | assemble `emu-bundle.tar.gz` from the bridge + xcvr-emu repo |
| `deploy_on_dut.sh`  | (runs on DUT) install into /opt, register supervisord programs, verify |
| `ship_and_deploy.sh`| (runs on VM) ship bundle to DUT and run deploy_on_dut.sh |

## Usage

```bash
# 1) locally (has the xcvr-emu repo + bridge): build the bundle
cd dev/emu-deploy && ./build_bundle.sh            # -> emu-bundle.tar.gz

# 2) copy emu-bundle.tar.gz to the VM /tmp, then on the VM:
./ship_and_deploy.sh                              # deploys + starts emud + xcvrd

# 3) run the target test (from the mgmt container, conn graph injected):
#    see setup-sonic-testbed.sh -> inject_conn_graph, then pytest
```

Result: `platform_tests/test_xcvr_info_in_db.py::test_xcvr_info_in_db` PASSES.
