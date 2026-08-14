#![allow(dead_code, unused_variables, unused_imports)]
//! Port of `xcvrd_utilities/media_settings_parser.py`: media_settings.json -> APPL_DB SerDes (notify_media_setting).
//!
//! The Python module keys off a module-level `g_dict` (the parsed media_settings.json)
//! patched per test. Here the parsed settings are passed explicitly to each entry point
//! (`g_dict: &Value`) so the daemon can own the loaded settings and tests can inject
//! fixtures without global state. Regex matching goes through the look-around-capable
//! `common::re_match`/`re_fullmatch` helpers (production keys use negative look-ahead).

use std::cmp::Ordering;
use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::cmis::cmis_api::CmisApi;
use crate::db::Table;
use crate::dom::utilities::db::utils::py_str;
use crate::xcvrd_utilities::common::{
    check_port_in_range, get_cmis_application_desired, get_physical_port_name, re_fullmatch, re_match,
};
use crate::xcvrd_utilities::port_event_helper::PortMapping;
use crate::xcvrd_utilities::xcvr_table_helper::{
    NPU_SI_SETTINGS_NOTIFIED_VALUE, NPU_SI_SETTINGS_SYNC_STATUS_KEY,
};

// Constants mirroring the Python module constants.
pub const LANE_SPEED_KEY_PREFIX: &str = "speed:";
pub const DEFAULT_KEY: &str = "Default";
pub const RANGE_SEPARATOR: char = '-';
pub const COMMA_SEPARATOR: char = ',';
pub const LANE_SPEED_DEFAULT_KEY: &str = "speed:Default";
pub const GLOBAL_MEDIA_SETTINGS_KEY: &str = "GLOBAL_MEDIA_SETTINGS";
pub const PORT_MEDIA_SETTINGS_KEY: &str = "PORT_MEDIA_SETTINGS";
pub const CUSTOM_MEDIA_SETTINGS_KEY: &str = "CUSTOM_MEDIA_SETTINGS";
pub const PHYSICAL_PORT_NOT_EXIST: i64 = -1;

/// The four-part media-settings key (`get_media_settings_key`'s dict). `lane_speed_key`
/// is `Option` because CMIS bring-up may not resolve a host-electrical-interface id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSettingsKey {
    pub vendor_key: String,
    pub media_key: String,
    pub lane_speed_key: Option<String>,
    pub medium_lane_speed_key: String,
}

fn empty_obj() -> Value {
    Value::Object(Map::new())
}

/// A JSON object with at least one member (Python truthiness for a dict).
fn is_nonempty_obj(v: &Value) -> bool {
    v.as_object().map_or(false, |o| !o.is_empty())
}

/// `str(media_len) != 0` — Python only treats a *numeric* zero as "== 0"; every string
/// (including `''`/`'0'`) compares unequal to the integer 0.
fn ne_zero(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => true,
    }
}

/// `natsorted(dict)` restricted to the lane-key dictionaries the parser slices: order
/// keys by their alternating non-digit / digit runs so `lane2` sorts before `lane10`.
fn natsorted_keys(obj: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<String> = obj.keys().cloned().collect();
    keys.sort_by(|a, b| natcmp(a, b));
    keys
}

fn natcmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let na = take_number(&mut ai);
                    let nb = take_number(&mut bi);
                    match na.cmp(&nb) {
                        Ordering::Equal => continue,
                        other => return other,
                    }
                } else {
                    match ca.cmp(&cb) {
                        Ordering::Equal => {
                            ai.next();
                            bi.next();
                            continue;
                        }
                        other => return other,
                    }
                }
            }
        }
    }
}

fn take_number(it: &mut std::iter::Peekable<std::str::Chars>) -> u64 {
    let mut n: u64 = 0;
    while let Some(c) = it.peek().copied() {
        if let Some(d) = c.to_digit(10) {
            n = n.saturating_mul(10).saturating_add(d as u64);
            it.next();
        } else {
            break;
        }
    }
    n
}

