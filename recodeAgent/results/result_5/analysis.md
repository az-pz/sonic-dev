# xcvrd Python → Rust — Analyzer Design (analysis.md)

> ReCodeAgent Analyzer output (paper §3.2, Figure 5). Three sections:
> **(1) Source Project Research**, **(2) Third-Party Library Analysis**,
> **(3) Target Project Design** (structural mapping, module tree, STATE_DB schema
> contract, PyO3 platform-bridge boundary, unit-test strategy, and a source-cited
> **behavior inventory for scoping**). This document writes **no Rust** and does
> **not** define milestones — the Scoper partitions the behavior inventory into
> milestones downstream.
>
> **Provenance of cited paths.** The daemon source is the modular refactor
> installed on the DUT at `pmon:/usr/local/lib/python3.13/dist-packages/xcvrd/`
> (26 `.py` files, 6502 LOC) — the same tree the pipeline places at
> `source/xcvrd/`; the Python behavioral unit tests live beside it at
> `.../dist-packages/tests/test_xcvrd.py` (7104 LOC, 187 tests) + `mock_*.py`,
> i.e. `source/xcvrd/tests/`. All `source/xcvrd/...` citations below were read
> from that live tree. The immutable scaffolding is under `crate/`; the black-box
> oracle is `../xcvrd-tests/` (41 `test_*.py` modules, 103 tests). Line numbers
> are from the read snapshot and are indicative.

---

## 1. Source Project Research

### 1.1 Overview

**xcvrd** is SONiC's transceiver-monitoring daemon (runs in the `pmon` container).
It discovers pluggable optics, reads their identity/DOM/status via the platform
`sonic_platform` HAL, drives CMIS/SFF module bring-up, and publishes everything to
Redis **STATE_DB** so the rest of SONiC (CLI, telemetry, orchagent) can consume a
consistent transceiver view. Upstream:
<https://github.com/sonic-net/sonic-platform-daemons/tree/master/sonic-xcvrd>;
design intent: the Transceiver-Monitoring HLD
(`doc/xrcvd/transceiver-monitor-hld.md`).

The top-level daemon is **`DaemonXcvrd`** (`source/xcvrd/xcvrd.py:890`). Its
`init()` (`xcvrd.py:1034`) loads `sonic_platform.platform.Platform().get_chassis()`
into the global `platform_chassis`, waits for `PortConfigDone`
(`wait_for_port_config_done`, `xcvrd.py:929`), builds the logical↔physical
`PortMapping` from CONFIG_DB, seeds `NPU_SI_SETTINGS_SYNC_STATUS` in STATE_DB
`PORT_TABLE`, builds `sfp_obj_dict` (`initialize_sfp_obj_dict`, `xcvrd.py:975`),
and clears stale `TRANSCEIVER_INFO` for absent modules
(`remove_stale_transceiver_info`, `xcvrd.py:999`). `run()` (`xcvrd.py:1154`) then
spawns the worker threads, `stop_event.wait()`s, and on shutdown joins them and
runs `deinit()` (`xcvrd.py:1095`) which purges the per-port tables (deliberately
**not** `TRANSCEIVER_INFO`, to avoid a Tx-disable glitch — `xcvrd.py:1117`).

**Task threads** (started in `DaemonXcvrd.run`, gated by CLI flags):

| Thread | Class / file | Cadence | Responsibility |
|---|---|---|---|
| **SfpStateUpdateTask** | `xcvrd.py:259` | event-driven (`get_change_event`) + 60 s | Presence/identity: `TRANSCEIVER_INFO`, `TRANSCEIVER_STATUS_SW.status/error`, DOM/VDM thresholds at insert, plug/unplug table teardown, EEPROM read-retry, error-bitmap handling, media-settings notify. The system-ready state machine (INIT/NORMAL/EXIT). |
| **CmisManagerTask** | `cmis/cmis_manager_task.py:41` | ~1 s loop | CMIS datapath state machine; writes `TRANSCEIVER_STATUS_SW.cmis_state` through every state (INSERTED→…→READY/FAILED); app-select, host/media lane masks, coherent tx-power/laser-freq tuning, decommission. |
| **DomInfoUpdateTask** | `dom/dom_mgr.py:141` | `dom_update_interval` (default **60 s**) | Periodic hardware polling: `TRANSCEIVER_DOM_SENSOR`, `TRANSCEIVER_DOM_FLAG` (+metadata), `TRANSCEIVER_STATUS`, `TRANSCEIVER_STATUS_FLAG` (+metadata), `TRANSCEIVER_VDM_*`, `TRANSCEIVER_PM`, `TRANSCEIVER_FIRMWARE_INFO`; DOM gating during CMIS init and `dom_polling=disabled`. |
| **DomThermalInfoUpdateTask** | `dom/dom_mgr.py` (`DomThermalInfoUpdateBase`) | `dom_temperature_poll_interval` (opt-in) | `TRANSCEIVER_DOM_TEMPERATURE`. |
| **SffManagerTask** | `sff_mgr.py:45` | event/loop | Deterministic link bring-up for **non-CMIS SFF** (SFF-8636/8472) modules: drives `tx_disable`, low-power-disable, high-power-class enable via APPL_DB `PORT_TABLE` + platform. Opt-in (`--enable_sff_mgr`). |

CLI (`main()`, `xcvrd.py:1257`): `--skip_cmis_mgr`, `--enable_sff_mgr`,
`--dom_temperature_poll_interval`, `--dom_update_interval`.

### 1.2 Directory Structure (`source/xcvrd/`)

```
xcvrd/
├── __init__.py
├── xcvrd.py                    # DaemonXcvrd, SfpStateUpdateTask, post_port_sfp_info_to_db, _wrapper_* helpers, main()
├── sff_mgr.py                  # SffManagerTask (SFF/8636 link bring-up), SffLoggerForPortUpdateEvent
├── cmis/
│   ├── __init__.py             # re-exports CmisManagerTask
│   └── cmis_manager_task.py    # CmisManagerTask: CMIS datapath state machine
├── dom/
│   ├── __init__.py
│   ├── dom_mgr.py              # DomInfoUpdateBase/Task, DomThermalInfoUpdateTask
│   └── utilities/
│       ├── db/utils.py         # DBUtils: post_diagnostic_values_to_db, flag-metadata tracking, beautify, get_current_time
│       ├── dom_sensor/{utils.py,db_utils.py}   # DOMUtils / DOMDBUtils: DOM sensor/flag/threshold/temperature
│       ├── status/{utils.py,db_utils.py}       # StatusUtils / StatusDBUtils: TRANSCEIVER_STATUS(+FLAG)
│       └── vdm/{utils.py,db_utils.py}          # VDMUtils / VDMDBUtils: TRANSCEIVER_VDM_* (+ freeze/statistic)
└── xcvrd_utilities/
    ├── common.py               # presence/status_sw writers, del helpers, CMIS helpers, _wrapper_*
    ├── sfp_status_helper.py    # SFP error-bit model (blocking/generic/vendor masks + descriptions)
    ├── port_event_helper.py    # PortMapping, PortChangeEvent, PortChangeObserver, get_port_mapping
    ├── xcvr_table_helper.py    # STATE_DB table-name constants + XcvrTableHelper (all Table handles)
    ├── media_settings_parser.py# media_settings.json → APPL_DB SerDes settings (notify_media_setting)
    ├── optics_si_parser.py     # optics_si_settings.json parser
    └── utils.py                # XCVRDUtils: presence/lpmode/flat-memory helpers over sfp_obj_dict
```

