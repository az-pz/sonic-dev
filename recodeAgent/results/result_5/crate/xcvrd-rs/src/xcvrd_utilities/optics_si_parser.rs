//! Port of `xcvrd_utilities/optics_si_parser.py`: the `optics_si_settings.json`
//! per-vendor module Signal-Integrity settings parser.
//!
//! The Python module keeps a global `g_optics_si_dict` loaded once at startup; here the
//! parsed settings are threaded in as `&Value` so the lookups stay pure and testable.
//! `notify`/bring-up code fetches the per-vendor SI dict (`fetch_optics_si_setting`) and
//! the CMIS state machine stages it via `CmisApi::stage_custom_si_settings`.
use serde_json::Value;

use crate::cmis::cmis_api::CmisApi;
use crate::xcvrd_utilities::common::{check_port_in_range, re_fullmatch, re_fullmatch_checked};
use crate::xcvrd_utilities::media_settings_parser::load_settings_file;

const GLOBAL_MEDIA_SETTINGS_KEY: &str = "GLOBAL_MEDIA_SETTINGS";
const PORT_MEDIA_SETTINGS_KEY: &str = "PORT_MEDIA_SETTINGS";
const DEFAULT_KEY: &str = "Default";
const RANGE_SEPARATOR: char = '-';
const COMMA_SEPARATOR: char = ',';

fn empty_obj() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Python dict truthiness: a non-empty object.
fn is_nonempty_obj(v: &Value) -> bool {
    v.as_object().map(|o| !o.is_empty()).unwrap_or(false)
}

/// `_match_optics_si_key`: match a settings key against the vendor key using `re.fullmatch`
/// (so patterns like `ABCDE-(1234|56789)` work), falling back to plain string comparison
/// when the pattern is not a valid regex. Tries the full vendor key, the vendor-prefix
/// (before the first `-`), and the bare vendor name.
pub fn match_optics_si_key(dict_key: &str, key: &str, vendor_name_str: &str) -> bool {
    let key_prefix = key.split('-').next().unwrap_or("");
    // `re_fullmatch_checked` reports whether `dict_key` compiles; the text is irrelevant to
    // that verdict, so a single probe tells us regex-vs-literal.
    if re_fullmatch_checked(dict_key, key).is_err() {
        return dict_key == key || dict_key == key_prefix || dict_key == vendor_name_str;
    }
    re_fullmatch(dict_key, key)
        || re_fullmatch(dict_key, key_prefix)
        || re_fullmatch(dict_key, vendor_name_str)
}

/// `_get_global_media_settings`: resolve the SI dict from GLOBAL_MEDIA_SETTINGS for a port +
/// lane speed. Returns `(Some(settings), default)` on a vendor match, else `(None, default)`
/// where `default` is the speed's `Default` entry if present.
pub fn get_global_media_settings(
    g_optics_si_dict: &Value,
    physical_port: i32,
    lane_speed: i64,
    key: &str,
    vendor_name_str: &str,
) -> (Option<Value>, Value) {
    let speed_key = format!("{lane_speed}G_SPEED");
    let mut default_dict = empty_obj();
    let mut optics_si_dict = empty_obj();

    let global = match g_optics_si_dict.get(GLOBAL_MEDIA_SETTINGS_KEY).and_then(|v| v.as_object()) {
        Some(o) => o,
        None => return (None, default_dict),
    };

    for (keys, value) in global {
        if keys.contains(COMMA_SEPARATOR) {
            for port in keys.split(COMMA_SEPARATOR) {
                if port.contains(RANGE_SEPARATOR) {
                    if check_port_in_range(port, physical_port) {
                        optics_si_dict = value.clone();
                        break;
                    }
                } else if physical_port.to_string() == port {
                    optics_si_dict = value.clone();
                    break;
                }
            }
        } else if keys.contains(RANGE_SEPARATOR) {
            if check_port_in_range(keys, physical_port) {
                optics_si_dict = value.clone();
            }
        }

        if let Some(speed_map) = optics_si_dict.get(&speed_key).and_then(|v| v.as_object()) {
            for (dict_key, dv) in speed_map {
                if match_optics_si_key(dict_key, key, vendor_name_str) {
                    return (Some(dv.clone()), default_dict);
                }
            }
            if let Some(d) = speed_map.get(DEFAULT_KEY) {
                default_dict = d.clone();
            }
        }
    }

    (None, default_dict)
}