/// `is_si_per_speed_supported`: the settings are keyed by lane speed iff the *first* key
/// contains the `speed:` prefix (mirroring `LANE_SPEED_KEY_PREFIX in list(dict.keys())[0]`).
pub fn is_si_per_speed_supported(media_dict: &Value) -> bool {
    media_dict
        .as_object()
        .and_then(|o| o.keys().next())
        .map_or(false, |k| k.contains(LANE_SPEED_KEY_PREFIX))
}

/// `get_media_settings_for_speed`: resolve the per-lane-speed sub-dictionary. When the
/// dict is not speed-keyed it is returned as-is; otherwise the first candidate whose
/// (prefix-stripped) pattern `re.fullmatch`es the lane speed wins, falling back to
/// `speed:Default` (or `{}`).
pub fn get_media_settings_for_speed(settings_dict: &Value, lane_speed_key: Option<&str>) -> Value {
    if !is_si_per_speed_supported(settings_dict) {
        return settings_dict.clone();
    }
    let lsk = match lane_speed_key {
        Some(s) if !s.is_empty() => s,
        _ => return empty_obj(),
    };
    let lane_speed_str = lsk.get(LANE_SPEED_KEY_PREFIX.len()..).unwrap_or("");
    if let Some(obj) = settings_dict.as_object() {
        for (candidate, value_dict) in obj {
            let pattern = candidate.get(LANE_SPEED_KEY_PREFIX.len()..).unwrap_or("");
            if re_fullmatch(pattern, lane_speed_str) {
                return value_dict.clone();
            }
        }
        return obj
            .get(LANE_SPEED_DEFAULT_KEY)
            .cloned()
            .unwrap_or_else(empty_obj);
    }
    empty_obj()
}

/// `MediaSettingsParserBase.get_media_settings`: match a `media_dict` first by
/// vendor/media key (vendor, vendor-name-before-`-`, or media key), then by the medium+
/// lane-speed key. `None` when nothing matches.
pub fn get_media_settings(key: &MediaSettingsKey, media_dict: &Value) -> Option<Value> {
    let obj = media_dict.as_object()?;
    let vendor_name = key.vendor_key.split('-').next().unwrap_or("");
    for (dict_key, value) in obj {
        if re_match(dict_key, &key.vendor_key)
            || re_match(dict_key, vendor_name)
            || re_match(dict_key, &key.media_key)
        {
            return Some(get_media_settings_for_speed(value, key.lane_speed_key.as_deref()));
        }
    }
    for (dict_key, value) in obj {
        if re_match(dict_key, &key.medium_lane_speed_key) {
            return Some(get_media_settings_for_speed(value, key.lane_speed_key.as_deref()));
        }
    }
    None
}

/// The base parser's static helpers (`MediaSettingsParserBase`).
pub struct MediaSettingsParserBase;

impl MediaSettingsParserBase {
    /// `_get_lane_values_str`: slice a per-lane dict for the port's subport and join the
    /// values with commas (values coerced with `str()` semantics).
    pub fn get_lane_values_str(val_dict: &Value, lane_count: i64, subport_num: i64) -> String {
        let obj = match val_dict.as_object() {
            Some(o) => o,
            None => return String::new(),
        };
        let mut start = if subport_num != 0 {
            (subport_num - 1) * lane_count
        } else {
            0
        };
        if start + lane_count > obj.len() as i64 {
            start = 0;
        }
        let vals: Vec<String> = natsorted_keys(obj).iter().map(|k| py_str(&obj[k])).collect();
        let s = (start.max(0) as usize).min(vals.len());
        let e = ((start + lane_count).max(0) as usize).min(vals.len());
        vals[s..e].join(",")
    }

