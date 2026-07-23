# Analyzer Design Document — SONiC `xcvrd` Python → Rust

> ReCodeAgent (arXiv:2604.07341 §3.2) Analyzer output. Three parts, mirroring
> Figure 5: **(1) Source Project Research**, **(2) Third‑Party Library Analysis**,
> **(3) Target Project Design**. The Planner/Translator/Validator agents treat
> Part 3 as authoritative.
>
> **Hard project constraints baked in below** (README §2–§3): thick HAL via
> `platform-bridge` (PyO3 → `sonic_platform`); STATE_DB via `swss-common`;
> two validation layers (Rust mocked unit tests **+** the immutable `xcvrd-tests`
> black‑box oracle); mirror the Python package layout in Rust; extend the M1
> bootstrap in `crate/xcvrd-rs/` (never edit `crate/`; work in `pipeline/crate/`).
> **This document writes no Rust.**

---

# Part 1 — Source Project Research

## 1.1 Overview

`xcvrd` is SONiC's **transceiver information update daemon**. It runs in the
`pmon` container, discovers pluggable optics (SFP/QSFP/QSFP‑DD/OSFP) through the
platform HAL, and continuously projects their state into Redis **STATE_DB** so
the rest of SONiC (CLI, SNMP, telemetry, orchagent) sees a live view of every
optic. It also drives module bring‑up (CMIS datapath init) and applies host SI /
media settings.

The entrypoint is `main()` in `source/xcvrd/xcvrd.py`, which builds a
**`DaemonXcvrd(daemon_base.DaemonBase)`** and calls `run()`. `DaemonXcvrd.init()`
loads the platform chassis (`sonic_platform.platform.Platform().get_chassis()`),
waits for port config, builds the logical↔physical **port mapping** from
CONFIG_DB, and cleans stale rows. `DaemonXcvrd.run()` then spawns a set of
**task threads** and blocks on `stop_event` (`xcvrd.py:1142‑1236`):

| Thread (class) | File | Responsibility | Enabled |
|---|---|---|---|
| **`SfpStateUpdateTask`** | `xcvrd.py:259` | The core: presence/hot‑plug event loop; publishes `TRANSCEIVER_INFO` + `TRANSCEIVER_STATUS_SW`, DOM/VDM thresholds on insert, decodes error events, deletes rows on removal | always |
| **`CmisManagerTask`** | `cmis/cmis_manager_task.py:41` | CMIS datapath state machine (INSERTED→…→READY); writes `TRANSCEIVER_STATUS_SW.cmis_state`; configures app‑sel, tx power, laser freq | unless `--skip_cmis_mgr` |
| **`DomInfoUpdateTask`** | `dom/dom_mgr.py:141` | ~60 s poll of DOM sensors, HW status, DOM/status **flags**, VDM, PM, firmware; publishes `TRANSCEIVER_DOM_SENSOR`, `TRANSCEIVER_STATUS`, etc. | always |
| **`DomThermalInfoUpdateTask`** | `dom/dom_mgr.py:526` | Faster module‑temperature poll into `TRANSCEIVER_DOM_TEMPERATURE` | if `--dom_temperature_poll_interval` |
| **`SffManagerTask`** | `sff_mgr.py:45` | Non‑CMIS (SFF‑8636) TX enable/disable + high‑power‑class from `host_tx_ready`/`admin_status` | only if `--enable_sff_mgr` |

Stimulus comes from the HAL: `chassis.get_change_event(timeout)` returns
plug/unplug/error events, and each `sfp.get_transceiver_*()` reads and CMIS/SFF‑
decodes the module EEPROM. On the testbed the HAL is the `sonic_platform` plugin
that speaks gRPC to the **`xcvr-emu`** emulator.

> **Local snapshot vs. upstream.** `source/xcvrd/` is a **modular refactor** of
> upstream `sonic-net/sonic-platform-daemons/sonic-xcvrd`: DOM logic is split into
> `dom/` (a `DomInfoUpdateTask` + `dom/utilities/{db,dom_sensor,status,vdm}`),
> and CMIS into `cmis/`. The upstream **behavioral unit tests**
> (`source/xcvrd/tests/test_xcvrd.py`) are largely from `master`, so some map
> straight onto the refactored modules and some must become **new** Rust tests
> (called out in §3.6). The **`xcvrd-tests/`** black‑box suite is snapshot‑
> independent — it only asserts STATE_DB.

## 1.2 Directory structure (`source/xcvrd/`)

```
xcvrd/
├── __init__.py
├── xcvrd.py                     # DaemonXcvrd, SfpStateUpdateTask, post_port_sfp_info_to_db, _wrapper_* (1083 L)
├── sff_mgr.py                   # SffManagerTask (SFF-8636 TX control) (452 L)
├── cmis/
│   ├── __init__.py              # re-exports CmisManagerTask
│   └── cmis_manager_task.py     # CmisManagerTask CMIS state machine (1177 L)
├── dom/
│   ├── __init__.py
│   ├── dom_mgr.py               # DomInfoUpdateBase/Task, DomThermalInfoUpdateTask (486 L)
│   └── utilities/
│       ├── db/utils.py          # DBUtils.post_diagnostic_values_to_db + flag-metadata engine (188 L)
│       ├── dom_sensor/{utils.py,db_utils.py}   # DOMUtils / DOMDBUtils  (DOM_SENSOR, DOM_FLAG*, DOM_THRESHOLD)
│       ├── status/{utils.py,db_utils.py}       # StatusUtils / StatusDBUtils  (STATUS, STATUS_FLAG*)
│       └── vdm/{utils.py,db_utils.py}          # VDMUtils / VDMDBUtils  (VDM_* real values, thresholds, flags)
└── xcvrd_utilities/
    ├── common.py                # wrappers (_wrapper_get_presence...), STATE_DB SW-status helpers, CMIS_STATE_* consts (305 L)
    ├── port_event_helper.py     # PortMapping, PortChangeObserver, get_port_mapping, subscribe/handle_port_config_change (324 L)
    ├── sfp_status_helper.py     # SFP error bitmasks + descriptions; detect_port_in_error_status (30 L)
    ├── xcvr_table_helper.py     # XcvrTableHelper: every TRANSCEIVER_* Table name + accessor (251 L)
    ├── media_settings_parser.py # media_settings.json → APPL_DB PORT SI (523 L)
    ├── optics_si_parser.py      # optics_si_settings.json → NPU SI (179 L)
    └── utils.py                 # XCVRDUtils: presence/flat-memory/lpmode helpers (29 L)
tests/                          # Python behavioral unit tests + mocks (Part-B input) — see §1.7
```

## 1.3 Key structures & interfaces

