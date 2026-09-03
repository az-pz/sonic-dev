# sonic-dev

A self-contained SONiC virtual testbed for developing, testing and benchmarking the
transceiver daemon (`xcvrd`) — including an automated pipeline that translates it from
Python to Rust and grades the result.

Everything runs on one Linux host with nested virtualization: a KVM SONiC DUT, its
neighbor VMs, a `sonic-mgmt` container, and an emulated transceiver plant. No physical
optics required.

```
       setup-sonic-testbed.sh                the one entry point
                 |
   +-------------+--------------------------------+
   |             |                                |
 KVM testbed   xcvr-emu (emulated optics)   xcvrd under test
 vlab-01 DUT   via the sonic_platform         python  |  rust
 + 4 neighbors      gRPC bridge             (reversibly injected)
                                                   |
                                    graded by xcvrd-tests, measured by benchmark/
```

## Running it

`setup-sonic-testbed.sh` is the interface to the whole repo — 1 700 lines, ~34 phases,
each idempotent and individually re-runnable. Build the testbed from nothing:

```bash
./setup-sonic-testbed.sh                 # runs every setup phase in order
```

Then drive it a phase at a time:

```bash
./setup-sonic-testbed.sh emulator                              # deploy the emulated optics
./setup-sonic-testbed.sh xcvrd_tests                           # black-box suite vs stock python xcvrd
./setup-sonic-testbed.sh xcvrd_tests_rust recodeAgent/results/result_4
./setup-sonic-testbed.sh xcvrd_status                          # which daemon is live?
./setup-sonic-testbed.sh xcvrd_restore                         # un-strand an injected rust daemon
```

Tab completion:

```bash
eval "$(./setup-sonic-testbed.sh --completion bash)"
```

<details>
<summary><code>./setup-sonic-testbed.sh --help</code></summary>

```
setup-sonic-testbed.sh — one-shot, idempotent SONiC KVM virtual testbed

USAGE
  ./setup-sonic-testbed.sh [<phase>] [args...]
  ./setup-sonic-testbed.sh --help | --list-phases | --completion bash

  With no phase it runs all (every setup phase in order). Every phase is
  re-runnable on its own.

SETUP PHASES (in the order `all` runs them)
  all                        Run every phase in order (default when no phase is given)
  preflight                  Verify KVM/nested-virt, OS version and passwordless sudo
  install_prereqs            Install host packages, docker and python deps
  setup_storage              Lay out the big-disk storage under the DATA mount point
  clone_repo                 Clone/refresh the sonic-mgmt repo
  setup_mgmt_network         Create the mgmt bridge network for the testbed
  download_image             Download the sonic-vs DUT image
  setup_container            Start the docker-sonic-mgmt container
  setup_ssh                  Set up key-based SSH from the container to the vm_host
  start_vms                  Start the neighbor VMs (see VM_TYPE / NUM_VMS below)
  add_topo                   Deploy the topology (see TESTBED_NAME below)
  deploy_mg                  Deploy the minigraph/config to the DUT
  verify                     Verify the DUT is reachable and BGP sessions are up
  inject_conn_graph          Inject the connection graph used by the transceiver tests

TESTS
  smoke_test [test] [-v]                Run the BGP verification test
  run_pytest [--rust <folder>] <target> Run ARBITRARY sonic-mgmt pytest targets/args
  transceiver_tests [-v]                xcvrd/SFP tests that pass on a vs DUT
  transceiver_tests_all [-v]            Full validated set + the transceiver/eeprom suite
  transceiver_eeprom_tests [-v]         Declarative transceiver/eeprom suite
  transceiver_emu_test                  test_xcvr_info_in_db (needs the emulator)
  hotplug_test [PORT]                   Unplug/replug a module, assert xcvrd reacts
  xcvrd_tests [-- pytest args]          Ship xcvrd-tests/ to the DUT and run it there

EMULATOR (xcvr-emu)
  emulator                              Native deploy (bridge + pmon inject + container)
  emulator_revert                       Undo it, restore the stock platform
  emulator_e2e                          emulator + transceiver_emu_test in one go

RUST xcvrd / recodeAgent
  transceiver_tests_rust <folder>       Build+inject a Rust xcvrd, run, always restore
  transceiver_tests_all_rust <folder>   Same, FULL validated set
  xcvrd_tests_rust <folder>             Black-box suite against an injected Rust xcvrd
  transceiver_tests_noop                NEGATIVE CONTROL: no-op xcvrd; tests SHOULD fail
  transceiver_tests_all_noop            NEGATIVE CONTROL over the full set
  xcvrd_status / xcvrd_info             Which xcvrd is in pmon: PYTHON vs RUST (read-only)
  xcvrd_restore                         Restore stock Python if a Rust one is stranded

TEARDOWN & RECOVERY
  remove_topo                           Tear down the topology and stop the VMs
  rebuild                               Recover after a /mnt/data wipe

COMMON ENV OVERRIDES
  VERBOSE=1              Full tracebacks; same as the -v flag
  RESET_TESTS=0          Skip the SLOW module-reset tests
  DOM_UPDATE_INTERVAL=   DOM poll seconds for EVERY xcvrd, python or rust
  EMU_NO_SPECIAL=0       Also provision the 4 special emulator modules
  TESTBED_NAME / DUT / DUT_IP / VM_TYPE / NUM_VMS / EMU_MODULES / DATA ...
```