    /// `to_db_value`: convert a traditional media dict to `(field, value)` tuples. Lane
    /// dicts are sliced (gearbox line-side keys use `gearbox_line_lane_count`); scalar
    /// values are stringified directly.
    pub fn to_db_value(
        media_dict: &Value,
        lane_count: i64,
        subport_num: i64,
        gearbox_line_lane_count: Option<i64>,
    ) -> Vec<(String, String)> {
        let obj = match media_dict.as_object() {
            Some(o) if !o.is_empty() => o,
            _ => return Vec::new(),
        };
        let mut fvs = Vec::new();
        for (media_key, media_value) in obj {
            let val_str = if media_value.is_object() {
                let mut lane_count_si = lane_count;
                if let Some(gb) = gearbox_line_lane_count {
                    if media_key.contains("gb_line") {
                        lane_count_si = gb;
                    }
                }
                MediaSettingsParserBase::get_lane_values_str(media_value, lane_count_si, subport_num)
            } else {
                py_str(media_value)
            };
            fvs.push((media_key.clone(), val_str));
        }
        fvs
    }
}

/// `GlobalMediaSettingsParser` — matches port ranges/lists in `GLOBAL_MEDIA_SETTINGS`.
#[derive(Default)]
pub struct GlobalMediaSettingsParser;

impl GlobalMediaSettingsParser {
    pub fn new() -> Self {
        GlobalMediaSettingsParser
    }

    /// Returns `(result, default_fallback)`; either can be `{}`.
    pub fn parse(
        &self,
        settings: &Value,
        physical_port: i32,
        key: &MediaSettingsKey,
    ) -> (Value, Value) {
        let mut default_dict = empty_obj();
        let settings_obj = match settings.as_object() {
            Some(o) => o,
            None => return (empty_obj(), default_dict),
        };
        for (keys, value) in settings_obj {
            let mut media_dict = empty_obj();
            if keys.contains(COMMA_SEPARATOR) {
                for port in keys.split(COMMA_SEPARATOR) {
                    if port.contains(RANGE_SEPARATOR) {
                        if check_port_in_range(port, physical_port) {
                            media_dict = value.clone();
                            break;
                        }
                    } else if physical_port.to_string() == port {
                        media_dict = value.clone();
                        break;
                    }
                }
            } else if keys.contains(RANGE_SEPARATOR) {
                if check_port_in_range(keys, physical_port) {
                    media_dict = value.clone();
                }
            }

            if is_nonempty_obj(&media_dict) {
                match get_media_settings(key, &media_dict) {
                    Some(ms) => return (ms, empty_obj()),
                    None => {
                        if let Some(def) = media_dict.get(DEFAULT_KEY) {
                            default_dict =
                                get_media_settings_for_speed(def, key.lane_speed_key.as_deref());
                        }
                    }
                }
            }
        }
        (empty_obj(), default_dict)
    }
}

/// `PortMediaSettingsParser` — matches an exact physical port in `PORT_MEDIA_SETTINGS`.
#[derive(Default)]
pub struct PortMediaSettingsParser;

impl PortMediaSettingsParser {
    pub fn new() -> Self {
        PortMediaSettingsParser
    }

    pub fn parse(
        &self,
        settings: &Value,
        physical_port: i32,
        key: &MediaSettingsKey,
    ) -> (Value, Value) {
        let settings_obj = match settings.as_object() {
            Some(o) => o,
            None => return (empty_obj(), empty_obj()),
        };
        let mut media_dict = empty_obj();
        for (keys, value) in settings_obj {
            if keys.parse::<i32>().ok() == Some(physical_port) {
                media_dict = value.clone();
                break;
            }
        }
        if !is_nonempty_obj(&media_dict) {
            return (empty_obj(), empty_obj());
        }
        match get_media_settings(key, &media_dict) {
            Some(ms) => (ms, empty_obj()),
            None => {
                if let Some(def) = media_dict.get(DEFAULT_KEY) {
                    (
                        empty_obj(),
                        get_media_settings_for_speed(def, key.lane_speed_key.as_deref()),
                    )
                } else {
                    (empty_obj(), empty_obj())
                }
            }
        }
    }
}

/// `CustomMediaSettingsParser` — CUSTOM: SerDes attributes with port selectors.
#[derive(Default)]
pub struct CustomMediaSettingsParser;

impl CustomMediaSettingsParser {
    pub const CUSTOM_SERDES_ATTR_PREFIX: &'static str = "CUSTOM:";
    pub const CUSTOM_SERDES_ATTRS_TOP_LEVEL_KEY: &'static str = "attributes";
    pub const CUSTOM_SERDES_ATTRS_KEY_IN_DB: &'static str = "custom_serdes_attrs";

