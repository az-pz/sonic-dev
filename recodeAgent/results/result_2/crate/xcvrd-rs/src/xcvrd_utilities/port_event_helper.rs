//! Port of `xcvrd_utilities/port_event_helper.py` — logical<->physical port mapping
//! plus the CONFIG_DB/STATE_DB change observer that drives `PortChangeEvent`s.
//!
//! M1 realizes [`PortMapping`] (`port_event_helper.py:212`) and `get_port_mapping`
//! (`port_event_helper.py:346`). The production builder probes CONFIG_DB for the
//! emulator's `Ethernet{i*4}` front-panel ports (the SFP change-event index is the
//! physical index `i`); the row-based builder mirrors the reference
//! `Table.getKeys()`/`get()` flow the unit tests exercise, filtering non-front-panel
//! ports via [`is_front_panel_port`].

use std::collections::BTreeMap;

use swss_common::DbConnector;

use crate::error::Result;

/// `PortChangeEvent` event types (ADD/REMOVE for CONFIG_DB PORT; SET/DEL for the
/// subscribed STATE_DB tables). Mirrors the `PORT_ADD=0/PORT_REMOVE=1/PORT_SET=2/
/// PORT_DEL=3` integer constants (`port_event_helper.py:14`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortChangeEventType {
    PortAdd,
    PortRemove,
    PortSet,
    PortDel,
}

/// A single port change (`port_event_helper.py:13`).
#[derive(Debug, Clone)]
pub struct PortChangeEvent {
    pub port_name: String,
    pub port_index: i32,
    pub asic_id: usize,
    pub event_type: PortChangeEventType,
    pub port_dict: BTreeMap<String, String>,
    pub db_name: Option<String>,
    pub table_name: Option<String>,
}

impl PortChangeEvent {
    /// `PortChangeEvent(port_name, port_index, asic_id, event_type)`.
    pub fn new(
        port_name: impl Into<String>,
        port_index: i32,
        asic_id: usize,
        event_type: PortChangeEventType,
    ) -> Self {
        PortChangeEvent {
            port_name: port_name.into(),
            port_index,
            asic_id,
            event_type,
            port_dict: BTreeMap::new(),
            db_name: None,
            table_name: None,
        }
    }
}

// --- Front-panel filter (sonic_py_common.multi_asic.is_front_panel_port) --------

const FRONT_PANEL_PREFIX: &str = "Ethernet";
const BACKPLANE_PREFIX: &str = "Ethernet-BP";
const INBAND_PREFIX: &str = "Ethernet-IB";
const RECIRC_PREFIX: &str = "Ethernet-Rec";

/// `multi_asic.is_role_internal` — internal roles are `Int/Inb/Rec/Dpc`.
fn is_role_internal(role: Option<&str>) -> bool {
    matches!(role, Some("Int") | Some("Inb") | Some("Rec") | Some("Dpc"))
}

/// `multi_asic.is_front_panel_port(port, role)` — a front-panel port starts with
/// `Ethernet`, is not a backplane/inband/recirc port or subinterface, and does not
/// carry an internal role.
pub fn is_front_panel_port(port: &str, role: Option<&str>) -> bool {
    if !port.starts_with(FRONT_PANEL_PREFIX) {
        return false;
    }
    if port.starts_with(BACKPLANE_PREFIX)
        || port.starts_with(INBAND_PREFIX)
        || port.starts_with(RECIRC_PREFIX)
    {
        return false;
    }
    if port.contains('.') {
        return false;
    }
    !is_role_internal(role)
}

/// `PortMapping` (`port_event_helper.py:212`): the logical<->physical maps built from
/// CONFIG_DB `PORT|*`.
#[derive(Debug, Default, Clone)]
pub struct PortMapping {
    pub logical_port_list: Vec<String>,
    pub logical_to_physical: BTreeMap<String, usize>,
    pub physical_to_logical: BTreeMap<usize, Vec<String>>,
    pub logical_to_asic: BTreeMap<String, usize>,
}

impl PortMapping {
    pub fn new() -> Self {
        PortMapping::default()
    }

    /// `handle_port_change_event` — apply an ADD/REMOVE to the maps.
    pub fn handle_port_change_event(&mut self, event: &PortChangeEvent) {
        match event.event_type {
            PortChangeEventType::PortAdd => self.handle_port_add(event),
            PortChangeEventType::PortRemove => self.handle_port_remove(event),
            _ => {}
        }
    }

