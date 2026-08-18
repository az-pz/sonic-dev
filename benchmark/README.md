# xcvrd benchmark harness

Microbenchmarks comparing the **Rust xcvrd translation** (`recodeAgent/results/result_N`)
against the **reference Python xcvrd**, at the daemon-orchestration layer.

---

## 0. Framing — read this before quoting any number

**This harness does NOT measure "Rust vs Python".** It measures the *orchestration
layer*, which is the only part the translation actually rewrote.

In the deployed system the Rust daemon calls Python for every transceiver
operation (`platform-bridge` -> `sonic_platform` -> `sonic_platform_base`), and
marshals each result through `json.dumps` -> `serde_json`. The CMIS/SFF decode --
the actual byte-crunching -- is *the same Python code in both daemons*. A naive
end-to-end benchmark therefore mostly measures shared Python, plus a marshalling
tax that only the Rust side pays.

So both daemons are benchmarked with their platform and DB edges **mocked**:

```
  [ MockHal / MockSfp ]  ->  DAEMON UNDER TEST  ->  [ MockDbTable ]
   (pure Rust, no PyO3)      (real task code)        (in-memory)

  [ mock_platform.py  ]  ->  DAEMON UNDER TEST  ->  [ mock_swsscommon.py ]
   (pure Python)             (real task code)        (in-memory)
```

Each daemon runs against mocks written in *its own* language, so the shared
Python HAL is removed from both sides and what remains is event loops, state
machines, port mapping, formatting and DB call patterns -- i.e. the rewrite.

### What this deliberately erases

| Erased | Consequence |
|---|---|
| Python CMIS/SFF decode | Shared by both daemons; would cancel out anyway |
| PyO3 marshalling (`json.dumps` + `serde_json`) | Rust-only cost -- **excluded here**, measure separately |
| **The GIL** | With `MockHal` no Python runs in the Rust path, so its 4 worker threads stop contending. Mocked fan-out numbers are therefore *optimistic* vs deployment. |
| Redis / gRPC I/O | Machine and network noise |

**The GIL erasure is the most important caveat.** The Rust daemon spawns four
Python-touching threads (`SffManagerTask`, `DomInfoUpdateTask`, `CmisManagerTask`,
`DomThermalInfoUpdateTask`) and the bridge never calls `allow_threads`, so in
production they serialize on CPython's GIL for every decode and every marshal.
That contention is invisible here **by construction**.

To quantify it, run the same fan-out sweep under `MockHal` *and* `BridgeHal`:
the divergence between the two curves is the PyO3 + GIL tax, isolated. That
number is also the answer to "what would porting `sonic_platform_base` buy us?".

### Questions this harness can and cannot answer

| Question | Answerable here? |
|---|---|
| Did the rewrite make the orchestration layer faster/leaner? | **Yes -- this is the target** |
| Do both daemons do the *same work*? | **Yes -- see the equivalence gate** |
| Is Rust faster than Python in general? | No (and it is not a meaningful question here) |
| What does the deployed daemon cost? | No -- use the on-DUT suite |
| How much does the PyO3 boundary cost? | Only via the Mock-vs-Bridge delta |

---

## 1. Validity gate -- run this before any timing

`equivalence/` compares a **call trace** from each daemon for an identical
scenario: every HAL method invoked (per port) and every DB write (per table/key,
with field counts). If the two daemons issue different amounts of work, timing
them is meaningless -- you would be comparing two different programs.

**If the gate fails, stop and reconcile before measuring anything.**

Counter metrics are also the only signal here that is fully machine- and
language-independent, so they are worth reporting in their own right (e.g. read
amplification per DOM cycle).

Known, accepted divergence: `result_4` permanently deselects
`test_link_change_triggers_fast_flag_recapture`, so its link-change fast path is
not behaviourally equivalent. Scenarios must not exercise that path (or must
declare it).

---

## 2. Layout

```
benchmark/
  README.md              # this file -- framing is non-negotiable
  schema/                # trace + scenario record formats (shared contract)
  fixtures/              # canned HAL payloads, byte-identical across languages
  scenarios/             # declarative workloads (N ports, events, cadence)
  rust/                  # counting decorators + trace recorder (criterion later)
  equivalence/           # trace differ (exit != 0 on mismatch)
  tools/                 # build/run wrappers
  results/<sha>/         # raw JSONL + environment manifest
```

The target crate is **not modified**: `recodeAgent/results/result_N` are recorded
pipeline artifacts and must stay immutable. `benchmark/rust` depends on the
target through the `rust/target-crate` symlink, which `tools/select_target.sh`
repoints -- so the same harness benchmarks `result_4`, `result_5`, ... unchanged.

---

## 3. Where to run

**On the build host, not the KVM DUT.** These are microbenchmarks: they need no
testbed, no emulator and no Redis. Running both daemons on the same host removes
steal time and host noise, and keeps the harness usable while the DUT is being
rebuilt.

`xcvrd-rs` links `swss-common` and `pyo3` even when every seam is mocked, so it
still builds inside the `recode-rust-build` container with
`RUSTFLAGS='-L native=/swsslib'` (same as `recodeAgent/tools/unit_test.sh`).
Nothing calls into Python at runtime under `MockHal`: `pyo3`'s interpreter is
`auto-initialize`d lazily on the first `Python::with_gil`, which never happens.

---

## 4. Method (when you get to timing)

- **Paired, interleaved A/B/A** -- never "all Python, then all Rust"; drift becomes
  the dominant effect.
- **Percentiles, not means** -- these distributions are poll-quantised and
  long-tailed. Report p50/p95/p99.
- **Mann-Whitney U** (non-parametric) + bootstrap CI, not a t-test.
- **Calibrate the mocks first.** Mock cost must be ~0 on both sides. The Python
  reference mocks SFPs with `MagicMock`, which is genuinely slow (dynamic
  attribute creation + call recording per access); a hand-written fake is
  required or the harness measures mock overhead and flatters Rust.
- Discard the first full cycle as warm-up; record the git SHA, build profile and
  fixture set in the environment manifest.

Note the target's build profile is `opt-level = 2` with no LTO and no
`codegen-units=1`. Either normalise it before benchmarking or state it -- it is
worth a few percent.

---

## 5. Status

Phase 0 (this commit): shared contract + Rust-side call-trace recorder.
Phase 1: criterion timing benches (Rust).
Phase 2: Python harness -- **blocked** on vendoring `mock_platform.py` /
`mock_swsscommon.py` from sonic-platform-daemons `tests/`, and on making the
`xcvrd` package importable on the build host (it currently exists only inside the
DUT's pmon container).
Phase 3: analysis + report.