    pub fn new() -> Self {
        CustomMediaSettingsParser
    }

    /// `is_port_selected`: does a `"7"`/`"1-4"`/`"1,3-4,8"` selector cover `physical_port`?
    /// Non-string selectors and malformed tokens are rejected (the latter skipped).
    pub fn is_port_selected(port_selector: &Value, physical_port: i32) -> bool {
        let s = match port_selector.as_str() {
            Some(s) => s,
            None => return false,
        };
        for token in s.split(COMMA_SEPARATOR) {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let (start_str, end_str) = match token.find(RANGE_SEPARATOR) {
                Some(idx) => (&token[..idx], &token[idx + 1..]),
                None => (token, token),
            };
            let start = match start_str.trim().parse::<i32>() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let end = match end_str.trim().parse::<i32>() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if start <= physical_port && physical_port <= end {
                return true;
            }
        }
        false
    }

    /// `_get_lane_values`: slice the per-lane values (as a `Vec<Value>`) for a subport.
    pub fn get_lane_values(val_dict: &Value, lane_count: i64, subport_num: i64) -> Vec<Value> {
        let obj = match val_dict.as_object() {
            Some(o) => o,
            None => return Vec::new(),
        };
        let val_list: Vec<Value> = natsorted_keys(obj).iter().map(|k| obj[k].clone()).collect();
        let mut start = if subport_num != 0 {
            (subport_num - 1) * lane_count
        } else {
            0
        };
        if start + lane_count > val_list.len() as i64 {
            start = 0;
        }
        let s = (start.max(0) as usize).min(val_list.len());
        let e = ((start + lane_count).max(0) as usize).min(val_list.len());
        val_list[s..e].to_vec()
    }

    /// `to_db_value`: serialize the `CUSTOM:` attributes to the compact JSON stored in
    /// APP_DB, or `None` when there are no custom attributes.
    pub fn to_db_value(
        custom_media_dict: &Value,
        lane_count: i64,
        subport_num: i64,
    ) -> Option<String> {
        let obj = match custom_media_dict.as_object() {
            Some(o) if !o.is_empty() => o,
            _ => return None,
        };
        let mut attrs: Vec<Value> = Vec::new();
        for (key, value) in obj {
            if !key.starts_with(Self::CUSTOM_SERDES_ATTR_PREFIX) {
                continue;
            }
            let stripped = &key[Self::CUSTOM_SERDES_ATTR_PREFIX.len()..];
            let lane_values = Self::get_lane_values(value, lane_count, subport_num);
            let mut inner = Map::new();
            inner.insert("value".to_string(), Value::Array(lane_values));
            let mut attr = Map::new();
            attr.insert(stripped.to_string(), Value::Object(inner));
            attrs.push(Value::Object(attr));
        }
        if attrs.is_empty() {
            return None;
        }
        let mut payload = Map::new();
        payload.insert(
            Self::CUSTOM_SERDES_ATTRS_TOP_LEVEL_KEY.to_string(),
            Value::Array(attrs),
        );
        // serde_json's default writer is compact (no spaces) == Python separators=(',',':').
        serde_json::to_string(&Value::Object(payload)).ok()
    }

    pub fn parse(
        &self,
        settings: &Value,
        physical_port: i32,
        key: &MediaSettingsKey,
    ) -> (Value, Value) {
        let settings_obj = match settings.as_object() {
            Some(o) if !o.is_empty() => o,
            _ => return (empty_obj(), empty_obj()),
        };
        let mut default_dict = empty_obj();
        for (port_selector, media_dict) in settings_obj {
            if !Self::is_port_selected(&Value::String(port_selector.clone()), physical_port) {
                continue;
            }
            if let Some(ms) = get_media_settings(key, media_dict) {
                if is_nonempty_obj(&ms) {
                    return (ms, empty_obj());
                }
            }
            if media_dict.get(DEFAULT_KEY).is_some() && !is_nonempty_obj(&default_dict) {
                default_dict = get_media_settings_for_speed(
                    &media_dict[DEFAULT_KEY],
                    key.lane_speed_key.as_deref(),
                );
            }
        }
        (empty_obj(), default_dict)
    }
}