### 1.3.1 Platform (HAL) surface xcvrd calls
All HW access funnels through thin wrappers in `xcvrd.py` / `common.py` around
`platform_chassis` (`sonic_platform` `Chassis`) and per‑port `Sfp` objects:

| xcvrd call site | Platform method | Purpose |
|---|---|---|
| `_wrapper_get_transceiver_change_event` `xcvrd.py:141` | `chassis.get_change_event(timeout)` → `(status, {'sfp':{p:code}, 'sfp_error':{p:code}})` | plug/unplug/error events |
| `initialize_sfp_obj_dict` `xcvrd.py:962` | `chassis.get_sfp(i)` / `get_num_sfps()` | per‑port handle |
| `_wrapper_get_presence` `common.py:124` | `sfp.get_presence()` | module present? |
| `_wrapper_get_transceiver_info` `xcvrd.py:114` | `sfp.get_transceiver_info()` | identity dict → `TRANSCEIVER_INFO` |
| `DOMUtils.*` `dom/utilities/dom_sensor/utils.py` | `sfp.get_transceiver_dom_real_value()`, `get_temperature()`, `get_transceiver_dom_flags()`, `get_transceiver_threshold_info()` | DOM |
| `StatusUtils.*` `dom/utilities/status/utils.py` | `sfp.get_transceiver_status()`, `get_transceiver_status_flags()` | HW status |
| `_wrapper_is_replaceable` `xcvrd.py:105` | `sfp.is_replaceable()` | `is_replaceable` field |
| `_wrapper_get_sfp_type` / `_error_description` `xcvrd.py:154/167` | `sfp.sfp_type`, `sfp.get_error_description()` | type / vendor error text |
| CMIS mgr | `sfp.get_xcvr_api()` → `CmisApi` (`set_lpmode`, `reset`, app‑sel, datapath regs) | bring‑up |

> **All of these are already exposed by the provided `platform-bridge`**
> (`crate/platform-bridge/src/lib.rs`) as typed methods — see §3.4. The Rust
> daemon never re‑implements CMIS/SFF decode.

### 1.3.2 Port mapping (logical ↔ physical), `port_event_helper.py:212`
`PortMapping` holds four dicts: `logical_port_list`, `logical_to_physical`
(`Ethernet100`→`25`), `physical_to_logical` (`25`→`[Ethernet100,…]`, natsorted),
`logical_to_asic`. Built by `get_port_mapping(namespaces)` (`:346`) which scans
CONFIG_DB `PORT` and emits `PortChangeEvent(key, fvp['index'], asic, PORT_ADD)`.
`PortChangeObserver` (`:46`) subscribes to CONFIG_DB/APPL_DB `PORT` changes at
runtime. **On the testbed the mapping is `Ethernet{index*4}` ↔ `index`**
(`xcvrd-tests/lib/emu.py:port_to_index`, and the bootstrap `discover_ports` in
`crate/xcvrd-rs/src/daemon.rs`).

### 1.3.3 `SfpStateUpdateTask` — the core loop (`xcvrd.py:395‑693`)
`init()` posts all present ports' info + DOM/VDM thresholds once
(`_post_port_sfp_info_and_dom_thr_to_db_once`) and seeds `TRANSCEIVER_STATUS_SW`
(`_init_port_sfp_status_sw_tbl`). `task_worker` runs an **INIT/NORMAL/EXIT state
machine** over `get_change_event`. Per physical port event code `value`:
* `'1'` (`SFP_STATUS_INSERTED`) → `STATUS_SW.status='1'`, `post_port_sfp_info_to_db` (retry once on `SFP_EEPROM_NOT_READY`), then post DOM/VDM thresholds + media settings.
* `'0'` (`SFP_STATUS_REMOVED`) → `STATUS_SW.status='0'`, `del_port_sfp_dom_info_from_db` across INFO/DOM/STATUS/VDM/PM/FW tables.
* other (error bitmap) → decode → `STATUS_SW.error='|'.join(descriptions)`; if blocking bit set, delete DOM tables (keep static INFO).

### 1.3.4 `post_port_sfp_info_to_db` (`xcvrd.py:178`)
For a present port, take `get_transceiver_info()`; **if `cmis_rev` present** write
**every** field as `str(value)` plus `is_replaceable` (the CMIS branch — the case
on this testbed); else write the fixed SFF field list. This exact behavior is
already reproduced by the bootstrap `sync_port` (`crate/xcvrd-rs/src/daemon.rs`).

### 1.3.5 DOM poll loop `DomInfoUpdateTask.task_worker` (`dom/dom_mgr.py:284`)
Every `dom_update_interval` (default **60 s**), for each physical port not in
error and present: post firmware info, `post_port_dom_sensor_info_to_db`,
DOM flags, HW status, HW status flags, and (if VDM supported) VDM real values +
flags + PM. The generic write path is `DBUtils.post_diagnostic_values_to_db`
(`dom/utilities/db/utils.py:19`): read a `{field:value}` dict from the SFP,
`beautify` (stringify / strip units), append `last_update_time`, `table.set`.
Flag tables additionally maintain change‑count / set‑time / clear‑time metadata
(`_update_flag_metadata_tables`).

### 1.3.6 `CmisManagerTask` (`cmis/cmis_manager_task.py`)
The most complex module: a per‑lport CMIS **datapath state machine**
(`process_cmis_state_machine`, states `CMIS_STATE_*` in `common.py:22‑39`:
`INSERTED, DP_PRE_INIT_CHECK, DP_DEINIT, AP_CONFIGURED, DP_INIT, DP_TXON, READY,
REMOVED, FAILED`). It sets `TRANSCEIVER_STATUS_SW.cmis_state`
(`update_port_transceiver_status_table_sw_cmis_state:85`). `process_single_lport`
(`:1247`) short‑circuits to **`READY`** for non‑CMIS / flat‑memory / no‑api
modules, and computes app‑sel/host‑lane masks otherwise. **Observable contract
for the oracle is only `cmis_state == "READY"`** (see §1.5) — the emulator module
reaches datapath‑up, so a much‑reduced state machine can satisfy the tests
(§3.7, M3).

### 1.3.7 STATE_DB table handles `XcvrTableHelper` (`xcvr_table_helper.py:55`)
Central registry that opens one `swsscommon.Table` per TRANSCEIVER_* table per
ASIC (list in §1.4). Getters like `get_intf_tbl(asic)`, `get_dom_tbl`,
`get_status_sw_tbl`, `get_dom_threshold_tbl` are used throughout.

## 1.4 Data models — STATE_DB schema xcvrd produces