/// `_get_port_media_settings`: resolve the SI dict from PORT_MEDIA_SETTINGS for a port +
/// lane speed, falling back to `default_dict` (from GLOBAL) when the port has no match.
pub fn get_port_media_settings(
    g_optics_si_dict: &Value,
    physical_port: i32,
    lane_speed: i64,
    key: &str,
    vendor_name_str: &str,
    default_dict: &Value,
) -> Value {
    let speed_key = format!("{lane_speed}G_SPEED");
    let mut optics_si_dict = empty_obj();

    if let Some(port_map) = g_optics_si_dict.get(PORT_MEDIA_SETTINGS_KEY).and_then(|v| v.as_object())
    {
        for (keys, value) in port_map {
            if keys.parse::<i32>().ok() == Some(physical_port) {
                optics_si_dict = value.clone();
                break;
            }
        }

        if !is_nonempty_obj(&optics_si_dict) {
            if is_nonempty_obj(default_dict) {
                return default_dict.clone();
            }
            eprintln!(
                "xcvrd-rs: No optics-SI values for physical port '{physical_port}' lane speed \
                 '{lane_speed}' key '{key}' vendor '{vendor_name_str}'"
            );
            return empty_obj();
        }

        if let Some(speed_map) = optics_si_dict.get(&speed_key).and_then(|v| v.as_object()) {
            for (dict_key, dv) in speed_map {
                if match_optics_si_key(dict_key, key, vendor_name_str) {
                    return dv.clone();
                }
            }
            if let Some(d) = speed_map.get(DEFAULT_KEY) {
                return d.clone();
            } else if is_nonempty_obj(default_dict) {
                return default_dict.clone();
            }
        }
    }

    default_dict.clone()
}

/// `get_optics_si_settings_value`: GLOBAL match first, else PORT (which carries the GLOBAL
/// `Default` forward).
pub fn get_optics_si_settings_value(
    g_optics_si_dict: &Value,
    physical_port: i32,
    lane_speed: i64,
    key: &str,
    vendor_name_str: &str,
) -> Value {
    let (global_settings, default_dict) =
        get_global_media_settings(g_optics_si_dict, physical_port, lane_speed, key, vendor_name_str);
    if let Some(s) = global_settings {
        return s;
    }
    get_port_media_settings(
        g_optics_si_dict,
        physical_port,
        lane_speed,
        key,
        vendor_name_str,
        &default_dict,
    )
}

/// `get_module_vendor_key`: build `(VENDOR-PN, VENDOR)` from the module api's manufacturer +
/// model (upper-cased, trimmed). `None` when the api or either field is unreadable.
pub fn get_module_vendor_key(
    physical_port: i32,
    api: Option<&dyn CmisApi>,
) -> Option<(String, String)> {
    let api = match api {
        Some(a) => a,
        None => {
            eprintln!("xcvrd-rs: Module {physical_port} xcvrd api not found");
            return None;
        }
    };
    let vendor_name = match api.get_manufacturer() {
        Some(v) => v,
        None => {
            eprintln!("xcvrd-rs: Module {physical_port} vendor name not found");
            return None;
        }
    };
    let vendor_pn = match api.get_model() {
        Some(v) => v,
        None => {
            eprintln!("xcvrd-rs: Module {physical_port} vendor part number not found");
            return None;
        }
    };
    let vendor = vendor_name.to_uppercase().trim().to_string();
    let pn = vendor_pn.to_uppercase().trim().to_string();
    Some((format!("{vendor}-{pn}"), vendor))
}

/// `fetch_optics_si_setting`: presence-gated per-vendor SI lookup for a physical port. Returns
/// an empty object when SI is not loaded, the module is absent, or the vendor key is unknown.
pub fn fetch_optics_si_setting(
    g_optics_si_dict: &Value,
    physical_port: i32,
    lane_speed: i64,
    api: Option<&dyn CmisApi>,
    present: bool,
) -> Value {
    if !is_nonempty_obj(g_optics_si_dict) {
        return empty_obj();
    }
    if !present {
        eprintln!("xcvrd-rs: Module {physical_port} presence not detected during notify");
        return empty_obj();
    }
    let (vendor_key, vendor_name) = match get_module_vendor_key(physical_port, api) {
        Some(v) => v,
        None => {
            eprintln!("xcvrd-rs: Error: No Vendor Key found for Module {physical_port}");
            return empty_obj();
        }
    };
    get_optics_si_settings_value(g_optics_si_dict, physical_port, lane_speed, &vendor_key, &vendor_name)
}

/// `load_optics_si_settings`: read `optics_si_settings.json` (HWSKU dir preferred, then
/// platform dir); `{}` when neither exists.
pub fn load_optics_si_settings(platform_path: &str, hwsku_path: &str) -> Value {
    load_settings_file(platform_path, hwsku_path, "optics_si_settings.json")
}

/// `optics_si_present`: whether any optics-SI settings were loaded.
pub fn optics_si_present(g_optics_si_dict: &Value) -> bool {
    is_nonempty_obj(g_optics_si_dict)
}

#[cfg(test)]
#[path = "optics_si_parser_tests.rs"]
mod tests;
