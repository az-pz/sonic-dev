//! Port mapping + change events — port of `xcvrd_utilities/port_event_helper.py`.
//!
//! `PortMapping` is the logical<->physical registry (`Ethernet100` <-> `25`) the
//! whole daemon indexes by. `get_port_mapping` builds it from CONFIG_DB `PORT`;
//! `PortChangeObserver` subscribes to runtime PORT changes. On the testbed the
//! mapping is `Ethernet{index*4}` <-> `index` (see bootstrap `discover_ports`).
//! Struct shapes are real; behavioural bodies are stubs for the Translator.

#![allow(dead_code, unused_variables)]

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crate::statedb::{DbError, StateDb, TableApi};

/// CONFIG_DB `PORT` table name (`swsscommon.CFG_PORT_TABLE_NAME`).
pub const CFG_PORT_TABLE_NAME: &str = "PORT";

/// `multi_asic.PORT_ROLE` — the CONFIG_DB `PORT` field that tags a port's role.
pub const PORT_ROLE: &str = "role";

/// `SELECT_TIMEOUT_MSECS` (`port_event_helper.py:6`): default select() timeout.
pub const SELECT_TIMEOUT_MSECS: u64 = 1000;

/// `PortChangeEvent.event_type` (`port_event_helper.py:13`). `Add`/`Remove` are
/// the CONFIG_DB `PORT` config transitions (`PORT_ADD`/`PORT_REMOVE`); `Set`/`Del`
/// are the raw runtime table-update ops (`PORT_SET`/`PORT_DEL`) surfaced by
/// `PortChangeObserver`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortChangeEventType {
    Add,
    Remove,
    Set,
    Del,
}

/// `PortChangeEvent` (`port_event_helper.py:13`): a single PORT add/remove/set/del.
/// `port_index` mirrors the Python `int(index)` (can be `-1` when the update
/// carries no `index`); `port_dict`/`db_name`/`table_name` are populated for the
/// runtime `Set`/`Del` events produced by `PortChangeObserver`.
#[derive(Debug, Clone, PartialEq)]
pub struct PortChangeEvent {
    pub port_name: String,
    pub port_index: i64,
    pub asic_id: i32,
    pub event_type: PortChangeEventType,
    pub port_dict: Option<BTreeMap<String, String>>,
    pub db_name: Option<String>,
    pub table_name: Option<String>,
}

impl PortChangeEvent {
    /// Config add/remove event (physical index is a non-negative `usize`). Kept
    /// `usize`-typed so the M1 call sites (which pass a physical port) are source
    /// compatible; stored as the Python `int` (`i64`).
    pub fn new(port_name: &str, port_index: usize, asic_id: i32, event_type: PortChangeEventType) -> Self {
        Self::from_index(port_name, port_index as i64, asic_id, event_type)
    }

    /// Event carrying a raw (possibly `-1`) `int` index and no dict payload.
    pub fn from_index(port_name: &str, port_index: i64, asic_id: i32, event_type: PortChangeEventType) -> Self {
        Self {
            port_name: port_name.to_string(),
            port_index,
            asic_id,
            event_type,
            port_dict: None,
            db_name: None,
            table_name: None,
        }
    }

    /// Full runtime `Set`/`Del` event with the soaked field dict + originating
    /// DB/table (`PortChangeObserver.handle_port_update_event`).
    pub fn with_details(
        port_name: &str,
        port_index: i64,
        asic_id: i32,
        event_type: PortChangeEventType,
        port_dict: BTreeMap<String, String>,
        db_name: &str,
        table_name: &str,
    ) -> Self {
        Self {
            port_name: port_name.to_string(),
            port_index,
            asic_id,
            event_type,
            port_dict: Some(port_dict),
            db_name: Some(db_name.to_string()),
            table_name: Some(table_name.to_string()),
        }
    }
}

/// `PortMapping` (`port_event_helper.py:212`): logical<->physical registry.
#[derive(Debug, Default, Clone)]
pub struct PortMapping {
    /// Logical port names in add order, e.g. `["Ethernet0", "Ethernet4"]`.
    pub logical_port_list: Vec<String>,
    /// `Ethernet100` -> physical port index `25`.
    pub logical_to_physical: BTreeMap<String, usize>,
    /// physical index -> natsorted logical ports sharing it.
    pub physical_to_logical: BTreeMap<usize, Vec<String>>,
    /// logical port -> ASIC id.
    pub logical_to_asic: BTreeMap<String, i32>,
}