Table names are constants in `xcvr_table_helper.py:11‑47`; keys are the **logical
port** (`Ethernet100`), i.e. Redis hash key `"<TABLE>|<lport>"`.

| Table | Written by | Source method | Milestone |
|---|---|---|---|
| **`TRANSCEIVER_INFO`** | SfpStateUpdate | `get_transceiver_info()` (+`is_replaceable`) | **M1** |
| **`TRANSCEIVER_STATUS_SW`** | SfpStateUpdate (`status`,`error`) + Cmis (`cmis_state`) | `update_port_transceiver_status_table_sw` `common.py:110`; cmis `:85` | **M1/M3** |
| **`TRANSCEIVER_DOM_SENSOR`** | DomInfoUpdate | `get_transceiver_dom_real_value()` | **M2** |
| **`TRANSCEIVER_DOM_THRESHOLD`** | SfpStateUpdate on insert | `get_transceiver_threshold_info()` | **M2/M6** |
| `TRANSCEIVER_DOM_TEMPERATURE` | DomThermal | `get_temperature()` | (opt) |
| **`TRANSCEIVER_STATUS`** | DomInfoUpdate | `get_transceiver_status()` | M3/M6 |
| `TRANSCEIVER_DOM_FLAG` (+`_CHANGE_COUNT`/`_SET_TIME`/`_CLEAR_TIME`) | DomInfoUpdate | `get_transceiver_dom_flags()` | later |
| `TRANSCEIVER_STATUS_FLAG` (+metadata trio) | DomInfoUpdate | `get_transceiver_status_flags()` | later |
| `TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_{THRESHOLD,FLAG,…}`, `TRANSCEIVER_VDM_REAL_VALUE` | Sfp/Dom + VDMDBUtils | VDM api | later |
| `TRANSCEIVER_PM` | DomInfoUpdate | `get_transceiver_pm()` | later |
| `TRANSCEIVER_FIRMWARE_INFO` | DomInfoUpdate | `get_transceiver_firmware_info()` | later |

### The oracle contract (what `xcvrd-tests` actually asserts)
Cross‑referenced against `xcvrd-tests/tests/*` and `lib/golden.py`:

* **`TRANSCEIVER_INFO`** — populated iff present; cleared on unplug; `manufacturer=="xcvr-emu"`, `model=="EMU-40G-LR4"`, `vendor_oui=="01-02-03"`, `serial=="0123456789"`, `vendor_rev in ("01","1")`, `type` contains `QSFP`+`DD`, `ext_identifier` contains `"Power Class 8"` (`test_info_content.py`, `test_presence.py`).
* **`TRANSCEIVER_STATUS_SW`** — `status`=`"1"`/`"0"` with plug state; `cmis_state=="READY"` when present; `error` set to decoded description(s) on injected error, back to `"N/A"` on recovery (`test_presence.py`, `test_status_error.py`).
* **`TRANSCEIVER_DOM_SENSOR`** — has `temperature` & `voltage`; a raw emulator write propagates after the ~60 s refresh (`test_dom.py`).
* **Blocking error** removes `TRANSCEIVER_DOM_SENSOR` but keeps `TRANSCEIVER_INFO`; **non‑blocking** keeps DOM (`test_status_error.py`).
* **Golden (M6)** — exact match of the projection `{TRANSCEIVER_INFO, TRANSCEIVER_STATUS_SW, TRANSCEIVER_DOM_THRESHOLD}` (minus volatile `last_update_time`) vs. `xcvrd-tests/golden/Ethernet100.json`. That file is the **canonical field list** for M1/M2/M3 outputs (e.g. `TRANSCEIVER_INFO` = `active_apsel_hostlane1..8, application_advertisement, cable_length, cable_type, cmis_rev, connector, encoding, ext_identifier, ext_rateselect_compliance, hardware_rev, host_lane_count, is_replaceable, manufacturer, media_interface_technology, media_lane_count, model, nominal_bit_rate, serial, specification_compliance, type, type_abbrv_name, vdm_supported, vendor_date, vendor_oui, vendor_rev`; `TRANSCEIVER_STATUS_SW` = `{cmis_state:READY, error:N/A, status:1}`; `TRANSCEIVER_DOM_THRESHOLD` = 24 temp/vcc/tx/rx/laser threshold fields).
* **Interaction trace** — xcvrd must actually **read** the EEPROM (emulator `Monitor` stream): steady‑state reads + a ≥3‑read burst after re‑plug + page‑00h reads (`test_interaction_trace.py`). This means the Rust daemon must genuinely poll via the bridge, not cache.
* **lpmode/reset** — driven through `sfputil`/the bridge, **not** through the Rust daemon: `reset`→`00h:26=0x08`, `lpmode on`→`0x10` on the Monitor trace (`test_lpmode_reset.py`). See §3.7 M4.

## 1.5 Error handling

* **Absent module:** `_wrapper_get_presence` false → `post_port_sfp_info_to_db` skips; on a removal event `del_port_sfp_dom_info_from_db` wipes the port's rows and `STATUS_SW.status='0'`. `remove_stale_transceiver_info` (`xcvrd.py:986`) clears INFO for absent ports at boot.
* **Hardware/EEPROM errors (SfpBase bitmaps):** `sfp_status_helper.py` — `SFP_ERRORS_BLOCKING_MASK=0x02`, `SFP_ERRORS_GENERIC_MASK=0x0000FFFE`, `SFP_ERRORS_VENDOR_SPECIFIC_MASK=0xFFFF0000`. `fetch_generic_error_description` maps bits via `SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT`; blocking (`is_error_block_eeprom_reading`) → drop DOM. Event codes: `'1'`/`'0'` are insert/remove, anything else is an error bitmap. `xcvrd-tests/lib/errors.py` enumerates the same bits/descriptions the oracle expects (`"Bus stuck (I2C data or clock shorted)"`, `"Bad or unsupported EEPROM"`, `"High temperature"`, blocking `"Blocking EEPROM from being read"`).
* **`NotImplementedError`:** pervasive `try/except NotImplementedError` → treat as "feature absent" (skip / empty dict). `NOT_IMPLEMENTED_ERROR=3` causes `sys.exit` only in the info/firmware post paths.
* **Threading/shutdown:** signals in `DaemonXcvrd.signal_handler` (SIGINT/SIGTERM set `stop_event`, SIGHUP re‑reads log level); a fatal `SfpStateUpdateTask` state → `os.kill(getppid(), SIGTERM)`; child‑thread exceptions escalate to `SIGKILL`; `sfp_error_event` → `sys.exit(SFP_SYSTEM_ERROR=4)`.

## 1.6 Dependencies (Python imports)