/// `get_media_settings_value`: precedence GLOBAL explicit → PORT explicit → PORT Default
/// → GLOBAL Default → `{}`.
pub fn get_media_settings_value(
    g_dict: &Value,
    physical_port: i32,
    key: &MediaSettingsKey,
) -> Value {
    let mut global_default = empty_obj();
    if let Some(gms) = g_dict.get(GLOBAL_MEDIA_SETTINGS_KEY) {
        let (result, gd) = GlobalMediaSettingsParser::new().parse(gms, physical_port, key);
        global_default = gd;
        if is_nonempty_obj(&result) {
            return result;
        }
    }
    if let Some(pms) = g_dict.get(PORT_MEDIA_SETTINGS_KEY) {
        let (result, port_default) = PortMediaSettingsParser::new().parse(pms, physical_port, key);
        if is_nonempty_obj(&result) {
            return result;
        }
        if is_nonempty_obj(&port_default) {
            return port_default;
        }
    }
    if is_nonempty_obj(&global_default) {
        return global_default;
    }
    empty_obj()
}

/// `get_custom_media_settings_value`: resolve the custom SerDes dict for a port (or `{}`).
pub fn get_custom_media_settings_value(
    g_dict: &Value,
    physical_port: i32,
    key: &MediaSettingsKey,
) -> Value {
    let custom = match g_dict.get(CUSTOM_MEDIA_SETTINGS_KEY) {
        Some(c) if is_nonempty_obj(c) => c,
        _ => return empty_obj(),
    };
    let (result, default_dict) = CustomMediaSettingsParser::new().parse(custom, physical_port, key);
    if is_nonempty_obj(&result) {
        return result;
    }
    default_dict
}

/// `get_speed_lane_count_and_subport`: read `(speed, lane_count, subport)` for a logical
/// port from CONFIG_DB PORT; `(0, 0, 0)` if `speed`/`lanes` are missing.
pub fn get_speed_lane_count_and_subport(port: &str, cfg_port_tbl: &dyn Table) -> (i64, u32, i64) {
    let row = cfg_port_tbl.get(port).ok().flatten();
    let found = row.is_some();
    let dict: HashMap<String, String> = row.unwrap_or_default().into_iter().collect();
    if found && dict.contains_key("speed") && dict.contains_key("lanes") {
        let port_speed = dict["speed"].parse::<i64>().unwrap_or(0);
        let lane_count = dict["lanes"].split(COMMA_SEPARATOR).count() as u32;
        let subport = dict
            .get("subport")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        (port_speed, lane_count, subport)
    } else {
        (0, 0, 0)
    }
}

/// `get_lane_speed_key`: the CMIS host-electrical-interface (`speed:<HEID>`) or, for
/// non-CMIS, the arithmetic `speed:<Gbps>G` key. `None` when CMIS advertisement lookup
/// fails.
pub fn get_lane_speed_key(
    is_cmis: bool,
    api: Option<&dyn CmisApi>,
    port_speed: i64,
    lane_count: u32,
) -> Option<String> {
    if is_cmis {
        let api = api?;
        let appl_adv = api.get_application_advertisement();
        let app_id = get_cmis_application_desired(api, lane_count, port_speed as u32)?;
        let entry = appl_adv.get(app_id.to_string())?;
        let heid = entry.get("host_electrical_interface_id")?.as_str()?;
        let first = heid.split_whitespace().next().unwrap_or("");
        Some(format!("{LANE_SPEED_KEY_PREFIX}{first}"))
    } else if lane_count == 0 {
        None
    } else {
        Some(format!(
            "{LANE_SPEED_KEY_PREFIX}{}G",
            port_speed / lane_count as i64 / 1000
        ))
    }
}

