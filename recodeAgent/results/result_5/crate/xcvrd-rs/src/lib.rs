//! xcvrd-rs library.
//!
//! The daemon (`daemon`, `env`) is built on the platform-bridge HAL and the
//! swss-common STATE_DB bindings. The Python xcvrd package layout is mirrored under
//! `src/` (directory -> module, file -> submodule) with two extra **trait seams** for
//! unit testing:
//!
//!   - [`hal`] — `Chassis`/`Sfp` over the PyO3 `platform_bridge` HAL. Real impls
//!     delegate to the bridge; mock impls live in [`mock`].
//!   - [`db`]  — `StateDb`/`Table` over `swss_common` (STATE_DB). Real impls wrap
//!     `swss_common::{DbConnector,Table}`; the `BTreeMap`-backed mock mirrors
//!     `tests/mock_swsscommon.py`.
//!
//! Daemon logic is written against `&dyn Chassis`/`&dyn Table`, so a unit test
//! injects mocks (`cargo test`) while the deployed daemon injects the
//! bridge/swss-common. The bindings stay wired as dependencies:
//!
//!   - [`platform_bridge`] — the Python `sonic_platform` HAL (xcvr-emu) via PyO3.
//!   - [`swss_common`] — STATE_DB (Redis) via the official sonic-net bindings.
//!
//! Runnable demonstrations live in `examples/`.

pub mod daemon;
pub mod env;

// Trait seams for the mockable HAL + STATE_DB (unit-test strategy).
pub mod hal;
pub mod db;

// Daemon package, mirroring the Python `xcvrd/` layout.
pub mod xcvrd;
pub mod sff_mgr;
pub mod cmis;
pub mod dom;
pub mod xcvrd_utilities;

// Test doubles for the seams (compiled only under `cargo test`).
#[cfg(test)]
pub mod mock;
