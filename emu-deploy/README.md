# emu-deploy — run xcvrd + host sfputil against emulated CMIS optics on the KVM DUT

This wires the [xcvr-emu](https://github.com/ishidawataru/xcvr-emu) CMIS
transceiver emulator into a stock `sonic-vs` DUT at runtime, so the transceiver
control plane works end-to-end with **no real optics** — both the in-container
`xcvrd` **and** the host-side `sfputil`/`sfpshow` talk to the emulator.

With this deployed, these all pass on `vlab-01`:
`test_xcvr_info_in_db`, `test_sfpshow` (presence/eeprom),
`test_check_sfputil_presence`/`eeprom`/**`reset`**.

## Why this is needed
Stock `sonic-platform-vs` is a bare virtual-chassis stub: its `sonic_platform`
ships only `platform.py`/`chassis.py` (no SFP), and `pmon_daemon_control.json`
sets `skip_xcvrd: true`. So `xcvrd` never runs and host `sfputil` can't
instantiate a Chassis. Our bridge (`Sfp(SfpOptoeBase)` → gRPC to the emulator)
is exactly the missing "vendor SFP" implementation.

## The native deploy — 3 changes + the emulator (all at runtime, no image rebuild)
```
DUT vlab-01 (host OS)
 ├─ xcvr-emu container   docker run --network host --restart unless-stopped
 │     xcvr-emud -c /emu_config.yaml   (gRPC :50051, 33 QSFP-DD modules)
 │        ▲ localhost:50051 (host netns, shared with pmon)
 ├─ HOST /usr/lib/python3/dist-packages/sonic_platform  := our bridge   (1)
 │     └─ host sfputil / sfpshow -> emulator
 └─ pmon container
       ├─ /usr/local/lib/python3.13/dist-packages/sonic_platform := our bridge  (3)
       └─ xcvrd  (enabled via skip_xcvrd=false)  (2)  -> emulator -> STATE_DB
```
1. **HOST `sonic_platform` := our bridge** (stock backed up to `sonic_platform.orig`) — fixes host `sfputil`/`sfpshow`, incl. `reset`.
2. **`skip_xcvrd` → false** in `device/x86_64-kvm_x86_64-r0/pmon_daemon_control.json` (backed up to `.orig`) → pmon's native supervisord runs `xcvrd`.
3. **Inject the bridge into pmon** `dist-packages` so native `xcvrd` imports it.

`reset()` needs no custom code: the stock `SfpOptoeBase.reset()` → `CmisApi.reset()`
writes the CMIS **SoftwareReset** register through our `write_eeprom` → the
emulator honors it. Key bridge detail: `Chassis.get_change_event()` reports a
stable plant so xcvrd's threads don't crash. The package `__init__.py` binds the
`platform`/`chassis`/`sfp` submodules so `sfputil`'s `sonic_platform.platform.Platform()`
attribute access works.

> Runtime, not baked-in: a DUT **reboot/re-image or `config reload`-triggered pmon
> recreation** wipes the pmon injection (and a re-image wipes the host changes too).
> Re-run the deploy (`setup-sonic-testbed.sh emulator`, or it's part of `rebuild`).

## Files
| File | Purpose |
|------|---------|
| `gen_emu_config.py` | generate `emu_config.yaml` with N present QSFP-DD modules |
| `emu_config.yaml`   | 33 present modules (indices 0..32) |
| `kvm_platform.json` | `chassis.sfps` inventory (32×40G) installed as the platform's `platform.json` — required by `platform_tests/api/test_sfp.py` (`duthost.facts["chassis"]["sfps"]`) |
| `build_emu_image.sh`| build `xcvr-emu:local` from the repo Dockerfile → cached `xcvr-emu-image.tar.gz` |
| `build_bundle.sh`   | assemble `emu-bundle.tar.gz` (bridge `sonic_platform` + `xcvr_emu` proto stubs + config) |
| `deploy_on_dut.sh`  | (runs on DUT) the native deploy: emulator container + host platform + skip_xcvrd + pmon inject |
| `revert_on_dut.sh`  | (runs on DUT) restore stock host platform + skip_xcvrd, remove pmon inject, restart pmon |
| `ship_and_deploy.sh`| (runs on VM) ship image+bundle+scripts to the DUT and run `deploy_on_dut.sh` (detached + polled) |
| `ship_and_revert.sh`| (runs on VM) ship + run `revert_on_dut.sh` |

## Usage (via the top-level helper — recommended)
```bash
cd dev
./setup-sonic-testbed.sh emulator          # native deploy (also runs as part of `all` and `rebuild`)
./setup-sonic-testbed.sh transceiver_tests  # sfpshow + sfputil presence/eeprom/reset  -> all PASS
./setup-sonic-testbed.sh transceiver_emu_test   # test_xcvr_info_in_db -> PASS
./setup-sonic-testbed.sh emulator_revert    # undo it (restore stock platform)
```
The emulator now deploys automatically as part of a full `./setup-sonic-testbed.sh`
run and during `rebuild` (post-wipe recovery).
