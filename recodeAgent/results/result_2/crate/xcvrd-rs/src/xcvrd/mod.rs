//! `xcvrd/` package (analysis 3.2) - the daemon orchestration (`DaemonXcvrd`),
//! the presence/identity poster (`post_port_sfp_info_to_db`), and the
//! presence/identity/error state machine (`sfp_state_update`).
//!
//! M1 realizes the presence + identity path: [`post_port_sfp_info_to_db`] publishes
//! `TRANSCEIVER_INFO`, and [`DaemonXcvrd::remove_stale_transceiver_info`] purges the
//! stale rows of absent modules at boot. The production daemon loop that drives this
//! logic against the real HAL/DB lives in [`crate::daemon`]; the identity/status
//! transitions on plug/unplug + the EEPROM retry loop live in
//! [`sfp_state_update::SfpStateUpdateTask`].

pub mod sfp_state_update;

use crate::db::DbTable;
use crate::error::{Result, XcvrdError};
use crate::hal::Hal;
use crate::xcvrd_utilities::common;
use crate::xcvrd_utilities::port_event_helper::PortMapping;

/// `DaemonXcvrd` (`xcvrd.py:877`) - process lifecycle: init -> spawn worker threads
/// -> wait -> deinit.
pub struct DaemonXcvrd {
    pub skip_cmis_mgr: bool,
    pub enable_sff_mgr: bool,
    pub dom_temperature_poll_interval: Option<u64>,
    pub dom_update_interval: Option<u64>,
    pub port_mapping: PortMapping,
}

impl DaemonXcvrd {
    pub fn new(skip_cmis_mgr: bool, enable_sff_mgr: bool) -> Self {
        DaemonXcvrd {
            skip_cmis_mgr,
            enable_sff_mgr,
            dom_temperature_poll_interval: None,
            dom_update_interval: None,
            port_mapping: PortMapping::new(),
        }
    }

    /// `init` - wait for PortConfigDone/PortInitDone, build the port mapping, purge
    /// stale `TRANSCEIVER_INFO`, init SFP objects.
    pub fn init(&mut self) -> Result<()> {
        todo!("xcvrd.py:DaemonXcvrd.init")
    }

    /// `run` - spawn the task threads and join (the full daemon; the bootstrap
    /// `daemon::run` covers M0/M1 today).
    pub fn run(&mut self) -> Result<()> {
        todo!("xcvrd.py:DaemonXcvrd.run (spawn SfpStateUpdate/Dom/Cmis/Sff tasks)")
    }

    /// `deinit` - reboot-aware teardown of the STATUS/STATUS_SW tables (M13).
    pub fn deinit(&mut self) -> Result<()> {
        todo!("xcvrd.py:DaemonXcvrd.deinit")
    }

    /// `wait_for_port_config_done`.
    pub fn wait_for_port_config_done(&self, _namespace: &str) -> Result<()> {
        todo!("xcvrd.py:DaemonXcvrd.wait_for_port_config_done")
    }

    /// `remove_stale_transceiver_info` (`xcvrd.py:986`) - at boot, drop the
    /// `TRANSCEIVER_INFO` row of any port whose module is physically absent (STATE_DB
    /// survives a daemon restart, so a module unplugged while xcvrd was down leaves a
    /// stale row). For each logical port with an existing INFO row: resolve its
    /// physical port, and if the module is not present, delete the row.
    pub fn remove_stale_transceiver_info(
        &self,
        port_mapping: &PortMapping,
        int_tbl: &dyn DbTable,
        hal: &dyn Hal,
    ) {
        for lport in &port_mapping.logical_port_list {
            if int_tbl.get(lport).is_none() {
                continue;
            }
            let pport = match port_mapping.get_logical_to_physical(lport) {
                Some(list) if !list.is_empty() => list[0],
                _ => {
                    eprintln!("xcvrd-rs: remove_stale: no physical port for lport {lport}");
                    continue;
                }
            };
            let present = match hal.sfp(pport) {
                Ok(sfp) => sfp.get_presence().unwrap_or(false),
                Err(_) => false,
            };
            if !present {
                common::del_port_sfp_dom_info_from_db(lport, port_mapping, &[int_tbl]);
            }
        }
    }
}