impl PortMapping {
    pub fn new() -> Self {
        Self::default()
    }

    /// Dispatch a `PortChangeEvent` to add/remove. `Set`/`Del` runtime ops are
    /// ignored here (they are handled by `PortChangeObserver`).
    pub fn handle_port_change_event(&mut self, event: &PortChangeEvent) {
        match event.event_type {
            PortChangeEventType::Add => self.handle_port_add(event),
            PortChangeEventType::Remove => self.handle_port_remove(event),
            PortChangeEventType::Set | PortChangeEventType::Del => {}
        }
    }

    /// `_handle_port_add` (`port_event_helper.py:235`).
    fn handle_port_add(&mut self, event: &PortChangeEvent) {
        let port_name = event.port_name.clone();
        let phys = event.port_index as usize;
        self.logical_port_list.push(port_name.clone());
        self.logical_to_physical.insert(port_name.clone(), phys);
        let entry = self.physical_to_logical.entry(phys).or_default();
        entry.push(port_name.clone());
        if entry.len() > 1 {
            // Natural-sort the logical ports sharing this physical port (ganged).
            entry.sort_by(|a, b| natural_cmp(a, b));
        }
        self.logical_to_asic.insert(port_name, event.asic_id);
    }

    /// `_handle_port_remove` (`port_event_helper.py:251`).
    fn handle_port_remove(&mut self, event: &PortChangeEvent) {
        let port_name = &event.port_name;
        let phys = event.port_index as usize;
        self.logical_port_list.retain(|p| p != port_name);
        self.logical_to_physical.remove(port_name);
        if let Some(list) = self.physical_to_logical.get_mut(&phys) {
            list.retain(|p| p != port_name);
            if list.is_empty() {
                self.physical_to_logical.remove(&phys);
            }
        }
        self.logical_to_asic.remove(port_name);
    }

    pub fn get_asic_id_for_logical_port(&self, port_name: &str) -> Option<i32> {
        self.logical_to_asic.get(port_name).copied()
    }

    pub fn is_logical_port(&self, port_name: &str) -> bool {
        self.logical_to_physical.contains_key(port_name)
    }

    pub fn get_logical_to_physical(&self, port_name: &str) -> Option<Vec<usize>> {
        self.logical_to_physical.get(port_name).map(|&idx| vec![idx])
    }

    pub fn get_physical_to_logical(&self, physical_port: usize) -> Option<Vec<String>> {
        self.physical_to_logical.get(&physical_port).cloned()
    }

    /// `logical_port_name_to_physical_port_list`: numeric name -> `[n]`, else map.
    pub fn logical_port_name_to_physical_port_list(&self, port_name: &str) -> Option<Vec<usize>> {
        if let Ok(n) = port_name.parse::<usize>() {
            return Some(vec![n]);
        }
        if self.is_logical_port(port_name) {
            self.get_logical_to_physical(port_name)
        } else {
            None
        }
    }
}

/// Natural (human) ordering of two strings, case-insensitive — the Rust analogue
/// of `natsort.natsorted(..., key=lambda x: x.lower())` used for `physical_to_logical`.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let na: String = take_digits(&mut ai);
                    let nb: String = take_digits(&mut bi);
                    let va: u64 = na.parse().unwrap_or(0);
                    let vb: u64 = nb.parse().unwrap_or(0);
                    match va.cmp(&vb) {
                        Ordering::Equal => continue,
                        ord => return ord,
                    }
                } else {
                    match ca.cmp(&cb) {
                        Ordering::Equal => {
                            ai.next();
                            bi.next();
                        }
                        ord => return ord,
                    }
                }
            }
        }
    }
}

fn take_digits(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut s = String::new();
    while let Some(&c) = it.peek() {
        if c.is_ascii_digit() {
            s.push(c);
            it.next();
        } else {
            break;
        }
    }
    s
}