| Import | Used for |
|---|---|
| `sonic_platform_base` (`SfpBase`, `sonic_xcvr.api.public.c_cmis.CmisApi`) | error‑bit dict; CMIS api typing/decode |
| `swsscommon.swsscommon` (`Table`, `ProducerStateTable`, `SubscriberStateTable`, `Select`, `FieldValuePairs`, `SonicDBConfig`) | all Redis DB access |
| `sonic_py_common` (`daemon_base`, `syslogger`/`logger`, `multi_asic`) | daemon base, logging, ASIC namespaces |
| `natsort.natsorted` | natural port ordering in `PortMapping` |
| stdlib: `threading, time, datetime, signal, subprocess, argparse, copy, json, ast, re, ctypes, os, sys, traceback` | loops, timers, CLI, parsing |

## 1.7 Unit tests (`source/xcvrd/tests/`)

`test_xcvrd.py` (6200 L, ~180 `test_*` methods) across `TestXcvrdThreadException`,
`TestXcvrdScript`, `TestOpticSiParser`. It **imports xcvrd modules directly** and
**mocks the two boundaries**:

* **Platform:** per‑port SFP objects are `unittest.mock.MagicMock()` with
  `.get_transceiver_info.return_value = {...}` etc. and
  `.side_effect = NotImplementedError` for the "not implemented" paths
  (e.g. `test_get_transceiver_status:6082`, `test_post_port_sfp_info_to_db:1468`).
  Presence is patched: `@patch('...common._wrapper_get_presence', MagicMock(return_value=True))`.
  (`mock_platform.py` itself only mocks chassis/fans/thermals — SFP mocking is
  inline MagicMock.)
* **STATE_DB:** `mock_swsscommon.Table` — an **in‑memory dict** table with
  `set/get/hget/hdel/getKeys/get_size` (`tests/mock_swsscommon.py`). Global
  `swsscommon.Table = MagicMock()` at import; `daemon_base.db_connect = MagicMock()`.
* **Port mapping:** real `PortMapping()` fed synthetic `PortChangeEvent(...)`.

**Behavior coverage** (drives the Rust unit‑test design, §3.6):

| Area | Representative tests | → Rust milestone |
|---|---|---|
| Info publish | `test_post_port_sfp_info_to_db`, `_with_sfp_not_present`, `_and_dom_thr_to_db_once` | M1 |
| SW status seed | `test_init_port_sfp_status_sw_tbl`(`_no_physical_port_found`) | M1 |
| Event mapping / state machine | `test_SfpStateUpdateTask_mapping_event_from_change_event`, `_task_worker`, `test_sfp_insert_events`, `test_sfp_remove_events`, `test_sfp_removal_from_dict` | M1/M3/M5 |
| DOM/status/thresholds | `test_post_port_dom_sensor_info_to_db`, `_dom_temperature_`, `_dom_thresholds_`, `_transceiver_hw_status_`, `test_beautify_dom_info_dict` | M2/M3 |
| Flag metadata | `test_update_flag_metadata_tables`, `test_post_port_dom_flags_to_db` | later |
| Error status | `test_detect_port_in_error_status`, `test_is_error_sfp_status` | M3 |
| DOM task loop | `test_DomInfoUpdateTask_task_worker`(+`_vdm_*`,`_stop_event`), `_get_dom_polling_from_config_db` | M2 |
| Port mapping / events | `test_get_port_mapping`, `test_handle_port_update_event`, `test_handle_port_config_change` | M1/M5 |
| CMIS | `test_CmisManagerTask_update_port_transceiver_status_table_sw_cmis_state`, `_process_single_lport_*`, `_task_worker` | M3 |
| Wrappers | `test_wrapper_get_presence/_is_replaceable/_get_transceiver_info/_get_sfp_type/_change_event` | M1 |
| Daemon lifecycle | `test_DaemonXcvrd_run`, `_init_deinit_*`, `_signal_handler` | M0/M1 |
| Media/optics SI, gearbox | `test_notify_media_setting*`, `TestOpticSiParser*` | out of oracle scope; low priority |

**Key insight:** the Python tests validate the same seams we must reproduce — a
**mockable SFP** and a **mockable STATE_DB Table**. That maps directly to two
Rust trait seams (§3.6).

---

# Part 2 — Third‑Party Library Analysis

Format per dependency: **Overview · How xcvrd uses it · Rust recommendation**.
The decisive question is *which needs are already met by the provided scaffolding*
so the Translator does **not** reinvent interop.

### 2.1 `sonic_platform` plugin + `sonic_platform_base` (CMIS/SFF decode, gRPC)
* **Overview:** the platform HAL. `Sfp(SfpOptoeBase)` turns raw EEPROM into the
  full transceiver API (`get_transceiver_info/_dom_real_value/_status/_threshold_info`,
  lpmode/reset via `CmisApi`); `Chassis` gives `get_num_sfps/get_sfp/get_change_event`.
* **xcvrd use:** every HW read/write and every plug/error event.
* **Rust recommendation — ALREADY MET by `crate/platform-bridge` (PyO3 → the real
  plugin).** `platform_bridge::{Platform, Chassis, Sfp, ChangeEvent}` expose the
  exact surface as typed methods (scalars) and `serde_json::Value` (dicts). **Do
  NOT** port CMIS/SFF decode, gRPC, or `emu_client` to Rust — it stays in Python
  behind the bridge (README §3a; proven on the DUT). No new crate.

### 2.2 `swsscommon` (Redis STATE_DB / CONFIG_DB / APPL_DB)
* **Overview:** SONiC's C++ DB access lib (`Table`, `ProducerStateTable`,
  `SubscriberStateTable`, `Select`, `FieldValuePairs`, `SonicDBConfig`).
* **xcvrd use:** all STATE_DB writes; CONFIG_DB `PORT` reads for port mapping;
  APPL_DB `PORT_TABLE` subscription for `wait_for_port_config_done` and DOM link
  events.
* **Rust recommendation — ALREADY MET by the official `swss-common` crate**
  (pinned git rev `7faca59…` in `crate/xcvrd-rs/Cargo.toml`), exposing
  `DbConnector`, `Table`, `ProducerStateTable`, `SubscriberStateTable`. Bootstrap
  uses `DbConnector::new_unix(6, "/var/run/redis/redis.sock", 0)?.hset(key, field,
  &CxxString::from(v))` (see `env.rs`, both `examples/`). Use `Table` for
  table‑scoped `set`/`del` where it mirrors the Python `XcvrTableHelper` closely.
  No new crate; no hand‑rolled Redis client.

### 2.3 `sonic_py_common` — `daemon_base`, `syslogger`/`logger`, `multi_asic`
* **Overview:** `DaemonBase` (signals, plugin loading, `db_connect`), a syslog
  logger, and multi‑ASIC namespace helpers.