### 1.3 Key Structures & Interfaces

**The platform API surface xcvrd calls** (all through `platform_chassis` /
`platform_chassis.get_sfp(i)`; wrappers in `xcvrd.py:105-173` and
`common.py:124-332`):

| Python call | Where | Purpose |
|---|---|---|
| `platform_chassis.get_num_sfps()` | `initialize_sfp_obj_dict` | slot count |
| `platform_chassis.get_sfp(i)` | everywhere | per-SFP handle (0-based physical index) |
| `platform_chassis.get_change_event(timeout)` | `_wrapper_get_transceiver_change_event` `xcvrd.py:141` | `(status, {'sfp':{port:code}, 'sfp_error':{port:code}})` insert=`'1'`/remove=`'0'`/else error bitmap |
| `sfp.get_presence()` | `common._wrapper_get_presence` `common.py:124` | module present? |
| `sfp.is_replaceable()` | `_wrapper_is_replaceable` `xcvrd.py:105` | FRU flag |
| `sfp.get_transceiver_info()` | `_wrapper_get_transceiver_info` `xcvrd.py:114` | identity dict → `TRANSCEIVER_INFO` |
| `sfp.get_transceiver_dom_real_value()` | `DOMUtils` | live DOM → `TRANSCEIVER_DOM_SENSOR` |
| `sfp.get_transceiver_dom_flags()` / `..._thresholds()` / `..._temperature()` | `DOMUtils` | DOM flags/thresholds/temp |
| `sfp.get_transceiver_status()` / `..._status_flags()` | `StatusUtils` `status/utils.py` | `TRANSCEIVER_STATUS(+FLAG)` |
| `sfp.get_transceiver_pm()` | `common._wrapper_get_transceiver_pm` `common.py:325` | `TRANSCEIVER_PM` |
| `sfp.get_transceiver_info_firmware_versions()` | `common._wrapper_get_transceiver_firmware_info` `common.py:316` | `TRANSCEIVER_FIRMWARE_INFO` |
| `sfp.get_xcvr_api()` → `api.*` | CMIS/SFF/VDM | `is_flat_memory`, `is_coherent_module`, `get_module_type_abbreviation`, `get_application_advertisement`, datapath/config regs, VDM freeze |
| `sfp.set_lpmode()/get_lpmode()/reset()` | CMIS/SFF/sfputil | low-power + reset control |
| `sfp.get_error_description()`, `sfp.sfp_type`, `sfp.get_reset_status()` | wrappers | error text / type / reset line |
| `sfp.remove_xcvr_api()` | on removal `xcvrd.py:589` | drop cached api |

**port_mapping (logical↔physical).** `PortMapping`
(`port_event_helper.py`) is built by `get_port_mapping(namespaces)`
(`port_event_helper.py:346`) from CONFIG_DB `PORT` table: each front-panel `PORT|Ethernet*`
key's `index` field is the physical SFP index. Accessors:
`logical_to_physical`, `physical_to_logical`, `logical_to_asic`,
`get_asic_id_for_logical_port`, `get_logical_to_physical`,
`get_physical_to_logical`, `logical_port_name_to_physical_port_list`
(`port_event_helper.py:260-281`). On this testbed the mapping is
`Ethernet{index*4} ↔ index` (matches `crate/.../daemon.rs:87` and
`../xcvrd-tests/DESIGN.md §9`). `PortChangeObserver` (`port_event_helper.py:46`)
subscribes CONFIG_DB `PORT` / STATE_DB `TRANSCEIVER_INFO` / STATE_DB `PORT_TABLE`
to raise `PortChangeEvent` (ADD/REMOVE/SET/DEL).

**XcvrTableHelper** (`xcvrd_utilities/xcvr_table_helper.py:55`) is the single place
that opens STATE_DB and constructs every `swsscommon.Table` handle (one per
ASIC), plus APPL_DB `ProducerStateTable` and CONFIG_DB port table. It defines all
table-name constants (`xcvr_table_helper.py:11-47`) — the authoritative STATE_DB
schema list in §3.4.

### 1.4 Data Models / STATE_DB schema (produced by xcvrd)

All rows are keyed by **logical port name** (e.g. `TRANSCEIVER_INFO|Ethernet100`)
except `TRANSCEIVER_PM` which uses the physical-port name (`dom_mgr.py:263`, same
as logical on non-ganged testbeds). Field values are stringified; diagnostic rows
append a `last_update_time` (`db/utils.py:53`). Full list from
`xcvr_table_helper.py`:

