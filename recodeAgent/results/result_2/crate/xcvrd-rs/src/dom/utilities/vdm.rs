//! Port of `dom/utilities/vdm/{utils,db_utils}.py` — `VDMUtils` (reads, incl.
//! freeze/unfreeze around the statistic read) and `VDMDBUtils` (posts) for
//! `TRANSCEIVER_VDM_REAL_VALUE` and the per-type
//! `TRANSCEIVER_VDM_{HALARM,LALARM,HWARN,LWARN}_{THRESHOLD,FLAG}` (+metadata).
//!
//! VDM decode stays in the Python CMIS stack — every read here is a `call_json`
//! escape-hatch call into the corresponding no-arg `Sfp` method
//! (`is_transceiver_vdm_supported`, `get_transceiver_vdm_real_value_basic`, …),
//! exactly as the Python `VDMUtils` delegates to `self.sfp_obj_dict[port].<method>()`.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::db::DbTable;
use crate::hal::{Hal, SfpHandle};
use crate::xcvrd_utilities::port_event_helper::PortMapping;
use crate::xcvrd_utilities::xcvr_table_helper::VDM_THRESHOLD_TYPES;

use super::db::{value_to_py_str, DbUtils, Fvs};

/// `MAX_tVDMF_TIME_MSECS` (`vdm/utils.py:4`) — settle time after issuing a
/// freeze/unfreeze before the done bit is polled.
const MAX_TVDMF_TIME_MSECS: u64 = 10;
/// `MAX_VDM_FREEZE_UNFREEZE_TIME_MSECS` (`vdm/utils.py:5`) — overall poll budget for
/// the freeze/unfreeze done bit.
const MAX_VDM_FREEZE_UNFREEZE_TIME_MSECS: u64 = 1000;
/// `FREEZE_UNFREEZE_DONE_POLLING_INTERVAL_MSECS` (`vdm/utils.py:6`).
const FREEZE_UNFREEZE_DONE_POLLING_INTERVAL_MSECS: u64 = 1;

/// Per-type VDM threshold value tables (`TRANSCEIVER_VDM_{TYPE}_THRESHOLD`), keyed by
/// threshold type (`halarm`/`lalarm`/`hwarn`/`lwarn`). Published once per insert by
/// [`crate::xcvrd::sfp_state_update::SfpStateUpdateTask`], mirroring the Python
/// `post_port_vdm_thresholds_to_db` call at insert (`xcvrd.py:351/831/858`).
#[derive(Clone)]
pub struct VdmThresholdTables {
    pub thresholds: HashMap<String, Arc<dyn DbTable>>,
}

/// Per-type VDM flag value tables plus their change-count / set-time / clear-time
/// metadata (`TRANSCEIVER_VDM_{TYPE}_FLAG[_CHANGE_COUNT|_SET_TIME|_CLEAR_TIME]`),
/// each keyed by threshold type. Published off the DOM loop and the link-change
/// fast re-read.
#[derive(Clone)]
pub struct VdmFlagTables {
    pub flag: HashMap<String, Arc<dyn DbTable>>,
    pub change_count: HashMap<String, Arc<dyn DbTable>>,
    pub set_time: HashMap<String, Arc<dyn DbTable>>,
    pub clear_time: HashMap<String, Arc<dyn DbTable>>,
}

/// `VDMUtils` — VDM reads + the freeze/unfreeze handshake.
pub struct VdmUtils;

impl VdmUtils {
    pub fn new() -> Self {
        VdmUtils
    }

