//! STATE_DB trait seam, mirroring `tests/mock_swsscommon.py`.
//!
//! The Python tests use a dict-backed fake `Table` (`set/get→(found,fvs)/hget/
//! hdel/_del/getKeys/get_size`) so producers can be asserted without Redis. Here
//! that surface is the [`Table`] trait; [`StateDb`] mints per-table handles. The
//! **real** impls wrap `swss_common::{DbConnector,Table}`; the **mock** impls
//! ([`crate::mock::MockTable`]) are a `BTreeMap`-backed fake.

use std::rc::Rc;

use swss_common::{CxxString, DbConnector, Table as SwssTable};

pub type DbResult<T> = std::result::Result<T, String>;

/// A STATE_DB table handle keyed by `TABLE|<logical_port>` (HGETALL semantics).
/// Method names track `mock_swsscommon.Table` so ported tests read naturally.
pub trait Table {
    fn set(&self, key: &str, fvs: &[(String, String)]) -> DbResult<()>;
    fn get(&self, key: &str) -> DbResult<Option<Vec<(String, String)>>>;
    fn hget(&self, key: &str, field: &str) -> DbResult<Option<String>>;
    fn hset(&self, key: &str, field: &str, value: &str) -> DbResult<()>;
    fn hdel(&self, key: &str, field: &str) -> DbResult<()>;
    fn del(&self, key: &str) -> DbResult<()>;
    fn get_keys(&self) -> DbResult<Vec<String>>;
    fn get_size(&self) -> DbResult<usize>;
}

/// STATE_DB/CONFIG_DB/APPL_DB connection factory (the `XcvrTableHelper` seam).
/// Returns `Rc` handles so a mock can hand the daemon and the test the *same*
/// table (shared rows), the analogue of a `MagicMock` table shared across patches.
pub trait StateDb {
    fn table(&self, name: &str) -> DbResult<Rc<dyn Table>>;
}

// ---- real implementations (wrap swss-common; NOT daemon logic) --------------

/// Real DB: remembers the socket + logical db id and opens a fresh
/// `swss_common::Table` (which owns its own `DbConnector`) per table name.
pub struct RealStateDb {
    pub db_id: i32,
    pub sock: String,
}

impl RealStateDb {
    pub fn new(db_id: i32, sock: impl Into<String>) -> Self {
        RealStateDb { db_id, sock: sock.into() }
    }
}

impl StateDb for RealStateDb {
    fn table(&self, name: &str) -> DbResult<Rc<dyn Table>> {
        let conn = DbConnector::new_unix(self.db_id, self.sock.clone(), 0).map_err(|e| format!("{e:?}"))?;
        let t = SwssTable::new(conn, name).map_err(|e| format!("{e:?}"))?;
        Ok(Rc::new(RealTable(t)))
    }
}

/// Real table: thin adapter over `swss_common::Table`.
pub struct RealTable(pub SwssTable);
impl Table for RealTable {
    fn set(&self, key: &str, fvs: &[(String, String)]) -> DbResult<()> {
        for (f, v) in fvs {
            self.0.hset(key, f, &CxxString::new(v.as_str())).map_err(|e| format!("{e:?}"))?;
        }
        Ok(())
    }
    fn get(&self, key: &str) -> DbResult<Option<Vec<(String, String)>>> {
        let got = self.0.get(key).map_err(|e| format!("{e:?}"))?;
        Ok(got.map(|fvs| fvs.into_iter().map(|(k, c)| (k, c.to_string_lossy().into_owned())).collect()))
    }
    fn hget(&self, key: &str, field: &str) -> DbResult<Option<String>> {
        let got = self.0.hget(key, field).map_err(|e| format!("{e:?}"))?;
        Ok(got.map(|c| c.to_string_lossy().into_owned()))
    }
    fn hset(&self, key: &str, field: &str, value: &str) -> DbResult<()> {
        self.0.hset(key, field, &CxxString::new(value)).map_err(|e| format!("{e:?}"))
    }
    fn hdel(&self, key: &str, field: &str) -> DbResult<()> {
        self.0.hdel(key, field).map_err(|e| format!("{e:?}"))
    }
    fn del(&self, key: &str) -> DbResult<()> {
        self.0.del(key).map_err(|e| format!("{e:?}"))
    }
    fn get_keys(&self) -> DbResult<Vec<String>> {
        self.0.get_keys().map_err(|e| format!("{e:?}"))
    }
    fn get_size(&self) -> DbResult<usize> {
        Ok(self.0.get_keys().map_err(|e| format!("{e:?}"))?.len())
    }
}

/// A [`Table`] backed by a raw [`DbConnector`] that builds `<name><sep><key>` keys
/// **explicitly**. The swss `Table` derives its key separator from a globally
/// initialized `SonicDBConfig`, which is not reliably available over a bare
/// `new_unix` connection; the rest of the daemon writes/reads STATE_DB with explicit
/// separator-joined keys for exactly this reason. The media producer tables do the
/// same so the published rows land on the precise keys their consumers read: APPL_DB
/// `PORT_TABLE:<port>` (`:`), STATE_DB `PORT_TABLE|<port>` and CONFIG_DB `PORT|<port>`
/// (`|`).
pub struct SepTable {
    conn: DbConnector,
    name: String,
    sep: char,
}