* **xcvrd use:** `DaemonXcvrd(daemon_base.DaemonBase)`, logging everywhere,
  `multi_asic.get_front_end_namespaces()` / `get_asic_index_from_namespace`.
* **Rust recommendation:**
  * *DaemonBase lifecycle* → **std**: the M0/M1 bootstrap already runs under the
    pmon supervisor via a Python shim (README §6); implement signals with the
    **`signal-hook`** crate (or a `ctrlc` handler) writing an `AtomicBool`/channel.
  * *Logging* → **`log`** + **`env_logger`** (or `syslog` crate) — the bootstrap
    currently uses `eprintln!`, which supervisor captures; a `log` facade is the
    idiomatic upgrade. Small, std‑adjacent; acceptable NEW deps.
  * *multi‑ASIC* → **out of scope for the testbed** (single ASIC, namespace `""`,
    `STATE_DB=6`/`CONFIG_DB=4` in `env.rs`). Model `asic_id` as a constant `0`;
    keep a `namespaces: Vec<String>` seam for fidelity but don't build multi‑ASIC.

### 2.4 `natsort.natsorted`
* **Overview/use:** natural sort of logical ports sharing a physical port
  (`PortMapping._handle_port_add`).
* **Rust recommendation:** trivial — the **`natord`** crate, or a hand‑rolled
  natural comparator. On the testbed each physical port has exactly one logical
  port, so plain sort suffices; add `natord` only if breakout ports are modeled.

### 2.5 Python stdlib (`threading`, `time`/`datetime`, `json`, `re`, `argparse`, `subprocess`, `signal`, `copy`)
* **xcvrd use:** task threads; poll timers + `last_update_time` timestamps; parsing
  emulator dicts; regex in media/DOM beautify; CLI flags; `is_fast_reboot_enabled`
  shells out to `sonic-db-cli`; deep‑copies of port mapping.
* **Rust recommendation — mostly std**, no exotic crates:
  * threads → **`std::thread`** + `std::sync::{Arc, Mutex, mpsc}` + `AtomicBool` stop flag.
  * time → **`std::time::{Duration, Instant}`**; timestamps (`get_current_time` uses `%a %b %d %H:%M:%S %Y` UTC) → **`chrono`** (only field goldened is `last_update_time`, which the oracle **drops** as volatile, so exact format is non‑critical).
  * JSON → **`serde_json`** (already a dep; the bridge returns `serde_json::Value`).
  * regex → **`regex`** (only for media/DOM unit stripping; DOM `_beautify` unit strip in `dom_sensor/db_utils.py:120` is simple suffix trimming — can be plain `str` ops, deferring `regex`).
  * CLI flags (`--skip_cmis_mgr`, `--enable_sff_mgr`, `--dom_*_interval`) → **`clap`** or manual `std::env::args`.
  * `is_fast_reboot_enabled` → `std::process::Command` **or** read the same key via `swss-common` (preferred — avoids shelling out).

**Summary:** the two hard interop needs (HAL, STATE_DB) are **fully pre‑solved by
the scaffolding**. The only genuinely new crates are small utilities —
`log`(+`env_logger`), `signal-hook`, `chrono`, optionally `clap`/`regex`/`natord`
— all mainstream. CONFIG_DB port mapping uses `swss-common` (already wired).

---

# Part 3 — Target Project Design (authoritative)

## 3.1 Overview & translation requirements

Reimplement **only the xcvrd daemon logic** in Rust — task loops, polling cadence,
event handling, state decisions, and STATE_DB writes — on top of the **pre‑built
thick HAL** (`platform-bridge`) and **STATE_DB binding** (`swss-common`).
Correctness is defined by **two layers** (README §2):

1. **Rust unit tests** (Part B) against **mocks** of the HAL and STATE_DB
   (`cargo test`, no DUT), mirroring `mock_platform.py`/`mock_swsscommon.py`.
2. **`xcvrd-tests`** end‑to‑end black‑box on the DUT — **immutable, the ultimate
   oracle**. Never translated/modified. The design targets its STATE_DB contract
   (§1.4).

**Non‑negotiables:** thick HAL (no Rust CMIS/SFF/gRPC); STATE_DB via `swss-common`;
CONFIG_DB port mapping via `swss-common`; mirror the Python package layout; extend
the M1 bootstrap; keep M0/M1 green at every step; `crate/` immutable (work in
`pipeline/crate/`).

## 3.2 Source → Rust structural mapping

| Python | Rust | Notes |
|---|---|---|
| package `xcvrd/` | crate `xcvrd-rs` (`src/`) | mirror layout |
| `xcvrd.py` (`DaemonXcvrd`, `SfpStateUpdateTask`, `post_port_sfp_info_to_db`) | `src/xcvrd/mod.rs` + `src/xcvrd/sfp_state_update.rs` | `DaemonXcvrd`→`Daemon` struct orchestrating threads; extends bootstrap `daemon.rs` |
| `sff_mgr.py` | `src/sff_mgr.rs` | `SffManagerTask`→struct + `run()` (M‑late) |
| `cmis/cmis_manager_task.py` | `src/cmis/mod.rs`, `src/cmis/cmis_manager_task.rs` | `CmisManagerTask`→struct + state enum |
| `dom/dom_mgr.py` | `src/dom/mod.rs`, `src/dom/dom_mgr.rs` | `DomInfoUpdateTask`→struct + `run()` |
| `dom/utilities/db/utils.py` | `src/dom/utilities/db/utils.rs` | `post_diagnostic_values_to_db` generic writer |
| `dom/utilities/{dom_sensor,status,vdm}/…` | `src/dom/utilities/{dom_sensor,status,vdm}/…` | `*Utils`/`*DBUtils` |
| `xcvrd_utilities/common.py` | `src/xcvrd_utilities/common.rs` | wrappers→trait calls; `CMIS_STATE_*`→enum; SW‑status helpers |
| `xcvrd_utilities/port_event_helper.py` | `src/xcvrd_utilities/port_event_helper.rs` | `PortMapping` struct; `get_port_mapping` |
| `xcvrd_utilities/sfp_status_helper.py` | `src/xcvrd_utilities/sfp_status_helper.rs` | error bitmasks + descriptions |
| `xcvrd_utilities/xcvr_table_helper.py` | `src/xcvrd_utilities/xcvr_table_helper.rs` | table‑name consts + handle registry |
| `xcvrd_utilities/{media_settings,optics_si}_parser.py`, `utils.py` | same‑named `.rs` | out of oracle scope; stubs first |
| `tests/mock_swsscommon.py`, mock SFPs | `src/mock.rs` (`#[cfg(test)]`) | mock DB + mock HAL (§3.6) |
| `tests/test_xcvrd.py` | `#[cfg(test)] mod tests` per module + `tests/` | §3.6 |

