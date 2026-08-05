# xcvrd Python → Rust — Analyzer Design Document

> ReCodeAgent Analyzer output (arXiv:2604.07341 §3.2, Figure 5). Three sections:
> (1) Source Project Research, (2) Third-Party Library Analysis, (3) Target Project
> Design. **No Rust is written here.** This document is the authoritative reference the
> Scoper, Planner, Translator, and Validator build on. It bakes in the four
> non-negotiable project adaptations: **thick HAL** via `platform-bridge` (PyO3 →
> `sonic_platform`), **STATE_DB via `swss-common`**, **two validation layers**
> (mocked Rust unit tests + the fixed `xcvrd-tests` e2e oracle), and **immutable
> input** (`crate/` → `pipeline/crate/`). Milestone partitioning is the **Scoper's**
> job — this document supplies a source-cited behavior inventory for it, but assigns
> **no milestone ids**.

Everything cited was read from the local snapshot under
`source/xcvrd/` (authoritative for *what* to translate), `crate/` (the immutable
Rust input, read-only), and `../xcvrd-tests/` (the e2e oracle, never translated).
Section-cross-references in the existing `crate/` scaffold (`hal.rs` "analysis §3.6",
`error.rs` "analysis §3.5", …) refer to the corresponding subsections below.

---

## 1. Source Project Research

### 1.1 Overview

`xcvrd` is SONiC's **transceiver information/monitoring daemon** (pmon container). It
owns the `TRANSCEIVER_*` tables in **STATE_DB**: it discovers pluggable optics,
decodes their EEPROM (CMIS / SFF-8636 / SFF-8472), publishes identity + live
diagnostics, drives CMIS datapath bring-up, and reacts to insert/remove/error events.
Upstream:
<https://github.com/sonic-net/sonic-platform-daemons/tree/master/sonic-xcvrd>; design
intent (STATE_DB tables, task/threading model): the SONiC *Transceiver Monitoring*
HLD (`doc/xrcvd/transceiver-monitor-hld.md`).

The entry point is `main()` (`source/xcvrd/xcvrd.py:1245`) → `DaemonXcvrd`
(`xcvrd.py:877`, subclasses `sonic_py_common.daemon_base.DaemonBase`). `DaemonXcvrd.run`
(`xcvrd.py:1142`) initializes the platform + port mapping, then starts a set of
**worker threads**, each a `threading.Thread` with its own `task_worker` loop and a
`task_stopping_event`:

| Thread (class) | File | Started when | Role |
|---|---|---|---|
| `SfpStateUpdateTask` | `xcvrd.py:259` | always | Presence/identity + the plug/unplug/error **state machine**; owns `TRANSCEIVER_INFO`, `TRANSCEIVER_STATUS_SW.{status,error}`, boot DOM/VDM thresholds, media-settings notify. |
| `CmisManagerTask` | `cmis/cmis_manager_task.py:41` | unless `--skip_cmis_mgr` | CMIS **datapath bring-up** state machine; owns `TRANSCEIVER_STATUS_SW.cmis_state` and `active_apsel_hostlaneN` in `TRANSCEIVER_INFO`. |
| `DomInfoUpdateTask` | `dom/dom_mgr.py:141` | always | Periodic DOM/status/VDM/PM/firmware poll; owns `TRANSCEIVER_DOM_SENSOR`, `TRANSCEIVER_DOM_FLAG*`, `TRANSCEIVER_STATUS`, `TRANSCEIVER_STATUS_FLAG*`, `TRANSCEIVER_VDM_*`, `TRANSCEIVER_PM`, `TRANSCEIVER_FIRMWARE_INFO`. |
| `DomThermalInfoUpdateTask` | `dom/dom_mgr.py:526` | if `--dom_temperature_poll_interval` set | Fast temperature poll; owns `TRANSCEIVER_DOM_TEMPERATURE`. |
| `SffManagerTask` | `sff_mgr.py:45` | if `--enable_sff_mgr` | SFF-8472/8636 deterministic link bring-up (tx_disable per `host_tx_ready`/`admin_status`). Off by default. |

`DaemonXcvrd.run` blocks on `self.stop_event.wait()` and, on SIGINT/SIGTERM
(`signal_handler`, `xcvrd.py:902`), joins all threads and runs `deinit`
(`xcvrd.py:1082`, clears the `TRANSCEIVER_*` tables). CLI flags (`main()`):
`--skip_cmis_mgr`, `--enable_sff_mgr`, `--dom_temperature_poll_interval`,
`--dom_update_interval`.

The daemon reaches "hardware" **only** through the platform plugin
(`platform_chassis = sonic_platform.platform.Platform().get_chassis()`,
`xcvrd.py:1030`) and its per-module `Sfp` objects (`platform_chassis.get_sfp(i)`),
plus Redis via `swsscommon`. In this project the emulator `xcvr-emu` backs the
plugin over gRPC.

### 1.2 Directory Structure (`source/xcvrd/`)

The source is a **Python package** (`xcvrd/`) with two subpackages (`cmis/`, `dom/`)
and a helpers subpackage (`xcvrd_utilities/`). The Rust port mirrors this shape (§3.3).

```
source/xcvrd/
├── __init__.py
├── xcvrd.py                     # DaemonXcvrd, SfpStateUpdateTask, post_port_sfp_info_to_db,
│                                #   _wrapper_* platform shims, main()
├── sff_mgr.py                   # SffManagerTask, SffLoggerForPortUpdateEvent
├── cmis/
│   ├── __init__.py              # re-exports CmisManagerTask
│   └── cmis_manager_task.py     # CmisManagerTask (CMIS datapath state machine)
├── dom/
│   ├── __init__.py
│   ├── dom_mgr.py               # DomInfoUpdateBase, DomInfoUpdateTask, DomThermalInfoUpdateTask
│   └── utilities/
│       ├── db/utils.py          # DBUtils  (base: post_diagnostic_values_to_db, flag-metadata engine)
│       ├── dom_sensor/utils.py  # DOMUtils      (SFP-object DOM getters)
│       ├── dom_sensor/db_utils.py# DOMDBUtils   (→ DOM_SENSOR/_TEMPERATURE/_FLAG/_THRESHOLD)
│       ├── status/utils.py      # StatusUtils   (SFP-object status getters)
│       ├── status/db_utils.py   # StatusDBUtils (→ TRANSCEIVER_STATUS/_FLAG)
│       ├── vdm/utils.py         # VDMUtils      (VDM getters + freeze/unfreeze context)
│       └── vdm/db_utils.py      # VDMDBUtils    (→ TRANSCEIVER_VDM_* real/threshold/flag)
├── xcvrd_utilities/
│   ├── __init__.py
│   ├── common.py                # CMIS_STATE_* consts, platform _wrapper_*, DB helpers, CMIS helpers
│   ├── utils.py                 # XCVRDUtils (presence / flat-memory / lpmode)
│   ├── sfp_status_helper.py     # SFP_STATUS_*, error masks + decode
│   ├── xcvr_table_helper.py     # XcvrTableHelper (all TRANSCEIVER_* table handles + names)
│   ├── port_event_helper.py     # PortChangeEvent, PortChangeObserver, PortMapping, get_port_mapping
│   ├── media_settings_parser.py # media_settings.json parser + notify_media_setting
│   └── optics_si_parser.py      # optics_si_settings.json parser
└── tests/                       # Part-B input (upstream unit tests + mocks)
    ├── test_xcvrd.py            # 183 test methods
    ├── mock_platform.py         # MockChassis/MockDevice/... (fans/thermals; SFPs are MagicMock)
    ├── mock_swsscommon.py       # Table (in-memory dict; set/get/hget/hdel/_del/getKeys/get_size*)
    ├── media_settings.json  gearbox_media_settings.json  media_settings_extended_format.json
    ├── optics_si_settings.json  t0-sample-port-config.ini
```

> **Modular-refactor vs. upstream note.** This snapshot is a *modular refactor*: DOM /
> status / VDM posting is split into `dom/utilities/{dom_sensor,status,vdm}/…` on a
> shared `DBUtils` base. The `tests/` directory, however, is **upstream (master)**, so
> some `test_xcvrd.py` methods target the refactored classes (`DOMDBUtils`,
> `StatusDBUtils`, `VDMDBUtils`, `DBUtils`) and some target monolithic-era names still
> present in `xcvrd.py`. The Translator maps directly where the class exists and writes
> **new** Rust tests where the refactor moved logic (called out in §3.5).

