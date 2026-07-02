# sonic-xcvrd dev environment

Local, throwaway tooling to build and test **sonic-xcvrd** in Docker without
touching the cloned repos. Nothing here is committed back into the repos.

The image (`sonic-xcvrd-dev`) uses the **real** SWIG/C++ swss-common Python
bindings, installed from SONiC's prebuilt Debian packages.

## Layout

```
toRust/
├── sonic-platform-daemons/   # cloned repo with xcvrd (pristine, read-only at test time)
├── sonic-platform-common/    # cloned dependency repo (its sonic_xcvr tests run too)
├── sonic-swss-common/        # cloned dependency repo (reference only)
├── xcvr-emu/                 # cloned CMIS transceiver emulator (reference; installed via pip)
└── dev/                      # everything in this folder is ours
    ├── Dockerfile            # builds sonic-xcvrd-dev (REAL swsscommon, debian:trixie)
    ├── entrypoint.sh         # stands up a /dev/log syslog sink, then runs cmd
    ├── runtests              # in-container helper: clean test run (artifacts to /tmp)
    ├── run-tests.sh          # host: runs the xcvrd suite (repo mounted read-only)
    ├── run-tests-common.sh   # host: runs the sonic-platform-common sonic_xcvr suite
    ├── shell.sh              # host: opens an interactive shell in the container
    ├── emu-demo.sh           # host: end-to-end CmisApi-vs-emulator read demo
    ├── emu-demo.py           # the demo driver (runs inside the container)
    ├── emu-shell.sh          # host: shell with xcvr-emud running + bridge on PYTHONPATH
    ├── fetch-swsscommon.sh   # host: downloads prebuilt swss-common debs from SONiC CI
    ├── platform/             # the sonic_platform bridge plugin (gRPC -> xcvr-emu)
    │   └── sonic_platform/   #   Platform / Chassis / Sfp(SfpOptoeBase) + emu_client
    └── vendor/
        ├── sonic-py-common/          # vendored from sonic-buildimage (not on PyPI)
        ├── sonic-config-engine-stub/ # tiny stub to satisfy a build-time guard
        └── debs/trixie-<arch>/       # prebuilt real swss-common .deb packages
```

## How dependencies are handled

The xcvrd tests / source need these Python packages:

| Package | How we provide it | Why |
|---|---|---|
| `swsscommon` | **real** SONiC deb (`python3-swsscommon`) | the genuine SWIG/C++ bindings; installed from the prebuilt deb (see below) |
| `sonic-platform-common` | pip from git `master` (`--no-deps`) | provides `sonic_platform_base.*` (CmisApi, Sff86xx); tests use it for real |
| `sonic-py-common` | vendored source, `pip --no-build-isolation` | not on PyPI; lives in sonic-buildimage |
| `sonic-config-engine` | **stub** distribution | only needed to satisfy sonic-platform-common's build-time guard |
| `pytest`, `pytest-cov`, `mock`, `natsort`, ... | pip | test tooling / runtime deps |

`/dev/log`: the SONiC `SysLogger` logs to a syslog socket that a slim image
lacks; `entrypoint.sh` creates a draining datagram sink there so logging does
not crash (CI containers already have one).

## Real swsscommon

The swss-common bindings are a compiled artifact, not on PyPI, so we use the
prebuilt Debian packages published by SONiC's public CI
(`Azure.sonic-swss-common` on `dev.azure.com/mssonic`, anonymous download).
`dev/fetch-swsscommon.sh` downloads them into `dev/vendor/debs/trixie-<arch>/`.

Why **trixie**: SONiC master targets Debian trixie, and trixie ships natively
every runtime dep the swss-common deb needs (`libyang3 3.12.2`,
`libboost-serialization1.83.0`, `libnl-3`, `libhiredis`, `libzmq5`) — so `apt`
resolves the whole closure with no extra SONiC dependency debs. (Bookworm ships
boost 1.74 and lacks libyang3, so it would need additional packages.)

`sonic-db-cli` and `redis-server` are included in the image, so you can run the
real library against a live Redis (handy for future integration testing).

## Usage

> Commands are written for **git bash** on Windows.

One-time: download the prebuilt swss-common debs (auto-detects your Docker arch):

```bash
dev/fetch-swsscommon.sh trixie          # -> dev/vendor/debs/trixie-<arch>/
```

Build the image:

```bash
docker build -t sonic-xcvrd-dev -f dev/Dockerfile dev
```

