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

pub mod daemon;
pub mod env;

// --- Translation-pipeline skeleton (Planner) -------------------------------
// Trait seams that make the daemon logic mockable for `cargo test` (Part B),
// plus the module tree mirroring the Python `xcvrd/` package (Part A). The
// deployed M0/M1 binary still runs `daemon::run`; these modules are stubs the
// Translator fills in milestone by milestone (see pipeline/plan.json).
pub mod hal; // HAL seam (trait Hal/SfpApi) + real PlatformHal over platform-bridge
pub mod statedb; // STATE_DB seam (trait StateDb/TableApi) + real SwssStateDb over swss-common

pub mod cmis; // <- cmis/
pub mod dom; // <- dom/
pub mod sff_mgr; // <- sff_mgr.py
pub mod xcvrd; // <- xcvrd.py (SfpStateUpdateTask, post_port_sfp_info_to_db, Daemon)
pub mod xcvrd_utilities; // <- xcvrd_utilities/

// Mock HAL + STATE_DB (ports of tests/mock_platform.py + mock_swsscommon.py).
// Test-only: never compiled into the deployed daemon.
#[cfg(test)]
pub mod mock;

// M6 golden-conformance projection test (Part B): the MockHal/MockDb analogue of
// the immutable `xcvrd-tests/tests/test_golden.py` oracle.
#[cfg(test)]
mod golden;