### 1.3 Key Structures & Interfaces

**`SfpStateUpdateTask` (`xcvrd.py:259`).** The presence/identity engine.
- `init` (`:384`) → `_post_port_sfp_info_and_dom_thr_to_db_once` (`:309`, boot-time
  publish of `TRANSCEIVER_INFO` + DOM/VDM thresholds, honoring warm-start) and
  `_init_port_sfp_status_sw_tbl` (`:356`, seed `TRANSCEIVER_STATUS_SW.status` = `1`/`0`).
- `task_worker` (`:395`) — a 3-state machine (`STATE_INIT`/`STATE_NORMAL`/`STATE_EXIT`,
  events `SYSTEM_NOT_READY`/`SYSTEM_BECOME_READY`/`NORMAL_EVENT`/`SYSTEM_FAIL`) driven by
  `_wrapper_get_transceiver_change_event(timeout)` (`:141`, → `get_change_event`). On a
  `NORMAL_EVENT` it dispatches per physical-port code: `"1"` insert
  (`post_port_sfp_info_to_db` + DOM/VDM thresholds + media notify + `STATUS_SW`=inserted),
  `"0"` remove (`remove_xcvr_api`, delete every `TRANSCEIVER_*` row via
  `common.del_port_sfp_dom_info_from_db`, `STATUS_SW`=removed), else an **error bitmap**
  (decode via `sfp_status_helper`, write `STATUS_SW.error`; if blocking, delete DOM rows).
- Event soak: `_wrapper_soak_sfp_insert_event` (`:127`) delays insert handling by
  `MGMT_INIT_TIME_DELAY_SECS`. Retry: `retry_eeprom_reading` (`:837`, `RETRY_EEPROM_READING_INTERVAL=60`).
- CONFIG_DB port add/remove: `on_port_config_change`/`on_add_logical_port`/`on_remove_logical_port`
  (`:723`–`:835`). Shutdown: `raise_exception` uses `ctypes.PyThreadState_SetAsyncExc` to
  interrupt a sleeping thread (`:710`).
- Free functions: `post_port_sfp_info_to_db` (`:178`) builds the `TRANSCEIVER_INFO`
  `FieldValuePairs` — **two shapes**: full CMIS dict (`'cmis_rev' in port_info_dict`) vs.
  a fixed SFF field list; returns sentinels `PHYSICAL_PORT_NOT_EXIST=-1`,
  `SFP_EEPROM_NOT_READY=-2`.

**`CmisManagerTask` (`cmis/cmis_manager_task.py:41`).** CMIS datapath bring-up.
- Constants: `CMIS_MAX_RETRIES=3`, `CMIS_DEF_EXPIRED=60`, `CMIS_MAX_HOST_LANES=8`,
  `CMIS_MODULE_TYPES` (`:45`). Per-lport `port_dict` holds `api`, `appl`,
  `host_lanes_mask`, `media_lanes_mask`, `cmis_expired`, `cmis_retries`, laser_freq,
  tx_power, host_tx_ready, admin_status, …
- `task_worker` (`:1324`) subscribes via `PortChangeObserver` (CONFIG_DB/APPL_DB/STATE_DB)
  and calls `process_single_lport` → `process_cmis_state_machine` (`:1061`) per port.
  State (read from `TRANSCEIVER_STATUS_SW.cmis_state`): `INSERTED → DP_PRE_INIT_CHECK →
  DP_DEINIT → AP_CONFIGURED → DP_INIT → DP_TXON → READY`, plus `FAILED`/`REMOVED`
  (constants in `common.py:23`, `CMIS_TERMINAL_STATES` `:35`). Per-state handlers
  `handle_cmis_*_state` (`:848`–`:1059`) drive the `CmisApi` (via `get_xcvr_api`):
  `set_datapath_deinit`, `tx_disable_channel`, `set_lpmode`, `set_application`,
  `apply_datapath_init`, `get_module_state`, `get_datapath_state`, `is_coherent_module`,
  `configure_tx_output_power`, `configure_laser_frequency`. Publishes `cmis_state` via
  `update_port_transceiver_status_table_sw_cmis_state` (`:85`) and `active_apsel_hostlaneN`
  via `post_port_active_apsel_to_db` (`:751`). Coherent/ZR tuning and decommission
  (`is_decommission_required` `:483`, `clear/set_decomm_pending`) live here.

**`DomInfoUpdateTask` (`dom/dom_mgr.py:141`)** and base **`DomInfoUpdateBase` (`:39`)**.
- `task_worker` (`:284`): periodic loop (`DEFAULT_DOM_INFO_UPDATE_PERIOD_SECS=60`,
  overridable by `--dom_update_interval`) that, per present + DOM-polling-enabled +
  non-error port, calls (in order) `post_port_sfp_firmware_info_to_db` (`:203`),
  `dom_db_utils.post_port_dom_sensor_info_to_db`, `post_port_dom_flags_to_db`,
  `status_db_utils.post_port_transceiver_hw_status_to_db`,
  `post_port_transceiver_hw_status_flags_to_db`, then (if VDM supported) the VDM
  freeze→capture→unfreeze sequence + `post_port_pm_info_to_db` (`:238`).
- Gating: `is_port_dom_monitoring_disabled` (`:198`) skips ports with CONFIG_DB
  `dom_polling=disabled` or still in CMIS init (`is_port_in_cmis_initialization_process`
  `:182`, checks `cmis_state ∉ CMIS_TERMINAL_STATES`). Link-change fast path via
  APPL_DB `PORT_TABLE.flap_count` (`on_port_update_event` `:424`,
  `update_port_db_diagnostics_on_link_change` `:442`).

**`DBUtils` (`dom/utilities/db/utils.py:5`)** — the shared posting engine:
- `post_diagnostic_values_to_db` (`:19`): validate port, read via a `get_values_func`,
  `beautify`, append `last_update_time` (`get_current_time`, UTC `"%a %b %d %H:%M:%S %Y"`),
  `table.set`.
- `_validate_and_get_physical_port` (`:62`): stop-event / port-map / sfp-object /
  presence / (optional) flat-memory checks.
- **Flag-metadata engine** `_update_flag_metadata_tables` (`:107`) + `_update_flag_metadata`
  + `_initialize_metadata_tables`: for each `*_FLAG` table, on a value transition it
  bumps the sibling `*_FLAG_CHANGE_COUNT` and stamps `*_FLAG_SET_TIME`/`*_FLAG_CLEAR_TIME`
  (`NEVER="never"`, skips `N/A`). `DOMDBUtils`/`StatusDBUtils`/`VDMDBUtils` subclass this.
- `DOMDBUtils` (`dom_sensor/db_utils.py:7`) `_beautify_dom_info_dict` strips units
  (`temperature`→ strip `C`, `voltage`→`Volts`, `(tx|rx)[1-8]power`→`dBm`, `…bias`→`mA`).

**`XcvrTableHelper` (`xcvrd_utilities/xcvr_table_helper.py:55`).** Builds one
`swsscommon.Table` per `TRANSCEIVER_*` table per ASIC (`get_intf_tbl`, `get_dom_tbl`,
`get_status_tbl`, `get_status_sw_tbl`, `get_pm_tbl`, `get_firmware_info_tbl`,
`get_vdm_*` families, …), plus CONFIG_DB `cfg_port_tbl`, APPL_DB `app_port_tbl`
(ProducerStateTable), STATE_DB `state_port_tbl`. Table-name + `VDM_THRESHOLD_TYPES =
['halarm','lalarm','hwarn','lwarn']` + `NPU_SI_SETTINGS_*` constants live here (`:11`–`:53`).

**Port mapping (`xcvrd_utilities/port_event_helper.py`).** `PortMapping` (`:212`) is the
logical↔physical map (`logical_to_physical`, `physical_to_logical` [natsorted lists],
`logical_to_asic`). `get_port_mapping(namespaces)` (`:346`) builds it from CONFIG_DB
`PORT` (front-panel only). `PortChangeObserver` (`:46`) subscribes to a configurable
`{DB: table [, FILTER]}` map and soaks/deduplicates events into `PortChangeEvent`
(`:13`, types `PORT_ADD/REMOVE/SET/DEL`). `subscribe_port_config_change` /
`handle_port_config_change` (`:283`/`:294`) are the CONFIG_DB PORT add/remove watch the
tasks share.