/// `post_port_sfp_info_to_db` (`xcvrd.py:178`) - publish `TRANSCEIVER_INFO` for a
/// logical port. Resolve the physical port(s); skip an absent module; on a present
/// module read identity via the HAL. If the read is unavailable (bridge error or a
/// null/malformed result - the emulator `FAULT_READ` shape) return
/// [`XcvrdError::EepromNotReady`] so the caller retries. CMIS modules (identity has
/// `cmis_rev`) write every field via [`stringify`] + `is_replaceable`; non-CMIS
/// modules write the fixed field subset.
pub fn post_port_sfp_info_to_db(
    logical_port_name: &str,
    port_mapping: &PortMapping,
    int_tbl: &dyn DbTable,
    hal: &dyn Hal,
) -> Result<()> {
    let physical_port_list = match port_mapping.logical_port_name_to_physical_port_list(logical_port_name)
    {
        Some(list) => list,
        None => {
            eprintln!("xcvrd-rs: no physical ports for logical port {logical_port_name}");
            return Err(XcvrdError::PhysicalPortNotExist);
        }
    };

    let ganged = physical_port_list.len() > 1;
    let mut ganged_member_num = 1;

    for physical_port in physical_port_list {
        let sfp = hal.sfp(physical_port)?;
        if !sfp.get_presence()? {
            continue;
        }

        let port_name = common::get_physical_port_name(logical_port_name, ganged_member_num, ganged);
        ganged_member_num += 1;

        // A bridge error or a null/non-object identity means the EEPROM isn't
        // readable yet (emulator FAULT_READ) -> signal a retry.
        let info = match sfp.get_transceiver_info() {
            Ok(v) => v,
            Err(_) => return Err(XcvrdError::EepromNotReady),
        };
        let obj = match info.as_object() {
            Some(o) => o,
            None => return Err(XcvrdError::EepromNotReady),
        };

        let is_replaceable = sfp.is_replaceable().unwrap_or(false);
        let mut fvs: Vec<(String, String)> = Vec::new();

        if obj.contains_key("cmis_rev") {
            // CMIS module: publish every advertised field.
            for (field, value) in obj {
                if let Some(s) = stringify(value) {
                    fvs.push((field.clone(), s));
                }
            }
            fvs.push(("is_replaceable".to_string(), pybool(is_replaceable).to_string()));
        } else {
            // Non-CMIS module: the fixed field subset (xcvrd.py:218).
            const FIELDS: [&str; 16] = [
                "type",
                "vendor_rev",
                "serial",
                "manufacturer",
                "model",
                "vendor_oui",
                "vendor_date",
                "connector",
                "encoding",
                "ext_identifier",
                "ext_rateselect_compliance",
                "cable_type",
                "cable_length",
                "specification_compliance",
                "nominal_bit_rate",
                "application_advertisement",
            ];
            for field in FIELDS {
                let rendered = obj.get(field).and_then(stringify);
                let value = if field == "application_advertisement" {
                    rendered.unwrap_or_else(|| "N/A".to_string())
                } else {
                    rendered.unwrap_or_default()
                };
                fvs.push((field.to_string(), value));
            }
            fvs.push(("is_replaceable".to_string(), pybool(is_replaceable).to_string()));
            let dom_capability = obj
                .get("dom_capability")
                .and_then(stringify)
                .unwrap_or_else(|| "N/A".to_string());
            fvs.push(("dom_capability".to_string(), dom_capability));
        }

        int_tbl.set(&port_name, &fvs);
    }

    Ok(())
}