| Table | Producer (source) | Key fields (oracle-observed) |
|---|---|---|
| `TRANSCEIVER_INFO` | `post_port_sfp_info_to_db` `xcvrd.py:178` (CMIS branch dumps the whole `get_transceiver_info()` dict + `is_replaceable`) | `manufacturer, model, vendor_oui, serial, vendor_rev, vendor_date, type, type_abbrv_name, connector, cable_length, cable_type, ext_identifier, specification_compliance, cmis_rev, media_interface_technology, application_advertisement, is_replaceable, vdm_supported, host_lane_count, media_lane_count, active_apsel_hostlane1..8, hardware_rev, nominal_bit_rate, encoding, ext_rateselect_compliance, supported_max_laser_freq (coherent)` |
| `TRANSCEIVER_STATUS_SW` | `common.update_port_transceiver_status_table_sw` `common.py:110` (`status`,`error`) + `CmisManagerTask.update_port_transceiver_status_table_sw_cmis_state` `cmis_manager_task.py:90` (`cmis_state`) | `status` (`'1'`/`'0'`), `error` (`'N/A'`/`'|'`-joined), `cmis_state` |
| `TRANSCEIVER_DOM_SENSOR` | `DOMDBUtils.post_port_dom_sensor_info_to_db` `dom_sensor/db_utils.py:40` | `temperature, voltage, tx1..8power, rx1..8power, tx1..8bias, last_update_time` (units stripped, `dom_sensor/db_utils.py:120-143`) |
| `TRANSCEIVER_DOM_THRESHOLD` | `post_port_dom_thresholds_to_db` `dom_sensor/db_utils.py:107` (cached at insert) | `temp/vcc/tx*/rx*/txbias {high,low}{alarm,warning}`, `lasertemp*` |
| `TRANSCEIVER_DOM_FLAG` (+ `_CHANGE_COUNT` / `_SET_TIME` / `_CLEAR_TIME`) | `post_port_dom_flags_to_db` `dom_sensor/db_utils.py:53` + `DBUtils._update_flag_metadata_tables` `db/utils.py:107` | latched monitor flags + change-count/set-time/clear-time metadata |
| `TRANSCEIVER_DOM_TEMPERATURE` | `DomThermalInfoUpdateTask` | module temperature (opt-in thread) |
| `TRANSCEIVER_STATUS` | `StatusDBUtils.post_port_transceiver_hw_status_to_db` `status/db_utils.py:21` | `module_state, module_fault_cause, DP1..8State, config_state_hostlane1..8, dpinit_pending_hostlane1..8, dpdeinit_hostlane1..8, txNdisable, txNOutputStatus, rxNOutputStatusHostlane, tx_disabled_channel` (golden: `../xcvrd-tests/golden/steady_state/Ethernet100.json`) |
| `TRANSCEIVER_STATUS_FLAG` (+ metadata trio) | `post_port_transceiver_hw_status_flags_to_db` `status/db_utils.py:41` | latched status flags + metadata |
| `TRANSCEIVER_VDM_REAL_VALUE` | `VDMDBUtils.post_port_vdm_real_values_from_dict_to_db` `vdm/db_utils.py:25` | VDM observables (basic + statistic) |
| `TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_THRESHOLD` | `post_port_vdm_thresholds_to_db` `vdm/db_utils.py:62` | per-type thresholds |
| `TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_FLAG` (+ metadata trio each) | `post_port_vdm_flags_to_db` `vdm/db_utils.py:58` | per-type VDM flags + metadata |
| `TRANSCEIVER_PM` | `DomInfoUpdateTask.post_port_pm_info_to_db` `dom_mgr.py:238` (coherent, via VDM-freeze) | `prefec_ber_avg, cd_avg, dgd_avg, osnr_avg, tx_power_avg, rx_tot_power_avg, …` |
| `TRANSCEIVER_FIRMWARE_INFO` | `DomInfoUpdateTask.post_port_sfp_firmware_info_to_db` `dom_mgr.py:203` | `active_firmware, inactive_firmware` |

Non-transceiver rows xcvrd also touches: STATE_DB `PORT_TABLE|<lport>` field
`NPU_SI_SETTINGS_SYNC_STATUS` (`xcvrd.py:805,954`); APPL_DB `PORT_TABLE`
(`ProducerStateTable`, `xcvr_table_helper.py:100`) for SFF/CMIS `tx_disable` /
host-side settings.

**Cross-reference to the oracle** (`../xcvrd-tests/`, the ultimate contract):
`TRANSCEIVER_INFO` fields → `test_info_content.py`, `test_presence.py`;
`TRANSCEIVER_STATUS_SW.{status,error,cmis_state}` →
`test_presence.py`,`test_status_error.py`,`test_cmis_state_progression.py`;
`TRANSCEIVER_STATUS` rich fields → `test_transceiver_status.py`,`test_cmis_datapath.py`;
DOM sensor/threshold/flag → `test_dom.py`,`test_dom_flag_meta.py`,`test_last_update_time.py`;
VDM → `test_vdm.py`,`test_vdm_statistic.py`; PM → `test_pm.py`;
firmware → `test_firmware_info.py`; removal set → `test_removal_tables.py`;
golden projection of `{INFO, STATUS_SW, DOM_THRESHOLD, STATUS, DOM_FLAG}` →
`test_golden.py` + `golden/*/*.json`.

### 1.5 Error Handling

- **Absent module:** every writer guards on `common._wrapper_get_presence`
  (`common.py:124`) / `_validate_and_get_physical_port` (`db/utils.py:62`) and
  no-ops when the SFP is not present. `remove_stale_transceiver_info`
  (`xcvrd.py:999`) clears `TRANSCEIVER_INFO` at boot for absent ports.
- **Hardware / SFP error bitmaps:** `sfp_status_helper.py` models the SfpBase
  error bits — `SFP_ERRORS_BLOCKING_MASK=0x02`, `SFP_ERRORS_GENERIC_MASK=0x0000FFFE`,
  `SFP_ERRORS_VENDOR_SPECIFIC_MASK=0xFFFF0000`; `is_error_block_eeprom_reading`,
  `fetch_generic_error_description`, `has_vendor_specific_error`
  (`sfp_status_helper.py:12-27`). On a change-event error code
  (`xcvrd.py:622-666`) xcvrd sets `STATUS_SW.error` to the `'|'`-joined
  descriptions and, for a **blocking** error, deletes the DOM/VDM/PM/status
  tables while keeping static `TRANSCEIVER_INFO` (`xcvrd.py:641`).
- **EEPROM-not-ready:** `post_port_sfp_info_to_db` returns `SFP_EEPROM_NOT_READY`
  (`xcvrd.py:242`); the port goes into `retry_eeprom_set` and is retried every 60 s
  (`retry_eeprom_reading`, `xcvrd.py:849`).
- **Threading/shutdown:** each task caches its exception in `self.exc`, sets
  `main_thread_stop_event` (`xcvrd.py:712`, `dom_mgr.py:123`), and `join()`
  re-raises. `SfpStateUpdateTask` supports async interrupt via
  `PyThreadState_SetAsyncExc` (`xcvrd.py:721`). The system-ready state machine
  can escalate to `STATE_EXIT` → `os.kill(getppid(), SIGTERM)` (`xcvrd.py:698`).
  `NotImplementedError` on a required platform op → `sys.exit(NOT_IMPLEMENTED_ERROR)`.

### 1.6 Dependencies (Python imports)