**Platform API surface xcvrd calls** (per-`Sfp` unless noted; via `platform_chassis`):
`get_num_sfps`, `get_sfp(i)`, `get_change_event(timeout)` (chassis);
`get_presence`, `is_replaceable`, `get_reset_status`, `sfp_type` (attr),
`get_error_description`, `get_transceiver_info`, `get_transceiver_dom_real_value`,
`get_temperature`, `get_transceiver_dom_flags`, `get_transceiver_threshold_info`,
`get_transceiver_status`, `get_transceiver_status_flags`, `get_transceiver_pm`,
`get_transceiver_info_firmware_versions`, `get_lpmode`/`set_lpmode`/`reset`,
`is_transceiver_vdm_supported`/`is_vdm_statistic_supported`/
`get_transceiver_vdm_real_value_basic`/`…_statistic`/`get_transceiver_vdm_flags`/
`get_transceiver_vdm_thresholds`/`freeze_vdm_stats`/`unfreeze_vdm_stats`/
`get_vdm_freeze_status`/`get_vdm_unfreeze_status`, `remove_xcvr_api`, and
`get_xcvr_api()` → `CmisApi`/`Sff8472Api` (CMIS bring-up + `is_flat_memory`,
`is_copper`, `is_coherent_module`, datapath control). The §3.4 table maps each to a
`platform-bridge` call.

### 1.4 Data Models / STATE_DB schema

All tables are hash-of-hashes keyed by **logical port** (`Ethernet0`, redis
`TABLE|Ethernet0`); xcvrd is the **sole producer**. Table names/handles are in
`xcvr_table_helper.py`. Field-level truth is the emulator golden capture
(`../xcvrd-tests/golden/steady_state/Ethernet100.json`) and per-table e2e assertions.

| Table | Producer (thread → method) | Key fields (source) |
|---|---|---|
| `TRANSCEIVER_INFO` | SfpStateUpdateTask → `post_port_sfp_info_to_db`; `active_apsel_hostlaneN` by CmisManagerTask → `post_port_active_apsel_to_db` | `type, type_abbrv_name, hardware_rev, serial, manufacturer, model, connector, encoding, ext_identifier, ext_rateselect_compliance, cable_length, cable_type, nominal_bit_rate, specification_compliance, vendor_date, vendor_oui, vendor_rev, cmis_rev, application_advertisement, host_lane_count, media_lane_count, media_interface_technology, active_apsel_hostlane1..8, is_replaceable, vdm_supported, dom_capability` (from `get_transceiver_info`) |
| `TRANSCEIVER_STATUS_SW` | SfpStateUpdateTask (`status`,`error`, `common.update_port_transceiver_status_table_sw`), CmisManagerTask (`cmis_state`) | `status` (`"1"`/`"0"`), `error` (`'|'`-joined descriptions or `N/A`), `cmis_state` |
| `TRANSCEIVER_DOM_THRESHOLD` | SfpStateUpdateTask + DOMDBUtils.`post_port_dom_thresholds_to_db` | temp/vcc/txbias/txpower/rxpower/lasertemp high/low alarm/warning (`get_transceiver_threshold_info`) |
| `TRANSCEIVER_DOM_SENSOR` | DomInfoUpdateTask → DOMDBUtils.`post_port_dom_sensor_info_to_db` | `temperature, voltage, tx[1-8]power, rx[1-8]power, tx[1-8]bias, …` (`get_transceiver_dom_real_value`) + `last_update_time` |
| `TRANSCEIVER_DOM_TEMPERATURE` | DomThermalInfoUpdateTask → `post_port_dom_temperature_info_to_db` | `temperature` (`get_temperature`) + `last_update_time` |
| `TRANSCEIVER_DOM_FLAG` (+ `_CHANGE_COUNT`, `_SET_TIME`, `_CLEAR_TIME`) | DomInfoUpdateTask → DOMDBUtils.`post_port_dom_flags_to_db` | latched DOM alarm/warning flags (`get_transceiver_dom_flags`); metadata via flag-engine |
| `TRANSCEIVER_STATUS` | DomInfoUpdateTask → StatusDBUtils.`post_port_transceiver_hw_status_to_db` | `module_state, module_fault_cause, DP[1-8]State, config_state_hostlane[1-8], dpinit_pending_hostlane[1-8], dpdeinit_hostlane[1-8], txN/rxN OutputStatus, txNdisable, tx_disabled_channel, …` (`get_transceiver_status`) |
| `TRANSCEIVER_STATUS_FLAG` (+ `_CHANGE_COUNT`, `_SET_TIME`, `_CLEAR_TIME`) | DomInfoUpdateTask → StatusDBUtils.`post_port_transceiver_hw_status_flags_to_db` | latched status flags (`get_transceiver_status_flags`) + metadata |
| `TRANSCEIVER_VDM_REAL_VALUE` | DomInfoUpdateTask → VDMDBUtils.`post_port_vdm_real_values_from_dict_to_db` | merged basic+statistic VDM observables |
| `TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_THRESHOLD` | SfpStateUpdateTask + VDMDBUtils.`post_port_vdm_thresholds_to_db` | per-threshold-type VDM thresholds (`get_transceiver_vdm_thresholds`) |
| `TRANSCEIVER_VDM_{…}_FLAG` (+ `_CHANGE_COUNT`, `_SET_TIME`, `_CLEAR_TIME`) | DomInfoUpdateTask → VDMDBUtils.`post_port_vdm_flags_to_db` | per-type VDM flags (`get_transceiver_vdm_flags`) + metadata |
| `TRANSCEIVER_PM` | DomInfoUpdateTask → `post_port_pm_info_to_db` | performance monitors (`get_transceiver_pm`, skipped if flat-memory) |
| `TRANSCEIVER_FIRMWARE_INFO` | DomInfoUpdateTask → `post_port_sfp_firmware_info_to_db` | `get_transceiver_info_firmware_versions` |

Non-`TRANSCEIVER_*` DB touch points: **STATE_DB** `PORT_TABLE|<lport>` field
`NPU_SI_SETTINGS_SYNC_STATUS` (seeded/reset by SfpStateUpdateTask/CmisManagerTask);
**APPL_DB** `PORT_TABLE` (`host_tx_ready`, `flap_count` — read/subscribed) and the
SFF producer path; **CONFIG_DB** `PORT` (mapping + `dom_polling`, `speed`, `lanes`,
`admin_status`, `subport`). Warm/fast-reboot gating reads STATE_DB
`WARM_RESTART_*` / `FAST_RESTART_ENABLE_TABLE` (`common.is_syncd_warm_restore_complete`,
`is_fast_reboot_enabled`).

**e2e contract cross-reference** (`../xcvrd-tests/`, the ultimate oracle; reads STATE_DB
via `lib/statedb.py` = `sonic-db-cli`, NUL-stripped). Emulator oracle values
(`emu_config.yaml`): `manufacturer=xcvr-emu`, `model=EMU-40G-LR4`,
`vendor_oui=01-02-03`, `serial=0123456789`, `type_abbrv_name=QSFP-DD`, `cmis_rev=5.2`,
`connector` contains `MPO`, `cable_length=100.0`, `specification_compliance=
sm_media_interface`, `is_replaceable=True`, `vdm_supported=False`,
`ext_identifier` contains `Power Class 8`, `vendor_date` starts `2024-12-14`
(`tests/test_info_content.py`). `TRANSCEIVER_STATUS` admin-down baseline:
`module_state=ModuleLowPwr`, all `DP[1-8]State=DataPathDeactivated`
(`tests/test_transceiver_status.py`); `STATUS_SW.cmis_state=READY`,
`status=1`, `error=N/A` (golden). DOM: `temperature`+`voltage`+24 per-lane
`tx/rx power`/`tx bias` keys must all appear (`tests/test_dom.py`); thresholds decode to
engineering values. Error injection (`tests/test_status_error.py`): a blocking error
(I2C-stuck / bad-EEPROM) sets `STATUS_SW.error` to the decoded descriptions **and
removes DOM but keeps `TRANSCEIVER_INFO`**; a non-blocking error (high-temp) sets the
error but **keeps DOM**; recovery (plug-in) clears the error and repopulates DOM.