/// `get_media_settings_key`: build the vendor/media/lane-speed/medium key for a physical
/// port from its transceiver info. `is_cmis`/`api`/`is_copper` are the decode seams the
/// Python resolves through `platform_chassis.get_sfp(port).get_xcvr_api()`.
pub fn get_media_settings_key(
    physical_port: i32,
    transceiver_dict: &Value,
    port_speed: i64,
    lane_count: u32,
    is_cmis: bool,
    api: Option<&dyn CmisApi>,
    is_copper: bool,
) -> MediaSettingsKey {
    const SUP_COMPLIANCE_STR: &str = "10/40G Ethernet Compliance Code";
    const SUP_LEN_STR: &str = "Length Cable Assembly(m)";
    const EXTENDED_SPEC_COMPLIANCE_STR: &str = "Extended Specification Compliance";

    let info = transceiver_dict
        .get(physical_port.to_string())
        .cloned()
        .unwrap_or_else(empty_obj);
    let get_str = |field: &str| {
        info.get(field)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    let vendor_name_str = get_str("manufacturer");
    let vendor_pn_str = get_str("model");
    let vendor_key = format!("{}-{}", vendor_name_str.to_uppercase(), vendor_pn_str);

    let mut media_len = Value::String(String::new());
    if get_str("cable_type") == SUP_LEN_STR {
        media_len = info
            .get("cable_length")
            .cloned()
            .unwrap_or(Value::String(String::new()));
    }

    let media_compliance_dict_str = get_str("specification_compliance");
    let mut media_compliance_code = String::new();
    if is_cmis {
        media_compliance_code = media_compliance_dict_str.clone();
    } else if let Ok(Value::Object(m)) =
        serde_json::from_str::<Value>(&media_compliance_dict_str.replace('\'', "\""))
    {
        if let Some(code) = m.get(SUP_COMPLIANCE_STR).and_then(|v| v.as_str()) {
            media_compliance_code = code.to_string();
            if (media_compliance_code == "Extended" || media_compliance_code == "Unknown")
                && m.contains_key(EXTENDED_SPEC_COMPLIANCE_STR)
            {
                if let Some(ext) = m.get(EXTENDED_SPEC_COMPLIANCE_STR).and_then(|v| v.as_str()) {
                    media_compliance_code = ext.to_string();
                }
            }
        }
    }

    let media_type = get_str("type_abbrv_name");
    let mut media_key = String::new();
    if !media_type.is_empty() {
        media_key.push_str(&media_type);
    }
    if !media_compliance_code.is_empty() {
        media_key.push('-');
        media_key.push_str(&media_compliance_code);
        if is_cmis {
            if media_compliance_code == "passive_copper_media_interface" && ne_zero(&media_len) {
                media_key.push_str(&format!("-{}M", py_str(&media_len)));
            }
        } else if ne_zero(&media_len) {
            media_key.push_str(&format!("-{}M", py_str(&media_len)));
        }
    } else {
        media_key.push_str("-*");
    }

    let lane_speed_key = get_lane_speed_key(is_cmis, api, port_speed, lane_count);
    let medium = if is_copper { "COPPER" } else { "OPTICAL" };
    let speed = if lane_count == 0 {
        0
    } else {
        port_speed / lane_count as i64 / 1000
    };
    let medium_lane_speed_key = format!("{medium}{speed}");

    MediaSettingsKey {
        vendor_key,
        media_key,
        lane_speed_key,
        medium_lane_speed_key,
    }
}

/// `media_settings_present`: truthy iff media settings were loaded.
pub fn media_settings_present(g_dict: &Value) -> bool {
    is_nonempty_obj(g_dict) || g_dict.as_array().map_or(false, |a| !a.is_empty())
}

/// `load_media_settings`: read `media_settings.json` from the HWSKU dir (preferred) or
/// the platform dir; `{}` when neither exists.
pub fn load_media_settings(platform_path: &str, hwsku_path: &str) -> Value {
    load_settings_file(platform_path, hwsku_path, "media_settings.json")
}

pub(crate) fn load_settings_file(platform_path: &str, hwsku_path: &str, file_name: &str) -> Value {
    let hwsku_file = std::path::Path::new(hwsku_path).join(file_name);
    let platform_file = std::path::Path::new(platform_path).join(file_name);
    let path = if hwsku_file.is_file() {
        hwsku_file
    } else if platform_file.is_file() {
        platform_file
    } else {
        return empty_obj();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| empty_obj()),
        Err(_) => empty_obj(),
    }
}

