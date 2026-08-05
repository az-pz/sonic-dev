//! xcvrd-rs — Python `xcvrd` → Rust port (ReCodeAgent Planner skeleton).
//!
//! Two environment bindings are wired as dependencies so the daemon never
//! re-solves interop:
//!   - [`platform_bridge`] — the Python `sonic_platform` HAL (xcvr-emu) via PyO3.
//!   - [`swss_common`] — STATE_DB (Redis) via the official sonic-net bindings.
//!
//! ## Layout (mirrors the Python `xcvrd` package, analysis §3.3)
//!
//! The bootstrap ([`daemon`] + [`env`]) implements M0/M1 (presence + identity) and
//! is self-contained, so it keeps the black-box suite's clean-baseline fixture green
//! while the rest of the tree is filled in milestone by milestone.
//!
//!   - [`hal`] / [`db`] — the two mockable trait seams (HAL over `platform-bridge`,
//!     STATE_DB over `swss-common`); [`mock`] provides the test doubles (the Rust
//!     analogue of `tests/mock_platform.py` / `tests/mock_swsscommon.py`).
//!   - [`error`] — the crate `XcvrdError` + `Result` alias (Python sentinels).
//!   - [`xcvrd`] — `DaemonXcvrd` helpers + `SfpStateUpdateTask` (presence/identity).
//!   - [`cmis`] — `CmisManagerTask` (CMIS datapath bring-up).
//!   - [`dom`] — `DomInfoUpdateTask`/`DomThermalInfoUpdateTask` + the DOM/status/VDM
//!     posters under `dom::utilities`.
//!   - [`sff_mgr`] — optional `SffManagerTask`.
//!   - [`xcvrd_utilities`] — shared helpers (common/utils/error-decode/table-registry/
//!     port-mapping/media+optics SI).
//!
//! Everything below the bootstrap ships as **stubbed signatures** (`todo!()`/no-op
//! behind clear TODOs) plus `#[cfg(test)] mod tests` (`#[ignore]` stubs) that mirror
//! the Python `tests/test_xcvrd.py` structure. The Translator fills the bodies in
//! per `pipeline/plan.json`; the seams ([`hal`], [`db`], [`mock`]) and small pure
//! helpers (e.g. [`dom::utilities::db::value_to_py_str`]) are already functional so
//! the crate compiles and `cargo test` runs.

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