`sonic_platform_base.sonic_xcvr.api.*` (CMIS/SFF decode — `CmisApi`, `Sff8472Api`),
`swsscommon` (STATE_DB/APPL_DB/CONFIG_DB `Table`/`ProducerStateTable`/
`SubscriberStateTable`/`Select`), `sonic_py_common`
(`daemon_base.db_connect`, `syslogger`, `multi_asic`), `natsort.natsorted`, and
the stdlib (`threading`, `time`, `datetime`, `copy`, `ctypes`, `signal`,
`argparse`, `re`, `json`, `subprocess`). CMIS/SFF **decode** is entirely in
`sonic_platform_base` behind `get_xcvr_api()` — this is what the thick HAL keeps in
Python (see §2, §3.3).

### 1.7 Unit tests (`source/xcvrd/tests/`)

`test_xcvrd.py` (7104 LOC, **187** `test_*`) imports the **modular** packages
directly (`from xcvrd.dom.dom_mgr import *`, `from xcvrd.cmis import
CmisManagerTask`, `from xcvrd.dom.utilities.*` …) — i.e. **this test file is
co-evolved with the modular refactor**, not stock master, so most behaviors map
to a Rust module 1:1 (call this out: fewer "new test" gaps than a pure-master
port). Mocking strategy (`test_xcvrd.py:1-45`):

- **STATE_DB is mocked**, not real: `daemon_base.db_connect = MagicMock()` and
  `swsscommon.Table/ProducerStateTable/SubscriberStateTable/SonicDBConfig =
  MagicMock()`. Where a test needs real table semantics it uses
  `from .mock_swsscommon import Table` — a dict-backed fake with
  `set/get(→(found,fvs))/hget/hdel/_del/getKeys/get_size` (`tests/mock_swsscommon.py`).
- **Platform is mocked** at the wrapper/api level: `@patch('xcvrd.xcvrd._wrapper_get_transceiver_info', MagicMock(return_value={...}))`
  (`test_xcvrd.py:1427`), `MagicMock()` SFP objects whose `get_transceiver_*`
  return canned dicts, and `sfp_obj_dict[pport] = MagicMock()`. The shared
  `tests/mock_platform.py` (`MockChassis`) is the daemons-repo fixture (fans/psu/
  thermals); the xcvrd-relevant fake is `mock_swsscommon.Table` plus inline
  MagicMocks. 1352 `@patch`/`MagicMock` uses total.
- Test classes: `TestXcvrdThreadException` (`:393`), `TestXcvrdScript` (`:598`,
  the bulk), `TestOpticSiParser` (`:6944`). Fixture JSON
  (`media_settings.json`, `gearbox_media_settings.json`, `optics_si_settings.json`)
  loaded from the tests dir.

**Which behaviors are unit-testable and how they mock the boundary** — the two
seams are exactly (a) the **platform API** (mock the `get_transceiver_*`/presence/
change-event returns) and (b) **STATE_DB** (`mock_swsscommon.Table`). Every
`test_*` maps to one of the two seams, which is why the daemon must expose a
mockable HAL trait and DB trait in Rust (§3.6). Representative coverage:

`test_SfpStateUpdateTask_task_worker / _on_add_logical_port / _mapping_event_from_change_event / _retry_eeprom_reading / _handle_port_change_event`,
`test_CmisManagerTask_task_worker[_decommission/_fastboot/_host_tx_ready_*] / _process_single_lport_* / _get_cmis_host_lanes_mask / _is_cmis_application_update_required / _post_port_active_apsel_to_db / _update_port_transceiver_status_table_sw_cmis_state`,
`test_DomInfoUpdateTask_task_worker[_vdm_failure/_vdm_freeze_conditions] / _is_port_in_cmis_initialization_process / _get_dom_polling_from_config_db`,
`test_SffManagerTask_task_worker / _enable_high_power_class / _get_host_tx_status`,
`test_DaemonXcvrd_init_deinit_cold / _run[_with_exception] / _signal_handler / _wait_for_port_config_done / _initialize_port_init_control_fields_in_port_table`,
plus pure-helper units: `test_get_transceiver_dom_sensor_real_value / _dom_flags / _dom_thresholds / _dom_temperature / _get_transceiver_status[_flags] / _get_vdm_* / _beautify_[dom_]info_dict / _del_port_sfp_dom_info_from_db / _detect_port_in_error_status / _get_interface_speed / _check_port_in_range / _get_port_mapping / _get_state_db_port_table_val_by_key / _custom_media_settings_* / _fetch_optics_si_setting*`.

---

## 2. Third-Party Library Analysis

For each Python dependency: what it does, how xcvrd uses it, and the Rust
recommendation — flagging what the **provided scaffolding already covers** so the
Translator does **not** reinvent it.