**Idiom mapping:** `dict` STATE_DB writes → `Table::set`/`DbConnector::hset` with
`CxxString`; Python `None` → `Option<T>`; exceptions → `Result<T, E>`
(`thiserror` error enum; `NotImplementedError`→a variant treated as "skip");
`try/except NotImplementedError: return {}` → `Result`→`unwrap_or_default()`;
`threading.Thread` subclass → struct + `fn run(self)` spawned with
`std::thread::spawn`; `stop_event` → `Arc<AtomicBool>`; keep **snake_case**
identifiers (`post_port_sfp_info_to_db`, `sync_port`, `discover_ports`,
`cmis_state`, `status_sw`) so the port is traceable.

## 3.3 Module structure for `xcvrd-rs` (Planner creates in `pipeline/crate/`)

Extend the **existing** bootstrap (`src/main.rs`, `src/lib.rs`, `src/env.rs`,
`src/daemon.rs`) — do not rewrite it. Target tree:

```
xcvrd-rs/src/
├── main.rs                       # (exists) -> xcvrd_rs::run()
├── lib.rs                        # (exists) pub mod ... ; add new modules
├── env.rs                        # (exists) open_platform/open_state_db/open_config_db (+ real trait impls)
├── daemon.rs                     # (exists, M1) grows into the orchestrator OR is re-exported by xcvrd/mod.rs
├── hal.rs                        # NEW: trait Hal + trait SfpApi (seam); PlatformHal wraps platform-bridge
├── statedb.rs                    # NEW: trait StateDb + trait TableApi (seam); SwssStateDb wraps swss-common
├── xcvrd/
│   ├── mod.rs                    # Daemon orchestrator (spawns tasks); post_port_sfp_info_to_db
│   └── sfp_state_update.rs       # SfpStateUpdateTask event loop + state machine
├── sff_mgr.rs                    # SffManagerTask (late)
├── cmis/{mod.rs, cmis_manager_task.rs}
├── dom/
│   ├── mod.rs
│   ├── dom_mgr.rs                # DomInfoUpdateTask / DomThermalInfoUpdateTask
│   └── utilities/
│       ├── db/utils.rs
│       ├── dom_sensor/{utils.rs, db_utils.rs}
│       ├── status/{utils.rs, db_utils.rs}
│       └── vdm/{utils.rs, db_utils.rs}
├── xcvrd_utilities/
│   ├── mod.rs
│   ├── common.rs                 # CMIS_STATE enum, SW-status helpers, wrappers-as-trait-calls
│   ├── port_event_helper.rs      # PortMapping, get_port_mapping
│   ├── sfp_status_helper.rs      # error bitmasks + descriptions
│   ├── xcvr_table_helper.rs      # TRANSCEIVER_* names + handles
│   ├── media_settings_parser.rs  # stub first
│   ├── optics_si_parser.rs       # stub first
│   └── utils.rs
└── mock.rs                       # #[cfg(test)] MockHal/MockSfp + MockStateDb/MockTable (mirrors mock_*.py)
```
Each module carries a `#[cfg(test)] mod tests` (Part‑B unit tests). Cross‑module
integration tests may also live in `xcvrd-rs/tests/`. **Do not break the M0
deploy‑smoke or the M1 gate** while adding modules — the default `xcvrd-rs`
binary must always compile and stay RUNNING.

## 3.4 PyO3 platform‑bridge boundary (exact call replacement)

The daemon talks to hardware **only** through `platform-bridge`
(`crate/platform-bridge/src/lib.rs`). Mapping of Python platform calls → bridge:

| Python (`common.py`/`xcvrd.py`) | `platform-bridge` Rust |
|---|---|
| `chassis.get_num_sfps()` | `Platform::num_sfps() -> usize` |
| `chassis.get_sfp(i)` | `Platform::sfp(i) -> Sfp` |
| `chassis.get_change_event(t)` | `Platform::get_change_event(timeout_ms) -> ChangeEvent{status, sfp: BTreeMap<String,String>, sfp_error}` |
| `sfp.get_presence()` | `Sfp::get_presence() -> bool` |
| `sfp.is_replaceable()` | `Sfp::is_replaceable() -> bool` |
| `sfp.get_reset_status()` | `Sfp::get_reset_status() -> bool` |
| `sfp.sfp_type` | `Sfp::sfp_type() -> String` |
| `sfp.get_error_description()` | `Sfp::get_error_description() -> Option<String>` |
| `sfp.get_transceiver_info()` | `Sfp::get_transceiver_info() -> serde_json::Value` |
| `sfp.get_transceiver_dom_real_value()` | `Sfp::get_transceiver_dom_real_value() -> Value` |
| `sfp.get_transceiver_status()` | `Sfp::get_transceiver_status() -> Value` |
| `sfp.get_transceiver_threshold_info()` | `Sfp::get_transceiver_threshold_info() -> Value` |
| `sfp.get_lpmode()/set_lpmode(b)/reset()` | `Sfp::get_lpmode()/set_lpmode(bool)/reset()` |
| `sfp.read_eeprom(o,n)/write_eeprom(o,d)` | `Sfp::read_eeprom(offset,n)->Option<Vec<u8>>` / `write_eeprom(offset,&[u8])` |
| any other no‑arg method | `Sfp::call_json(method, args) -> Value` (escape hatch) |

**Marshalling rules to preserve behavior:** dict getters come back as
`serde_json::Value` (bridge does `json.dumps(…, default=str)`); render each field
to a STATE_DB string exactly as the Python `str(value)` does, and **strip trailing
NULs** on CMIS strings (bootstrap `stringify`/`pybool` in `daemon.rs` is the
reference — reuse it). Booleans → `"True"/"False"`. Skip `Value::Null` fields.

**Bridge boundary caveats:**
* `get_change_event` keys are **physical‑port strings**; values `"1"`/`"0"` =
  insert/remove, else an **error bitmap** (parse `i64`, mask per §1.5). The bridge
  surfaces `sfp_error` too (vendor‑specific text).
* CMIS `cmis_state` and lpmode/reset **register writes** are NOT returned by the
  info/status getters — lpmode/reset are driven by `sfputil`/the plugin
  (`Sfp::set_lpmode/reset` exist but M4's oracle asserts the *plugin's* Monitor
  writes, see §3.7). `cmis_state=READY` is daemon logic (§3.7 M3).
