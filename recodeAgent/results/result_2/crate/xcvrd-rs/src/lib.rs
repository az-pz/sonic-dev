//! xcvrd-rs bootstrap library.
//!
//! This is the starting point the ReCodeAgent translation pipeline builds on. The
//! two environment bindings the daemon needs are already wired as dependencies, so
//! agents can `use` them directly and never re-solve interop:
//!
//!   - [`platform_bridge`] — the Python `sonic_platform` HAL (xcvr-emu) via PyO3.
//!     `Platform`/`Chassis`/`Sfp` expose `get_transceiver_info`, `get_change_event`,
//!     DOM/status, lpmode/reset, eeprom, … (CMIS decode stays in Python).
//!   - [`swss_common`] — STATE_DB (Redis) via the official sonic-net bindings.
//!     `DbConnector`, `Table`, `ProducerStateTable`, `SubscriberStateTable`, …
//!
//! [`env`] gives ready-made constructors ([`env::open_platform`],
//! [`env::open_state_db`]) so daemon code has one documented place to obtain a HAL
//! handle and a STATE_DB connection. Agents grow this module into the real
//! platform/DB layer as the milestones (see the repo README §5) require.
//!
//! Runnable demonstrations live in `examples/` (build with `cargo build
//! --examples`, or `bash tools/env_check.sh` to run them inside pmon):
//!   - `statedb_probe`   — STATE_DB round-trip via swss-common only.
//!   - `hal_to_statedb`  — the agent pattern: read a transceiver via the bridge,
//!                          then publish it to STATE_DB (what `SfpStateUpdateTask`
//!                          will do for real).

//! ## Translation skeleton (ReCodeAgent Planner)
//!
//! Beyond the M0/M1 bootstrap ([`daemon`] + [`env`]), the crate now carries the
//! stubbed module tree that mirrors the Python `xcvrd` package (analysis §3.3):
//!
//!   - [`hal`] / [`db`] — the two mockable trait seams (HAL over `platform-bridge`,
//!     STATE_DB over `swss-common`); [`mock`] provides the test doubles (the Rust
//!     analogue of `tests/mock_platform.py` / `tests/mock_swsscommon.py`).
//!   - [`xcvrd`] — `DaemonXcvrd` + `SfpStateUpdateTask` (presence/identity).
//!   - [`cmis`] — `CmisManagerTask` (datapath bring-up).
//!   - [`dom`] — `DomInfoUpdateTask`/`DomThermalInfoUpdateTask` + the DOM/status/VDM
//!     posters under `dom::utilities`.
//!   - [`sff_mgr`] — optional `SffManagerTask`.
//!   - [`xcvrd_utilities`] — shared helpers (common/table-registry/port-mapping/
//!     error-decode/utils/media+optics SI).
//!   - [`error`] — the crate `XcvrdError` + `Result` alias.
//!
//! All bodies are `todo!()` stubs behind clear TODOs; the Translator fills them in
//! milestone by milestone (see `pipeline/plan.json`). The bootstrap in [`daemon`]
//! keeps M0/M1 green while the tree grows.

pub mod daemon;
pub mod env;

pub mod db;
pub mod error;
pub mod hal;
pub mod mock;

pub mod cmis;
pub mod dom;
pub mod sff_mgr;
pub mod xcvrd;
pub mod xcvrd_utilities;