| Python dep | How xcvrd uses it | Recommendation in Rust |
|---|---|---|
| **`sonic_platform` + `sonic_platform_base.sonic_xcvr` (CMIS/SFF decode)** | ALL transceiver I/O + CMIS/SFF decode via `platform_chassis.get_sfp(i)` and `sfp.get_xcvr_api()` | **Already provided — do NOT re-implement.** Use `platform_bridge::{Platform,Sfp}` (`crate/platform-bridge/src/lib.rs`), a PyO3 thick boundary that keeps decode in Python. Typed wrappers exist for the common calls; `Sfp::call_json(method, ())` reaches any un-wrapped **no-arg** method. Thick-HAL is the project's non-negotiable adaptation #1. |
| **`swsscommon` (STATE_DB/CONFIG_DB/APPL_DB)** | `Table`/`ProducerStateTable`/`SubscriberStateTable`/`Select`; `daemon_base.db_connect` | **Already provided — do NOT hand-roll a Redis client.** Use the official `swss-common` crate (`crate/xcvrd-rs/Cargo.toml:31`, pinned rev). `env.rs` opens STATE_DB(6)/CONFIG_DB(4) over the unix socket; wrap a `DbConnector` in `swss_common::Table` for table-scoped `set/get/del/getKeys`. Subscribe/select for port-config change likewise via swss-common (`SubscriberStateTable`, `Select`). |
| **`sonic_py_common.multi_asic`** | namespace/asic-id resolution; front-panel-port filter | std logic. Single-ASIC testbed ⇒ namespace `''`, asic_id `0` (`common.get_namespace_from_asic_id` `common.py:85`). Port `PortMapping` in a Rust `xcvrd_utilities::port_event_helper` module. No new crate. |
| **`sonic_py_common.syslogger` / `daemon_base.DaemonBase`** | logging + daemon skeleton | The bootstrap already logs via `eprintln!` (`daemon.rs`). Recommend a light logging facade — **`log` + `env_logger`** (or `syslog` crate) if syslog parity is wanted; std `eprintln!` is sufficient for the oracle. Keep the `DaemonBase` role as a plain `DaemonXcvrd` struct owning `stop_event`/threads. |
| **`natsort.natsorted`** | natural sort of port keys | `BTreeMap`/`Vec` + a small natural-order comparator, or the **`natord`** crate. Minor; only affects iteration order, not the STATE_DB contract. |
| **`threading` / `threading.Event` / `ctypes` async-exc** | 5 worker threads + cooperative stop | **`std::thread`** + `Arc<AtomicBool>` (or `std::sync::mpsc` / `Condvar`) for the stop event; `JoinHandle` for join. The `ctypes` async-interrupt is a Python-GIL hack with no Rust analogue — model as a cooperative stop flag checked in the loop. |
| **`time` / `datetime`** | poll cadence, `last_update_time` (`"%a %b %d %H:%M:%S %Y"` UTC, `db/utils.py:161`) | **`std::time`** for cadence; **`chrono`** for the exact UTC `strftime` format that `test_last_update_time.py` observes. |
| **`re`** | media-settings / DOM-key regex (`^(tx|rx)[1-8]power$`), port-range | **`regex`** crate. |
| **`json` / `ast.literal_eval`** | media/optics settings JSON; `application_advertisement` is a Python-`repr` dict string | **`serde_json`** (already a dep). Note the oracle reads `application_advertisement` with `ast.literal_eval` (`test_info_content.py:93`) — the bridge returns it verbatim from Python, so preserve the Python-`repr` string as-is (don't re-serialize to JSON). |
| **`subprocess` / `argparse` / `signal`** | reboot-status checks, CLI, SIGTERM/SIGINT/SIGHUP | **`clap`** (or hand-rolled) for the 4 flags; **`signal-hook`** for signals; `std::process::Command` for the rare shell-outs. |

**Net:** the two hard interop problems (transceiver I/O, STATE_DB) are **solved by
the provided crates**. Genuinely new crates are only small utilities:
`chrono`, `regex`, optionally `log`/`env_logger`, `signal-hook`, `clap`,
`natord`. `serde_json` + `thiserror` are already wired.

---

## 3. Target Project Design

### 3.1 Overview & Translation Requirements

Translate the **daemon logic** of `source/xcvrd/` into the `xcvrd-rs` crate so the
observable STATE_DB behavior is functionally equivalent to reference Python xcvrd,
as measured by the black-box `../xcvrd-tests/` suite (the ultimate oracle, never
modified) **and** by Rust unit tests ported from `test_xcvrd.py`. Hard constraints
baked in:

1. **Thick HAL** — all transceiver I/O + CMIS/SFF decode goes through
   `platform_bridge` (PyO3 → `sonic_platform`); no Rust re-implementation of
   CMIS/SFF/gRPC/emulator.
2. **STATE_DB via `swss-common`** — no hand-rolled Redis.
3. **Two validation layers** — Rust unit tests over **mocks** (Part B) + the e2e
   suite on the DUT.
4. **Immutable input** — `crate/` is read-only; work happens in `pipeline/crate/`,
   extending the existing M1 bootstrap (`daemon.rs`, `env.rs`), never breaking M0
   (stays RUNNING under supervisor) or M1 (presence + identity already green).
5. **Scopable** — the behavior inventory (§3.7) is complete/unambiguous for the
   Scoper; this doc assigns **no** milestone ids.

### 3.2 Source→Rust structural mapping (preserve the package layout)

Python module → Rust module of the same name; Python subpackage → Rust submodule
directory; Python class/task → Rust `struct` + methods (or a thread `run` loop);
dict→STATE_DB writes → `swss_common::Table` calls. Idiom mappings: exceptions →
`Result<T, E>` (`thiserror`); `None` → `Option`; `dict` → `serde_json::Value` /
`BTreeMap<String,String>`; `threading.Thread` → `std::thread` + stop flag;
`str(value)` field rendering → the `stringify`/`pybool` helpers already in
`daemon.rs:121-142` (NUL-trim CMIS strings). Preserve `snake_case` identifiers so
the port stays traceable.

| Python (`source/xcvrd/…`) | Rust (`pipeline/crate/xcvrd-rs/src/…`) | Notes |
|---|---|---|
| `xcvrd.py` `DaemonXcvrd` | `xcvrd.rs` (or `xcvrd/mod.rs`) `struct DaemonXcvrd` | owns `stop_event: Arc<AtomicBool>`, thread handles; `init/run/deinit/signal_handler`. `daemon.rs::run()` stays the entrypoint and delegates here (keeps M0/M1). |
| `xcvrd.py` `SfpStateUpdateTask` | `xcvrd.rs` `struct SfpStateUpdateTask` + `run()` | change-event loop, `post_port_sfp_info_to_db`, retry set, error-bitmap handling, the INIT/NORMAL/EXIT state machine (constants `xcvrd.py:66-89`). |
| `xcvrd.py` `post_port_sfp_info_to_db` | `xcvrd.rs::post_port_sfp_info_to_db` | the M1 bootstrap `sync_port` (`daemon.rs:96`) is its seed; extend to the CMIS/SFF branch (`xcvrd.py:211-239`). |
| `sff_mgr.py` `SffManagerTask` | `sff_mgr.rs` `struct SffManagerTask` | non-CMIS link bring-up. |
| `cmis/cmis_manager_task.py` `CmisManagerTask` | `cmis/cmis_manager_task.rs` (`cmis/mod.rs` re-exports) | the datapath state machine + `cmis_state` writer. |
| `dom/dom_mgr.py` `DomInfoUpdateTask`,`DomThermalInfoUpdateTask` | `dom/dom_mgr.rs` | periodic poll loop. |
| `dom/utilities/db/utils.py` `DBUtils` | `dom/utilities/db/utils.rs` | `post_diagnostic_values_to_db`, flag-metadata tracking, `beautify`, `get_current_time`. |
| `dom/utilities/dom_sensor/{utils,db_utils}.py` | `dom/utilities/dom_sensor/{utils,db_utils}.rs` | DOM sensor/flag/threshold. |
| `dom/utilities/status/{utils,db_utils}.py` | `dom/utilities/status/{utils,db_utils}.rs` | `TRANSCEIVER_STATUS(+FLAG)`. |
| `dom/utilities/vdm/{utils,db_utils}.py` | `dom/utilities/vdm/{utils,db_utils}.rs` | VDM real/threshold/flag + freeze. |
| `xcvrd_utilities/common.py` | `xcvrd_utilities/common.rs` | presence, `update_port_transceiver_status_table_sw`, `del_port_sfp_dom_info_from_db`, CMIS state constants (`common.py:23-39`). |
| `xcvrd_utilities/sfp_status_helper.py` | `xcvrd_utilities/sfp_status_helper.rs` | error-bit masks + descriptions. |
| `xcvrd_utilities/port_event_helper.py` | `xcvrd_utilities/port_event_helper.rs` | `PortMapping`, `PortChangeEvent`, `PortChangeObserver`, `get_port_mapping`. |
| `xcvrd_utilities/xcvr_table_helper.py` | `xcvrd_utilities/xcvr_table_helper.rs` | table-name consts + `XcvrTableHelper`. |
| `xcvrd_utilities/media_settings_parser.py` | `xcvrd_utilities/media_settings_parser.rs` | `notify_media_setting`. |
| `xcvrd_utilities/optics_si_parser.py` | `xcvrd_utilities/optics_si_parser.rs` | optics-SI parse. |
| `xcvrd_utilities/utils.py` (`XCVRDUtils`) | `xcvrd_utilities/utils.rs` | presence/lpmode/flat-memory helpers. |

### 3.3 Module structure for the `xcvrd-rs` crate

Mirror the Python package under `pipeline/crate/xcvrd-rs/src/`, adding **HAL/DB
trait seams** and a **mock module** without disturbing the existing bootstrap:

```
src/
├── lib.rs                 # pub mod { env, daemon, xcvrd, sff_mgr, cmis, dom, xcvrd_utilities, hal, db }  + #[cfg(test)] mock
├── main.rs                # (unchanged) -> xcvrd_rs::daemon::run()
├── env.rs                 # (extend) open_platform/open_state_db/open_config_db (already present)
├── daemon.rs              # (extend) run(): keep M0/M1 loop, delegate to xcvrd::DaemonXcvrd
├── hal.rs                 # trait Chassis + trait Sfp (HAL seam); real impl wraps platform_bridge
├── db.rs                  # trait StateDb + trait Table (DB seam); real impl wraps swss_common
├── xcvrd.rs               # DaemonXcvrd, SfpStateUpdateTask, post_port_sfp_info_to_db, constants
├── sff_mgr.rs             # SffManagerTask
├── cmis/
│   ├── mod.rs
│   └── cmis_manager_task.rs
├── dom/
│   ├── mod.rs
│   ├── dom_mgr.rs
│   └── utilities/
│       ├── mod.rs
│       ├── db/utils.rs
│       ├── dom_sensor/{mod.rs, utils.rs, db_utils.rs}
│       ├── status/{mod.rs, utils.rs, db_utils.rs}
│       └── vdm/{mod.rs, utils.rs, db_utils.rs}
└── xcvrd_utilities/
    ├── mod.rs
    ├── common.rs
    ├── sfp_status_helper.rs
    ├── port_event_helper.rs
    ├── xcvr_table_helper.rs
    ├── media_settings_parser.rs
    ├── optics_si_parser.rs
    └── utils.rs
```

Mocks + unit tests: a `mock` module (behind `#[cfg(test)]` or a `mock` feature)
provides `MockChassis`/`MockSfp` (canned `serde_json::Value` identity/DOM/status,
programmable presence + change events) and `MockTable`/`MockStateDb` (a
`BTreeMap`-backed fake mirroring `tests/mock_swsscommon.py`). Each source module
carries a `#[cfg(test)] mod tests { … }` with the ported `test_xcvrd.py` cases
(§3.6). **This does not break M0/M1**: the real daemon path stays
`daemon::run → env::open_* → platform_bridge/swss_common`; traits are default-wired
to the real impls, and mocks compile only under `test`.

### 3.4 STATE_DB schema contract (per-table field mapping)

Authoritative table names from `xcvrd_utilities/xcvr_table_helper.py:11-47`; the
Rust `xcvr_table_helper.rs` must reproduce these exact strings. The observable
per-field contract each producer must reproduce is the §1.4 table, anchored by the
golden projections (`../xcvrd-tests/golden/steady_state/Ethernet100.json`,
`activated_datapath/Ethernet4.json`, `dom_flag/Ethernet4.json`) and the module
assertions (`../xcvrd-tests/tests/test_*.py`). Contract invariants the Rust port
must hold:

- **Keying:** `TABLE|<logical_port>` HGETALL semantics; values are strings;
  `TRANSCEIVER_STATUS_SW = {status∈{'1','0'}, error, cmis_state}`.
- **String rendering:** CMIS fixed-width strings are NUL-padded — trim trailing
  NUL/space (`daemon.rs:133`); booleans render Python-style `True/False`
  (`daemon.rs:122`, oracle `is_replaceable=="True"`).
- **Diagnostic rows** append `last_update_time` in the exact UTC strftime
  (`db/utils.py:161`) asserted by `test_last_update_time.py`.
- **Flag metadata trio** (`_CHANGE_COUNT`/`_SET_TIME`/`_CLEAR_TIME`) update only on
  value change; first write initializes count `0`, times `never`
  (`db/utils.py:173-224`; `test_dom_flag_meta.py`, `test_status_flag.py`,
  `test_vdm.py`).
- **Removal contract** (`test_removal_tables.py`; `xcvrd.py:598-620`): a physical
  unplug deletes the full hardware-table set (INFO, DOM_SENSOR, DOM_FLAG+trio,
  STATUS, STATUS_FLAG+trio, FIRMWARE_INFO, PM, VDM*) **but preserves**
  `TRANSCEIVER_STATUS_SW`, updated to `status='0'`.
- **Error contract** (`test_status_error.py`; `xcvrd.py:622-666`): blocking error
  ⇒ `STATUS_SW.error` set + DOM removed + INFO kept; non-blocking ⇒ error set, DOM
  kept; recovery (plug-in) clears.

### 3.5 Error handling & the PyO3 platform-bridge boundary

Exact bridge substitutions (`crate/platform-bridge/src/lib.rs`) for the Python
platform calls (§1.3). **Typed wrappers already provided:**

| Python platform call | Bridge (`platform_bridge`) | Status |
|---|---|---|
| `get_num_sfps()` | `Platform::num_sfps()` | ✅ |
| `get_sfp(i)` | `Platform::sfp(i)` | ✅ |
| `get_change_event(t)` | `Platform::get_change_event(ms) -> ChangeEvent{status,sfp,sfp_error}` | ✅ (`lib.rs:153`) |
| `sfp.get_presence()` | `Sfp::get_presence()` | ✅ |
| `sfp.is_replaceable()` | `Sfp::is_replaceable()` | ✅ |
| `sfp.get_transceiver_info()` | `Sfp::get_transceiver_info() -> Value` | ✅ |
| `sfp.get_transceiver_dom_real_value()` | `Sfp::get_transceiver_dom_real_value()` | ✅ |
| `sfp.get_transceiver_status()` | `Sfp::get_transceiver_status()` | ✅ |
| `sfp.get_transceiver_threshold_info()` | `Sfp::get_transceiver_threshold_info()` | ✅ |
| `sfp.get_lpmode()/set_lpmode(b)/reset()` | `Sfp::{get_lpmode,set_lpmode,reset}` | ✅ |
| `sfp.get_error_description()`, `sfp.sfp_type`, `sfp.get_reset_status()` | `Sfp::{get_error_description,sfp_type,get_reset_status}` | ✅ |
| `sfp.read_eeprom/write_eeprom` | `Sfp::{read_eeprom,write_eeprom}` | ✅ |

**Not yet typed — reach via `Sfp::call_json(method, ())`** (the escape hatch,
`lib.rs:287`, marshals any **no-arg** method to `serde_json::Value` with no bridge
change): `get_transceiver_status_flags`, `get_transceiver_dom_flags`,
`get_transceiver_dom_thresholds`/`..._temperature` (the DOM util split),
`get_transceiver_info_firmware_versions`, `get_transceiver_pm`. **Gaps that need
new typed wrappers in the working-copy bridge (a Planner decision, since they take
args or drive a multi-call sequence):** the VDM-statistic **freeze/unfreeze**
sequence + `is_transceiver_vdm_supported`/`get_vdm_*` and any
`get_xcvr_api().<method>(args)` the CMIS state machine needs
(`is_flat_memory`, `is_coherent_module`, `get_module_type_abbreviation`,
`get_application_advertisement`, datapath/config-state reads, app-select writes).
These are decode/api calls that must stay in Python behind the bridge — expose
them as thin typed methods rather than re-implementing decode in Rust.

**Error mapping:** `platform_bridge::BridgeError` and `swss_common::*` errors →
the daemon's `Result`; per-port failures are logged and skipped (never tear down
the daemon — mirror `daemon.rs:56-62` and the Python `try/except` guards). Absent
module / `NotImplementedError` → `Option::None`/no-op. Blocking vs non-blocking
error bitmaps decoded exactly as `sfp_status_helper.rs`.