    /// `is_transceiver_vdm_supported` — `False` on a not-implemented / non-bool read.
    pub fn is_transceiver_vdm_supported(&self, sfp: &dyn SfpHandle) -> bool {
        sfp.call_json("is_transceiver_vdm_supported")
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// `is_vdm_statistic_supported` — `False` on a not-implemented / non-bool read.
    pub fn is_vdm_statistic_supported(&self, sfp: &dyn SfpHandle) -> bool {
        sfp.call_json("is_vdm_statistic_supported")
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// `get_vdm_real_values_basic` — `{}` on error (Python logs + returns `{}`).
    pub fn get_vdm_real_values_basic(&self, sfp: &dyn SfpHandle) -> Map<String, Value> {
        match sfp.call_json("get_transceiver_vdm_real_value_basic") {
            Ok(Value::Object(o)) => o,
            _ => Map::new(),
        }
    }

    /// `get_vdm_real_values_statistic` (read under freeze) — `{}` on error.
    pub fn get_vdm_real_values_statistic(&self, sfp: &dyn SfpHandle) -> Map<String, Value> {
        match sfp.call_json("get_transceiver_vdm_real_value_statistic") {
            Ok(Value::Object(o)) => o,
            _ => Map::new(),
        }
    }

    /// `get_vdm_flags` — the raw per-observable flag dict (keys carry the `_{type}`
    /// suffix). `None` short-circuits the poster (nothing published).
    pub fn get_vdm_flags(&self, sfp: &dyn SfpHandle) -> Option<Value> {
        sfp.call_json("get_transceiver_vdm_flags").ok()
    }

    /// `get_vdm_thresholds` — the raw per-observable threshold dict (keys carry the
    /// `_{type}` suffix).
    pub fn get_vdm_thresholds(&self, sfp: &dyn SfpHandle) -> Option<Value> {
        sfp.call_json("get_transceiver_vdm_thresholds").ok()
    }

    /// `_vdm_action_and_confirm` — issue the action, sleep `MAX_tVDMF_TIME_MSECS`,
    /// then poll `status_check` every `FREEZE_UNFREEZE_DONE_POLLING_INTERVAL_MSECS`
    /// until it reports done or `MAX_VDM_FREEZE_UNFREEZE_TIME_MSECS` elapses. A failed
    /// action (or a never-set done bit) returns `false`.
    fn action_and_confirm(
        &self,
        sfp: &dyn SfpHandle,
        action_method: &str,
        status_method: &str,
    ) -> bool {
        let status = sfp
            .call_json(action_method)
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !status {
            return false;
        }
        sleep(Duration::from_millis(MAX_TVDMF_TIME_MSECS));
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(MAX_VDM_FREEZE_UNFREEZE_TIME_MSECS) {
            if sfp
                .call_json(status_method)
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return true;
            }
            sleep(Duration::from_millis(FREEZE_UNFREEZE_DONE_POLLING_INTERVAL_MSECS));
        }
        false
    }

    /// `_freeze_vdm_stats_and_confirm`.
    pub fn freeze_and_confirm(&self, sfp: &dyn SfpHandle) -> bool {
        self.action_and_confirm(sfp, "freeze_vdm_stats", "get_vdm_freeze_status")
    }

    /// `_unfreeze_vdm_stats_and_confirm`.
    pub fn unfreeze_and_confirm(&self, sfp: &dyn SfpHandle) -> bool {
        self.action_and_confirm(sfp, "unfreeze_vdm_stats", "get_vdm_unfreeze_status")
    }

    /// `vdm_freeze_context` (closure form) — freeze, run `body(frozen)`, then ALWAYS
    /// unfreeze (the Python `finally`). `frozen` is `false` when the freeze did not
    /// confirm, so the body can skip the statistic read exactly as the Python
    /// `if not vdm_frozen:` branch does.
    pub fn with_vdm_freeze<T>(&self, sfp: &dyn SfpHandle, body: impl FnOnce(bool) -> T) -> T {
        let frozen = self.freeze_and_confirm(sfp);
        let result = body(frozen);
        self.unfreeze_and_confirm(sfp);
        result
    }
}

impl Default for VdmUtils {
    fn default() -> Self {
        VdmUtils::new()
    }
}

/// `VDMDBUtils` — the VDM posters (per-type fan-out over `VDM_THRESHOLD_TYPES`).
pub struct VdmDbUtils;

impl VdmDbUtils {
    pub fn new() -> Self {
        VdmDbUtils
    }

    /// `post_port_vdm_real_values_from_dict_to_db` → `TRANSCEIVER_VDM_REAL_VALUE`.
    /// Posts a pre-merged (basic + statistic) real-value dict with a single trailing
    /// `last_update_time`. Validates the port (presence), skips an empty dict, and
    /// beautifies (stringifies) before writing — mirroring `db_utils.py:25`.
    pub fn post_port_vdm_real_values_from_dict_to_db(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        port_mapping: &PortMapping,
        table: &dyn DbTable,
        hal: &dyn Hal,
        mut vdm_real_values: Map<String, Value>,
    ) {
        let db = DbUtils::new();
        if db
            .validate_and_get_physical_port(stop, logical_port_name, port_mapping, hal, true)
            .is_none()
        {
            return;
        }
        if port_mapping
            .get_asic_id_for_logical_port(logical_port_name)
            .is_none()
        {
            return;
        }
        if vdm_real_values.is_empty() {
            return;
        }
        db.beautify_info_dict(&mut vdm_real_values);
        let mut fvs: Fvs = vdm_real_values
            .iter()
            .map(|(k, v)| (k.clone(), value_to_py_str(v)))
            .collect();
        fvs.push(("last_update_time".to_string(), db.get_current_time()));
        table.set(logical_port_name, &fvs);
    }