    fn handle_port_add(&mut self, event: &PortChangeEvent) {
        let name = event.port_name.clone();
        let phys = event.port_index as usize;
        if !self.logical_port_list.contains(&name) {
            self.logical_port_list.push(name.clone());
        }
        self.logical_to_physical.insert(name.clone(), phys);
        let entry = self.physical_to_logical.entry(phys).or_default();
        if !entry.contains(&name) {
            entry.push(name.clone());
        }
        // natsorted(..., key=lambda x: x.lower()) — breakout ports on one physical.
        entry.sort_by(|a, b| natural_cmp(&a.to_lowercase(), &b.to_lowercase()));
        self.logical_to_asic.insert(name, event.asic_id);
    }

    fn handle_port_remove(&mut self, event: &PortChangeEvent) {
        let name = &event.port_name;
        self.logical_port_list.retain(|p| p != name);
        self.logical_to_physical.remove(name);
        let phys = event.port_index as usize;
        if let Some(list) = self.physical_to_logical.get_mut(&phys) {
            list.retain(|p| p != name);
            if list.is_empty() {
                self.physical_to_logical.remove(&phys);
            }
        }
        self.logical_to_asic.remove(name);
    }

    /// `get_asic_id_for_logical_port`.
    pub fn get_asic_id_for_logical_port(&self, port_name: &str) -> Option<usize> {
        self.logical_to_asic.get(port_name).copied()
    }

    /// `get_logical_to_physical` -> `[index]` (a single-element list) or `None`.
    pub fn get_logical_to_physical(&self, port_name: &str) -> Option<Vec<usize>> {
        self.logical_to_physical.get(port_name).map(|&p| vec![p])
    }

    /// `get_physical_to_logical`.
    pub fn get_physical_to_logical(&self, physical_port: usize) -> Option<Vec<String>> {
        self.physical_to_logical.get(&physical_port).cloned()
    }

    /// `is_logical_port`.
    pub fn is_logical_port(&self, port_name: &str) -> bool {
        self.logical_to_physical.contains_key(port_name)
    }

    /// `logical_port_name_to_physical_port_list` — a numeric name maps to itself as
    /// a physical port index; otherwise resolve via the logical->physical map.
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

/// Build a [`PortMapping`] from `(port_name, index, role)` rows, applying the
/// front-panel filter — the reference `get_port_mapping` flow over `Table.getKeys()`
/// / `Table.get()` (`port_event_helper.py:346`).
pub fn build_port_mapping_from_rows(
    asic_id: usize,
    rows: &[(String, i32, Option<String>)],
) -> PortMapping {
    let mut mapping = PortMapping::new();
    for (name, index, role) in rows {
        if !is_front_panel_port(name, role.as_deref()) {
            continue;
        }
        let ev = PortChangeEvent::new(name.clone(), *index, asic_id, PortChangeEventType::PortAdd);
        mapping.handle_port_change_event(&ev);
    }
    mapping
}

/// Production `get_port_mapping` for the single-ASIC emulator testbed: the change
/// events index modules by physical SFP index `i`, and the emulator names SFP `i` as
/// `Ethernet{i*4}`. Probe CONFIG_DB `PORT|Ethernet{i*4}` (no `Table.getKeys()` on
/// the raw `DbConnector`) and add each present front-panel port with physical
/// index = `i` so the map stays consistent with `get_change_event`.
pub fn get_port_mapping(config: &DbConnector, num_sfps: usize) -> Result<PortMapping> {
    let mut mapping = PortMapping::new();
    for phys in 0..num_sfps {
        let name = format!("Ethernet{}", phys * 4);
        let key = format!("PORT|{name}");
        if !config.exists(&key)? {
            continue;
        }
        let role = config
            .hget(&key, "role")?
            .map(|v| v.to_string_lossy().into_owned());
        if !is_front_panel_port(&name, role.as_deref()) {
            continue;
        }
        let ev = PortChangeEvent::new(name, phys as i32, 0, PortChangeEventType::PortAdd);
        mapping.handle_port_change_event(&ev);
    }
    Ok(mapping)
}

/// Natural order compare (digits compared numerically) so breakout logical ports on
/// one physical index sort like `natsort.natsorted` (`Ethernet2` before `Ethernet10`).
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ac = a.chars().peekable();
    let mut bc = b.chars().peekable();
    loop {
        match (ac.peek().copied(), bc.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut an = String::new();
                while let Some(&c) = ac.peek() {
                    if c.is_ascii_digit() {
                        an.push(c);
                        ac.next();
                    } else {
                        break;
                    }
                }
                let mut bn = String::new();
                while let Some(&c) = bc.peek() {
                    if c.is_ascii_digit() {
                        bn.push(c);
                        bc.next();
                    } else {
                        break;
                    }
                }
                let av: u64 = an.parse().unwrap_or(0);
                let bv: u64 = bn.parse().unwrap_or(0);
                match av.cmp(&bv) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
            }
            (Some(x), Some(y)) => {
                ac.next();
                bc.next();
                match x.cmp(&y) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
            }
        }
    }
}