### 3.6 Unit-test strategy (Part B)

**Make the daemon unit-testable via two trait seams**, mirroring how
`test_xcvrd.py` mocks the platform (`_wrapper_*`/MagicMock SFPs) and STATE_DB
(`mock_swsscommon.Table`):

- **HAL seam (`hal.rs`)** — traits `Chassis { num_sfps; sfp(i); get_change_event }`
  and `Sfp { get_presence; is_replaceable; get_transceiver_info; …; call_json }`.
  **Real impl** wraps `platform_bridge::{Platform,Sfp}` (the thick HAL, unchanged);
  **mock impl** `MockChassis`/`MockSfp` returns canned `serde_json::Value`
  identity/DOM/status and a scripted change-event/presence sequence — the analogue
  of `@patch('xcvrd.xcvrd._wrapper_get_transceiver_info', …)`.
- **DB seam (`db.rs`)** — traits `StateDb` / `Table { set; get; hget; del; getKeys
  }`. **Real impl** wraps `swss_common::{DbConnector,Table}`; **mock impl**
  `MockTable` is a `BTreeMap`-backed fake mirroring `tests/mock_swsscommon.py`
  (`set/get→(bool,fvs)/hget/_del/getKeys/get_size`), letting a test assert the
  exact rows written without Redis.