/// Front-panel test — Rust analogue of `multi_asic.is_front_panel_port(key, role)`.
/// Backplane/inband/recycle-named ports and internal roles (e.g. `Dpc`) are excluded.
fn is_front_panel_port(port_name: &str, role: Option<&str>) -> bool {
    const NON_FP_PREFIXES: [&str; 3] = ["Ethernet-BP", "Ethernet-IB", "Ethernet-Rec"];
    if NON_FP_PREFIXES.iter().any(|p| port_name.starts_with(p)) {
        return false;
    }
    if let Some(r) = role {
        if matches!(r, "Dpc" | "Inb" | "Rec" | "Int" | "BmcMgmt") {
            return false;
        }
    }
    true
}

/// `get_port_mapping` (`port_event_helper.py:346`): scan the CONFIG_DB `PORT`
/// table and build a full `PortMapping`. Front-panel ports only; the physical
/// index comes from the row's `index` field. Single-ASIC testbed -> asic id 0.
pub fn get_port_mapping<D: StateDb>(config_db: &D) -> Result<PortMapping, DbError> {
    let mut port_mapping = PortMapping::new();
    let asic_id = 0;
    let port_table = config_db.table(CFG_PORT_TABLE_NAME)?;
    for key in port_table.keys()? {
        let cfg = match port_table.get(&key)? {
            Some(c) => c,
            None => continue,
        };
        if !is_front_panel_port(&key, cfg.get("role").map(String::as_str)) {
            continue;
        }
        let index = match cfg.get("index").and_then(|i| i.parse::<usize>().ok()) {
            Some(i) => i,
            None => continue,
        };
        let event = PortChangeEvent::new(&key, index, asic_id, PortChangeEventType::Add);
        port_mapping.handle_port_change_event(&event);
    }
    Ok(port_mapping)
}

// --------------------------------------------------------------------------
// Runtime PORT-change subscription seam (swsscommon.Select + SubscriberStateTable)
// --------------------------------------------------------------------------

/// `swsscommon.Select.select` outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectResult {
    /// An object is ready (`swsscommon.Select.OBJECT`).
    Object,
    /// The select timed out (`swsscommon.Select.TIMEOUT`).
    Timeout,
    /// Any other/error return.
    Error,
}

/// DB write op on a subscribed table (`swsscommon.SET_COMMAND`/`DEL_COMMAND`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortOp {
    Set,
    Del,
}

impl PortOp {
    /// STATE_DB op string (`"SET"`/`"DEL"`) stored in the event field dict.
    pub fn as_str(&self) -> &'static str {
        match self {
            PortOp::Set => "SET",
            PortOp::Del => "DEL",
        }
    }
}

/// One raw event popped from a subscribed table: mirrors `SubscriberStateTable.pop()`
/// returning `(key, op, field-value-pairs)`.
#[derive(Debug, Clone)]
pub struct RawPortEvent {
    pub key: String,
    pub op: PortOp,
    pub fvp: Vec<(String, String)>,
}

/// Metadata for one subscribed table (a `swsscommon.SubscriberStateTable`): the
/// DB + table name, the optional field filter, and the ASIC id it belongs to.
#[derive(Debug, Clone)]
pub struct SubMeta {
    pub db_name: String,
    pub table_name: String,
    pub filter: Option<Vec<String>>,
    pub asic_id: i32,
}

/// The runtime PORT-change source (`swsscommon.Select` + the subscribed tables).
/// The daemon calls `select` then `drain_tables`; the real impl wraps swss-common,
/// the mock scripts both (mirroring the Python tests' mocked `select`/`pop`).
pub trait PortEventSource {
    /// `swsscommon.Select.select(timeout)`.
    fn select(&mut self, timeout_ms: u64) -> SelectResult;
    /// Drain every subscribed table's pending events (each table's `pop()` loop
    /// until the empty-key sentinel), returning `(metadata, events)` per table.
    fn drain_tables(&mut self) -> Vec<(SubMeta, Vec<RawPortEvent>)>;
}