* Each bridge call re‑acquires the GIL and hits gRPC — treat as blocking I/O;
  keep per‑port work sequential within a task (matches Python) and rely on the
  ~60 s cadence for DOM.

## 3.5 STATE_DB schema contract (per milestone)

Redis hash key = `"<TABLE>|<lport>"`. Write via `swss-common`
(`DbConnector::hset`/`del`, or a `Table` wrapper). Contract the milestones must
reproduce (values verified against `xcvrd-tests` + `golden/Ethernet100.json`):

* **M1 — `TRANSCEIVER_INFO`:** on present, every `get_transceiver_info()` field as
  `str(value)` (NUL‑stripped) **plus** `is_replaceable`; on absent/removal,
  `del` the key. **`TRANSCEIVER_STATUS_SW`:** `status`=`"1"`/`"0"`; seed
  `cmis_state` (`"READY"` acceptable on this testbed) and `error="N/A"`. *(Bootstrap
  already satisfies M1.)*
* **M2 — `TRANSCEIVER_DOM_SENSOR`:** from `get_transceiver_dom_real_value()`,
  beautified (unit‑stripped) + `last_update_time`; posted by the DOM poll and
  refreshed each ~60 s cycle. **`TRANSCEIVER_DOM_THRESHOLD`:** from
  `get_transceiver_threshold_info()` on insert (in the golden projection).
* **M3 — `TRANSCEIVER_STATUS_SW.error`:** on an error event, `'|'.join(descriptions)`
  from the decoded bitmap; blocking → also `del` the DOM tables (keep INFO);
  recovery (plug event) → `error` back to `"N/A"` + DOM repopulates.
  **`cmis_state`** driven to `"READY"`. **`TRANSCEIVER_STATUS`** from
  `get_transceiver_status()`.
* **M6 — golden:** `{TRANSCEIVER_INFO, TRANSCEIVER_STATUS_SW,
  TRANSCEIVER_DOM_THRESHOLD}` must match `golden/Ethernet100.json` byte‑for‑byte
  minus `last_update_time`.

Field **beautify** rules (must match, `DBUtils.beautify_info_dict` +
`DOMDBUtils._beautify_dom_info_dict`): non‑str → `str()`; `temperature` strip
`C`, `voltage` strip `Volts`, `(tx|rx)[1-8]power` strip `dBm`,
`(tx|rx)[1-8]bias` strip `mA`.

## 3.6 Unit‑test strategy (Part B) — trait seams + mocks

