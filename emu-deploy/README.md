# emu-deploy — run xcvrd against emulated CMIS optics on the KVM DUT

This deploys the [xcvr-emu](https://github.com/ishidawataru/xcvr-emu) CMIS
transceiver emulator plus a `sonic_platform` gRPC bridge into the DUT's `pmon`
container, then launches `xcvrd` so it populates `TRANSCEIVER_INFO` and
`TRANSCEIVER_DOM_SENSOR` in `STATE_DB` — exactly what transceiver tests such as
`platform_tests/test_xcvr_info_in_db.py` require.

Nothing here modifies the cloned SONiC repos. The bridge + emulator are copied
into the pmon container at runtime (disposable; rebuilt whenever pmon is
recreated).

## Architecture

```
pmon container (on DUT vlab-01)
 ├─ xcvr-emud            (python3 -m xcvr_emu.xcvr_emud, gRPC :50051, 33 QSFP-DD)
 ├─ sonic_platform/      (bridge: SfpOptoeBase -> gRPC Read/Write/GetInfo)
 │    Sfp(i)  <->  emulator module i  <->  Ethernet(i*4)
 └─ xcvrd                (reads platform API -> writes TRANSCEIVER_INFO/DOM)
```

Key bridge detail: `Chassis.get_change_event()` is implemented to report a
stable plant (no hotplug). Without it xcvrd falls back to
`platform_sfputil.get_transceiver_change_event()` which is `None` on this
emulated platform, crashing every xcvrd thread (so DOM never populates).

## Files

| File | Purpose |
|------|---------|
| `gen_emu_config.py` | generate `emu_config.yaml` with N present QSFP-DD modules |
| `emu_config.yaml`   | 33 present modules (indices 0..32) |
| `build_bundle.sh`   | assemble `emu-bundle.tar.gz` from the bridge + xcvr-emu repo |
| `deploy_on_dut.sh`  | (runs on DUT) install into pmon, start emud, wait, start xcvrd, verify |
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