impl SepTable {
    pub fn new(conn: DbConnector, name: impl Into<String>, sep: char) -> Self {
        SepTable { conn, name: name.into(), sep }
    }
    fn full(&self, key: &str) -> String {
        sep_table_key(&self.name, self.sep, key)
    }
}

/// Build a `<table><sep><key>` Redis key. Free helper so the separator convention
/// (APPL_DB `:`, STATE_DB / CONFIG_DB `|`) is unit-testable without a live connection.
pub fn sep_table_key(name: &str, sep: char, key: &str) -> String {
    format!("{name}{sep}{key}")
}

impl Table for SepTable {
    fn set(&self, key: &str, fvs: &[(String, String)]) -> DbResult<()> {
        let full = self.full(key);
        for (f, v) in fvs {
            self.conn
                .hset(&full, f, &CxxString::new(v.as_str()))
                .map_err(|e| format!("{e:?}"))?;
        }
        Ok(())
    }
    fn get(&self, key: &str) -> DbResult<Option<Vec<(String, String)>>> {
        let got = self.conn.hgetall(&self.full(key)).map_err(|e| format!("{e:?}"))?;
        if got.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            got.into_iter()
                .map(|(k, c)| (k, c.to_string_lossy().into_owned()))
                .collect(),
        ))
    }
    fn hget(&self, key: &str, field: &str) -> DbResult<Option<String>> {
        let got = self.conn.hget(&self.full(key), field).map_err(|e| format!("{e:?}"))?;
        Ok(got.map(|c| c.to_string_lossy().into_owned()))
    }
    fn hset(&self, key: &str, field: &str, value: &str) -> DbResult<()> {
        self.conn
            .hset(&self.full(key), field, &CxxString::new(value))
            .map_err(|e| format!("{e:?}"))
    }
    fn hdel(&self, key: &str, field: &str) -> DbResult<()> {
        self.conn
            .hdel(&self.full(key), field)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }
    fn del(&self, key: &str) -> DbResult<()> {
        self.conn.del(&self.full(key)).map(|_| ()).map_err(|e| format!("{e:?}"))
    }
    fn get_keys(&self) -> DbResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn get_size(&self) -> DbResult<usize> {
        Ok(0)
    }
}

/// A no-op [`Table`] used as a placeholder for optional table handles that are
/// not wired (e.g. the DOM-flag metadata trio before the flag
/// poster is attached). Every read is empty and every write is dropped.
pub struct NullTable;

impl Table for NullTable {
    fn set(&self, _key: &str, _fvs: &[(String, String)]) -> DbResult<()> {
        Ok(())
    }
    fn get(&self, _key: &str) -> DbResult<Option<Vec<(String, String)>>> {
        Ok(None)
    }
    fn hget(&self, _key: &str, _field: &str) -> DbResult<Option<String>> {
        Ok(None)
    }
    fn hset(&self, _key: &str, _field: &str, _value: &str) -> DbResult<()> {
        Ok(())
    }
    fn hdel(&self, _key: &str, _field: &str) -> DbResult<()> {
        Ok(())
    }
    fn del(&self, _key: &str) -> DbResult<()> {
        Ok(())
    }
    fn get_keys(&self) -> DbResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn get_size(&self) -> DbResult<usize> {
        Ok(0)
    }
}

/// Real STATE_DB read seam for the warm/fast-reboot detectors
/// ([`crate::xcvrd_utilities::common::StateDbHget`]): a thin adapter over a bare
/// `swss_common::DbConnector` that resolves a `<table>|<key>` Redis hash field. The
/// deployed daemon opens one STATE_DB connection and hands it to `is_fast_reboot_enabled`
/// / `is_syncd_warm_restore_complete`; a DB error or an absent field is `None`.
impl crate::xcvrd_utilities::common::StateDbHget for DbConnector {
    fn get_field(&self, key: &str, field: &str) -> Option<String> {
        self.hget(key, field).ok().flatten().map(|c| c.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the explicit-separator producer tables join keys with
    /// the DB's own separator so the published media rows land on the exact keys their
    /// consumers read — APPL_DB `PORT_TABLE:<port>` (`:`), STATE_DB `PORT_TABLE|<port>`
    /// and CONFIG_DB `PORT|<port>` (`|`) — rather than relying on the swss `Table`'s
    /// `SonicDBConfig`-derived separator (unreliable over a bare `new_unix` connection).
    #[test]
    fn sep_table_key_uses_db_separator() {
        assert_eq!(sep_table_key("PORT_TABLE", ':', "Ethernet32"), "PORT_TABLE:Ethernet32");
        assert_eq!(sep_table_key("PORT_TABLE", '|', "Ethernet32"), "PORT_TABLE|Ethernet32");
        assert_eq!(sep_table_key("PORT", '|', "Ethernet4"), "PORT|Ethernet4");
    }
}
