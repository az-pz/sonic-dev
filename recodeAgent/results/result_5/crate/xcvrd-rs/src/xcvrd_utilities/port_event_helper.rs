#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `xcvrd_utilities/port_event_helper.py`: PortMapping, PortChangeEvent(+Observer), get_port_mapping.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;
use crate::hal::Sfp;
use crate::db::Table;

pub const SELECT_TIMEOUT_MSECS: i64 = 1000;

/// swss `Table` field carrying a port's role (`multi_asic.PORT_ROLE`).
pub const PORT_ROLE: &str = "role";
/// swss DB operation strings (`swsscommon.SET_COMMAND` / `DEL_COMMAND`).
pub const SET_COMMAND: &str = "SET";
pub const DEL_COMMAND: &str = "DEL";

// Front-panel-port classification (sonic_py_common `interface`/`multi_asic`). The
// front-panel prefix is `Ethernet`; the internal variants are excluded either by name
// prefix (backplane/inband/recirc), by a sub-interface `.`, or by an internal role.
const FRONT_PANEL_PREFIX: &str = "Ethernet";
const BACKPLANE_PREFIX: &str = "Ethernet-BP";
const INBAND_PREFIX: &str = "Ethernet-IB";
const RECIRC_PREFIX: &str = "Ethernet-Rec";
const EXTERNAL_PORT: &str = "Ext";

/// `multi_asic.is_role_internal(role)` — the internal role set (Internal/Inband/Recirc/
/// DPU-connect). An empty/`None` role is never internal.
fn is_role_internal(role: Option<&str>) -> bool {
    matches!(role, Some("Int") | Some("Inb") | Some("Rec") | Some("Dpc"))
}

/// `multi_asic.is_front_panel_port(port, role)` — true for a front-panel `Ethernet*`
/// logical port that is not a backplane/inband/recirc internal port, not a
/// sub-interface, and whose role (when set) is not an internal role.
pub fn is_front_panel_port(port: &str, role: Option<&str>) -> bool {
    if !port.starts_with(FRONT_PANEL_PREFIX) {
        return false;
    }
    if port.starts_with(BACKPLANE_PREFIX) || port.starts_with(INBAND_PREFIX) || port.starts_with(RECIRC_PREFIX) {
        return false;
    }
    if port.contains('.') {
        return false;
    }
    let role = role.filter(|r| !r.is_empty());
    !is_role_internal(role)
}

/// swss `Select` result state (`swsscommon.Select.{OBJECT,TIMEOUT}`), plus an `Error`
/// variant for the `!= OBJECT && != TIMEOUT` branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectState {
    Object,
    Timeout,
    Error,
}

/// One popped row from a `SubscriberStateTable`: `(key, op, field-values)`, the tuple
/// the Python `port_tbl.pop()` returns. An empty `key` marks "no more rows this cycle"
/// (the Python `if not port_name: break`).
#[derive(Debug, Clone)]
pub struct PortPop {
    pub key: String,
    pub op: String,
    pub fvp: Vec<(String, String)>,
}

impl PortPop {
    pub fn new(key: impl Into<String>, op: impl Into<String>, fvp: Vec<(String, String)>) -> Self {
        PortPop { key: key.into(), op: op.into(), fvp }
    }
}

/// The event source behind [`PortChangeObserver`] / [`handle_port_config_change`] — the
/// analogue of a swss `Select` over one `SubscriberStateTable` per subscribed table. Unit
/// tests inject a scripted source (mirroring `mock_select.select` + `mock_selectable.pop`);
/// the deployed daemon wraps `swsscommon.Select`/`SubscriberStateTable`.
pub trait PortEventSource {
    /// `Select.select(timeout)`.
    fn select(&self, timeout_msecs: i64) -> SelectState;
    /// Drain the next `(key, op, fvp)` for subscribed table `table_index` this cycle
    /// (`None`/empty-key ends the drain).
    fn pop(&self, table_index: usize) -> Option<PortPop>;
}

/// Port change event kind (`PortChangeEvent.PORT_ADD/REMOVE/SET/DEL` in Python).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortEventType {
    PortAdd,
    PortRemove,
    PortSet,
    PortDel,
}

/// Rust port of the Python `PortChangeEvent` — a CONFIG_DB PORT table delta.
#[derive(Debug, Clone)]
pub struct PortChangeEvent {
    /// Logical port name, e.g. `Ethernet0`.
    pub port_name: String,
    /// Physical port index (the PORT table `index` field in CONFIG_DB).
    pub port_index: i32,
    /// ASIC ID (for multi-ASIC).
    pub asic_id: i32,
    /// Event kind.
    pub event_type: PortEventType,
    /// Optional port config dict (SET/DEL events).
    pub port_dict: Option<Vec<(String, String)>>,
    pub db_name: Option<String>,
    pub table_name: Option<String>,
}

impl PortChangeEvent {
    pub fn new(port_name: impl Into<String>, port_index: i32, asic_id: i32, event_type: PortEventType) -> Self {
        PortChangeEvent {
            port_name: port_name.into(),
            port_index,
            asic_id,
            event_type,
            port_dict: None,
            db_name: None,
            table_name: None,
        }
    }