/// The two producer tables `notify_media_setting` writes to.
pub struct MediaNotifyTables<'a> {
    pub app_port_tbl: &'a dyn Table,
    pub state_port_tbl: &'a dyn Table,
}

/// `notify_media_setting`: resolve the NPU/ASIC-side SerDes settings for a logical port
/// and publish them to APPL_DB `PORT_TABLE`, stamping STATE_DB `PORT_TABLE.NPU_SI_SETTINGS_
/// SYNC_STATUS = NOTIFIED`.
///
/// The Python module-level seams patched by tests are passed explicitly:
/// - `media_present` / `npu_si_update_required`: the gating booleans,
/// - `(port_speed, lane_count, subport_num)`: `get_speed_lane_count_and_subport`,
/// - `gearbox_lanes_dict`: `get_gearbox_line_lanes_dict`,
/// - `presence_of`: `common._wrapper_get_presence`,
/// - `key_of(phys, effective_lane_count)`: `get_media_settings_key`.
#[allow(clippy::too_many_arguments)]
pub fn notify_media_setting(
    logical_port_name: &str,
    transceiver_dict: &Value,
    g_dict: &Value,
    port_mapping: &PortMapping,
    media_present: bool,
    npu_si_update_required: bool,
    port_speed: i64,
    lane_count: u32,
    subport_num: i64,
    gearbox_lanes_dict: &HashMap<String, u32>,
    tables: &MediaNotifyTables,
    presence_of: &dyn Fn(i32) -> bool,
    key_of: &dyn Fn(i32, u32) -> MediaSettingsKey,
) -> i64 {
    if !media_present {
        return 0;
    }
    if !npu_si_update_required {
        return 0;
    }

    let physical_port_list =
        match port_mapping.logical_port_name_to_physical_port_list(logical_port_name) {
            Some(list) => list,
            None => return PHYSICAL_PORT_NOT_EXIST,
        };
    let ganged_port = physical_port_list.len() > 1;
    let mut ganged_member_num = 1;

    for physical_port in physical_port_list {
        if !presence_of(physical_port) {
            continue;
        }
        if transceiver_dict.get(physical_port.to_string()).is_none() {
            continue;
        }

        let port_name = get_physical_port_name(logical_port_name, ganged_member_num, ganged_port);
        ganged_member_num += 1;

        let gearbox_line_lane_count = gearbox_lanes_dict.get(logical_port_name).copied();
        let effective_lane_count = gearbox_line_lane_count.unwrap_or(lane_count);
        let key = key_of(physical_port, effective_lane_count);

        let media_dict = get_media_settings_value(g_dict, physical_port, &key);
        let custom_media_dict = get_custom_media_settings_value(g_dict, physical_port, &key);

        if !is_nonempty_obj(&media_dict) && !is_nonempty_obj(&custom_media_dict) {
            return 0;
        }

        let mut fvs_list = MediaSettingsParserBase::to_db_value(
            &media_dict,
            lane_count as i64,
            subport_num,
            gearbox_line_lane_count.map(|g| g as i64),
        );

        if let Some(custom_db) = CustomMediaSettingsParser::to_db_value(
            &custom_media_dict,
            lane_count as i64,
            subport_num,
        ) {
            fvs_list.push((
                CustomMediaSettingsParser::CUSTOM_SERDES_ATTRS_KEY_IN_DB.to_string(),
                custom_db,
            ));
        }

        if fvs_list.is_empty() {
            return 0;
        }

        let _ = tables.app_port_tbl.set(&port_name, &fvs_list);
        let _ = tables.state_port_tbl.set(
            logical_port_name,
            &[(
                NPU_SI_SETTINGS_SYNC_STATUS_KEY.to_string(),
                NPU_SI_SETTINGS_NOTIFIED_VALUE.to_string(),
            )],
        );
    }
    0
}

#[cfg(test)]
#[path = "media_settings_parser_tests.rs"]
mod tests;