### 1.5 Error Handling

- **Absent module / not-ready EEPROM:** posters no-op when `_wrapper_get_presence`
  is false; `post_port_sfp_info_to_db` returns `SFP_EEPROM_NOT_READY (-2)` /
  `PHYSICAL_PORT_NOT_EXIST (-1)`; the caller retries (`retry_eeprom_set`, 60 s cadence).
- **Hardware error bitmaps (`sfp_status_helper.py`):** masks `SFP_ERRORS_BLOCKING_MASK=
  0x02`, `SFP_ERRORS_GENERIC_MASK=0x0000FFFE`, `SFP_ERRORS_VENDOR_SPECIFIC_MASK=
  0xFFFF0000`. `fetch_generic_error_description` maps bits via
  `SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT`; `is_error_block_eeprom_reading` decides
  whether DOM rows are deleted; `has_vendor_specific_error` appends
  `get_error_description()`. `detect_port_in_error_status` gates DOM polling on
  `SfpBase.SFP_ERROR_DESCRIPTION_BLOCKING` in `STATUS_SW.error`.
- **`NotImplementedError`:** posters `sys.exit(NOT_IMPLEMENTED_ERROR=3)`; other platform
  errors are logged and skipped per port (`try/except` around each poster call in the
  DOM loop, `dom_mgr.py:349`–`:417`).
- **Threading/shutdown:** each `task_worker` is wrapped in `run()` `try/except` that logs
  the traceback (`common.log_exception_traceback`) and sets the *main* stop event on an
  unhandled exception; `join()` re-raises the stored `self.exc`. A crashed child triggers
  `os.kill(getpid(), SIGKILL)` from the main loop. `SfpStateUpdateTask.raise_exception`
  async-injects `SystemExit` to break a sleeping poll during graceful shutdown.
  `SFP_SYSTEM_ERROR=4` exit if the SFP error event stayed set.

### 1.6 Dependencies (Python imports)

- `swsscommon.swsscommon` — Redis DB access: `Table`, `ProducerStateTable`,
  `SubscriberStateTable`, `Select`, `FieldValuePairs`, `DBConnector`, `SonicDBConfig`,
  table-name consts (`APP_PORT_TABLE_NAME`, `CFG_PORT_TABLE_NAME`, `STATE_PORT_TABLE_NAME`).
- `sonic_py_common` — `daemon_base.DaemonBase`/`db_connect`, `syslogger.SysLogger`,
  `logger.Logger`, `multi_asic` (namespace/asic mapping, front-panel test).
- `sonic_platform_base` / `sonic_platform` — `SfpBase` (error dict), `SfpOptoeBase`,
  `sonic_xcvr.api.public.c_cmis.CmisApi`, `…sff8472.Sff8472Api`; the plugin
  (`Platform().get_chassis()`).
- `natsort.natsorted` — natural sort of `physical_to_logical` breakout lists.
- Stdlib: `threading`, `ctypes` (async thread interrupt), `json`, `ast`, `copy`, `re`,
  `datetime`, `time`, `os`, `signal`, `subprocess` (fast-reboot query), `argparse`,
  `traceback`, `contextlib` (VDM freeze context).

### 1.7 Unit tests (`source/xcvrd/tests/`)

`test_xcvrd.py` has **183 `test_*` methods** across `TestXcvrdThreadException`,
`TestXcvrdScript`, `TestOpticSiParser`. Test infra (`test_xcvrd.py:38`–`:51`):
module-level **`swsscommon.Table/ProducerStateTable/SubscriberStateTable/SonicDBConfig =
MagicMock()`** and **`daemon_base.db_connect = MagicMock()`**, plus
`from .mock_swsscommon import Table` for the cases that need a *real* in-memory table.
`os.environ["XCVRD_UNIT_TESTING"]="1"`. Boundaries are mocked two ways:

1. **Platform**: `@patch('xcvrd.xcvrd.platform_chassis', MagicMock())`,
   `@patch('...common._wrapper_get_presence', MagicMock(return_value=...))`, and per-SFP
   `MagicMock()` objects whose getters return canned dicts (e.g.
   `mock_sfp.get_transceiver_info.return_value = {...}`). `mock_platform.py` supplies
   `MockChassis`/`MockDevice`/`MockFan`/`MockThermal` (chassis-shape mocks; SFP behavior
   itself is `MagicMock`).
2. **STATE_DB**: `mock_swsscommon.Table` (`tests/mock_swsscommon.py`) — an in-memory dict
   with `set`/`get`/`hget`/`hdel`/`_del`/`getKeys`/`get_size`/`get_size_for_key`. Tests
   assert **field counts** (`get_size_for_key`) and values directly.

Representative unit-testable behaviors (→ these drive the Rust unit tests, §3.5):
`test_post_port_sfp_info_to_db*`, `test_post_port_dom_sensor_info_to_db`,
`test_post_port_dom_flags_to_db`, `test_update_flag_metadata_tables` (parametrized
change-count/set/clear), `test_post_port_dom_thresholds_to_db`,
`test_post_port_transceiver_hw_status_to_db`/`…_flags_to_db`,
`test_post_port_vdm_thresholds_to_db`/`…_real_values_from_dict_to_db`,
`test_post_port_pm_info_to_db`, `test_beautify_dom_info_dict`,
`test_SfpStateUpdateTask_task_worker`/`…_mapping_event_from_change_event`/
`…_on_add_logical_port`/`…_retry_eeprom_reading`, `test_sfp_insert_events`/`…_remove_events`,
`test_CmisManagerTask_task_worker*` (INSERTED→READY, fastboot, host_tx_ready, decommission),
`test_CmisManagerTask_get_cmis_host_lanes_mask`/`get_desired_app_map`/
`is_cmis_application_update_required`/`post_port_active_apsel_to_db`,
`test_DomInfoUpdateTask_task_worker*`/`get_dom_polling_from_config_db`/
`is_port_in_cmis_initialization_process`, `test_SffManagerTask_task_worker`/
`get_active_lanes_for_lport`/`enable_high_power_class`, `test_detect_port_in_error_status`,
`test_handle_port_update_event`/`test_get_port_mapping`, and the media/optics parser suite.
These map cleanly onto mockable HAL + DB seams (§3.5–§3.6).

---

## 2. Third-Party Library Analysis

For each Python dependency: overview, xcvrd's use, and the Rust recommendation —
flagging what is **already met** by the provided scaffolding so the Translator does
**not** reinvent it. The two pinned interop libraries (`platform-bridge`, `swss-common`)
are wired into `crate/xcvrd-rs/Cargo.toml` already.