    /// Full constructor for observer `SET`/`DEL` events carrying the soaked+filtered
    /// field-values (`port_dict`) and the originating `(db, table)`.
    pub fn new_full(
        port_name: impl Into<String>,
        port_index: i32,
        asic_id: i32,
        event_type: PortEventType,
        port_dict: BTreeMap<String, String>,
        db_name: impl Into<String>,
        table_name: impl Into<String>,
    ) -> Self {
        PortChangeEvent {
            port_name: port_name.into(),
            port_index,
            asic_id,
            event_type,
            port_dict: Some(port_dict.into_iter().collect()),
            db_name: Some(db_name.into()),
            table_name: Some(table_name.into()),
        }
    }

    /// The `port_dict` as an order-independent map for comparison/lookup.
    pub fn port_dict_map(&self) -> BTreeMap<String, String> {
        self.port_dict
            .as_ref()
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Mirrors Python `__str__`, e.g. `Add - name=Ethernet0 index=1 asic_id=0`.
    pub fn to_string_repr(&self) -> String {
        let kind = match self.event_type {
            PortEventType::PortAdd => "Add",
            PortEventType::PortRemove => "Remove",
            PortEventType::PortSet => "Set",
            PortEventType::PortDel => "Delete",
        };
        format!("{} - name={} index={} asic_id={}", kind, self.port_name, self.port_index, self.asic_id)
    }
}

/// Rust port of the Python `PortMapping`: logical↔physical↔asic bookkeeping.
#[derive(Default, Clone)]
pub struct PortMapping {
    /// Logical port names, e.g. `["Ethernet0", "Ethernet4"]`.
    pub logical_port_list: Vec<String>,
    /// Logical port name → physical port index.
    pub logical_to_physical: std::collections::HashMap<String, i32>,
    /// Physical port index → naturally-sorted list of logical port names.
    pub physical_to_logical: std::collections::HashMap<i32, Vec<String>>,
    /// Logical port name → ASIC ID.
    pub logical_to_asic: std::collections::HashMap<String, i32>,
}

impl PortMapping {
    pub fn new() -> Self { PortMapping::default() }

    pub fn handle_port_change_event(&mut self, ev: &PortChangeEvent) {
        match ev.event_type {
            PortEventType::PortAdd => self.handle_port_add(ev),
            PortEventType::PortRemove => self.handle_port_remove(ev),
            _ => {}
        }
    }

    fn handle_port_add(&mut self, ev: &PortChangeEvent) {
        let port_name = ev.port_name.clone();
        self.logical_port_list.push(port_name.clone());
        self.logical_to_physical.insert(port_name.clone(), ev.port_index);
        let bucket = self.physical_to_logical.entry(ev.port_index).or_default();
        bucket.push(port_name.clone());
        if bucket.len() > 1 {
            natsort_lower(bucket);
        }
        self.logical_to_asic.insert(port_name, ev.asic_id);
    }

    fn handle_port_remove(&mut self, ev: &PortChangeEvent) {
        let port_name = &ev.port_name;
        self.logical_port_list.retain(|p| p != port_name);
        self.logical_to_physical.remove(port_name);
        if let Some(bucket) = self.physical_to_logical.get_mut(&ev.port_index) {
            bucket.retain(|p| p != port_name);
            if bucket.is_empty() {
                self.physical_to_logical.remove(&ev.port_index);
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

    pub fn get_logical_to_physical(&self, port_name: &str) -> Option<Vec<i32>> {
        self.logical_to_physical.get(port_name).map(|&i| vec![i])
    }

    pub fn get_physical_to_logical(&self, physical_port: i32) -> Option<Vec<String>> {
        self.physical_to_logical.get(&physical_port).cloned()
    }

    /// `int(port_name)` first (a bare physical index), else logical→physical lookup.
    pub fn logical_port_name_to_physical_port_list(&self, port_name: &str) -> Option<Vec<i32>> {
        match port_name.parse::<i32>() {
            Ok(n) => Some(vec![n]),
            Err(_) => {
                if self.is_logical_port(port_name) {
                    self.get_logical_to_physical(port_name)
                } else {
                    None
                }
            }
        }
    }
}

/// Natural sort (case-insensitive), the analogue of `natsorted(l, key=str.lower)`.
fn natsort_lower(items: &mut [String]) {
    items.sort_by(|a, b| natsort_key(&a.to_lowercase()).cmp(&natsort_key(&b.to_lowercase())));
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum NatChunk {
    Num(u64),
    Text(String),
}

fn natsort_key(s: &str) -> Vec<NatChunk> {
    let mut chunks = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut num = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() { num.push(d); chars.next(); } else { break; }
            }
            chunks.push(NatChunk::Num(num.parse().unwrap_or(0)));
        } else {
            let mut text = String::new();
            while let Some(&d) = chars.peek() {
                if !d.is_ascii_digit() { text.push(d); chars.next(); } else { break; }
            }
            chunks.push(NatChunk::Text(text));
        }
    }
    chunks
}

/// A subscribed swss table for [`PortChangeObserver`]: its `(db, table)` identity, the
/// optional field-`filter`, and the owning ASIC id. The observer keeps these in the
/// order they were subscribed, matching the Python `self.selectables` iteration order.
#[derive(Debug, Clone)]
pub struct SubscribedTable {
    pub db_name: String,
    pub table_name: String,
    pub filter: Option<Vec<String>>,
    pub asic_id: i32,
}

impl SubscribedTable {
    pub fn new(
        db_name: impl Into<String>,
        table_name: impl Into<String>,
        filter: Option<Vec<String>>,
        asic_id: i32,
    ) -> Self {
        SubscribedTable {
            db_name: db_name.into(),
            table_name: table_name.into(),
            filter,
            asic_id,
        }
    }
}

/// The observer's `(port, db, table)` cache key.
type CacheKey = (String, String, String);
/// One cached row's field-values (order-independent, like a Python dict).
type Fvp = BTreeMap<String, String>;

/// An insertion-ordered `CacheKey → Fvp` map, mirroring Python dict semantics:
/// re-assigning an existing key keeps its original position, and `keys()` yields
/// insertion order (asserted by `test_handle_front_panel_filter`).
#[derive(Default)]
pub struct OrderedFvpMap {
    entries: Vec<(CacheKey, Fvp)>,
}

impl OrderedFvpMap {
    pub fn new() -> Self {
        OrderedFvpMap::default()
    }