/// `PortChangeObserver.apply_filter_to_fvp` (`port_event_helper.py:78`): when a
/// field filter is set, drop every field not in `filter ∪ {index,port_name,asic_id,op}`.
fn apply_filter_to_fvp(filter: Option<&[String]>, fvp: &mut BTreeMap<String, String>) {
    if let Some(filter) = filter {
        let mut keep: HashSet<&str> = filter.iter().map(String::as_str).collect();
        for k in ["index", "port_name", "asic_id", "op"] {
            keep.insert(k);
        }
        fvp.retain(|k, _| keep.contains(k.as_str()));
    }
}

/// `PortChangeObserver` (`port_event_helper.py:46`): monitors runtime PORT changes
/// across the subscribed DB tables and dispatches soaked/filtered `PortChangeEvent`s
/// to a handler. Generic over a `PortEventSource` so it runs against swss-common on
/// the DUT and against `mock::MockPortEventSource` under `cargo test`.
pub struct PortChangeObserver<S: PortEventSource> {
    source: S,
    stop_event: std::sync::Arc<AtomicBool>,
    /// Cached last-seen role per port (`role` attribute may be absent on STATE_DB
    /// notifications, so it is remembered from CONFIG_DB/APPL_DB).
    pub port_role_map: BTreeMap<String, String>,
    /// Last dispatched (filtered) field dict per `(port_name, db, table)` key,
    /// used to de-duplicate repeated/subset updates.
    pub port_event_cache: BTreeMap<(String, String, String), BTreeMap<String, String>>,
}

impl<S: PortEventSource> PortChangeObserver<S> {
    pub fn new(source: S, stop_event: std::sync::Arc<AtomicBool>) -> Self {
        Self {
            source,
            stop_event,
            port_role_map: BTreeMap::new(),
            port_event_cache: BTreeMap::new(),
        }
    }

    /// Mutable access to the underlying source (test scripting / real wiring).
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// `handle_port_update_event` (`port_event_helper.py:116`): select PORT updates,
    /// soak duplicate events per key (last wins), apply per-table field filters,
    /// drop no-op/subset updates, and dispatch a `PortChangeEvent` (`Set`/`Del`) per
    /// changed key. Returns whether at least one event was dispatched.
    pub fn handle_port_update_event(
        &mut self,
        handler: &mut dyn FnMut(PortChangeEvent),
        timeout_ms: u64,
    ) -> bool {
        let mut has_event = false;
        if self.stop_event.load(AtomicOrdering::Relaxed) {
            return has_event;
        }
        match self.source.select(timeout_ms) {
            SelectResult::Timeout => return has_event,
            SelectResult::Error => return has_event,
            SelectResult::Object => {}
        }

        // Soak: keep only the last event per (port_name, db_name, table_name).
        struct Soaked {
            fvp: BTreeMap<String, String>,
            filter: Option<Vec<String>>,
            op: PortOp,
            asic_id: i32,
        }
        let mut soak: BTreeMap<(String, String, String), Soaked> = BTreeMap::new();

        for (meta, events) in self.source.drain_tables() {
            for ev in events {
                let mut fvp: BTreeMap<String, String> = ev.fvp.into_iter().collect();
                // Track/lookup the port role (may be absent on STATE_DB notifs).
                if let Some(role) = fvp.get(PORT_ROLE).cloned() {
                    self.port_role_map.insert(ev.key.clone(), role);
                }
                let role = self.port_role_map.get(&ev.key).cloned();
                if !is_front_panel_port(&ev.key, role.as_deref()) {
                    continue;
                }
                fvp.entry("index".to_string()).or_insert_with(|| "-1".to_string());
                fvp.insert("port_name".to_string(), ev.key.clone());
                fvp.insert("asic_id".to_string(), meta.asic_id.to_string());
                fvp.insert("op".to_string(), ev.op.as_str().to_string());
                soak.insert(
                    (ev.key.clone(), meta.db_name.clone(), meta.table_name.clone()),
                    Soaked { fvp, filter: meta.filter.clone(), op: ev.op, asic_id: meta.asic_id },
                );
            }
        }

        for ((port_name, db_name, table_name), mut s) in soak {
            let port_index: i64 =
                s.fvp.get("index").and_then(|v| v.parse::<i64>().ok()).unwrap_or(-1);
            apply_filter_to_fvp(s.filter.as_deref(), &mut s.fvp);
            let cache_key = (port_name.clone(), db_name.clone(), table_name.clone());
            // De-dup: a new fvp that is a subset of the cached one is a no-op.
            let diff_empty = self
                .port_event_cache
                .get(&cache_key)
                .map(|prev| s.fvp.iter().all(|(k, v)| prev.get(k) == Some(v)))
                .unwrap_or(false);
            self.port_event_cache.insert(cache_key, s.fvp.clone());
            if diff_empty {
                continue;
            }
            let event_type = match s.op {
                PortOp::Set => PortChangeEventType::Set,
                PortOp::Del => PortChangeEventType::Del,
            };
            let event = PortChangeEvent::with_details(
                &port_name,
                port_index,
                s.asic_id,
                event_type,
                s.fvp,
                &db_name,
                &table_name,
            );
            has_event = true;
            handler(event);
        }
        has_event
    }
}