/// `PortChangeObserver` (`port_event_helper.py:46`) — in the reference, subscribes
/// to CONFIG_DB PORT (plus STATE_DB `TRANSCEIVER_INFO` and `PORT_TABLE.host_tx_ready`)
/// via `SubscriberStateTable` + `Select` and dispatches `PortChangeEvent`s. This crate
/// stands in for the `Select` with a lightweight **poll** of CONFIG_DB `PORT|*`
/// ([`poll_config_port_changes`]) — the same approach the DOM task uses for its
/// APPL_DB `PORT_TABLE` flap watch — so no long-lived `SubscriberStateTable`/`Select`
/// handles are threaded across the daemon; the observable behavior (a CONFIG_DB
/// logical-port add/remove → an ADD/REMOVE `PortChangeEvent`) is identical.
pub struct PortChangeObserver {
    // TODO(Translator): SubscriberStateTable set + Select over the seam (optional; the
    // daemon uses the poll analogue in `poll_config_port_changes`).
}

impl PortChangeObserver {
    pub fn new() -> Result<Self> {
        todo!("port_event_helper.py:PortChangeObserver.__init__")
    }

    /// `handle_port_update_event` — poll and dispatch pending events.
    pub fn handle_port_update_event(&mut self, _timeout_ms: u64) -> Result<Vec<PortChangeEvent>> {
        todo!("port_event_helper.py:PortChangeObserver.handle_port_update_event")
    }
}