/// Python-style bool rendering, matching `str(bool)` the reference daemon writes.
pub fn pybool(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

/// Render a `get_transceiver_info()` JSON value as the STATE_DB field string the
/// reference daemon writes via `str(value)`. Strings are trimmed of trailing NUL
/// padding (CMIS identity strings are fixed-width, NUL-/space-padded); JSON nulls
/// are skipped.
pub fn stringify(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.trim_end_matches('\0').trim_end().to_string()),
        Value::Bool(b) => Some(pybool(*b).to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDbTable, MockHal, MockSfp};
    use crate::xcvrd_utilities::port_event_helper::{PortChangeEvent, PortChangeEventType};
    use serde_json::json;

    fn mapping_with(ports: &[(&str, usize)]) -> PortMapping {
        let mut pm = PortMapping::new();
        for (name, phys) in ports {
            pm.handle_port_change_event(&PortChangeEvent::new(
                *name,
                *phys as i32,
                0,
                PortChangeEventType::PortAdd,
            ));
        }
        pm
    }

    // The observable STATE_DB value formatting (locked down deterministically).
    #[test]
    fn pybool_matches_python_str_bool() {
        assert_eq!(pybool(true), "True");
        assert_eq!(pybool(false), "False");
    }

    #[test]
    fn stringify_trims_trailing_nul_and_space_padding() {
        assert_eq!(stringify(&json!("QSFP-DD\0\0\0")).as_deref(), Some("QSFP-DD"));
        assert_eq!(stringify(&json!("Acacia  ")).as_deref(), Some("Acacia"));
        assert_eq!(stringify(&json!("Baz  \0\0")).as_deref(), Some("Baz"));
        assert_eq!(stringify(&json!("Ethernet0")).as_deref(), Some("Ethernet0"));
    }

    #[test]
    fn stringify_renders_scalars_like_python_str() {
        assert_eq!(stringify(&json!(true)).as_deref(), Some("True"));
        assert_eq!(stringify(&json!(false)).as_deref(), Some("False"));
        assert_eq!(stringify(&json!(42)).as_deref(), Some("42"));
        assert_eq!(stringify(&json!(1.5)).as_deref(), Some("1.5"));
    }

    #[test]
    fn stringify_skips_json_null() {
        assert!(stringify(&json!(null)).is_none());
    }

    // Port of tests/test_xcvrd.py:test_post_port_sfp_info_to_db - an empty port
    // mapping resolves no physical port, so the poster reports PHYSICAL_PORT_NOT_EXIST
    // without touching the table.
    #[test]
    fn test_post_port_sfp_info_to_db_no_physical_port() {
        let pm = PortMapping::new();
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let hal = MockHal::with_sfps(vec![]);
        let rc = post_port_sfp_info_to_db("Ethernet0", &pm, &int_tbl, &hal);
        assert!(matches!(rc, Err(XcvrdError::PhysicalPortNotExist)));
        assert_eq!(int_tbl.get_size(), 0);
    }

    // Port of tests/test_xcvrd.py:test_post_port_sfp_info_to_db_with_sfp_not_present -
    // a physically-absent module is skipped (no TRANSCEIVER_INFO write), returns Ok.
    #[test]
    fn test_post_port_sfp_info_to_db_with_sfp_not_present() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let hal = MockHal::with_sfps(vec![MockSfp::default()]); // not present
        let rc = post_port_sfp_info_to_db("Ethernet0", &pm, &int_tbl, &hal);
        assert!(rc.is_ok());
        assert_eq!(int_tbl.get_size(), 0);
    }

    // A present CMIS module publishes every advertised identity field + is_replaceable
    // (the emulator path); string values are NUL-trimmed.
    #[test]
    fn test_post_port_sfp_info_to_db_cmis_present() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let sfp = MockSfp::present().with_info(json!({
            "cmis_rev": "5.0",
            "manufacturer": "xcvr-emu\u{0}\u{0}",
            "model": "EMU-100G",
            "cable_length": 100.0,
        }));
        let hal = MockHal::with_sfps(vec![sfp]);

        let rc = post_port_sfp_info_to_db("Ethernet0", &pm, &int_tbl, &hal);
        assert!(rc.is_ok());
        assert_eq!(int_tbl.hget("Ethernet0", "manufacturer").as_deref(), Some("xcvr-emu"));
        assert_eq!(int_tbl.hget("Ethernet0", "cmis_rev").as_deref(), Some("5.0"));
        assert_eq!(int_tbl.hget("Ethernet0", "cable_length").as_deref(), Some("100.0"));
        assert_eq!(int_tbl.hget("Ethernet0", "is_replaceable").as_deref(), Some("True"));
    }

    // A present module whose identity read fails (null result -> emulator FAULT_READ)
    // reports EEPROM-not-ready and writes nothing (the caller retries).
    #[test]
    fn test_post_port_sfp_info_to_db_eeprom_not_ready() {
        let pm = mapping_with(&[("Ethernet0", 0)]);
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let sfp = MockSfp::present(); // present, but info defaults to JSON null
        let hal = MockHal::with_sfps(vec![sfp]);

        let rc = post_port_sfp_info_to_db("Ethernet0", &pm, &int_tbl, &hal);
        assert!(matches!(rc, Err(XcvrdError::EepromNotReady)));
        assert_eq!(int_tbl.get_size(), 0);
    }

    // Parametrized port of tests/test_xcvrd.py:test_remove_stale_transceiver_info -
    // an INFO row is purged iff its module is absent; present modules are kept.
    #[test]
    fn test_remove_stale_transceiver_info() {
        let cases: &[(&[bool], &[&str])] = &[
            (&[false, false], &["Ethernet0", "Ethernet4"]),
            (&[true, false], &["Ethernet4"]),
            (&[true, true], &[]),
        ];
        for (presence, expected_removed) in cases {
            let pm = mapping_with(&[("Ethernet0", 0), ("Ethernet4", 1)]);
            let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
            int_tbl.set("Ethernet0", &[("manufacturer".to_string(), "xcvr-emu".to_string())]);
            int_tbl.set("Ethernet4", &[("manufacturer".to_string(), "xcvr-emu".to_string())]);
            let sfps: Vec<MockSfp> = presence
                .iter()
                .map(|&p| if p { MockSfp::present() } else { MockSfp::default() })
                .collect();
            let hal = MockHal::with_sfps(sfps);

            let daemon = DaemonXcvrd::new(false, false);
            daemon.remove_stale_transceiver_info(&pm, &int_tbl, &hal);

            for port in ["Ethernet0", "Ethernet4"] {
                let removed = expected_removed.contains(&port);
                assert_eq!(
                    int_tbl.get(port).is_none(),
                    removed,
                    "port {port} removed?={removed} presence={presence:?}"
                );
            }
        }
    }

    // Empty logical-port list -> nothing to purge (test case 4 in the Python param).
    #[test]
    fn test_remove_stale_transceiver_info_no_ports() {
        let pm = PortMapping::new();
        let int_tbl = MockDbTable::new("TRANSCEIVER_INFO");
        let hal = MockHal::with_sfps(vec![]);
        let daemon = DaemonXcvrd::new(false, false);
        daemon.remove_stale_transceiver_info(&pm, &int_tbl, &hal);
        assert_eq!(int_tbl.get_size(), 0);
    }
}