/// `handle_port_config_change` (`port_event_helper.py:294`): select CONFIG_DB `PORT`
/// changes and, on an add/remove, notify the handler. Times out silently if there
/// is nothing to read. Generic over the subscriber seam.
pub fn handle_port_config_change<S, F>(
    source: &mut S,
    stop_event: &AtomicBool,
    port_mapping: &mut PortMapping,
    handler: &mut F,
) where
    S: PortEventSource,
    F: FnMut(&mut PortMapping, PortChangeEvent),
{
    if stop_event.load(AtomicOrdering::Relaxed) {
        return;
    }
    match source.select(SELECT_TIMEOUT_MSECS) {
        SelectResult::Timeout => return,
        SelectResult::Error => return,
        SelectResult::Object => {}
    }
    read_port_config_change(source, port_mapping, handler);
}

/// `read_port_config_change` (`port_event_helper.py:307`): translate raw CONFIG_DB
/// `PORT` `SET`/`DEL` ops into `PORT_ADD`/`PORT_REMOVE` events. A `SET` on a new
/// logical port adds it; a `SET` that changes an existing port's physical index
/// removes then re-adds it; a `DEL` of a known port removes it. The handler owns
/// the `PortMapping` mutation (so it can also update per-port DB state).
pub fn read_port_config_change<S, F>(
    source: &mut S,
    port_mapping: &mut PortMapping,
    handler: &mut F,
) where
    S: PortEventSource,
    F: FnMut(&mut PortMapping, PortChangeEvent),
{
    for (meta, events) in source.drain_tables() {
        for ev in events {
            let fvp: BTreeMap<String, String> = ev.fvp.iter().cloned().collect();
            if !is_front_panel_port(&ev.key, fvp.get(PORT_ROLE).map(String::as_str)) {
                continue;
            }
            match ev.op {
                PortOp::Set => {
                    let new_index = match fvp.get("index").and_then(|s| s.parse::<i64>().ok()) {
                        Some(i) => i,
                        // `'index' not in fvp` -> skip (Python `continue`).
                        None => continue,
                    };
                    if !port_mapping.is_logical_port(&ev.key) {
                        // New logical port created.
                        let e = PortChangeEvent::from_index(
                            &ev.key,
                            new_index,
                            meta.asic_id,
                            PortChangeEventType::Add,
                        );
                        handler(port_mapping, e);
                    } else {
                        let current = port_mapping
                            .get_logical_to_physical(&ev.key)
                            .and_then(|l| l.first().copied())
                            .map(|p| p as i64)
                            .unwrap_or(-1);
                        if current != new_index {
                            handler(
                                port_mapping,
                                PortChangeEvent::from_index(
                                    &ev.key,
                                    current,
                                    meta.asic_id,
                                    PortChangeEventType::Remove,
                                ),
                            );
                            handler(
                                port_mapping,
                                PortChangeEvent::from_index(
                                    &ev.key,
                                    new_index,
                                    meta.asic_id,
                                    PortChangeEventType::Add,
                                ),
                            );
                        }
                    }
                }
                PortOp::Del => {
                    if port_mapping.is_logical_port(&ev.key) {
                        let current = port_mapping
                            .get_logical_to_physical(&ev.key)
                            .and_then(|l| l.first().copied())
                            .map(|p| p as i64)
                            .unwrap_or(-1);
                        handler(
                            port_mapping,
                            PortChangeEvent::from_index(
                                &ev.key,
                                current,
                                meta.asic_id,
                                PortChangeEventType::Remove,
                            ),
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockStateDb;

    fn row(pairs: &[(&str, &str)]) -> crate::statedb::Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// <- test_get_port_mapping: front-panel ports land in the map with their
    /// `index`; inband-named and DPU-role (`Dpc`) ports are excluded.
    #[test]
    fn get_port_mapping_from_config_db_port_table() {
        let db = MockStateDb::new();
        let port_tbl = db.table(CFG_PORT_TABLE_NAME).unwrap();
        port_tbl.set("Ethernet0", &row(&[("index", "1")])).unwrap();
        port_tbl.set("Ethernet4", &row(&[("index", "2")])).unwrap();
        port_tbl.set("Ethernet-IB0", &row(&[("index", "3")])).unwrap();
        port_tbl.set("Ethernet8", &row(&[("index", "4"), ("role", "Dpc")])).unwrap();

        let pm = get_port_mapping(&db).unwrap();

        assert!(pm.logical_port_list.contains(&"Ethernet0".to_string()));
        assert_eq!(pm.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(pm.get_physical_to_logical(1), Some(vec!["Ethernet0".to_string()]));
        assert_eq!(pm.get_logical_to_physical("Ethernet0"), Some(vec![1]));

        assert!(pm.logical_port_list.contains(&"Ethernet4".to_string()));
        assert_eq!(pm.get_physical_to_logical(2), Some(vec!["Ethernet4".to_string()]));
        assert_eq!(pm.get_logical_to_physical("Ethernet4"), Some(vec![2]));

        // Inband port excluded by name.
        assert!(!pm.logical_port_list.contains(&"Ethernet-IB0".to_string()));
        assert_eq!(pm.get_asic_id_for_logical_port("Ethernet-IB0"), None);
        assert_eq!(pm.get_physical_to_logical(3), None);
        // DPU-role port excluded by role.
        assert!(!pm.logical_port_list.contains(&"Ethernet8".to_string()));
        assert_eq!(pm.get_physical_to_logical(4), None);
    }

    #[test]
    fn handle_port_change_event_add_then_remove() {
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortChangeEventType::Add));
        assert!(pm.is_logical_port("Ethernet0"));
        assert_eq!(pm.get_logical_to_physical("Ethernet0"), Some(vec![1]));
        assert_eq!(pm.get_physical_to_logical(1), Some(vec!["Ethernet0".to_string()]));

        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortChangeEventType::Remove));
        assert!(!pm.is_logical_port("Ethernet0"));
        assert_eq!(pm.get_physical_to_logical(1), None);
        assert!(pm.logical_port_list.is_empty());
    }

    #[test]
    fn logical_port_name_to_physical_port_list_numeric_and_logical() {
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 5, 0, PortChangeEventType::Add));
        // Numeric name -> [n] directly.
        assert_eq!(pm.logical_port_name_to_physical_port_list("7"), Some(vec![7]));
        // Known logical name -> its physical index.
        assert_eq!(pm.logical_port_name_to_physical_port_list("Ethernet0"), Some(vec![5]));
        // Unknown logical name -> None.
        assert_eq!(pm.logical_port_name_to_physical_port_list("Ethernet999"), None);
    }

    use crate::mock::MockPortEventSource;
    use std::sync::atomic::AtomicBool as StdAtomicBool;
    use std::sync::Arc;

    fn raw(key: &str, op: PortOp, fvp: &[(&str, &str)]) -> RawPortEvent {
        RawPortEvent {
            key: key.to_string(),
            op,
            fvp: fvp.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    fn meta(filter: Option<&[&str]>) -> SubMeta {
        SubMeta {
            db_name: "CONFIG_DB".to_string(),
            table_name: CFG_PORT_TABLE_NAME.to_string(),
            filter: filter.map(|f| f.iter().map(|s| s.to_string()).collect()),
            asic_id: 0,
        }
    }

    /// <- test_handle_port_update_event: soak + filter + de-dup + dispatch.
    #[test]
    fn handle_port_update_event_soak_filter_dedup() {
        let stop = Arc::new(StdAtomicBool::new(false));
        let mut observer = PortChangeObserver::new(MockPortEventSource::new(), stop);
        let mut dispatched: Vec<PortChangeEvent> = Vec::new();

        // 1. Basic single SET update, no filter: 'fec' is kept.
        observer
            .source_mut()
            .set_table(meta(None), vec![raw("Ethernet0", PortOp::Set, &[("index", "1"), ("speed", "40000"), ("fec", "rs")])]);
        {
            let mut handler = |e: PortChangeEvent| dispatched.push(e);
            assert!(observer.handle_port_update_event(&mut handler, 1000));
        }
        assert_eq!(dispatched.len(), 1);
        let cached = observer
            .port_event_cache
            .get(&("Ethernet0".to_string(), "CONFIG_DB".to_string(), CFG_PORT_TABLE_NAME.to_string()))
            .unwrap();
        assert_eq!(cached.get("fec").map(String::as_str), Some("rs"));
        assert_eq!(cached.get("speed").map(String::as_str), Some("40000"));
        assert_eq!(cached.get("op").map(String::as_str), Some("SET"));
        assert_eq!(dispatched[0].event_type, PortChangeEventType::Set);
        assert_eq!(dispatched[0].port_index, 1);
        assert_eq!(dispatched[0].asic_id, 0);
        assert_eq!(dispatched[0].db_name.as_deref(), Some("CONFIG_DB"));

        // 2. Filtered observer: 'fec' filtered out, 'speed' kept.
        let stop2 = Arc::new(StdAtomicBool::new(false));
        let mut observer = PortChangeObserver::new(MockPortEventSource::new(), stop2);
        observer.source_mut().set_table(
            meta(Some(&["speed"])),
            vec![raw("Ethernet0", PortOp::Set, &[("index", "1"), ("speed", "40000"), ("fec", "rs")])],
        );
        {
            let mut handler = |e: PortChangeEvent| dispatched.push(e);
            assert!(observer.handle_port_update_event(&mut handler, 1000));
        }
        let cached = observer
            .port_event_cache
            .get(&("Ethernet0".to_string(), "CONFIG_DB".to_string(), CFG_PORT_TABLE_NAME.to_string()))
            .unwrap();
        assert!(!cached.contains_key("fec"));
        assert_eq!(cached.get("speed").map(String::as_str), Some("40000"));
        assert_eq!(dispatched.last().unwrap().port_dict.as_ref().unwrap().get("speed").map(String::as_str), Some("40000"));

        // 3. Duplicate event on the same key -> no dispatch.
        let before = dispatched.len();
        observer.source_mut().set_table(
            meta(Some(&["speed"])),
            vec![raw("Ethernet0", PortOp::Set, &[("index", "1"), ("speed", "40000"), ("fec", "rs")])],
        );
        {
            let mut handler = |e: PortChangeEvent| dispatched.push(e);
            assert!(!observer.handle_port_update_event(&mut handler, 1000));
        }
        assert_eq!(dispatched.len(), before);

        // 4. Soak multiple different SET events on the same key -> only last wins.
        observer.source_mut().set_table(
            meta(Some(&["speed"])),
            vec![
                raw("Ethernet0", PortOp::Set, &[("index", "1"), ("speed", "100000")]),
                raw("Ethernet0", PortOp::Set, &[("index", "1"), ("speed", "200000")]),
                raw("Ethernet0", PortOp::Set, &[("index", "1"), ("speed", "400000")]),
            ],
        );
        {
            let mut handler = |e: PortChangeEvent| dispatched.push(e);
            assert!(observer.handle_port_update_event(&mut handler, 1000));
        }
        assert_eq!(dispatched.last().unwrap().port_dict.as_ref().unwrap().get("speed").map(String::as_str), Some("400000"));

        // 5. Select timeout -> no event.
        observer.source_mut().push_select(SelectResult::Timeout);
        observer.source_mut().set_table(
            meta(Some(&["speed"])),
            vec![raw("Ethernet0", PortOp::Set, &[("index", "1"), ("speed", "999")])],
        );
        {
            let n = dispatched.len();
            let mut handler = |e: PortChangeEvent| dispatched.push(e);
            assert!(!observer.handle_port_update_event(&mut handler, 1000));
            drop(handler);
            assert_eq!(dispatched.len(), n);
        }

        // 6. DEL command dispatches a Del event.
        observer.source_mut().set_table(
            meta(Some(&["speed"])),
            vec![raw("Ethernet0", PortOp::Del, &[("index", "1"), ("speed", "400000")])],
        );
        {
            let mut handler = |e: PortChangeEvent| dispatched.push(e);
            assert!(observer.handle_port_update_event(&mut handler, 1000));
        }
        assert_eq!(dispatched.last().unwrap().event_type, PortChangeEventType::Del);

        // 7. Subset of the cached DEL event -> de-dup (no dispatch), cache shrinks.
        observer
            .source_mut()
            .set_table(meta(Some(&["speed"])), vec![raw("Ethernet0", PortOp::Del, &[("index", "1")])]);
        {
            let n = dispatched.len();
            let mut handler = |e: PortChangeEvent| dispatched.push(e);
            assert!(!observer.handle_port_update_event(&mut handler, 1000));
            drop(handler);
            assert_eq!(dispatched.len(), n);
        }
        let cached = observer
            .port_event_cache
            .get(&("Ethernet0".to_string(), "CONFIG_DB".to_string(), CFG_PORT_TABLE_NAME.to_string()))
            .unwrap();
        assert!(!cached.contains_key("speed"));
    }

    /// A stop event set before `handle_port_update_event` short-circuits it.
    #[test]
    fn handle_port_update_event_honors_stop_event() {
        let stop = Arc::new(StdAtomicBool::new(true));
        let mut observer = PortChangeObserver::new(MockPortEventSource::new(), stop);
        observer
            .source_mut()
            .set_table(meta(None), vec![raw("Ethernet0", PortOp::Set, &[("index", "1")])]);
        let mut dispatched = 0;
        let mut handler = |_e: PortChangeEvent| dispatched += 1;
        assert!(!observer.handle_port_update_event(&mut handler, 1000));
        assert_eq!(dispatched, 0);
    }

    /// <- test_handle_port_config_change: SET creates a PORT_ADD in the mapping,
    /// DEL removes it again.
    #[test]
    fn handle_port_config_change_add_then_del() {
        let stop = StdAtomicBool::new(false);
        let mut pm = PortMapping::new();
        let mut source = MockPortEventSource::new();
        source.set_table(meta(None), vec![raw("Ethernet0", PortOp::Set, &[("index", "1")])]);

        let mut handler = |pm: &mut PortMapping, e: PortChangeEvent| pm.handle_port_change_event(&e);
        handle_port_config_change(&mut source, &stop, &mut pm, &mut handler);

        assert!(pm.logical_port_list.contains(&"Ethernet0".to_string()));
        assert_eq!(pm.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(pm.get_physical_to_logical(1), Some(vec!["Ethernet0".to_string()]));
        assert_eq!(pm.get_logical_to_physical("Ethernet0"), Some(vec![1]));

        source.set_table(meta(None), vec![raw("Ethernet0", PortOp::Del, &[("index", "1")])]);
        handle_port_config_change(&mut source, &stop, &mut pm, &mut handler);
        assert!(pm.logical_port_list.is_empty());
        assert!(pm.logical_to_physical.is_empty());
        assert!(pm.physical_to_logical.is_empty());
        assert!(pm.logical_to_asic.is_empty());
    }

    /// A SET that changes an existing port's physical index removes then re-adds it.
    #[test]
    fn handle_port_config_change_reindex_remaps() {
        let stop = StdAtomicBool::new(false);
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortChangeEventType::Add));

        let mut source = MockPortEventSource::new();
        source.set_table(meta(None), vec![raw("Ethernet0", PortOp::Set, &[("index", "2")])]);
        let mut handler = |pm: &mut PortMapping, e: PortChangeEvent| pm.handle_port_change_event(&e);
        handle_port_config_change(&mut source, &stop, &mut pm, &mut handler);

        // Remapped: old physical index gone, new one present.
        assert_eq!(pm.get_logical_to_physical("Ethernet0"), Some(vec![2]));
        assert_eq!(pm.get_physical_to_logical(1), None);
        assert_eq!(pm.get_physical_to_logical(2), Some(vec!["Ethernet0".to_string()]));
    }
}