Daemon logic (`SfpStateUpdateTask`, `CmisManagerTask`, `DomInfoUpdateTask`,
`SffManagerTask`, the `DBUtils`/`*Utils` helpers) is written **generic over these
traits** (or takes `&dyn Chassis`/`&dyn StateDb`), so a unit test injects mocks and
the e2e daemon injects the real bridge/swss-common.

**Where mocks + tests live:** `src/mock.rs` (or `#[cfg(test)]` submodules) for the
fakes; per-module `#[cfg(test)] mod tests` for the ported cases; cross-module
integration-style cases may go under `tests/`. `cargo test` (via
`tools/unit_test.sh`) is validation layer 1; the fixed `../xcvrd-tests/` is layer
2 (`tools/dut_validate.sh`).

**Which `test_xcvrd.py` behaviors translate vs. need new Rust tests** (the Scoper
slices these per milestone):
- **Direct 1:1 ports** (mock seam is identical): the `SfpStateUpdateTask`,
  `CmisManagerTask`, `DomInfoUpdateTask`, `SffManagerTask`, `DaemonXcvrd`, and
  pure-helper cases listed in §1.7 (e.g. `test_get_transceiver_dom_*`,
  `test_beautify_*`, `test_detect_port_in_error_status`, `test_check_port_in_range`,
  `test_get_port_mapping`, `test_get_interface_speed`,
  `test_..._mapping_event_from_change_event`, `test_..._retry_eeprom_reading`).
- **New Rust tests needed** where Python coverage is thin or Python-specific:
  the NUL-trim/`pybool` string rendering (bridge-shaped, `daemon.rs`), the
  `serde_json::Value`→STATE_DB stringification, `call_json` fallbacks, and any
  bridge-wrapper additions (VDM-freeze/PM). Behaviors exercised mainly by the e2e
  suite (firmware/PM/VDM publication end-to-end, golden projection) get a
  smaller-surface Rust unit test around the producer + a `MockTable` assertion,
  with full coverage deferred to layer 2.

### 3.7 Behavior inventory for scoping (source-cited; the Scoper partitions this)

Each behavior ties source → STATE_DB tables/fields → the `../xcvrd-tests/` module(s)
that grade it. **No milestone ids assigned here.** (Oracle universe: 41
`test_*.py`, 103 tests.)