Run `--help` for the full, current text — the registry in the script is the source of
truth and this excerpt is trimmed.
</details>

## Layout

```
sonic-dev/
├── setup-sonic-testbed.sh   the entry point above — testbed lifecycle, test runners,
│                            emulator deploy, Rust inject/restore
├── platform/                sonic_platform gRPC bridge: the SONiC platform API
│                            (Chassis/Sfp) backed by the emulator instead of hardware.
│                            Installed onto the DUT so xcvrd talks to emulated optics.
├── emu-deploy/              deploying that bridge + the xcvr-emu container onto the DUT:
│                            module config generation, special-module provisioning,
│                            transceiver inventory, and a clean revert path
├── xcvr-emu/                submodule — the CMIS transceiver emulator itself
├── xcvrd-tests/             the black-box oracle: ~105 e2e tests that judge a daemon
│                            purely by what it writes to STATE_DB, so they grade the
│                            Python and Rust xcvrd identically. Ships to the DUT and
│                            runs there
├── recodeAgent/             the Python→Rust translation pipeline: agents, orchestrator,
│                            the reference xcvrd source, DUT build/inject tooling, and
│                            the produced translations under results/result_N
├── CodeWeaver/              submodule — the general-purpose form of that pipeline. Same
│                            agent methodology with everything project-specific moved
│                            into a config file, so one engine drives any translation
│                            (Python→Rust, Java→Go, ...) rather than just xcvrd
├── benchmark/               performance harness comparing a Rust translation against
│                            the Python reference — on the live DUT and in-process —
│                            with provenance recording and a work-equivalence gate
└── vendor/                  prebuilt swss-common debs and SONiC python shims
```

Each directory carries its own README with the detail.

## How the pieces fit

**The emulator replaces hardware.** `platform/` implements the SONiC platform API over
gRPC to `xcvr-emu`, so an unmodified `xcvrd` reads and writes EEPROM on emulated optics.
`emu-deploy/` installs it. This is what makes plugging, faulting and reconfiguring a
transceiver a scripted operation.

**`xcvrd-tests/` is the correctness oracle.** It never imports the daemon — it drives
stimulus through the emulator and reads STATE_DB, so the same suite grades any
implementation. That independence is why it can serve as the pipeline's verdict.

**`recodeAgent/` produces the translations** under `results/result_N`, each graded by
that suite via `xcvrd_tests_rust`. `CodeWeaver/` is the same pipeline generalized —
identical agent methodology, but every project-specific detail lives in a config file,
so it translates arbitrary codebases rather than only `xcvrd`.

**`benchmark/` measures them.** The DUT harness benchmarks the real supervised process
(works for any translation); the in-process harness links a translation as a library for
per-task detail. See `benchmark/README.md`.

## Requirements

A Linux host with nested virtualization, passwordless sudo, docker, and a large data
mount (default `/mnt/data`). `./setup-sonic-testbed.sh preflight` checks all of it and
`install_prereqs` installs the rest.

Clone with submodules (`xcvr-emu`, `CodeWeaver`):

```bash
git clone --recurse-submodules https://github.com/gsoosk/sonic-dev
# already cloned:
git submodule update --init --recursive
```

## Troubleshooting

### `No route to host` / `Connection closed` talking to the DUT

```
[ship] scp image + bundle + scripts to DUT
ssh: connect to host 10.250.0.101 port 22: No route to host
```

`No route to host` is an L2/L3 failure, not an SSH one — ARP never resolved, so
nothing is at that address. (Contrast `Connection refused` = host up, sshd down;
`Connection timed out` = reachable but filtered.) Almost always this is a host that
rebooted: `br1` and the VMs do not survive one.

```bash
./setup-sonic-testbed.sh fix_dut_network     # diagnose + repair, then prove it
```

That attaches the DUT's mgmt tap (`vlab-01-0`) to `br1` and pings the DUT from the
`mgmt` container. It is idempotent, so it is safe to run when you are only guessing.
If it still cannot reach the DUT it prints the remaining suspects in order.

Full post-reboot recovery:

```bash
docker start mgmt ptf_vms6-1
./setup-sonic-testbed.sh setup_mgmt_network   # recreate br1
./setup-sonic-testbed.sh start_vms
./setup-sonic-testbed.sh add_topo             # now attaches the DUT tap itself
./setup-sonic-testbed.sh deploy_mg
./setup-sonic-testbed.sh emulator
```

**Why the tap needs attaching at all.** sonic-mgmt's `add-topo` enslaves the
*neighbour* VMs' `-m` interfaces to `br1` but not the DUT's own tap. Everything then
looks healthy — the VM runs, `br1` exists, `virsh list` is happy — while the DUT is
invisible on the mgmt network. `add_topo` now does this itself, so a fresh setup and
a recovery both end with a DUT that is actually reachable rather than one that only
looks deployed.

### Which xcvrd is live?

```bash
./setup-sonic-testbed.sh xcvrd_status    # PYTHON (stock) vs RUST (xcvrd-rs)
```

Read-only: reports supervisor state, the running process image, and the
inject/backup markers. Useful after an interrupted benchmark or validation run,
which restore the Python daemon on exit but cannot if they are killed outright.