| Python dep | How xcvrd uses it | Rust recommendation |
|---|---|---|
| **`swsscommon`** (`Table`, `ProducerStateTable`, `SubscriberStateTable`, `Select`, `FieldValuePairs`, `DBConnector`, `SonicDBConfig`) | All STATE_DB/APPL_DB/CONFIG_DB reads/writes; the `TRANSCEIVER_*` tables; port-config subscribe/select | **ALREADY MET → `swss-common`** (official sonic-net crate, pinned git rev in `crate/xcvrd-rs/Cargo.toml`): `DbConnector`, `Table`, `ProducerStateTable`, `SubscriberStateTable`, `CxxString`. Accessed behind the `DbTable` seam (§3.6); `env.rs` provides `open_state_db/open_config_db/open_appl_db`. Do **not** hand-roll a Redis client. |
| **`sonic_platform` / `sonic_platform_base`** (`Platform/Chassis/Sfp`, `SfpOptoeBase`, `CmisApi`, `Sff8472Api`, `SfpBase`) | Every transceiver I/O; CMIS/SFF EEPROM decode | **ALREADY MET → `platform-bridge`** (provided PyO3 crate): `Platform`/`Chassis`/`Sfp`/`ChangeEvent` (§3.4). CMIS/SFF decode **stays in Python** behind the bridge; the daemon consumes typed scalars + `serde_json::Value`. `SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT` is the one piece the daemon still needs Rust-side (a small const table, §3.4). |
| **`sonic_py_common.daemon_base`** (`DaemonBase`, `db_connect`, signal handling) | Base class; connect DBs by name; SIGINT/TERM/HUP | Partly bootstrap (`daemon.rs`/`env.rs`); **NEW (std/small crates)**: signals via `signal-hook` **or** `libc::sigaction` (SIGINT/SIGTERM→stop flag, SIGHUP→log-level). `db_connect(name)` → `env::open_*_db` (by db-id + unix socket). ⚠ The by-name connect's **`SonicDBConfig` load side-effect** must be reproduced explicitly (`env::init_embedded_db_config`) because the Rust bindings connect by id and never load the singleton — required for the emulator's SFP-error read path (§3.4). |
| **`sonic_py_common.multi_asic`** | Namespace↔asic mapping; front-panel test | **Assume single-ASIC**: `namespaces=[""]`, `asic_id=0`, `is_front_panel_port` ≈ true for `Ethernet*`. Keep a tiny `multi_asic`-shaped helper so multi-ASIC can be added later; the e2e testbed is single-ASIC. |
| **`sonic_py_common.syslogger` / `logger`** | Structured logging to syslog; runtime log-level | **NEW → `log` + a light impl, or direct `eprintln!` to stderr** (pmon's supervisor captures stderr, which is how the bootstrap already logs). A small `SysLogger`-shaped seam with `log_{info,notice,warning,error,debug}` keeps call sites 1:1 with Python. `log`/`env_logger` optional; avoid heavyweight frameworks. |
| **`natsort.natsorted`** | Natural sort of breakout logical-port lists in `physical_to_logical` | **NEW → `natord` crate** (or a ~15-line natural comparator). Only needed for stable multi-subport ordering; small and self-contained. |
| **`threading`** (`Thread`, `Event`), **`ctypes`** async-interrupt | Worker threads + stop events; interrupt a sleeping poll | **std**: `std::thread` per task; `Event`→`Arc<AtomicBool>` (or `Arc<(Mutex,Condvar)>`) stop flag; the `ctypes` async-exc hack is unnecessary — use bounded `get_change_event`/select timeouts + poll the stop flag. |
| **`json` / `ast`** | Marshal platform dicts; parse `application_advertisement` | **`serde_json`** (already a `platform-bridge` dep; bridge returns `serde_json::Value`). Field text must match Python `str(value)` (see `dom::utilities::db::value_to_py_str` in the scaffold, and the `-Infinity/NaN` sanitizer in `hal.rs`). |
| **`re`** | media/optics key regex; unit-strip regex (`^(tx|rx)[1-8]power$`) | **`regex` crate** (or hand-rolled matchers for the few fixed patterns). |
| **`datetime` / `time`** | `last_update_time` (UTC `"%a %b %d %H:%M:%S %Y"`), CMIS timers, poll cadence | **`chrono`** for the strftime-format timestamp (or a small formatter); `std::time::{Instant,Duration,SystemTime}` for cadence/timers. |
| **`subprocess`** (`sonic-db-cli` fast-reboot query) | `is_fast_reboot_enabled` | **`std::process::Command`** (mirror the exact `sonic-db-cli STATE_DB hget …`), **or** read the same key via the `DbTable` seam. |
| **`argparse`** | `--skip_cmis_mgr` etc. | **`clap`** (or `argh`, or a tiny manual parser); the flag set is small and fixed. |
| **`contextlib.contextmanager`** (VDM freeze) | `vdm_freeze_context` guaranteed unfreeze | **RAII guard** (`Drop`) around freeze/unfreeze. |

Net new crates the Translator may add (all small, std-adjacent): `regex`, `chrono`,
`natord`, `clap`, optionally `signal-hook`/`libc` and `log`. **No** new crate for DB,
platform I/O, or CMIS/SFF decode — those are covered by `swss-common` +
`platform-bridge`.

---

## 3. Target Project Design

### 3.1 Overview & Translation Requirements

Produce a Rust `xcvrd` that is **functionally equivalent as observed through STATE_DB**:
the `xcvrd-tests` e2e suite (the ultimate oracle, run unchanged on the DUT) plus the
translated Rust unit tests (mocked, `cargo test`) must both pass. Hard constraints:

1. **Thick HAL** — all transceiver I/O via `platform-bridge` (PyO3 → `sonic_platform`);
   CMIS/SFF decode stays in Python. The daemon translates only *daemon logic*: task
   loops, cadence, state decisions, STATE_DB writes.
2. **STATE_DB via `swss-common`** — no hand-rolled Redis.
3. **Mockable seams** — HAL + DB behind traits so the daemon logic runs under mocks in
   unit tests, mirroring `mock_platform.py`/`mock_swsscommon.py`.
4. **Immutable input** — never edit `crate/`; the Planner copies it to `pipeline/crate/`.
   Extend the existing bootstrap (`daemon.rs`, `env.rs`) and the stubbed module tree;
   keep M0/M1 green as the tree grows.

Idiom mapping: Python exceptions → `Result<T, XcvrdError>` (`error.rs`); `None` →
`Option`; dict → `serde_json::Value` / `Vec<(String,String)>` field pairs; `threading.
Thread.run` → a `std::thread` closure calling a `task_worker`; `threading.Event` → a
shared stop flag; module-global `platform_chassis` → an injected `&dyn Hal`.

### 3.2 Source → Rust structural mapping

One-to-one where sensible, **preserving names and the package/directory layout** so the
port is traceable (snake_case identifiers carry over verbatim).

| Python (`source/xcvrd/…`) | Rust (`pipeline/crate/xcvrd-rs/src/…`) | Notes |
|---|---|---|
| `xcvrd.py` :: `DaemonXcvrd` | `daemon.rs` :: `serve()` / `DaemonXcvrd` | boot sequence (`init`/`deinit`/`run`) + thread spawn; wires seams to real impls |
| `xcvrd.py` :: `SfpStateUpdateTask` | `xcvrd/sfp_state_update.rs` :: `SfpStateUpdateTask` | change-event state machine; `task_worker` |
| `xcvrd.py` :: `post_port_sfp_info_to_db`, `_wrapper_*` | `xcvrd/mod.rs` (+ `xcvrd_utilities`) | `TRANSCEIVER_INFO` field builder; wrappers fold into the `Hal` seam |
| `sff_mgr.py` :: `SffManagerTask` | `sff_mgr.rs` :: `SffManagerTask` | optional SFF link bring-up |
| `cmis/cmis_manager_task.py` :: `CmisManagerTask` | `cmis/cmis_manager_task.rs` :: `CmisManagerTask` | CMIS datapath state machine + handlers |
| `dom/dom_mgr.py` :: `DomInfoUpdateBase/Task`, `DomThermalInfoUpdateTask` | `dom/dom_mgr.rs` | periodic DOM/status/VDM/PM/firmware loop + thermal loop |
| `dom/utilities/db/utils.py` :: `DBUtils` | `dom/utilities/db.rs` | shared poster + flag-metadata engine; `value_to_py_str` |
| `dom/utilities/dom_sensor/{utils,db_utils}.py` | `dom/utilities/dom_sensor.rs` | `DOMUtils` + `DOMDBUtils` |
| `dom/utilities/status/{utils,db_utils}.py` | `dom/utilities/status.rs` | `StatusUtils` + `StatusDBUtils` |
| `dom/utilities/vdm/{utils,db_utils}.py` | `dom/utilities/vdm.rs` | `VDMUtils` (+ freeze RAII) + `VDMDBUtils` |
| `xcvrd_utilities/common.py` | `xcvrd_utilities/common.rs` | CMIS_STATE_* consts, DB helpers, CMIS helpers |
| `xcvrd_utilities/utils.py` :: `XCVRDUtils` | `xcvrd_utilities/utils.rs` | presence/flat-memory/lpmode over the `Hal` seam |
| `xcvrd_utilities/sfp_status_helper.py` | `xcvrd_utilities/sfp_status_helper.rs` | error masks + `SFP_ERROR_BIT_TO_DESCRIPTION_DICT` const |
| `xcvrd_utilities/xcvr_table_helper.py` | `xcvrd_utilities/xcvr_table_helper.rs` | table-name consts + a table registry over `DbTable` |
| `xcvrd_utilities/port_event_helper.py` | `xcvrd_utilities/port_event_helper.rs` | `PortMapping`, `PortChangeEvent`, observer/select |
| `xcvrd_utilities/media_settings_parser.py` | `xcvrd_utilities/media_settings_parser.rs` | media_settings.json parse + notify |
| `xcvrd_utilities/optics_si_parser.py` | `xcvrd_utilities/optics_si_parser.rs` | optics_si_settings.json parse |
| `tests/mock_swsscommon.py` :: `Table` | `mock.rs` :: `MockDbTable` | in-memory `DbTable` |
| `tests/mock_platform.py` + patched wrappers | `mock.rs` :: `MockHal`/`MockSfp` | canned `Hal`/`SfpHandle` |
| `tests/test_xcvrd.py` | `#[cfg(test)] mod tests` per module (+ `tests/`) | Part-B translation |

### 3.3 Module structure for `xcvrd-rs`

Mirror the Python package so the port is traceable (directory→module, file→submodule).
This matches the tree the bootstrap already declares in `crate/xcvrd-rs/src/lib.rs`
(`pub mod {daemon,env,db,error,hal,mock,cmis,dom,sff_mgr,xcvrd,xcvrd_utilities}`), which
the Planner extends in `pipeline/crate/` without breaking M0/M1:

```
xcvrd-rs/src/
├── main.rs                     # thin entry → xcvrd_rs::daemon::serve()   [bootstrap]
├── lib.rs                      # module declarations                     [bootstrap]
├── daemon.rs                   # DaemonXcvrd: init/deinit/run + thread spawn (BOOTSTRAP; extend)
├── env.rs                      # open_platform / open_{state,config,appl}_db / init_embedded_db_config [bootstrap]
├── error.rs                    # XcvrdError + Result (sentinels: EepromNotReady, PhysicalPortNotExist, …)
├── hal.rs                      # SEAM: trait Hal + SfpHandle; BridgeHal/BridgeSfp (real, over platform-bridge)
├── db.rs                       # SEAM: trait DbTable; RealDbTable (real, over swss-common DbConnector)
├── mock.rs                     # Part-B doubles: MockHal/MockSfp (Hal/SfpHandle), MockDbTable (DbTable)
├── xcvrd/
│   ├── mod.rs                  # DaemonXcvrd helpers, post_port_sfp_info_to_db (TRANSCEIVER_INFO builder)
│   └── sfp_state_update.rs     # SfpStateUpdateTask (change-event state machine)
├── cmis/
│   ├── mod.rs
│   └── cmis_manager_task.rs     # CmisManagerTask
├── dom/
│   ├── mod.rs
│   ├── dom_mgr.rs               # DomInfoUpdateTask + DomThermalInfoUpdateTask
│   └── utilities/
│       ├── mod.rs
│       ├── db.rs                # DBUtils (poster + flag-metadata engine + value_to_py_str)
│       ├── dom_sensor.rs        # DOMUtils + DOMDBUtils
│       ├── status.rs            # StatusUtils + StatusDBUtils
│       └── vdm.rs               # VDMUtils + VDMDBUtils
├── sff_mgr.rs                   # SffManagerTask
└── xcvrd_utilities/
    ├── mod.rs
    ├── common.rs                # CMIS_STATE_* consts, DB/CMIS helpers
    ├── utils.rs                 # XCVRDUtils
    ├── sfp_status_helper.rs     # error masks + SFP_ERROR_BIT_TO_DESCRIPTION_DICT
    ├── xcvr_table_helper.rs     # table-name consts + table registry
    ├── port_event_helper.rs     # PortMapping / PortChangeEvent / observer
    ├── media_settings_parser.rs
    └── optics_si_parser.rs
```

Unit tests live in `#[cfg(test)] mod tests` at the bottom of each module (using
`crate::mock`), with crate-level integration tests optionally under `xcvrd-rs/tests/`.
`mock.rs` is **public (not `#[cfg(test)]`)** so both inline and integration tests reuse
it. `platform-bridge` and `swss-common` stay wired but are only touched by `hal.rs`
(real) and `db.rs` (real) — nothing else imports PyO3/redis, which is what keeps the
daemon logic mockable.

### 3.4 Error handling & the PyO3 `platform-bridge` boundary

`error.rs` defines `XcvrdError { Bridge, Db, NotImplemented, EepromNotReady,
PhysicalPortNotExist, Other }` with `From<platform_bridge::BridgeError>` and
`From<swss_common::Exception>`, preserving the Python sentinels the posters branch on
(`SFP_EEPROM_NOT_READY=-2`, `PHYSICAL_PORT_NOT_EXIST=-1`, `NOT_IMPLEMENTED_ERROR=3`).
Per-port Python `try/except` → Rust `match`/`if let Err` that **logs + continues** (never
aborts the loop); `NotImplementedError`→`sys.exit(3)` → `XcvrdError::NotImplemented`
propagated to the task exit path.

**Which bridge call replaces which Python platform call** (`hal.rs` `Hal`/`SfpHandle`):

| Python platform call | `platform-bridge` (`crate/platform-bridge/src/lib.rs`) | `Hal`/`SfpHandle` method |
|---|---|---|
| `chassis.get_num_sfps()` | `Platform::num_sfps()` | `Hal::num_sfps` |
| `chassis.get_sfp(i)` | `Platform::sfp(i)` | `Hal::sfp` |
| `chassis.get_change_event(t)` | `Platform::get_change_event(ms)` → `ChangeEvent{status, sfp, sfp_error}` | `Hal::get_change_event` |
| `sfp.get_presence()` | `Sfp::get_presence()` | `SfpHandle::get_presence` |
| `sfp.is_replaceable()` | `Sfp::is_replaceable()` | `is_replaceable` |
| `sfp.get_reset_status()` | `Sfp::get_reset_status()` | `get_reset_status` |
| `sfp.sfp_type` | `Sfp::sfp_type()` | `sfp_type` |
| `sfp.get_error_description()` | `Sfp::get_error_description()` → `Option<String>` | `get_error_description` |
| `sfp.get_transceiver_info()` | `Sfp::get_transceiver_info()` → `Value` | `get_transceiver_info` |
| `sfp.get_transceiver_dom_real_value()` | `Sfp::get_transceiver_dom_real_value()` | `get_transceiver_dom_real_value` |
| `sfp.get_transceiver_status()` | `Sfp::get_transceiver_status()` | `get_transceiver_status` |
| `sfp.get_transceiver_threshold_info()` | `Sfp::get_transceiver_threshold_info()` (inf-safe re-read in `BridgeSfp`) | `get_transceiver_threshold_info` |
| `sfp.get_lpmode()/set_lpmode(x)/reset()` | `Sfp::get_lpmode()/set_lpmode()/reset()` | `get_lpmode/set_lpmode/reset` |
| `sfp.get_temperature()`, `get_transceiver_dom_flags()`, `get_transceiver_status_flags()`, `get_transceiver_pm()`, `get_transceiver_info_firmware_versions()`, `get_transceiver_vdm_*()`, `is_transceiver_vdm_supported()`, `is_vdm_statistic_supported()`, `freeze_vdm_stats()`/`unfreeze…`/`get_vdm_freeze_status()`/`…unfreeze…` | **`Sfp::call_json(method, ())`** escape hatch (`→ Value`/bool) | `SfpHandle::call_json(method)` |
| `sfp.get_xcvr_api()` CMIS datapath control (`set_datapath_deinit`, `tx_disable_channel`, `apply_datapath_init`, `set_application`, staged-control reads) | **`Sfp::read_eeprom/write_eeprom`** (the register bytes `CmisApi` would touch) | `SfpHandle::read_eeprom/write_eeprom` |
| `sfp.remove_xcvr_api()` (on plug-out) | `Sfp::call_json("remove_xcvr_api", ())` | `call_json` |

Design notes the Translator must honor:
- The bridge **typed** surface covers presence/identity/DOM-real/status/threshold/
  lpmode/reset/eeprom + change-event. Everything else (DOM flags, status flags, VDM, PM,
  firmware, temperature, flat-memory) is reached via **`call_json`** (no-arg JSON
  methods). CMIS bring-up cannot use `get_xcvr_api` (returns a non-JSON object), so it is
  expressed as `read_eeprom`/`write_eeprom` against the CMIS page-10h control bytes — the
  same registers `CmisApi` writes; CMIS decode stays in Python. This is exactly what the
  scaffold's `hal.rs` doc comments describe.
- **`SfpBase.SFP_ERROR_BIT_TO_DESCRIPTION_DICT`** is not on the bridge; replicate it as a
  Rust const in `sfp_status_helper.rs` (bit→description), plus the blocking sentinel
  string, so error decode is byte-identical to the e2e assertions
  (`"Blocking EEPROM from being read"`, `"Bus stuck (I2C data or clock shorted)"`, …).
- **`SonicDBConfig` load side-effect** (`env::init_embedded_db_config`,
  `daemon.rs` boot): the reference daemon's by-name `daemon_base.db_connect` force-loads
  the process-global `SonicDBConfig`; the Rust `swss-common` bindings connect by db-id +
  socket and never load it, so the daemon must load it explicitly at a clean
  single-threaded boot point — otherwise the emulator's `Chassis._get_statedb` fail-caches
  and injected SFP errors never surface (only `test_status_error.py` regresses). Keep ONE
  chassis for the daemon lifetime (recreating it resets the emulator change-event
  baseline).

### 3.5 STATE_DB schema contract (per-table, tied to `xcvrd-tests`)

Each table→field mapping a milestone must reproduce, keyed by logical port, written via
`swss_common::Table`/`DbConnector` behind the `DbTable` seam. Real `Table::set` **merges**
fields (additive `HSET`) so `cmis_state` and `status`/`error` writers don't clobber each
other — `RealDbTable::set` merges, `MockDbTable::set` replaces (matching Python
`mock_swsscommon.Table`). Field text must equal Python `str(value)` (`value_to_py_str`),
including `-inf/inf/nan` for a default module's zero-power thresholds.

- **`TRANSCEIVER_INFO|<lport>`** ← `get_transceiver_info()` (+ `is_replaceable`); CMIS
  path emits the full dict, SFF path the fixed field list (`post_port_sfp_info_to_db`).
  `active_apsel_hostlane1..8` filled by the CMIS task after datapath activation.
  Oracle fields/values: `manufacturer=xcvr-emu, model=EMU-40G-LR4, vendor_oui=01-02-03,
  serial=0123456789, type_abbrv_name=QSFP-DD, cmis_rev=5.2, connector⊇MPO,
  cable_length=100.0, specification_compliance=sm_media_interface, is_replaceable=True,
  vdm_supported=False, ext_identifier⊇"Power Class 8", vendor_date⊇2024-12-14,
  application_advertisement=<parseable dict>` → `test_info_content.py`, `test_presence.py`,
  golden `TRANSCEIVER_INFO`.
- **`TRANSCEIVER_STATUS_SW|<lport>`**: `status` `1`/`0`, `error` (`|`-joined or `N/A`),
  `cmis_state` (`READY` steady) → `test_presence.py` (status), `test_status_error.py`
  (error), `test_transceiver_status.py`/`test_cmis_*` (cmis_state), golden.
- **`TRANSCEIVER_DOM_SENSOR|<lport>`** ← `get_transceiver_dom_real_value()` (unit-stripped)
  + `last_update_time`: `temperature`, `voltage`, and 24 per-lane `tx[1-8]power`,
  `rx[1-8]power`, `tx[1-8]bias` keys → `test_dom.py`, `test_last_update_time.py`.
- **`TRANSCEIVER_DOM_THRESHOLD|<lport>`** ← `get_transceiver_threshold_info()`: temp/vcc/
  txbias/txpower/rxpower/lasertemp high/low alarm/warning decoded to engineering values →
  `test_dom.py`, golden `TRANSCEIVER_DOM_THRESHOLD`.
- **`TRANSCEIVER_DOM_TEMPERATURE|<lport>`** ← `get_temperature()` (thermal task).
- **`TRANSCEIVER_STATUS|<lport>`** ← `get_transceiver_status()`: `module_state`,
  `module_fault_cause`, `DP[1-8]State`, `config_state_hostlane[1-8]`,
  `dpinit_pending_hostlane[1-8]`, `dpdeinit_hostlane[1-8]`, `txN/rxN OutputStatus`,
  `txNdisable`, `tx_disabled_channel` → `test_transceiver_status.py`
  (baseline `ModuleLowPwr`/`DataPathDeactivated`), golden.
- **`TRANSCEIVER_DOM_FLAG` / `TRANSCEIVER_STATUS_FLAG` / `TRANSCEIVER_VDM_*_FLAG`** (+ each
  `_CHANGE_COUNT`/`_SET_TIME`/`_CLEAR_TIME`): the flag-metadata engine (change count bumped
  on transition; set/clear timestamps; `NEVER` init) → `test_dom_flag_meta.py`,
  `test_status_flag.py`, `test_link_change_flags.py`.
- **`TRANSCEIVER_VDM_REAL_VALUE` / `TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_THRESHOLD`**
  ← VDM getters → `test_vdm.py`, `test_vdm_statistic.py`.
- **`TRANSCEIVER_PM|<lport>`** ← `get_transceiver_pm()` (skip flat-memory) → `test_pm.py`.
- **`TRANSCEIVER_FIRMWARE_INFO|<lport>`** ← `get_transceiver_info_firmware_versions()` →
  `test_firmware_info.py`.
- **Removal/lifecycle**: plug-out / port-DEL deletes the port's rows across all
  `TRANSCEIVER_*` tables (`del_port_sfp_dom_info_from_db`), keeping `TRANSCEIVER_INFO`
  only where noted → `test_removal_tables.py`, `test_stale_info.py`, `test_warm_reboot.py`.
- **STATE_DB `PORT_TABLE|<lport>.NPU_SI_SETTINGS_SYNC_STATUS`** seeded/reset around
  media-settings notify → `test_media_settings.py`, `test_optics_si.py`.

### 3.6 Unit-test strategy (Part B) — mockable seams

The daemon logic is written against **two traits** so it runs unchanged under real
bindings (production) and canned doubles (tests) — the Rust analogue of the Python tests
patching `_wrapper_*` and swapping `swsscommon.Table` for `mock_swsscommon.Table`. Both
already exist in the immutable bootstrap and are the design target:

- **HAL seam (`hal.rs`)**: `trait Hal { num_sfps; sfp(i) -> Box<dyn SfpHandle>;
  get_change_event(ms) }` and `trait SfpHandle { get_presence, is_replaceable,
  get_reset_status, sfp_type, get_error_description, get_transceiver_info,
  get_transceiver_dom_real_value, get_transceiver_status, get_transceiver_threshold_info,
  get_lpmode, set_lpmode, reset, call_json(method), read_eeprom, write_eeprom }`.
  Real impl `BridgeHal`/`BridgeSfp` wraps `platform-bridge`; test impl
  `mock::MockHal`/`MockSfp` returns canned `serde_json::Value`s and logs `call_json` /
  `write_eeprom` invocations for assertions (the analogue of `MagicMock(...).return_value`
  and `assert_called_with`).
- **DB seam (`db.rs`)**: `trait DbTable { set, hset, get, hget, del, hdel, get_keys,
  get_size, get_size_for_key }` — a direct port of `mock_swsscommon.Table` so Rust tests
  assert **field counts** exactly (`dom_tbl.get_size_for_key("Ethernet0") == 27`). Real
  impl `RealDbTable` wraps a `swss-common` `DbConnector` (merge-on-set); test impl
  `mock::MockDbTable` is in-memory (replace-on-set).

Daemon logic and posters take `&dyn Hal` / `&dyn DbTable` (or generics), so a `task_worker`
body is identical in production and tests. Where to put things: `mock.rs` (public doubles),
`#[cfg(test)] mod tests` per module (unit tests), optional `xcvrd-rs/tests/` (integration).
`build_check.sh` compiles; `unit_test.sh` runs `cargo test` in the trixie container.

**Which `test_xcvrd.py` behaviors translate directly vs. need new Rust tests:**
- *Translate directly* (class exists 1:1): `post_port_sfp_info_to_db*`,
  `SfpStateUpdateTask_*` (task_worker, mapping_event, on_add_logical_port,
  retry_eeprom_reading, insert/remove events), `CmisManagerTask_*` (task_worker,
  host_lanes_mask, desired_app_map, application_update_required, active_apsel,
  is_timer_expired, decommission, fastboot), `DomInfoUpdateTask_*`,
  `SffManagerTask_*`, `detect_port_in_error_status`/error-mask decode,
  `get_port_mapping`/`handle_port_update_event`, media/optics parser suite,
  `beautify_dom_info_dict`.
- *New Rust tests* (refactor moved logic, or Rust-specific): the split
  `DOMDBUtils`/`StatusDBUtils`/`VDMDBUtils`/`DBUtils` posters + the flag-metadata engine
  (`_update_flag_metadata_tables` field-count/change-count/set-clear cases — some upstream
  tests target monolithic names); the `-Infinity/NaN` threshold sanitizer (`hal.rs`
  already carries these); `value_to_py_str` fidelity; `call_json`/`read_eeprom` seam
  behavior; the merge-vs-replace `set` semantics; and any newly-implemented parts flagged
  by the Parity Verifier. Tests that exercise pure-`MagicMock` platform plumbing
  (`test_wrapper_*`) collapse into the `MockSfp`/`BridgeSfp` seam.

### 3.7 Behavior inventory for scoping (source-cited)

For the **Scoper** to partition into milestones (it assigns the ids and the milestone set,
not this document). Each behavior is tied to the STATE_DB table(s) it produces and the
`xcvrd-tests` module(s) that observe it. Dependencies noted so the Scoper can order them.

1. **Deploy / health smoke** — daemon compiles, injects into pmon, supervisor `RUNNING`;
   no pytest gate. Source: `daemon.rs` boot + `DaemonXcvrd.run`. e2e: `test_health.py`,
   `test_daemon_control.py` (deploy-smoke).
2. **Presence + identity** — discover modules; publish `TRANSCEIVER_INFO` (full CMIS /
   SFF field builder) + `TRANSCEIVER_STATUS_SW.{status,error=N/A}`; boot `cmis_state=READY`
   projection so DOM can flow. Source: `SfpStateUpdateTask.init`,
   `post_port_sfp_info_to_db` (`xcvrd.py:178`), `_init_port_sfp_status_sw_tbl`. Tables:
   `TRANSCEIVER_INFO`, `TRANSCEIVER_STATUS_SW`. e2e: `test_presence.py`,
   `test_info_content.py`, `test_logical_port.py`.
3. **Plug/unplug event machine** — `get_change_event` loop; insert (publish info+thresholds
   +media), remove (`remove_xcvr_api`, delete rows), soak, EEPROM retry.
   Source: `SfpStateUpdateTask.task_worker` (`xcvrd.py:395`),
   `_mapping_event_from_change_event`, `retry_eeprom_reading`. Tables: all `TRANSCEIVER_*`
   (delete). e2e: `test_presence.py`, `test_interaction_trace.py`, `test_read_retry.py`,
   `test_removal_tables.py`, `test_stale_info.py`.
4. **DOM sensor + thresholds + temperature** — periodic `TRANSCEIVER_DOM_SENSOR`
   (temperature/voltage + 24 per-lane), boot `TRANSCEIVER_DOM_THRESHOLD`,
   `TRANSCEIVER_DOM_TEMPERATURE`; unit-strip + `last_update_time`; DOM gating
   (`dom_polling`, CMIS-init, error status). Source: `DomInfoUpdateTask.task_worker`,
   `DOMDBUtils`, `DomThermalInfoUpdateTask`. e2e: `test_dom.py`, `test_dom_polling.py`,
   `test_dom_gating.py`, `test_dom_lpmode.py`, `test_last_update_time.py`.
5. **Rich status / CMIS module+datapath state** — `TRANSCEIVER_STATUS`
   (`module_state`, `DP[1-8]State`, config/tx/rx per-lane) via DomInfoUpdateTask +
   `TRANSCEIVER_STATUS_SW.cmis_state` via CmisManagerTask. Source: `StatusDBUtils`,
   `CmisManagerTask.process_cmis_state_machine`. e2e: `test_transceiver_status.py`,
   `test_cmis_state_progression.py`, `test_cmis_datapath.py`.
6. **SFP error handling** — decode error bitmap to descriptions; set
   `STATUS_SW.error`; blocking → delete DOM but keep INFO; non-blocking → keep DOM;
   recover on plug-in. Source: `SfpStateUpdateTask.task_worker` error branch,
   `sfp_status_helper.py`. e2e: `test_status_error.py`, `test_status_flag.py`.
7. **DOM/status/VDM flag metadata** — `*_FLAG` + `_CHANGE_COUNT`/`_SET_TIME`/`_CLEAR_TIME`
   engine. Source: `DBUtils._update_flag_metadata_tables`. e2e: `test_dom_flag_meta.py`,
   `test_status_flag.py`, `test_link_change_flags.py`.
8. **lpmode / reset** — `get_lpmode`/`set_lpmode`/`reset` semantics. Source: `XCVRDUtils`,
   CMIS `set_lpmode`. e2e: `test_lpmode_reset.py`, `test_dom_lpmode.py`.
9. **CMIS datapath bring-up** — full `INSERTED→…→READY/FAILED` machine, app-select,
   host/media lane masks, `active_apsel_hostlaneN`, retries/timers, decommission,
   fast-reboot skip. Source: `cmis/cmis_manager_task.py`. Tables: `STATUS_SW.cmis_state`,
   `TRANSCEIVER_INFO.active_apsel_*`. e2e: `test_cmis_datapath.py`, `test_cmis_reconfig.py`,
   `test_cmis_failed.py`, `test_cmis_forced_tx.py`, `test_app_select.py`,
   `test_fast_reboot_dp_skip.py`, `test_host_tx_ready.py`.
10. **Coherent / ZR tuning** — `configure_tx_output_power`, `configure_laser_frequency`,
    grid validation. Source: CMIS handlers (`cmis_manager_task.py:713`–`:751`,
    `:982`–`:1004`). e2e: `test_coherent_tuning.py`.
11. **VDM (basic + statistic) + PM + firmware** — freeze→capture→unfreeze; VDM real/
    threshold/flag tables; `TRANSCEIVER_PM`; `TRANSCEIVER_FIRMWARE_INFO`. Source:
    `VDMUtils`/`VDMDBUtils`, `post_port_pm_info_to_db`, `post_port_sfp_firmware_info_to_db`.
    e2e: `test_vdm.py`, `test_vdm_statistic.py`, `test_pm.py`, `test_firmware_info.py`.
12. **Media settings / optics SI** — parse `media_settings.json`/`optics_si_settings.json`,
    `notify_media_setting`, `NPU_SI_SETTINGS_SYNC_STATUS`. Source:
    `media_settings_parser.py`, `optics_si_parser.py`. e2e: `test_media_settings.py`,
    `test_optics_si.py`.
13. **SFF control (optional)** — `SffManagerTask` tx_disable per `host_tx_ready`/
    `admin_status`; high-power class. Source: `sff_mgr.py`. e2e: `test_sff_control.py`,
    `test_sff8636.py`, `test_flat_memory.py`.
14. **Multiport concurrency** — all tasks correct across many ports/subports (natsort
    ordering, per-port isolation, breakout). Source: `PortMapping`, all task loops. e2e:
    `test_multiport.py`.
15. **Golden conformance / warm reboot / lifecycle** — full-suite field-exact match to
    the golden captures; warm/fast-reboot table retention. Source: `deinit`, warm-start
    gating (`common.is_syncd_warm_restore_complete`). e2e: `test_golden.py`,
    `test_warm_reboot.py` (and the whole suite cumulatively).

Ordering guidance for the Scoper (not milestone ids): (1) gates deploy; (2)+(3) unlock the
INFO/STATUS_SW baseline the whole suite's clean-baseline fixture needs; (4)+(5) depend on
the boot `cmis_state` projection; (6)+(7) build on (3)/(5); (9)/(10) depend on (5);
(11)/(12)/(13) are additive; (14) exercises everything concurrently; (15) is the terminal
full-suite gate. Every `xcvrd-tests` module above maps to exactly one behavior group, which
is what the Scoper partitions.

---

*End of Analyzer document. The Scoper reads §3.7 (+ §3.5) to author `pipeline/milestones.json`;
the Planner reads §3.2–§3.6 to lay out `pipeline/crate/` and `pipeline/plan.json`; the
Translator/Validator work against the §3.4 boundary, §3.5 contract, and §3.6 seams.*