| Behavior | Source | STATE_DB output | Oracle test module(s) |
|---|---|---|---|
| **Daemon liveness / deploy-smoke** | `DaemonXcvrd.run` `xcvrd.py:1154`; bootstrap `daemon.rs` | daemon RUNNING; baseline repopulates INFO | `test_health.py` |
| **Presence / hot-plug identity** | `SfpStateUpdateTask` + `post_port_sfp_info_to_db` `xcvrd.py:178`; change-event loop `xcvrd.py:492` | `TRANSCEIVER_INFO` populate/clear; `STATUS_SW.status` 1/0 | `test_presence.py`, `test_stale_info.py` |
| **Identity content correctness** | CMIS branch dumps full `get_transceiver_info()` `xcvrd.py:211` | all `TRANSCEIVER_INFO` fields (vendor/model/OUI/serial/type/connector/cmis_rev/ext_identifier/app_advert…) | `test_info_content.py`, `test_read_retry.py` |
| **CMIS state machine → cmis_state** | `CmisManagerTask` `cmis_manager_task.py` (states `common.py:23-39`; writer `:90`; `process_cmis_state_machine`, `process_single_lport` `:1256`) | `STATUS_SW.cmis_state` (INSERTED…READY/FAILED) | `test_cmis_state_progression.py`, `test_cmis_datapath.py`, `test_cmis_failed.py`, `test_cmis_reconfig.py`, `test_flat_memory.py` |
| **Rich module/datapath status** | `StatusDBUtils` `status/db_utils.py:21`; DOM poll `dom_mgr.py:368` | `TRANSCEIVER_STATUS` (module_state, DP1..8State, config/dpinit/dpdeinit, txN/rxN) | `test_transceiver_status.py`, `test_status_flag.py` |
| **DOM sensors** | `DOMDBUtils.post_port_dom_sensor_info_to_db` `dom_sensor/db_utils.py:40`; poll `dom_mgr.py:356` | `TRANSCEIVER_DOM_SENSOR` (temp/voltage/txN/rxN power+bias, `last_update_time`) | `test_dom.py`, `test_last_update_time.py` |
| **DOM thresholds** | `post_port_dom_thresholds_to_db` `dom_sensor/db_utils.py:107` (cached at insert) | `TRANSCEIVER_DOM_THRESHOLD` | `test_dom.py`, `test_golden.py` |
| **DOM flags + metadata** | `post_port_dom_flags_to_db` `dom_sensor/db_utils.py:53` + `_update_flag_metadata_tables` `db/utils.py:107` | `TRANSCEIVER_DOM_FLAG` (+CHANGE_COUNT/SET_TIME/CLEAR_TIME) | `test_dom_flag_meta.py`, `test_link_change_flags.py`, `test_golden.py` |
| **Status flags + metadata** | `post_port_transceiver_hw_status_flags_to_db` `status/db_utils.py:41` | `TRANSCEIVER_STATUS_FLAG` (+trio) | `test_status_flag.py` |
| **Error-event handling** | change-event error branch `xcvrd.py:622-666`; `sfp_status_helper.py` | `STATUS_SW.error`; blocking ⇒ DOM removed, INFO kept | `test_status_error.py` |
| **Removal table teardown** | removed branch `xcvrd.py:585-620`; `del_port_sfp_dom_info_from_db` `common.py:338` | delete full hw-table set; keep STATUS_SW (`status='0'`) | `test_removal_tables.py` |
| **lpmode / reset control** | CMIS/SFF + `sfp.set_lpmode/reset` (bridge `set_lpmode/reset`) | Monitor-trace 00h:26 writes; module MSM | `test_lpmode_reset.py`, `test_daemon_control.py`, `test_dom_lpmode.py` |
| **Multiport isolation** | per-port loops in all tasks; `sfp_obj_dict` | per-port rows, no cross-talk | `test_multiport.py`, `test_logical_port.py` |
| **VDM real/threshold/flags** | `VDMDBUtils` `vdm/db_utils.py`; `VDMUtils` (freeze) `vdm/utils.py`; poll `dom_mgr.py:381-417` | `TRANSCEIVER_VDM_REAL_VALUE`, `_{HALARM,LALARM,HWARN,LWARN}_THRESHOLD/_FLAG`(+trio); `INFO.vdm_supported` | `test_vdm.py`, `test_vdm_statistic.py` |
| **Coherent PM** | `post_port_pm_info_to_db` `dom_mgr.py:238` via VDM freeze; coherent tx-power/laser-freq `cmis_manager_task.py:688-756` | `TRANSCEIVER_PM`; `INFO.supported_max_laser_freq` | `test_pm.py`, `test_coherent_tuning.py` |
| **Firmware info** | `post_port_sfp_firmware_info_to_db` `dom_mgr.py:203` | `TRANSCEIVER_FIRMWARE_INFO` (active/inactive) | `test_firmware_info.py` |
| **Media / optics-SI settings** | `media_settings_parser.notify_media_setting` `xcvrd.py:348,583`; `optics_si_parser`; `NPU_SI_SETTINGS_SYNC_STATUS` `xcvrd.py:805` | APPL_DB SerDes; STATE_DB `PORT_TABLE.NPU_SI_SETTINGS_SYNC_STATUS` | `test_media_settings.py`, `test_optics_si.py` |
| **SFF (non-CMIS) control** | `SffManagerTask` `sff_mgr.py:45` (tx_disable, high-power-class) | APPL_DB `PORT_TABLE`; module writes | `test_sff_control.py`, `test_sff8636.py` |
| **App-select / lane-count / host_tx_ready** | `get_desired_app_map`/`get_cmis_host_lanes_mask`/`post_port_active_apsel_to_db` `cmis_manager_task.py:457,238-315,756`; host_tx_ready | `INFO.host/media_lane_count`, `active_apsel_hostlane*` | `test_app_select.py`, `test_host_tx_ready.py`, `test_cmis_forced_tx.py` |
| **DOM gating / polling knob** | `is_port_in_cmis_initialization_process` `dom_mgr.py:182`, `get_dom_polling_from_config_db` `dom_mgr.py:76` | DOM updates paused/halted | `test_dom_gating.py`, `test_dom_polling.py` |
| **Warm/fast-reboot lifecycle** | `is_syncd_warm_restore_complete`/`is_fast_reboot_enabled` `common.py:148-186`; `deinit` skip `xcvrd.py:1144` | datapath not disrupted; media-settings skipped | `test_warm_reboot.py`, `test_fast_reboot_dp_skip.py` |
| **Golden conformance (full-suite gate)** | whole daemon | projection of `{INFO,STATUS_SW,DOM_THRESHOLD,STATUS,DOM_FLAG}` == golden | `test_golden.py` (+ re-runs all) |
| *(instrumentation cross-cut)* | EEPROM read/write cadence | Monitor-stream trace | `test_interaction_trace.py` |

---

*Analyzer complete. This document is the design reference the Scoper, Planner,
Translator, and Validator build on. It contains no Rust and defines no milestones.*