    /// `post_port_vdm_thresholds_to_db` → per-type `_THRESHOLD` (posted at insert).
    pub fn post_port_vdm_thresholds_to_db(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        port_mapping: &PortMapping,
        hal: &dyn Hal,
        tables: &VdmThresholdTables,
    ) {
        self.post_thresholds_or_flags(
            stop,
            logical_port_name,
            port_mapping,
            hal,
            |sfp| VdmUtils::new().get_vdm_thresholds(sfp),
            &tables.thresholds,
            None,
        );
    }

    /// `post_port_vdm_flags_to_db` → per-type `_FLAG` (+ change-count / set-time /
    /// clear-time metadata).
    pub fn post_port_vdm_flags_to_db(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        port_mapping: &PortMapping,
        hal: &dyn Hal,
        tables: &VdmFlagTables,
    ) {
        self.post_thresholds_or_flags(
            stop,
            logical_port_name,
            port_mapping,
            hal,
            |sfp| VdmUtils::new().get_vdm_flags(sfp),
            &tables.flag,
            Some(tables),
        );
    }

    /// `_post_port_vdm_thresholds_or_flags_to_db` — read the raw per-observable dict
    /// (keys suffixed `_{type}`), split it into one dict per `VDM_THRESHOLD_TYPES`
    /// entry (stripping the suffix), and post each non-empty per-type dict to its own
    /// STATE_DB table with a trailing `last_update_time`. For the flag path
    /// (`flag_tables` set), the change-count / set-time / clear-time metadata is
    /// updated (against the previous value row) BEFORE the value row is overwritten.
    fn post_thresholds_or_flags<G>(
        &self,
        stop: &AtomicBool,
        logical_port_name: &str,
        port_mapping: &PortMapping,
        hal: &dyn Hal,
        get_values: G,
        value_tables: &HashMap<String, Arc<dyn DbTable>>,
        flag_tables: Option<&VdmFlagTables>,
    ) where
        G: FnOnce(&dyn SfpHandle) -> Option<Value>,
    {
        let db = DbUtils::new();
        let physical_port =
            match db.validate_and_get_physical_port(stop, logical_port_name, port_mapping, hal, true)
            {
                Some(p) => p,
                None => return,
            };
        if port_mapping
            .get_asic_id_for_logical_port(logical_port_name)
            .is_none()
        {
            return;
        }
        let sfp = match hal.sfp(physical_port) {
            Ok(s) => s,
            Err(_) => return,
        };
        // A `None`/non-object read posts nothing (Python: `if vdm_values_dict is None:
        // log + return`; an empty dict likewise ends up posting nothing below).
        let vdm_values = match get_values(&*sfp) {
            Some(Value::Object(o)) => o,
            _ => return,
        };
        let update_time = db.get_current_time();

        // Split into one dict per threshold type by removing the `_{type}` suffix.
        let mut per_type: HashMap<&'static str, Map<String, Value>> = HashMap::new();
        for t in VDM_THRESHOLD_TYPES {
            per_type.insert(t, Map::new());
        }
        for (key, value) in &vdm_values {
            for t in VDM_THRESHOLD_TYPES {
                let suffix = format!("_{t}");
                if key.contains(&suffix) {
                    let new_key = key.replace(&suffix, "");
                    if let Some(m) = per_type.get_mut(t) {
                        m.insert(new_key, value.clone());
                    }
                }
            }
        }

        // Flag metadata is updated before the value row is overwritten (COR flags).
        if let Some(ft) = flag_tables {
            for t in VDM_THRESHOLD_TYPES {
                let dict = &per_type[t];
                if dict.is_empty() {
                    continue;
                }
                db.update_flag_metadata_tables(
                    logical_port_name,
                    dict,
                    &update_time,
                    &*ft.flag[t],
                    &*ft.change_count[t],
                    &*ft.set_time[t],
                    &*ft.clear_time[t],
                    &format!("VDM {t}"),
                );
            }
        }

        // Post each non-empty per-type value row.
        for t in VDM_THRESHOLD_TYPES {
            let dict = match per_type.get_mut(t) {
                Some(d) if !d.is_empty() => d,
                _ => continue,
            };
            db.beautify_info_dict(dict);
            let mut fvs: Fvs = dict
                .iter()
                .map(|(k, v)| (k.clone(), value_to_py_str(v)))
                .collect();
            fvs.push(("last_update_time".to_string(), db.get_current_time()));
            value_tables[t].set(logical_port_name, &fvs);
        }
    }
}

impl Default for VdmDbUtils {
    fn default() -> Self {
        VdmDbUtils::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbTable;
    use crate::mock::{MockDbTable, MockHal, MockSfp};
    use crate::xcvrd_utilities::port_event_helper::{
        PortChangeEvent, PortChangeEventType, PortMapping,
    };
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn one_port_mapping() -> PortMapping {
        let mut pm = PortMapping::new();
        pm.handle_port_change_event(&PortChangeEvent::new(
            "Ethernet0",
            0,
            0,
            PortChangeEventType::PortAdd,
        ));
        pm
    }

    fn vdm_flag_tables() -> (VdmFlagTables, HashMap<String, Arc<MockDbTable>>) {
        let mut flag = HashMap::new();
        let mut change_count = HashMap::new();
        let mut set_time = HashMap::new();
        let mut clear_time = HashMap::new();
        let mut probe: HashMap<String, Arc<MockDbTable>> = HashMap::new();
        for t in VDM_THRESHOLD_TYPES {
            let f = Arc::new(MockDbTable::new(format!("VDM_{t}_FLAG")));
            let c = Arc::new(MockDbTable::new(format!("VDM_{t}_CC")));
            let s = Arc::new(MockDbTable::new(format!("VDM_{t}_ST")));
            let cl = Arc::new(MockDbTable::new(format!("VDM_{t}_CT")));
            flag.insert(t.to_string(), f.clone() as Arc<dyn DbTable>);
            change_count.insert(t.to_string(), c as Arc<dyn DbTable>);
            set_time.insert(t.to_string(), s.clone() as Arc<dyn DbTable>);
            clear_time.insert(t.to_string(), cl as Arc<dyn DbTable>);
            probe.insert(format!("{t}_flag"), f);
            probe.insert(format!("{t}_set"), s);
        }
        (
            VdmFlagTables {
                flag,
                change_count,
                set_time,
                clear_time,
            },
            probe,
        )
    }

    #[test]
    fn test_post_port_vdm_real_values_from_dict_to_db() {
        // A pre-merged real-value dict is stringified + stamped with last_update_time.
        let sfp = MockSfp::present();
        let hal = MockHal::with_sfps(vec![sfp]);
        let pm = one_port_mapping();
        let tbl = Arc::new(MockDbTable::new("TRANSCEIVER_VDM_REAL_VALUE"));
        let stop = AtomicBool::new(false);

        let mut merged = Map::new();
        merged.insert("laser_temperature_media1".to_string(), json!(45.5));
        merged.insert("esnr_media_input1".to_string(), json!(30));

        VdmDbUtils::new().post_port_vdm_real_values_from_dict_to_db(
            &stop,
            "Ethernet0",
            &pm,
            &*tbl,
            &hal,
            merged,
        );

        let row = tbl.get("Ethernet0").expect("VDM real value row");
        let map: HashMap<String, String> = row.into_iter().collect();
        assert_eq!(
            map.get("laser_temperature_media1").map(String::as_str),
            Some("45.5")
        );
        assert_eq!(map.get("esnr_media_input1").map(String::as_str), Some("30"));
        assert!(map.contains_key("last_update_time"));
    }

    #[test]
    fn test_post_port_vdm_real_values_empty_dict_noop() {
        let sfp = MockSfp::present();
        let hal = MockHal::with_sfps(vec![sfp]);
        let pm = one_port_mapping();
        let tbl = Arc::new(MockDbTable::new("TRANSCEIVER_VDM_REAL_VALUE"));
        let stop = AtomicBool::new(false);

        VdmDbUtils::new().post_port_vdm_real_values_from_dict_to_db(
            &stop,
            "Ethernet0",
            &pm,
            &*tbl,
            &hal,
            Map::new(),
        );
        assert!(tbl.get("Ethernet0").is_none());
    }

    #[test]
    fn test_post_port_vdm_thresholds_to_db_fans_out_by_type() {
        // The raw dict keys carry the type token in the MIDDLE with the lane trailing it
        // (`{prefix}_{type}{lane}`), exactly as the real HAL builds them in
        // `CmisApi.get_transceiver_vdm_thresholds` (cmis.py: `f"{db_key_name_prefix}_{
        // threshold_type_str}{lane}"`). The poster removes the `_{type}` token and writes
        // one row per type table keyed by the bare `{prefix}{lane}` observable name. Because
        // the lane trails the type, an `ends_with("_{type}")` split would MISS every key
        // (they end in `..._lwarn1`, not `..._lwarn`) — `contains` + strip is required.
        let raw = json!({
            "laser_temperature_media_halarm1": 80.0,
            "laser_temperature_media_lalarm1": 5.0,
            "laser_temperature_media_hwarn1": 75.0,
            // The exact VDM #1 regression value: a present, numeric LWARN threshold of 0.0
            // must survive the split into TRANSCEIVER_VDM_LWARN_THRESHOLD (neither dropped
            // nor coerced to 'N/A' by beautify), so `laser_temperature_media1` is published
            // with a real value in the lwarn table.
            "laser_temperature_media_lwarn1": 0.0,
        });
        let sfp = MockSfp::present().with_json("get_transceiver_vdm_thresholds", raw);
        let hal = MockHal::with_sfps(vec![sfp]);
        let pm = one_port_mapping();
        let stop = AtomicBool::new(false);

        let mut thresholds: HashMap<String, Arc<dyn DbTable>> = HashMap::new();
        let mut probe: HashMap<String, Arc<MockDbTable>> = HashMap::new();
        for t in VDM_THRESHOLD_TYPES {
            let tb = Arc::new(MockDbTable::new(format!("VDM_{t}_THRESHOLD")));
            thresholds.insert(t.to_string(), tb.clone() as Arc<dyn DbTable>);
            probe.insert(t.to_string(), tb);
        }
        let tables = VdmThresholdTables { thresholds };

        VdmDbUtils::new().post_port_vdm_thresholds_to_db(&stop, "Ethernet0", &pm, &hal, &tables);

        for t in VDM_THRESHOLD_TYPES {
            let row = probe[t]
                .get("Ethernet0")
                .unwrap_or_else(|| panic!("{t} row"));
            let map: HashMap<String, String> = row.into_iter().collect();
            // Suffix stripped: the field is the bare observable name (prefix + lane).
            assert!(
                map.contains_key("laser_temperature_media1"),
                "{t} missing bare key, got {:?}",
                map.keys().collect::<Vec<_>>()
            );
            assert!(!map.keys().any(|k| k.contains("_halarm") || k.contains("_lwarn")));
            assert!(map.contains_key("last_update_time"));
        }

        // Guard for the VDM #1 regression: the lwarn table carries the real numeric 0.0
        // threshold, proving a present-but-zero lwarn value is neither dropped by the split
        // nor rendered 'N/A' by beautify. (An actual 'N/A' would only arise from a raw-read
        // KeyError/TypeError in the HAL, i.e. a value-level timing issue, not this fan-out.)
        let lwarn: HashMap<String, String> = probe["lwarn"]
            .get("Ethernet0")
            .expect("lwarn row")
            .into_iter()
            .collect();
        assert_eq!(
            lwarn.get("laser_temperature_media1").map(String::as_str),
            Some("0.0"),
            "lwarn laser_temperature_media1 must publish the real 0.0 value, not drop/NA"
        );
    }

    #[test]
    fn test_post_port_vdm_flags_to_db_updates_metadata() {
        // First publish seeds metadata (count 0 / never); a subsequent True raise for
        // one field bumps that field's change-count and stamps its set-time. Keys use the
        // real HAL format `{prefix}_{type}{lane}` (cmis.py get_transceiver_vdm_flags:
        // `f"{db_key_name_prefix}_{flag_type_str}{lane}"`), the lane trailing the type.
        let false_flags = json!({
            "laser_temperature_media_halarm1": false,
            "laser_temperature_media_lalarm1": false,
        });
        let sfp = MockSfp::present().with_json("get_transceiver_vdm_flags", false_flags);
        let hal = MockHal::with_sfps(vec![sfp]);
        let pm = one_port_mapping();
        let stop = AtomicBool::new(false);
        let (tables, probe) = vdm_flag_tables();

        VdmDbUtils::new().post_port_vdm_flags_to_db(&stop, "Ethernet0", &pm, &hal, &tables);
        // Baseline: halarm flag row present + False.
        let row = probe["halarm_flag"]
            .get("Ethernet0")
            .expect("halarm flag row");
        let map: HashMap<String, String> = row.into_iter().collect();
        assert_eq!(
            map.get("laser_temperature_media1").map(String::as_str),
            Some("False")
        );
        // Set-time seeded to "never" on the first publish.
        let st = probe["halarm_set"]
            .get("Ethernet0")
            .expect("halarm set-time row");
        let stmap: HashMap<String, String> = st.into_iter().collect();
        assert_eq!(
            stmap.get("laser_temperature_media1").map(String::as_str),
            Some("never")
        );

        // Raise the halarm flag True → change count bumps, set-time stamped.
        let true_flags = json!({
            "laser_temperature_media_halarm1": true,
            "laser_temperature_media_lalarm1": false,
        });
        let sfp2 = MockSfp::present().with_json("get_transceiver_vdm_flags", true_flags);
        let hal2 = MockHal::with_sfps(vec![sfp2]);
        VdmDbUtils::new().post_port_vdm_flags_to_db(&stop, "Ethernet0", &pm, &hal2, &tables);

        let row = probe["halarm_flag"]
            .get("Ethernet0")
            .expect("halarm flag row 2");
        let map: HashMap<String, String> = row.into_iter().collect();
        assert_eq!(
            map.get("laser_temperature_media1").map(String::as_str),
            Some("True")
        );
        let st = probe["halarm_set"]
            .get("Ethernet0")
            .expect("halarm set-time row 2");
        let stmap: HashMap<String, String> = st.into_iter().collect();
        assert_ne!(
            stmap.get("laser_temperature_media1").map(String::as_str),
            Some("never")
        );
    }

    #[test]
    fn test_vdm_supported_and_statistic_reads() {
        let sfp = MockSfp::present()
            .with_json("is_transceiver_vdm_supported", json!(true))
            .with_json("is_vdm_statistic_supported", json!(false));
        assert!(VdmUtils::new().is_transceiver_vdm_supported(&sfp));
        assert!(!VdmUtils::new().is_vdm_statistic_supported(&sfp));
        // Missing canned result → defaults False (not-implemented parity).
        let bare = MockSfp::present();
        assert!(!VdmUtils::new().is_transceiver_vdm_supported(&bare));
    }

    #[test]
    fn test_vdm_freeze_context_confirms_and_reads_statistic() {
        // freeze/unfreeze report done immediately (canned True) so the handshake
        // returns frozen=true and the statistic read succeeds.
        let sfp = MockSfp::present()
            .with_json("freeze_vdm_stats", json!(true))
            .with_json("get_vdm_freeze_status", json!(true))
            .with_json("unfreeze_vdm_stats", json!(true))
            .with_json("get_vdm_unfreeze_status", json!(true))
            .with_json(
                "get_transceiver_vdm_real_value_statistic",
                json!({"laser_temperature_media1_max": 50.0}),
            );

        let vdm = VdmUtils::new();
        let stat = vdm.with_vdm_freeze(&sfp, |frozen| {
            assert!(frozen, "freeze must confirm when done-bit is set");
            vdm.get_vdm_real_values_statistic(&sfp)
        });
        assert_eq!(stat.get("laser_temperature_media1_max"), Some(&json!(50.0)));
    }

    #[test]
    fn test_vdm_freeze_fails_when_action_false() {
        // freeze action returns False → frozen=false, body sees it, no statistic read.
        let sfp = MockSfp::present().with_json("freeze_vdm_stats", json!(false));
        let vdm = VdmUtils::new();
        let saw = vdm.with_vdm_freeze(&sfp, |frozen| frozen);
        assert!(!saw);
    }
}