To run daemon logic under `cargo test` with **no DUT** (mirroring the Python
tests' MagicMock SFP + `mock_swsscommon.Table`), introduce **two trait seams** so
every task takes generic HAL/DB handles:

* **HAL seam** (`src/hal.rs`):
  * `trait SfpApi` — `get_presence()`, `is_replaceable()`, `sfp_type()`,
    `get_error_description()`, `get_transceiver_info() -> Value`,
    `get_transceiver_dom_real_value()`, `get_transceiver_status()`,
    `get_transceiver_threshold_info()`, `get_lpmode/set_lpmode/reset`,
    `read_eeprom/write_eeprom` (methods return `Result<…, HalError>`).
  * `trait Hal` — `num_sfps()`, `sfp(i) -> impl SfpApi`, `get_change_event(ms) -> ChangeEvent`.
  * **Real impl** `PlatformHal`/`PlatformSfp` = thin wrappers over
    `platform_bridge::{Platform, Sfp}` (a 1:1 delegation; `env::open_platform`
    returns it).
  * **Mock impl** `MockHal`/`MockSfp` in `src/mock.rs` — a `presence: bool`, a
    programmable `info/dom/status/threshold: Value`, and injectable
    `change_event`/error bitmap; methods can be set to return a "not implemented"
    variant (mirrors `MagicMock.side_effect = NotImplementedError`).
* **DB seam** (`src/statedb.rs`):
  * `trait TableApi` — `set(key, fields)`, `get(key) -> Option<Map>`,
    `hget(key, field)`, `del(key)`, `keys()` — the subset the daemon needs.
  * `trait StateDb` — opens/returns `TableApi` handles per table name (the
    `XcvrTableHelper` role).
  * **Real impl** `SwssStateDb`/`SwssTable` over `swss_common::{DbConnector, Table}`.
  * **Mock impl** `MockStateDb`/`MockTable` — an in‑memory
    `HashMap<String, BTreeMap<String,String>>`, a **direct port of
    `mock_swsscommon.Table`** (`set/get/hget/hdel/getKeys/get_size`), so
    assertions read back exactly what the daemon wrote.

Every task struct (`SfpStateUpdateTask`, `DomInfoUpdateTask`, `CmisManagerTask`,
…) is generic over `H: Hal` and `D: StateDb` (or takes `&dyn` trait objects), so
the same code runs with `PlatformHal`+`SwssStateDb` in production and
`MockHal`+`MockStateDb` in tests. This preserves the thick‑HAL design (real impl
still calls the bridge) while making the daemon logic unit‑testable.

**Where tests live:** `#[cfg(test)] mod tests` in each module (unit), plus
`xcvrd-rs/tests/*.rs` for cross‑module flows; `src/mock.rs` gated `#[cfg(test)]`
(or a `mock` feature). Run via `tools/unit_test.sh` (`cargo test` in the trixie
container).

**Python test → Rust test mapping (per milestone):**

| Python `test_xcvrd.py` behavior | Rust unit test | Milestone |
|---|---|---|
| `test_post_port_sfp_info_to_db*`, `_and_dom_thr_to_db_once` | `sfp_state_update::tests::info_publish_*` w/ `MockHal` present/absent → assert `MockTable` `TRANSCEIVER_INFO` rows | M1 |
| `test_init_port_sfp_status_sw_tbl*` | `status_sw` seed test | M1 |
| `test_SfpStateUpdateTask_mapping_event_from_change_event` | event‑code → action mapping test | M1 |
| `test_sfp_insert_events`/`_remove_events`/`test_sfp_removal_from_dict` | insert/remove/error → INFO+STATUS_SW+DOM del | M1/M3 |
| `test_get_transceiver_{dom_sensor,status,thresholds}` (NotImplementedError paths) | `MockSfp` returning empty/err → empty write | M2/M3 |
| `test_post_port_dom_sensor_info_to_db`, `test_beautify_dom_info_dict` | DOM publish + beautify unit‑strip | M2 |
| `test_update_flag_metadata_tables`, `test_post_port_dom_flags_to_db` | flag change‑count/set/clear engine | later |
| `test_detect_port_in_error_status`, `sfp_status_helper` bit decode | error‑bitmap decode + blocking→del‑DOM | M3 |
| `test_CmisManagerTask_update_..._cmis_state`, `_process_single_lport_*` | cmis_state write + READY short‑circuit | M3 |
| `test_get_port_mapping`, `test_handle_port_config_change` | `PortMapping` build from mock CONFIG_DB | M1/M5 |
| `test_DomInfoUpdateTask_task_worker*` | one poll pass over `MockHal` multi‑port | M2/M5 |

**New Rust tests (no direct Python analog, from the modular refactor / Rust
idioms):** the `Hal`/`StateDb` trait wiring itself; NUL‑stripping + `str(bool)`
rendering (`stringify`/`pybool`); `Ethernet{i*4}` mapping in `discover_ports`;
the `Arc<AtomicBool>` stop‑flag shutdown; and any `dom/utilities/*` split that
upstream tested as a monolith.

## 3.7 Milestone mapping (M0–M6)

Cumulative gates from `orchestrator/milestones.py` + `xcvrd-tests` (README §5).
Each milestone = daemon logic (Part A) **+** its Rust unit tests (Part B) **+**
the black‑box gate. **M0/M1 already pass on the bootstrap** — do not regress them.

| M | Adds (Part A) | Modules touched | Black‑box gate | Part‑B focus |
|---|---|---|---|---|
| **M0** | Skeleton compiles, injects, stays RUNNING | existing `main/lib/env/daemon` | deploy‑smoke (supervisor RUNNING; no pytest) | crate builds; trait seams compile |
| **M1** | Presence + identity: `TRANSCEIVER_INFO` publish/clear/restore; `STATUS_SW.status` 1/0; `cmis_state` seeded | `xcvrd/sfp_state_update.rs`, `port_event_helper.rs`, `hal.rs`, `statedb.rs` | `test_presence` + `test_info_content` | info publish/clear, event mapping, port mapping |
| **M2** | DOM poll: `DomInfoUpdateTask` publishes `TRANSCEIVER_DOM_SENSOR` (+`DOM_THRESHOLD` on insert); real EEPROM reads on the Monitor trace | `dom/dom_mgr.rs`, `dom/utilities/{db,dom_sensor}`, thread spawn in `xcvrd/mod.rs` | + `test_dom` + `test_interaction_trace` | DOM publish + beautify; 60 s cadence; plug read‑burst |
| **M3** | Status/CMIS/errors: decode error events → `STATUS_SW.error`, blocking→del DOM (keep INFO), non‑blocking→keep; drive `cmis_state=READY`; `TRANSCEIVER_STATUS` | `sfp_state_update.rs` (error branch), `sfp_status_helper.rs`, `cmis/…`, `dom/utilities/status` | + `test_status_error` | bitmap decode, blocking→del, recovery→N/A, cmis_state |
| **M4** | lpmode/reset: reflect lpmode state; ensure the `sfputil`/plugin path issues the CMIS `00h:26` writes (bridge `set_lpmode/reset` available; daemon must not interfere) | `sff_mgr.rs`/`cmis` as needed; mostly verify no regression | + `test_lpmode_reset` | lpmode on/off state; MGC write bits (0x08/0x10) |
| **M5** | Multiport concurrency: independent per‑port handling, no cross‑talk on simultaneous plug/DOM | task loops iterate all ports; per‑port isolation | + `test_multiport` | concurrent unplug/replug; partial‑unplug isolation; distinct DOM |
| **M6** | Golden conformance: exact `{INFO, STATUS_SW, DOM_THRESHOLD}` projection; full suite incl. slow | all above, field‑exact | + `test_golden` (+ every earlier module, no marker filter) | golden diff == empty |

> **M4 note:** `test_lpmode_reset` drives the module through `sfputil`/the
> `sonic_platform` bridge (the negative‑control shows it passes even against a
> dummy xcvrd — `xcvrd-tests/DESIGN.md §6`). So M4 is largely a **"don't regress"**
> milestone for the Rust daemon: expose/keep the lpmode path working and avoid
> stomping the `00h:26` writes. The bridge already provides `set_lpmode`/`reset`.
>
> **CMIS scope:** the full `CmisManagerTask` state machine (1177 L) is the
> heaviest port. The oracle only requires **`cmis_state == "READY"`** for a
> present module, and the emulator brings the datapath up itself, so M3 can ship a
> **reduced** CMIS driver that reaches READY (the `process_single_lport`
> short‑circuits at `cmis_manager_task.py:1247` — non‑CMIS/flat‑memory/no‑api all
> jump straight to READY — are the reference for a minimal path). Grow toward the
> full state machine only if a later gate demands it.

---

## Appendix — key symbols/paths (verified by read)

* Bootstrap M1 daemon: `crate/xcvrd-rs/src/daemon.rs` (`run`, `serve`,
  `discover_ports`, `sync_port`, `stringify`, `pybool`); seed
  `crate/xcvrd-rs/src/env.rs` (`STATE_DB=6`, `CONFIG_DB=4`, `open_platform`,
  `open_state_db`, `open_config_db`); `src/lib.rs`, `src/main.rs`.
* Bridge API: `crate/platform-bridge/src/lib.rs` (`Platform`, `Sfp`, `ChangeEvent`,
  `BridgeError`, `call_json`).
* swss usage: `crate/xcvrd-rs/examples/{statedb_probe,hal_to_statedb}.rs`
  (`DbConnector::new_unix`, `hset`, `hgetall`, `del`, `exists`, `CxxString::from`).
* Daemon core: `source/xcvrd/xcvrd.py` (`DaemonXcvrd:877`, `SfpStateUpdateTask:259`,
  `post_port_sfp_info_to_db:178`, `_wrapper_*:105‑173`).
* DOM: `source/xcvrd/dom/dom_mgr.py:141,284`; `dom/utilities/db/utils.py:19`;
  `dom/utilities/{dom_sensor,status}/utils.py`.
* CMIS: `source/xcvrd/cmis/cmis_manager_task.py:41,85,1247`; states
  `xcvrd_utilities/common.py:22‑39`.
* Helpers: `xcvrd_utilities/{xcvr_table_helper.py:11,sfp_status_helper.py,
  port_event_helper.py:212,346,common.py:110,124,259}`.
* Oracle: `xcvrd-tests/tests/test_{presence,info_content,dom,status_error,
  lpmode_reset,multiport,interaction_trace,golden}.py`; `lib/{golden.py,errors.py,
  statedb.py,emu.py,cmis.py}`; `golden/Ethernet100.json`; `conftest.py` (`Module`).
* Unit‑test mocks: `source/xcvrd/tests/{test_xcvrd.py,mock_swsscommon.py,
  mock_platform.py}`.
* Milestones: `orchestrator/milestones.py`.