    pub fn get(&self, k: &CacheKey) -> Option<&Fvp> {
        self.entries.iter().find(|(ek, _)| ek == k).map(|(_, v)| v)
    }

    pub fn insert(&mut self, k: CacheKey, v: Fvp) {
        if let Some(e) = self.entries.iter_mut().find(|(ek, _)| *ek == k) {
            e.1 = v;
        } else {
            self.entries.push((k, v));
        }
    }

    pub fn keys(&self) -> Vec<CacheKey> {
        self.entries.iter().map(|(k, _)| k.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// `PortChangeObserver.apply_filter_to_fvp(filter, fvp)` — when a `filter` is set, drop
/// every field that is neither in the filter nor one of the always-kept
/// `index/port_name/asic_id/op` fields.
fn apply_filter_to_fvp(filter: Option<&[String]>, fvp: &mut Fvp) {
    if let Some(f) = filter {
        let mut keep: HashSet<&str> = ["index", "port_name", "asic_id", "op"].into_iter().collect();
        for k in f {
            keep.insert(k.as_str());
        }
        fvp.retain(|k, _| keep.contains(k.as_str()));
    }
}

/// Rust port of the Python `PortChangeObserver`.
///
/// Holds the subscribed tables (in order), the persistent `port_event_cache`
/// (insertion-ordered), and the `port_role_map` (logical port → role) used for the
/// front-panel filter. The swss `Select`/`SubscriberStateTable` plumbing is behind the
/// [`PortEventSource`] seam so the daemon drives real swss while unit tests inject a
/// scripted source.
pub struct PortChangeObserver {
    pub subscribed: Vec<SubscribedTable>,
    pub port_event_cache: OrderedFvpMap,
    pub port_role_map: HashMap<String, String>,
}

impl PortChangeObserver {
    /// `PortChangeObserver.__init__` — build the subscribed-table list, seed the role map
    /// from the given CONFIG_DB PORT tables (`refresh_role_map`), and start with an empty
    /// event cache. (`subscribe_port_update_event` is folded into the source seam.)
    pub fn new(subscribed: Vec<SubscribedTable>, role_tables: &[&dyn Table]) -> Self {
        let mut observer = PortChangeObserver {
            subscribed,
            port_event_cache: OrderedFvpMap::new(),
            port_role_map: HashMap::new(),
        };
        observer.refresh_role_map(role_tables);
        observer
    }

    /// `PortChangeObserver.refresh_role_map()` — (re)load each front-panel logical port's
    /// role from the CONFIG_DB PORT table(s). Ports without a (non-empty) role are omitted.
    pub fn refresh_role_map(&mut self, role_tables: &[&dyn Table]) {
        self.port_role_map.clear();
        for tbl in role_tables {
            let keys = tbl.get_keys().unwrap_or_default();
            for key in keys {
                if let Ok(Some(fvs)) = tbl.get(&key) {
                    let dict: HashMap<String, String> = fvs.into_iter().collect();
                    if let Some(role) = dict.get(PORT_ROLE) {
                        if !role.is_empty() {
                            self.port_role_map.insert(key.clone(), role.clone());
                        }
                    }
                }
            }
        }
    }

    /// `PortChangeObserver.handle_port_update_event(...)`.
    ///
    /// One `select()`; on `OBJECT`, drain every subscribed table into a local
    /// insertion-ordered cache (soaking multiple rows per key, updating the role map from
    /// any role field, dropping non-front-panel ports), then apply each table's field
    /// filter, diff against the persistent `port_event_cache` (a subset ⇒ duplicate ⇒
    /// skip) and emit a `SET`/`DEL` [`PortChangeEvent`] for every real delta. Returns
    /// whether any event was emitted.
    pub fn handle_port_update_event(
        &mut self,
        source: &dyn PortEventSource,
        stop_requested: bool,
        timeout_msecs: i64,
        handler: &mut dyn FnMut(&PortChangeEvent),
    ) -> bool {
        let mut has_event = false;
        if stop_requested {
            return has_event;
        }
        match source.select(timeout_msecs) {
            SelectState::Timeout => return has_event,
            SelectState::Error => return has_event,
            SelectState::Object => {}
        }

        // Soak every pending row per subscribed table into a local, insertion-ordered
        // cache. Each entry carries its filter so it can be applied after soaking.
        let mut soaked: Vec<(CacheKey, Fvp, Option<Vec<String>>, i32)> = Vec::new();
        for (table_index, st) in self.subscribed.iter().enumerate() {
            loop {
                let pop = match source.pop(table_index) {
                    Some(p) => p,
                    None => break,
                };
                if pop.key.is_empty() {
                    break;
                }
                let mut fvp: Fvp = pop.fvp.iter().cloned().collect();
                // Role handling: a non-empty role field updates the role map; otherwise
                // fall back to the last known role for this port.
                let role = match fvp.get(PORT_ROLE) {
                    Some(r) if !r.is_empty() => {
                        self.port_role_map.insert(pop.key.clone(), r.clone());
                        Some(r.clone())
                    }
                    _ => self.port_role_map.get(&pop.key).cloned(),
                };
                if !is_front_panel_port(&pop.key, role.as_deref()) {
                    continue;
                }
                fvp.entry("index".to_string()).or_insert_with(|| "-1".to_string());
                fvp.insert("port_name".to_string(), pop.key.clone());
                fvp.insert("asic_id".to_string(), st.asic_id.to_string());
                fvp.insert("op".to_string(), pop.op.clone());
                let key = (pop.key.clone(), st.db_name.clone(), st.table_name.clone());
                // Update-in-place to preserve first-insertion order (Python dict semantics).
                if let Some(existing) = soaked.iter_mut().find(|(ek, _, _, _)| *ek == key) {
                    existing.1 = fvp;
                } else {
                    soaked.push((key, fvp, st.filter.clone(), st.asic_id));
                }
            }
        }

        for (key, mut fvp, filter, asic_id) in soaked.into_iter() {
            let port_index: i32 = fvp.get("index").and_then(|s| s.parse().ok()).unwrap_or(-1);
            apply_filter_to_fvp(filter.as_deref(), &mut fvp);

            // Diff against the cached event: a subset (no new (k,v)) is a duplicate.
            if let Some(prev) = self.port_event_cache.get(&key) {
                let has_diff = fvp.iter().any(|(k, v)| prev.get(k) != Some(v));
                if !has_diff {
                    self.port_event_cache.insert(key.clone(), fvp);
                    continue;
                }
            }
            self.port_event_cache.insert(key.clone(), fvp.clone());

            let op = fvp.get("op").cloned().unwrap_or_default();
            let (port_name, db_name, table_name) = key;
            let event_type = if op == SET_COMMAND {
                Some(PortEventType::PortSet)
            } else if op == DEL_COMMAND {
                Some(PortEventType::PortDel)
            } else {
                None
            };
            if let Some(et) = event_type {
                let ev = PortChangeEvent::new_full(
                    port_name, port_index, asic_id, et, fvp, db_name, table_name,
                );
                has_event = true;
                handler(&ev);
            }
        }
        has_event
    }
}

/// `get_port_mapping(port_mapping_tbl_list, asic_id_list)` — build a [`PortMapping`] from
/// the CONFIG_DB PORT table(s), including only front-panel ports (by name+role).
pub fn get_port_mapping(port_tables: &[(&dyn Table, i32)]) -> PortMapping {
    let mut port_mapping = PortMapping::new();
    for (tbl, asic_id) in port_tables {
        let keys = tbl.get_keys().unwrap_or_default();
        for key in keys {
            let fvs = match tbl.get(&key) {
                Ok(Some(f)) => f,
                _ => continue,
            };
            let dict: HashMap<String, String> = fvs.into_iter().collect();
            if !is_front_panel_port(&key, dict.get(PORT_ROLE).map(String::as_str)) {
                continue;
            }
            let index = match dict.get("index").and_then(|s| s.parse::<i32>().ok()) {
                Some(i) => i,
                None => continue,
            };
            port_mapping.handle_port_change_event(&PortChangeEvent::new(
                key,
                index,
                *asic_id,
                PortEventType::PortAdd,
            ));
        }
    }
    port_mapping
}

/// `subscribe_port_config_change(namespaces)` — return the per-table ASIC id list
/// (`asic_context`) the daemon iterates. Single-ASIC (the default namespace) has one
/// table for ASIC 0. The actual swss `Select`/`SubscriberStateTable` is built by the
/// daemon's [`PortEventSource`] implementation.
pub fn subscribe_port_config_change() -> Vec<i32> {
    vec![0]
}

/// `handle_port_config_change(sel, asic_context, stop_event, port_mapping, logger,
/// port_change_event_handler)` — one `select()`, and on `OBJECT` drain the subscribed
/// tables via [`read_port_config_change`], applying each resulting event to `port_mapping`
/// through `on_event` (in the daemon/tests the handler is `handle_port_change_event`).
pub fn handle_port_config_change(
    source: &dyn PortEventSource,
    asic_ids: &[i32],
    stop_requested: bool,
    timeout_msecs: i64,
    port_mapping: &mut PortMapping,
    on_event: &mut dyn FnMut(&mut PortMapping, &PortChangeEvent),
) {
    if stop_requested {
        return;
    }
    match source.select(timeout_msecs) {
        SelectState::Timeout => return,
        SelectState::Error => return,
        SelectState::Object => {}
    }
    read_port_config_change(source, asic_ids, port_mapping, on_event);
}

/// `read_port_config_change(asic_context, port_mapping, logger,
/// port_change_event_handler, ...)` — drain each subscribed table and translate each row
/// into add/remove events against the *current* `port_mapping`:
/// * `SET` of a new port → `PortAdd`.
/// * `SET` of an existing port whose physical index changed → `PortRemove(old)` then
///   `PortAdd(new)`.
/// * `DEL` of a known port → `PortRemove`.
/// Non-front-panel rows are ignored. Events are applied immediately so later rows in the
/// same drain observe the updated mapping (faithful to the Python handler mutating
/// `port_mapping`).
pub fn read_port_config_change(
    source: &dyn PortEventSource,
    asic_ids: &[i32],
    port_mapping: &mut PortMapping,
    on_event: &mut dyn FnMut(&mut PortMapping, &PortChangeEvent),
) {
    for table_index in 0..asic_ids.len().max(1) {
        let asic_id = asic_ids.get(table_index).copied().unwrap_or(0);
        loop {
            let pop = match source.pop(table_index) {
                Some(p) => p,
                None => break,
            };
            if pop.key.is_empty() {
                break;
            }
            let dict: HashMap<String, String> = pop.fvp.iter().cloned().collect();
            if !is_front_panel_port(&pop.key, dict.get(PORT_ROLE).map(String::as_str)) {
                continue;
            }
            // Compute events reading the current mapping, then apply them.
            let mut events: Vec<PortChangeEvent> = Vec::new();
            if pop.op == SET_COMMAND {
                let new_index = match dict.get("index").and_then(|s| s.parse::<i32>().ok()) {
                    Some(i) => i,
                    None => continue,
                };
                if !port_mapping.is_logical_port(&pop.key) {
                    events.push(PortChangeEvent::new(
                        pop.key.clone(),
                        new_index,
                        asic_id,
                        PortEventType::PortAdd,
                    ));
                } else {
                    let current = port_mapping
                        .get_logical_to_physical(&pop.key)
                        .and_then(|v| v.first().copied());
                    if current != Some(new_index) {
                        if let Some(cur) = current {
                            events.push(PortChangeEvent::new(
                                pop.key.clone(),
                                cur,
                                asic_id,
                                PortEventType::PortRemove,
                            ));
                        }
                        events.push(PortChangeEvent::new(
                            pop.key.clone(),
                            new_index,
                            asic_id,
                            PortEventType::PortAdd,
                        ));
                    }
                }
            } else if pop.op == DEL_COMMAND && port_mapping.is_logical_port(&pop.key) {
                let cur = port_mapping
                    .get_logical_to_physical(&pop.key)
                    .and_then(|v| v.first().copied())
                    .unwrap_or(0);
                events.push(PortChangeEvent::new(
                    pop.key.clone(),
                    cur,
                    asic_id,
                    PortEventType::PortRemove,
                ));
            }
            for ev in &events {
                on_event(port_mapping, ev);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockTable;

    #[test]
    fn port_mapping_add_remove() {
        let mut pm = PortMapping::new();
        let add = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd);
        pm.handle_port_change_event(&add);
        assert_eq!(pm.logical_port_list, vec!["Ethernet0".to_string()]);
        assert_eq!(pm.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(pm.get_physical_to_logical(1), Some(vec!["Ethernet0".to_string()]));
        assert_eq!(pm.get_logical_to_physical("Ethernet0"), Some(vec![1]));
        assert!(pm.is_logical_port("Ethernet0"));

        let rm = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortRemove);
        pm.handle_port_change_event(&rm);
        assert!(pm.logical_port_list.is_empty());
        assert!(pm.logical_to_physical.is_empty());
        assert!(pm.physical_to_logical.is_empty());
        assert!(pm.logical_to_asic.is_empty());
    }

    #[test]
    fn logical_port_name_to_physical_port_list_variants() {
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd));
        // Bare integer name → that physical index.
        assert_eq!(pm.logical_port_name_to_physical_port_list("7"), Some(vec![7]));
        // Known logical name → its physical index.
        assert_eq!(pm.logical_port_name_to_physical_port_list("Ethernet0"), Some(vec![1]));
        // Unknown logical name → None.
        assert_eq!(pm.logical_port_name_to_physical_port_list("Ethernet99"), None);
    }

    #[test]
    fn physical_to_logical_is_natsorted() {
        let mut pm = PortMapping::new();
        // Two logical ports on one physical index arrive out of natural order.
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet10", 0, 0, PortEventType::PortAdd));
        pm.handle_port_change_event(&PortChangeEvent::new("Ethernet2", 0, 0, PortEventType::PortAdd));
        assert_eq!(
            pm.get_physical_to_logical(0),
            Some(vec!["Ethernet2".to_string(), "Ethernet10".to_string()])
        );
    }

    #[test]
    fn port_change_event_to_string_repr() {
        let ev = PortChangeEvent::new("Ethernet0", 1, 0, PortEventType::PortAdd);
        assert_eq!(ev.to_string_repr(), "Add - name=Ethernet0 index=1 asic_id=0");
    }

    // ---- Mock PortEventSource for the observer/config-change tests ----

    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    struct MockPortSource {
        state: Cell<SelectState>,
        scripts: RefCell<Vec<VecDeque<PortPop>>>,
    }

    impl MockPortSource {
        fn new(num_tables: usize) -> Self {
            MockPortSource {
                state: Cell::new(SelectState::Object),
                scripts: RefCell::new(vec![VecDeque::new(); num_tables]),
            }
        }
        fn set_state(&self, s: SelectState) {
            self.state.set(s);
        }
        fn load(&self, table_index: usize, pops: Vec<PortPop>) {
            self.scripts.borrow_mut()[table_index] = pops.into_iter().collect();
        }
    }

    impl PortEventSource for MockPortSource {
        fn select(&self, _timeout_msecs: i64) -> SelectState {
            self.state.get()
        }
        fn pop(&self, table_index: usize) -> Option<PortPop> {
            self.scripts.borrow_mut()[table_index].pop_front()
        }
    }

    fn pop(key: &str, op: &str, fvp: &[(&str, &str)]) -> PortPop {
        PortPop::new(
            key,
            op,
            fvp.iter().map(|(f, v)| (f.to_string(), v.to_string())).collect(),
        )
    }

    fn port_table_with(rows: &[(&str, &[(&str, &str)])]) -> MockTable {
        let tbl = MockTable::new();
        for (key, fvs) in rows {
            let fvs: Vec<(String, String)> =
                fvs.iter().map(|(f, v)| (f.to_string(), v.to_string())).collect();
            tbl.set(key, &fvs).unwrap();
        }
        tbl
    }

    #[test]
    fn test_is_front_panel_port() {
        assert!(is_front_panel_port("Ethernet0", None));
        assert!(is_front_panel_port("Ethernet4", None));
        assert!(is_front_panel_port("Ethernet16", Some("Ext")));
        assert!(!is_front_panel_port("Ethernet-IB0", None));
        assert!(!is_front_panel_port("Ethernet-BP0", None));
        assert!(!is_front_panel_port("Ethernet-Rec0", None));
        assert!(!is_front_panel_port("Ethernet8", Some("Dpc")));
        assert!(!is_front_panel_port("Ethernet0.10", None));
        assert!(!is_front_panel_port("PortChannel0", None));
    }

    // Port of test_get_port_mapping: only front-panel ports (by name+role) are mapped.
    #[test]
    fn test_get_port_mapping() {
        let tbl = port_table_with(&[
            ("Ethernet0", &[("index", "1")]),
            ("Ethernet4", &[("index", "2")]),
            ("Ethernet-IB0", &[("index", "3")]),
            ("Ethernet8", &[("index", "4"), ("role", "Dpc")]),
        ]);
        let pm = get_port_mapping(&[(&tbl as &dyn Table, 0)]);

        assert!(pm.logical_port_list.contains(&"Ethernet0".to_string()));
        assert_eq!(pm.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(pm.get_physical_to_logical(1), Some(vec!["Ethernet0".to_string()]));
        assert_eq!(pm.get_logical_to_physical("Ethernet0"), Some(vec![1]));

        assert!(pm.logical_port_list.contains(&"Ethernet4".to_string()));
        assert_eq!(pm.get_logical_to_physical("Ethernet4"), Some(vec![2]));

        // Inband port excluded by name; DPU-connect port excluded by role.
        assert!(!pm.logical_port_list.contains(&"Ethernet-IB0".to_string()));
        assert_eq!(pm.get_asic_id_for_logical_port("Ethernet-IB0"), None);
        assert_eq!(pm.get_physical_to_logical(3), None);
        assert!(!pm.logical_port_list.contains(&"Ethernet8".to_string()));
        assert_eq!(pm.get_asic_id_for_logical_port("Ethernet8"), None);
        assert_eq!(pm.get_physical_to_logical(4), None);
    }

    // Port of test_handle_front_panel_filter: role map seeded from CONFIG_DB; the
    // front-panel filter drops the internal (role='Dpc') port, keeping Ethernet0/16 in
    // insertion order.
    #[test]
    fn test_handle_front_panel_filter() {
        let role_tbl = port_table_with(&[
            ("Ethernet0", &[("index", "1")]),
            ("Ethernet8", &[("index", "2"), ("role", "Dpc")]),
            ("Ethernet16", &[("index", "3"), ("role", "Ext")]),
        ]);
        let subscribed = vec![SubscribedTable::new("CONFIG_DB", "PORT", None, 0)];
        let mut observer = PortChangeObserver::new(subscribed, &[&role_tbl as &dyn Table]);

        assert_eq!(observer.port_role_map.get("Ethernet8").map(String::as_str), Some("Dpc"));
        assert_eq!(observer.port_role_map.get("Ethernet16").map(String::as_str), Some("Ext"));
        assert!(!observer.port_role_map.contains_key("Ethernet0"));

        let source = MockPortSource::new(1);
        source.load(
            0,
            vec![
                pop("Ethernet0", SET_COMMAND, &[("index", "1"), ("speed", "40000")]),
                pop("Ethernet8", SET_COMMAND, &[("index", "2"), ("speed", "80000"), ("role", "Dpc")]),
                pop("Ethernet16", SET_COMMAND, &[("index", "3"), ("speed", "80000"), ("role", "Ext")]),
            ],
        );

        let mut emitted: Vec<String> = Vec::new();
        let mut handler = |ev: &PortChangeEvent| emitted.push(ev.port_name.clone());
        assert!(observer.handle_port_update_event(&source, false, SELECT_TIMEOUT_MSECS, &mut handler));
        assert_eq!(emitted.len(), 2);
        assert_eq!(
            observer.port_event_cache.keys(),
            vec![
                ("Ethernet0".to_string(), "CONFIG_DB".to_string(), "PORT".to_string()),
                ("Ethernet16".to_string(), "CONFIG_DB".to_string(), "PORT".to_string()),
            ]
        );
    }

    // Port of test_handle_port_update_event: no-filter keeps all fields; FILTER=['speed']
    // drops 'fec'; duplicate (subset) events are ignored; multiple rows on one key soak to
    // the last; TIMEOUT yields no event; DEL is emitted then its subset ignored.
    #[test]
    fn test_handle_port_update_event() {
        let key = ("Ethernet0".to_string(), "CONFIG_DB".to_string(), "PORT".to_string());

        // --- No filter: 'fec' is retained. ---
        let subscribed = vec![SubscribedTable::new("CONFIG_DB", "PORT", None, 0)];
        let mut observer = PortChangeObserver::new(subscribed, &[]);
        let source = MockPortSource::new(1);
        source.load(
            0,
            vec![pop("Ethernet0", SET_COMMAND, &[("index", "1"), ("speed", "40000"), ("fec", "rs")])],
        );
        let mut count = 0usize;
        {
            let mut handler = |_ev: &PortChangeEvent| count += 1;
            assert!(observer.handle_port_update_event(&source, false, 1000, &mut handler));
        }
        assert_eq!(count, 1);
        let expected: Fvp = [
            ("port_name", "Ethernet0"),
            ("index", "1"),
            ("op", SET_COMMAND),
            ("asic_id", "0"),
            ("speed", "40000"),
            ("fec", "rs"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(observer.port_event_cache.get(&key), Some(&expected));

        // --- FILTER=['speed']: 'fec' is dropped; event fields propagate to the handler. ---
        let subscribed = vec![SubscribedTable::new("CONFIG_DB", "PORT", Some(vec!["speed".to_string()]), 0)];
        let mut observer = PortChangeObserver::new(subscribed, &[]);
        let source = MockPortSource::new(1);
        source.load(
            0,
            vec![pop("Ethernet0", SET_COMMAND, &[("index", "1"), ("speed", "40000"), ("fec", "rs")])],
        );
        let mut last: Option<PortChangeEvent> = None;
        {
            let mut handler = |ev: &PortChangeEvent| last = Some(ev.clone());
            assert!(observer.handle_port_update_event(&source, false, 1000, &mut handler));
        }
        let expected: Fvp = [
            ("port_name", "Ethernet0"),
            ("index", "1"),
            ("op", SET_COMMAND),
            ("asic_id", "0"),
            ("speed", "40000"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(observer.port_event_cache.get(&key), Some(&expected));
        let last = last.unwrap();
        assert_eq!(last.port_name, "Ethernet0");
        assert_eq!(last.event_type, PortEventType::PortSet);
        assert_eq!(last.port_index, 1);
        assert_eq!(last.asic_id, 0);
        assert_eq!(last.db_name.as_deref(), Some("CONFIG_DB"));
        assert_eq!(last.table_name.as_deref(), Some("PORT"));
        assert_eq!(last.port_dict_map(), expected);

        // --- Duplicate event on same key: no new event, cache unchanged. ---
        let source = MockPortSource::new(1);
        source.load(
            0,
            vec![pop("Ethernet0", SET_COMMAND, &[("index", "1"), ("speed", "40000"), ("fec", "rs")])],
        );
        {
            let mut handler = |_ev: &PortChangeEvent| panic!("duplicate should not emit");
            assert!(!observer.handle_port_update_event(&source, false, 1000, &mut handler));
        }
        assert_eq!(observer.port_event_cache.get(&key), Some(&expected));

        // --- Soaking multiple rows on one key: only the last (speed=400000) is processed. ---
        let source = MockPortSource::new(1);
        source.load(
            0,
            vec![
                pop("Ethernet0", SET_COMMAND, &[("index", "1"), ("speed", "100000")]),
                pop("Ethernet0", SET_COMMAND, &[("index", "1"), ("speed", "200000")]),
                pop("Ethernet0", SET_COMMAND, &[("index", "1"), ("speed", "400000")]),
            ],
        );
        let mut count = 0usize;
        {
            let mut handler = |_ev: &PortChangeEvent| count += 1;
            assert!(observer.handle_port_update_event(&source, false, 1000, &mut handler));
        }
        assert_eq!(count, 1);
        let expected_soak: Fvp = [
            ("port_name", "Ethernet0"),
            ("index", "1"),
            ("op", SET_COMMAND),
            ("asic_id", "0"),
            ("speed", "400000"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(observer.port_event_cache.get(&key), Some(&expected_soak));

        // --- TIMEOUT: no event. ---
        let source = MockPortSource::new(1);
        source.set_state(SelectState::Timeout);
        {
            let mut handler = |_ev: &PortChangeEvent| panic!("timeout should not emit");
            assert!(!observer.handle_port_update_event(&source, false, 1000, &mut handler));
        }

        // --- DEL event, then its subset (index only) is ignored. ---
        let source = MockPortSource::new(1);
        source.load(0, vec![pop("Ethernet0", DEL_COMMAND, &[("index", "1"), ("speed", "400000")])]);
        {
            let mut handler = |ev: &PortChangeEvent| assert_eq!(ev.event_type, PortEventType::PortDel);
            assert!(observer.handle_port_update_event(&source, false, 1000, &mut handler));
        }
        let expected_del: Fvp = [
            ("port_name", "Ethernet0"),
            ("index", "1"),
            ("op", DEL_COMMAND),
            ("asic_id", "0"),
            ("speed", "400000"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(observer.port_event_cache.get(&key), Some(&expected_del));

        let source = MockPortSource::new(1);
        source.load(0, vec![pop("Ethernet0", DEL_COMMAND, &[("index", "1")])]);
        {
            let mut handler = |_ev: &PortChangeEvent| panic!("subset should not emit");
            assert!(!observer.handle_port_update_event(&source, false, 1000, &mut handler));
        }
        let expected_subset: Fvp = [
            ("port_name", "Ethernet0"),
            ("index", "1"),
            ("op", DEL_COMMAND),
            ("asic_id", "0"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(observer.port_event_cache.get(&key), Some(&expected_subset));
    }

    // Port of test_handle_port_config_change: a SET adds Ethernet0 to the mapping; a
    // subsequent DEL removes it (via handle_port_change_event as the on_event handler).
    #[test]
    fn test_handle_port_config_change() {
        let asic_ids = subscribe_port_config_change();
        let mut port_mapping = PortMapping::new();
        let mut on_event =
            |pm: &mut PortMapping, ev: &PortChangeEvent| pm.handle_port_change_event(ev);

        let source = MockPortSource::new(1);
        source.load(0, vec![pop("Ethernet0", SET_COMMAND, &[("index", "1")])]);
        handle_port_config_change(&source, &asic_ids, false, 1000, &mut port_mapping, &mut on_event);
        assert!(port_mapping.logical_port_list.contains(&"Ethernet0".to_string()));
        assert_eq!(port_mapping.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(port_mapping.get_physical_to_logical(1), Some(vec!["Ethernet0".to_string()]));
        assert_eq!(port_mapping.get_logical_to_physical("Ethernet0"), Some(vec![1]));

        let source = MockPortSource::new(1);
        source.load(0, vec![pop("Ethernet0", DEL_COMMAND, &[("index", "1")])]);
        handle_port_config_change(&source, &asic_ids, false, 1000, &mut port_mapping, &mut on_event);
        assert!(port_mapping.logical_port_list.is_empty());
        assert!(port_mapping.logical_to_physical.is_empty());
        assert!(port_mapping.physical_to_logical.is_empty());
        assert!(port_mapping.logical_to_asic.is_empty());
    }

    // a port whose physical index is reassigned via SET emits Remove(old)
    // then Add(new), so the mapping tracks the move without cross-talk.
    #[test]
    fn port_config_change_updates_mapping() {
        let asic_ids = subscribe_port_config_change();
        let mut port_mapping = PortMapping::new();
        let mut on_event =
            |pm: &mut PortMapping, ev: &PortChangeEvent| pm.handle_port_change_event(ev);

        let source = MockPortSource::new(1);
        source.load(0, vec![pop("Ethernet0", SET_COMMAND, &[("index", "1")])]);
        handle_port_config_change(&source, &asic_ids, false, 1000, &mut port_mapping, &mut on_event);
        assert_eq!(port_mapping.get_logical_to_physical("Ethernet0"), Some(vec![1]));

        // Reassign Ethernet0 from physical 1 -> 5.
        let source = MockPortSource::new(1);
        source.load(0, vec![pop("Ethernet0", SET_COMMAND, &[("index", "5")])]);
        handle_port_config_change(&source, &asic_ids, false, 1000, &mut port_mapping, &mut on_event);
        assert_eq!(port_mapping.get_logical_to_physical("Ethernet0"), Some(vec![5]));
        assert_eq!(port_mapping.get_physical_to_logical(1), None);
        assert_eq!(port_mapping.get_physical_to_logical(5), Some(vec!["Ethernet0".to_string()]));
        // A repeated SET at the same index is a no-op (no duplicate add).
        let source = MockPortSource::new(1);
        source.load(0, vec![pop("Ethernet0", SET_COMMAND, &[("index", "5")])]);
        handle_port_config_change(&source, &asic_ids, false, 1000, &mut port_mapping, &mut on_event);
        assert_eq!(port_mapping.logical_port_list.iter().filter(|p| *p == "Ethernet0").count(), 1);
    }
}
