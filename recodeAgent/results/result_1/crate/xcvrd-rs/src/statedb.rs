//! STATE_DB seam — the mockable Redis table boundary.
//!
//! Port of the Python `swsscommon.Table` surface xcvrd uses via `XcvrTableHelper`
//! (`set`/`get`/`hget`/`_del`/`getKeys`). The daemon writes every `TRANSCEIVER_*`
//! row through these traits so it can target the real Redis STATE_DB on the DUT
//! *and* an in-memory `mock::MockStateDb` under `cargo test` (a direct port of
//! `tests/mock_swsscommon.py`). This is the Part-B unit-test seam from analysis §3.6.
//!
//! - `trait TableApi` — one table (`TRANSCEIVER_INFO`, …): row-keyed `set`/`get`/…
//! - `trait StateDb`  — opens `TableApi` handles by name (the `XcvrTableHelper` role).
//! - `SwssStateDb` / `SwssTable` — the REAL impl over `swss_common::DbConnector`.
//!   A table row is the Redis hash `"<TABLE>|<lport>"` (matches the bootstrap
//!   `daemon.rs` key scheme), written field-by-field with `hset`.
//!
//! Only the trait definitions + the thin real wrappers live here — no daemon
//! decision logic (that is the Translator's job in the task modules).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use swss_common::{CxxString, DbConnector};

/// A single table row: field -> value, both rendered as STATE_DB strings.
pub type Row = BTreeMap<String, String>;

/// Error surfaced by a STATE_DB call.
#[derive(Debug)]
pub enum DbError {
    /// Any failure from the underlying swss-common / Redis layer.
    Backend(String),
    /// Mock-injected failure (unit tests only).
    Mock(String),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Backend(s) => write!(f, "statedb backend error: {s}"),
            DbError::Mock(s) => write!(f, "statedb mock error: {s}"),
        }
    }
}

impl std::error::Error for DbError {}

pub type Result<T> = std::result::Result<T, DbError>;

/// One STATE_DB table (`swsscommon.Table`). Keys are logical ports (`Ethernet100`).
pub trait TableApi {
    /// Set (overwrite) all fields of `key`'s row.
    fn set(&self, key: &str, fields: &Row) -> Result<()>;
    /// Read `key`'s full row, or `None` if absent.
    fn get(&self, key: &str) -> Result<Option<Row>>;
    /// Read a single field of `key`, or `None` if row/field absent.
    fn hget(&self, key: &str, field: &str) -> Result<Option<String>>;
    /// Delete a single field of `key` (removing the row if it becomes empty).
    fn hdel(&self, key: &str, field: &str) -> Result<()>;
    /// Delete `key`'s entire row.
    fn del(&self, key: &str) -> Result<()>;
    /// All row keys currently in the table.
    fn keys(&self) -> Result<Vec<String>>;
}

/// The STATE_DB connection: hands out per-name `TableApi` handles (the
/// `XcvrTableHelper` role). Table-name constants live in
/// `xcvrd_utilities::xcvr_table_helper`.
pub trait StateDb {
    type Table: TableApi;
    fn table(&self, name: &str) -> Result<Self::Table>;
}

// --------------------------------------------------------------------------
// Real implementation: thin delegation to swss-common DbConnector (no logic).
// --------------------------------------------------------------------------

/// Real STATE_DB over `swss_common::DbConnector` (Redis unix socket).
///
/// The connection lives behind `Arc<Mutex<..>>` so a single STATE_DB is shared
/// across the daemon's spawned task threads: `swss_common::DbConnector` is `Send`
/// but not `Sync`, so a bare `Arc<DbConnector>` would be neither `Send` nor `Sync`
/// (`Arc<T>: Send` needs `T: Send + Sync`). The `Mutex` restores `Sync`, making
/// `SwssStateDb`/`SwssTable` `Send + Sync` and safe to move into `std::thread`
/// task loops (analysis §3, M5). Cloning shares the same underlying connection.
#[derive(Clone)]
pub struct SwssStateDb {
    db: Arc<Mutex<DbConnector>>,
}

impl SwssStateDb {
    /// Wrap an already-opened connection.
    pub fn new(db: DbConnector) -> Self {
        Self { db: Arc::new(Mutex::new(db)) }
    }

    /// Open STATE_DB via the shared env seed (`env::open_state_db`).
    pub fn open() -> Result<Self> {
        let db = crate::env::open_state_db().map_err(|e| DbError::Backend(e.to_string()))?;
        Ok(Self::new(db))
    }
}

impl StateDb for SwssStateDb {
    type Table = SwssTable;

    fn table(&self, name: &str) -> Result<SwssTable> {
        Ok(SwssTable { db: self.db.clone(), name: name.to_string() })
    }
}

/// Real table handle. Row key in Redis is `"<name>|<key>"`. Cheap to clone
/// (shares the mutexed connection).
#[derive(Clone)]
pub struct SwssTable {
    db: Arc<Mutex<DbConnector>>,
    name: String,
}

impl SwssTable {
    fn redis_key(&self, key: &str) -> String {
        format!("{}|{}", self.name, key)
    }
}

impl TableApi for SwssTable {
    fn set(&self, key: &str, fields: &Row) -> Result<()> {
        let rk = self.redis_key(key);
        let db = self.db.lock().map_err(|e| DbError::Backend(e.to_string()))?;
        for (field, value) in fields {
            db.hset(&rk, field, &CxxString::from(value.as_str()))
                .map_err(|e| DbError::Backend(e.to_string()))?;
        }
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<Row>> {
        let rk = self.redis_key(key);
        let db = self.db.lock().map_err(|e| DbError::Backend(e.to_string()))?;
        if !db.exists(&rk).map_err(|e| DbError::Backend(e.to_string()))? {
            return Ok(None);
        }
        let all = db.hgetall(&rk).map_err(|e| DbError::Backend(e.to_string()))?;
        let mut out = Row::new();
        for (field, value) in all {
            out.insert(field, value.to_string_lossy().into_owned());
        }
        Ok(Some(out))
    }

    fn hget(&self, key: &str, field: &str) -> Result<Option<String>> {
        Ok(self.get(key)?.and_then(|row| row.get(field).cloned()))
    }

    fn hdel(&self, key: &str, field: &str) -> Result<()> {
        // Implemented over confirmed primitives (get/del/set) rather than a raw
        // Redis HDEL: read the row, drop the field, and rewrite (or delete the
        // row when it becomes empty — matching mock_swsscommon.Table.hdel).
        if let Some(mut row) = self.get(key)? {
            if row.remove(field).is_some() {
                self.del(key)?;
                if !row.is_empty() {
                    self.set(key, &row)?;
                }
            }
        }
        Ok(())
    }

    fn del(&self, key: &str) -> Result<()> {
        let rk = self.redis_key(key);
        let db = self.db.lock().map_err(|e| DbError::Backend(e.to_string()))?;
        db.del(&rk).map_err(|e| DbError::Backend(e.to_string()))?;
        Ok(())
    }

    fn keys(&self) -> Result<Vec<String>> {
        // TODO(translator): scan Redis for `<name>|*` and strip the prefix.
        // Not needed by the M0/M1 bootstrap; the DUT reads rows directly.
        Ok(Vec::new())
    }
}

/// Compile-time proof that the real STATE_DB handles are `Send + Sync`, so the
/// daemon can move/share them into spawned task threads (M5 concurrency).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SwssStateDb>();
    assert_send_sync::<SwssTable>();
};