/// Poll CONFIG_DB `PORT|*` and derive the logical-port ADD/REMOVE events that have
/// occurred since the `port_mapping` snapshot — the polling analogue of the
/// reference `read_port_config_change` (`port_event_helper.py:307`) over a
/// `SubscriberStateTable`. Consistent with [`get_port_mapping`]: it probes the
/// emulator's `Ethernet{i*4}` front-panel names for physical SFP index `i` (so the
/// change-event index and the mapping stay aligned) rather than a raw `getKeys()`.
///
/// For each probed port, comparing CONFIG_DB presence against the current mapping
/// yields the diff:
///   * present in CONFIG_DB but NOT in the mapping ⇒ a new logical port ⇒ `PORT_ADD`;
///   * absent from CONFIG_DB but IN the mapping ⇒ the port was deconfigured ⇒
///     `PORT_REMOVE` (carrying the mapping's current physical index so the caller can
///     update `physical_to_logical`).
/// The non-front-panel filter mirrors the reference (`is_front_panel_port`). The diff
/// is idempotent: once the caller applies an event to the mapping, the next poll no
/// longer re-reports it.
pub fn poll_config_port_changes(
    config: &DbConnector,
    num_sfps: usize,
    port_mapping: &PortMapping,
) -> Result<Vec<PortChangeEvent>> {
    let mut events = Vec::new();
    for phys in 0..num_sfps {
        let name = format!("Ethernet{}", phys * 4);
        let key = format!("PORT|{name}");
        let exists = config.exists(&key)?;
        let is_logical = port_mapping.is_logical_port(&name);
        if exists {
            if is_logical {
                continue; // already known — nothing changed for this port
            }
            // A newly-configured logical port. Apply the same front-panel filter the
            // reference does before emitting the ADD (skip inband/backplane/recirc
            // names and internal-role ports).
            let role = config
                .hget(&key, "role")?
                .map(|v| v.to_string_lossy().into_owned());
            if !is_front_panel_port(&name, role.as_deref()) {
                continue;
            }
            events.push(PortChangeEvent::new(
                name,
                phys as i32,
                0,
                PortChangeEventType::PortAdd,
            ));
        } else if is_logical {
            // The port was removed from CONFIG_DB. Carry its CURRENT physical index so
            // the caller's `handle_port_change_event` can clear `physical_to_logical`.
            let phys_idx = port_mapping
                .get_logical_to_physical(&name)
                .and_then(|l| l.first().copied())
                .unwrap_or(phys);
            events.push(PortChangeEvent::new(
                name,
                phys_idx as i32,
                0,
                PortChangeEventType::PortRemove,
            ));
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Direct port of tests/test_xcvrd.py:test_get_port_mapping — build the mapping
    // from CONFIG_DB rows and assert the front-panel filter (inband name / internal
    // role are excluded) and the logical<->physical maps.
    #[test]
    fn test_get_port_mapping_filters_and_builds_maps() {
        let rows = vec![
            ("Ethernet0".to_string(), 1, None),
            ("Ethernet4".to_string(), 2, None),
            ("Ethernet-IB0".to_string(), 3, None),
            ("Ethernet8".to_string(), 4, Some("Dpc".to_string())),
        ];
        let m = build_port_mapping_from_rows(0, &rows);

        assert!(m.logical_port_list.contains(&"Ethernet0".to_string()));
        assert_eq!(m.get_asic_id_for_logical_port("Ethernet0"), Some(0));
        assert_eq!(m.get_physical_to_logical(1), Some(vec!["Ethernet0".to_string()]));
        assert_eq!(m.get_logical_to_physical("Ethernet0"), Some(vec![1]));

        assert!(m.logical_port_list.contains(&"Ethernet4".to_string()));
        assert_eq!(m.get_physical_to_logical(2), Some(vec!["Ethernet4".to_string()]));

        // Inband port (name-filtered) and Dpc-role port (role-filtered) are excluded.
        assert!(!m.logical_port_list.contains(&"Ethernet-IB0".to_string()));
        assert_eq!(m.get_asic_id_for_logical_port("Ethernet-IB0"), None);
        assert_eq!(m.get_physical_to_logical(3), None);
        assert!(!m.logical_port_list.contains(&"Ethernet8".to_string()));
        assert_eq!(m.get_physical_to_logical(4), None);
    }

    #[test]
    fn test_is_front_panel_port() {
        assert!(is_front_panel_port("Ethernet0", None));
        assert!(is_front_panel_port("Ethernet100", Some("Ext")));
        assert!(!is_front_panel_port("Ethernet-IB0", None));
        assert!(!is_front_panel_port("Ethernet-BP0", None));
        assert!(!is_front_panel_port("Ethernet8", Some("Dpc")));
        assert!(!is_front_panel_port("Ethernet0.10", None));
        assert!(!is_front_panel_port("PortChannel0", None));
    }

    // Direct port of the PortMapping add/remove/getter behavior exercised throughout
    // tests/test_xcvrd.py (PortChangeEvent handling).
    #[test]
    fn test_handle_port_change_event_add_remove() {
        let mut m = PortMapping::new();
        m.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet0",
            0,
            0,
            PortChangeEventType::PortAdd,
        ));
        assert!(m.is_logical_port("Ethernet0"));
        assert_eq!(m.get_logical_to_physical("Ethernet0"), Some(vec![0]));
        assert_eq!(m.get_physical_to_logical(0), Some(vec!["Ethernet0".to_string()]));

        m.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet0",
            0,
            0,
            PortChangeEventType::PortRemove,
        ));
        assert!(!m.is_logical_port("Ethernet0"));
        assert_eq!(m.get_physical_to_logical(0), None);
        assert!(m.logical_port_list.is_empty());
    }

    #[test]
    fn test_logical_port_name_to_physical_port_list() {
        let mut m = PortMapping::new();
        m.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet0",
            5,
            0,
            PortChangeEventType::PortAdd,
        ));
        // numeric name -> itself as physical index
        assert_eq!(m.logical_port_name_to_physical_port_list("3"), Some(vec![3]));
        // known logical port -> its physical index
        assert_eq!(
            m.logical_port_name_to_physical_port_list("Ethernet0"),
            Some(vec![5])
        );
        // unknown logical port -> None
        assert_eq!(m.logical_port_name_to_physical_port_list("Ethernet999"), None);
    }

    #[test]
    fn test_physical_to_logical_natural_sort() {
        // Two breakout logical ports on one physical index keep natural order.
        let mut m = PortMapping::new();
        m.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet10",
            0,
            0,
            PortChangeEventType::PortAdd,
        ));
        m.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet2",
            0,
            0,
            PortChangeEventType::PortAdd,
        ));
        assert_eq!(
            m.get_physical_to_logical(0),
            Some(vec!["Ethernet2".to_string(), "Ethernet10".to_string()])
        );
    }
}