Run the full test suite (repo stays clean — mounted read-only, artifacts to /tmp):

```bash
dev/run-tests.sh
```

Run a subset / pass pytest args:

```bash
dev/run-tests.sh -k cmis -x
```

### sonic-platform-common tests (the CMIS/SFF API layer)

`sonic-xcvrd` is built on the `sonic_xcvr` transceiver APIs that live in
`sonic-platform-common`. That repo's `tests/sonic_xcvr/` suite (CMIS, c-CMIS,
SFF-8636/8472/8436, optoe base, CDB firmware, VDM, …) directly exercises the
layer xcvrd drives at runtime, so it's worth running alongside the xcvrd tests:

```bash
dev/run-tests-common.sh                                  # tests/sonic_xcvr (929 tests)
dev/run-tests-common.sh tests/sonic_xcvr/test_cmis.py    # a single module
dev/run-tests-common.sh tests/sonic_xcvr -k VDM          # forward pytest args
```

The cloned `sonic-platform-common` is mounted read-only and put first on
`PYTHONPATH`, so its source (not the copy baked into the image) is what's tested.
Artifacts go to `/tmp`; the repo stays pristine.

> The full `tests` directory also has storage tests (need `psutil`) and
> `sfputilhelper_test` (needs the real `sonic-config-engine`, which we stub).
> Those are unrelated to xcvrd and aren't installed, so stick to `tests/sonic_xcvr`.

### Interactive shell

Drop into a shell inside the container to browse code, edit, and run tests.
The repo is mounted read-write at `/work` and you start in the `sonic-xcvrd`
directory:

```bash
dev/shell.sh
```

Inside the shell:

```sh
runtests                # full suite; artifacts go to /tmp, repo stays clean
runtests -k cmis -x     # forward args to pytest
pytest ...              # plain pytest also works (writes gitignored coverage files)
ls xcvrd ; python3 -c "from swsscommon import swsscommon; print(swsscommon.__file__)"
```

Run a one-off command non-interactively instead of opening a shell:

```bash
dev/shell.sh python3 -c "import sonic_platform_base; print('ok')"
```

Latest result: **339 passed**, ~89% coverage.

## CMIS transceiver emulator (xcvr-emu)

[`xcvr-emu`](https://github.com/ishidawataru/xcvr-emu) is a software CMIS
transceiver emulator: it models the full paged register space, the module state
machine and per-datapath DPSMs, and exposes them over a gRPC `SfpEmulatorService`
on port `50051`. It's baked into the image (pinned commit; modern grpc/protobuf
since upstream pins an old grpcio with no py3.13 wheels).

`dev/platform/sonic_platform/` is a **bridge plugin** that lets the *real* SONiC
transceiver stack drive the emulator. xcvrd loads a platform via
`import sonic_platform.platform; Platform().get_chassis()`; our `Sfp` subclasses
`SfpOptoeBase` and implements the only three hardware hooks
(`read_eeprom` / `write_eeprom` / `get_presence`) by translating to the
emulator's gRPC `Read`/`Write`/`GetInfo`. The optoe *linear* offset SONiC uses is
inverted back into the emulator's `(bank, page, window-offset)` form (verified by
reading `VendorName` at `(0, 0, 129)`).

So the same `CmisApi` the unit tests exercise runs unchanged — only the byte
fetch underneath is the emulator instead of a mock or real hardware.

### Read demo

Drives the real `CmisApi` against an emulated 400G-DR4 module (starts its own
`xcvr-emud`):

```bash
dev/emu-demo.sh
```

Expected: it prints `manufacturer: xcvr-emu`, `cmis_rev: 5.2`, the 400GBASE-DR4 /
200GBASE-DR4 application advertisement, and a presence remove/insert toggle.

### Interactive emulator shell

Opens a shell with `xcvr-emud` already running (bundled `config.yaml`: modules
0–6 present) and the bridge on `PYTHONPATH`:

```bash
dev/emu-shell.sh
```

Inside it:

```sh
xcvr-emush                                   # the emulator's own interactive client
python3 -c "from sonic_platform.platform import Platform; \
            print(Platform().get_chassis().get_sfp(0).get_xcvr_api().get_transceiver_info())"
```

The emulator address is configurable via `XCVR_EMU_ADDR` (default
`localhost:50051`); the number of fallback SFPs via `XCVR_EMU_NUM_SFPS`.
