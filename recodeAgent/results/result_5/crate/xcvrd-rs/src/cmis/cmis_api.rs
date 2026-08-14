//! `CmisApi` seam — the mockable CMIS control/decode surface the datapath bring-up
//! state machine drives (analysis §3.4).
//!
//! The Python `CmisManagerTask` talks to `sonic_platform_base...c_cmis.CmisApi`. That
//! object both *decodes* the module (identity/state/advertisement) and *controls* the
//! CMIS page-10h datapath registers. Here the two halves are split across the crate's
//! seams so unit tests can inject a double:
//!
//!   - **Decode reads** (`is_flat_memory`, `get_module_state`, `get_datapath_state`,
//!     `get_application_advertisement`, …) stay in Python — [`BridgeCmisApi`] sources
//!     them from the bridge's `get_transceiver_status()` / `get_transceiver_info()`
//!     getters (never re-decodes EEPROM in Rust).
//!   - **Control writes** (`set_datapath_deinit`, `set_application`,
//!     `scs_apply_datapath_init`, `set_datapath_init`, `tx_disable_channel`) are the raw
//!     page-10h register writes `CmisApi` performs, issued through
//!     [`crate::hal::Sfp::write_eeprom`] with the upstream `c_cmis.py` encodings.
//!     `set_lpmode` delegates to the bridge's own `set_lpmode` (Python decode).
//!
//! [`MockCmisApi`] is the test double (canned decode + settable/sequenced dynamic
//! state + write call-counters), the analogue of the Python tests' `MagicMock()` api.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::hal::Sfp;

/// Seam error: bridge/DB errors are flattened to a string (mirrors [`crate::hal::HalResult`]).
pub type ApiResult<T> = std::result::Result<T, String>;

/// The CMIS control/decode surface the [`super::cmis_manager_task::CmisManagerTask`]
/// bring-up state machine drives (`api.*` in `cmis_manager_task.py`). Split from
/// [`Sfp`] so unit tests inject [`MockCmisApi`]; production wraps a bridge handle in
/// [`BridgeCmisApi`].
pub trait CmisApi {
    // --- decode reads (stay in Python via the bridge) ---
    fn is_flat_memory(&self) -> bool;
    fn is_coherent_module(&self) -> bool;
    fn get_module_type_abbreviation(&self) -> Option<String>;
    fn get_module_state(&self) -> String;
    fn get_cmis_rev(&self) -> String;
    /// Vendor name (`get_manufacturer`) / part number (`get_model`), as advertised by the
    /// module. `None` mirrors the Python api returning `None` when the field is unreadable.
    /// Used by `optics_si_parser::get_module_vendor_key` to build the per-vendor SI key.
    fn get_manufacturer(&self) -> Option<String>;
    fn get_model(&self) -> Option<String>;
    /// Application advertisement, a JSON object keyed by the (stringified) app index
    /// (`"1".."15"`) → `{host_electrical_interface_id, host_lane_count, media_lane_count,
    /// host_lane_assignment_options, media_lane_assignment_options, …}`.
    fn get_application_advertisement(&self) -> Value;
    fn get_host_lane_assignment_option(&self, appl: u32) -> u32;
    fn get_media_lane_count(&self, appl: u32) -> u32;
    fn get_media_lane_assignment_option(&self, appl: u32) -> u32;
    /// `{DP{n}State: "DataPathActivated"|…}` for host lanes 1..8.
    fn get_datapath_state(&self) -> Value;
    /// `{ConfigStatusLane{n}: "ConfigSuccess"|…}`.
    fn get_config_datapath_hostlane_status(&self) -> Value;
    /// `{DPInitPending{n}: bool}`.
    fn get_dpinit_pending(&self) -> Value;
    /// `{ActiveAppSelLane{n}: <app>}`. `Err` mirrors the Python `NotImplementedError`.
    fn get_active_apsel_hostlane(&self) -> ApiResult<Value>;
    /// The application code currently applied to host `lane` (0-based).
    fn get_application(&self, lane: u32) -> u32;

    // --- durations, milliseconds (caller divides by 1000 for seconds) ---
    fn get_datapath_init_duration(&self) -> f64;
    fn get_datapath_deinit_duration(&self) -> f64;
    fn get_datapath_tx_turnon_duration(&self) -> f64;
    fn get_datapath_tx_turnoff_duration(&self) -> f64;
    fn get_module_pwr_up_duration(&self) -> f64;
    fn get_module_pwr_down_duration(&self) -> f64;

    // --- control writes (raw page-10h register writes / bridge set_lpmode) ---
    fn set_datapath_deinit(&self, host_lanes_mask: u32) -> bool;
    fn set_datapath_init(&self, host_lanes_mask: u32) -> bool;
    fn tx_disable_channel(&self, media_lanes_mask: u32, disable: bool) -> bool;
    fn set_lpmode(&self, lpmode: bool, wait_state_change: bool) -> bool;
    fn set_application(&self, host_lanes_mask: u32, appl: u32, ec: u32) -> bool;
    fn scs_apply_datapath_init(&self, host_lanes_mask: u32) -> bool;
    /// Stage the per-vendor custom Signal-Integrity settings from `optics_si_settings.json`
    /// into the page-10h Staged Control Set for the masked host lanes (`c_cmis.
    /// stage_custom_si_settings`). Each SI control is only written when the module advertises
    /// support for it (page-01h). `true` on success; `false` aborts the AP_CONF apply.
    fn stage_custom_si_settings(&self, host_lanes_mask: u32, optics_si_dict: &Value) -> bool;

    // --- coherent (ZR) tuning: page-04h capability reads + page-12h control writes ---
    /// `(min, max)` supported Tx output power in dBm (`c_cmis.get_supported_power_config`).
    fn get_supported_power_config(&self) -> (f64, f64);
    /// The currently configured Tx output power in dBm (`c_cmis.get_tx_config_power`).
    fn get_tx_config_power(&self) -> f64;
    /// Provision the Tx output power (dBm); `true` on success (`c_cmis.set_tx_power`).
    fn set_tx_power(&self, tx_power: f64) -> bool;
    /// `(grid_supported, low_ch, hi_ch, low_freq_ghz, high_freq_ghz)`
    /// (`c_cmis.get_supported_freq_config`).
    fn get_supported_freq_config(&self) -> (u32, i64, i64, i64, i64);
    /// The currently configured laser frequency in GHz (`c_cmis.get_laser_config_freq`).
    fn get_laser_config_freq(&self) -> i64;
    /// Provision the laser frequency (GHz) on the given grid (`c_cmis.set_laser_freq`).
    fn set_laser_freq(&self, freq: i64, grid: u32) -> bool;
    /// Whether a laser tuning is in progress (`c_cmis.get_tuning_in_progress`).
    fn get_tuning_in_progress(&self) -> bool;
}

pub const CMIS_MAX_HOST_LANES_USIZE: usize = 8;

// CMIS page-10h control-register *linear* (optoe) offsets. `linear = page*128 + offset`
// (bank 0), the inverse of `sfp.py:linear_to_bpo`. Page 0x10 (SCS0) upper memory.
const SCS0_PAGE: usize = 0x10;
const DPDEINIT_OFFSET: usize = 128; // 10h:128 DataPathDeinit (1 bit / host lane)
const OUTPUT_DISABLE_TX_OFFSET: usize = 130; // 10h:130 OutputDisableTx (1 bit / lane)
const APPLY_DPINIT_OFFSET: usize = 143; // 10h:143 ApplyDPInitLane trigger
const DPCONFIG_BASE_OFFSET: usize = 145; // 10h:145..152 DPConfigLane (one byte / lane)

// CMIS Signal-Integrity control registers (c_cmis stage_custom_si_settings). The
// page-10h Staged Control Set holds the per-lane SI controls (packed `bits_per_lane`
// bits, LSB-first, two/four/eight lanes per byte); the page-01h advertisement gates
// which controls the module accepts, and the per-control max (page-01h) bounds the
// value. Offsets/bit widths mirror `mem_maps/public/cmisSFF-8636/page10.py`+`page01.py`.
const SI_ADVT_PAGE: usize = 0x01;
const TX_SI_CTRL_ADVT_OFFSET: usize = 161; // 01h:161 TX SI-control advertisement bits
const RX_SI_CTRL_ADVT_OFFSET: usize = 162; // 01h:162 RX SI-control advertisement bits
const SI_TX_MAX_OFFSET: usize = 153; // 01h:153 lo nibble = TxInputEq max, hi nibble = RxOutputLevel(amp) max
const SI_RX_EQ_MAX_OFFSET: usize = 154; // 01h:154 lo nibble = RxOutputEqPreCursor max, hi nibble = post max
// page-10h Staged Control Set 0 SI-control base offsets (per-lane packed):
const SI_ADAPTIVE_ENABLE_TX_OFFSET: usize = 153; // 1 bit/lane
const SI_ADAPTIVE_RECALL_TX_OFFSET: usize = 154; // 2 bits/lane
const SI_FIXED_INPUT_EQ_TX_OFFSET: usize = 156; // 4 bits/lane
const SI_CDR_ENABLE_TX_OFFSET: usize = 160; // 1 bit/lane
const SI_CDR_ENABLE_RX_OFFSET: usize = 161; // 1 bit/lane
const SI_OUTPUT_EQ_PRE_RX_OFFSET: usize = 162; // 4 bits/lane
const SI_OUTPUT_EQ_POST_RX_OFFSET: usize = 166; // 4 bits/lane
const SI_OUTPUT_AMP_RX_OFFSET: usize = 170; // 4 bits/lane
const CMIS_REV_LINEAR: usize = 1; // 00h:1 CMIS revision (high nibble = major)
const CMIS_FLAT_MEM_LINEAR: usize = 2; // 00h:2 status; bit7 = FlatMem (c_cmis.is_flat_memory)
const CMIS_FLAT_MEM_BIT: u8 = 0x80;

// CMIS page-11h Active Control Set. `get_active_apsel_hostlane`/`get_application` read
// the *provisioned* (active) DPConfigLane bytes (11h:206..213); the AppSelCode is the
// upper nibble (bits 7:4) of each host lane's byte.
const ACS_PAGE: usize = 0x11;
const ACS_DPCONFIG_BASE_OFFSET: usize = 206; // 11h:206..213 ActiveControlSet DPConfigLane

// CMIS page-01h State Machine Durations Advertising (CMIS v5.2 §8.3.7 / §8.4.7). Each
// max-duration is a 4-bit code (Table 8-43) packed two per byte. `c_cmis.py` decodes the
// code with `CmisCodes.DP_PATH_TIMINGS` (code -> milliseconds); we read the raw page-01h
// byte and re-apply that same fixed lookup — no module-specific interpretation, so CMIS
// decode still effectively lives "in Python".
const DURATIONS_PAGE: usize = 0x01;
const DP_INIT_DEINIT_OFFSET: usize = 144; // 01h:144 hi=MaxDurationDPDeinit, lo=MaxDurationDPInit
const MODULE_PWR_OFFSET: usize = 167; // 01h:167 hi=MaxDurationModulePwrDn, lo=MaxDurationModulePwrUp
const DP_TX_TURN_OFFSET: usize = 168; // 01h:168 hi=MaxDurationDPTxTurnOff, lo=MaxDurationDPTxTurnOn

// `c_cmis.get_datapath_init_duration` scales a short (<=1000 ms) advertised DPInit value ×10.
const DATAPATH_INIT_DURATION_MULTIPLIER: f64 = 10.0;
const DATAPATH_INIT_DURATION_OVERRIDE_THRESHOLD: f64 = 1000.0;

// CMIS coherent (ZR) tuning registers. The capability advertisement is the non-banked
// page 04h (Laser Capabilities); the tuning control set is the banked page 12h (bank 0).
// Powers are signed shorts scaled ×100 (dBm); channels are signed shorts; grid/flag bytes
// are raw (`api/public/c_cmis.py` + `lib/coherent.py`).
const LASER_CAP_PAGE: usize = 0x04;
const SUPPORT_GRID_OFFSET: usize = 128; // 04h:128 (1 byte; bit7=75GHz, bit5=100GHz)
const LOW_CHANNEL_OFFSET: usize = 158; // 04h:158 (>h channel number)
const HIGH_CHANNEL_OFFSET: usize = 160; // 04h:160 (>h channel number)
const MIN_PROG_POWER_OFFSET: usize = 198; // 04h:198 (>h, ×100 → dBm)
const MAX_PROG_POWER_OFFSET: usize = 200; // 04h:200 (>h, ×100 → dBm)
const TUNE_PAGE: usize = 0x12;
const GRID_SPACING_OFFSET: usize = 128; // 12h:128 (1 byte; 0x70=75GHz, 0x50=100GHz, 0x80=150GHz)
const LASER_CONFIG_CHANNEL_OFFSET: usize = 136; // 12h:136 (>h channel number)
const TX_CONFIG_POWER_OFFSET: usize = 200; // 12h:200 (>h, ×100 → dBm)
const COHERENT_POWER_SCALE: f64 = 100.0;
const FREQ_BASE_GHZ: i64 = 193100;
const FREQ_75GHZ_STEP_GHZ: i64 = 25;

/// `CmisCodes.DP_PATH_TIMINGS` (codes_cmis.py) — 4-bit state-machine duration code →
/// milliseconds. Codes 14/15 are reserved (0).
const fn dp_path_timing_ms(code: u8) -> f64 {
    match code & 0x0f {
        0 => 1.0,
        1 => 5.0,
        2 => 10.0,
        3 => 50.0,
        4 => 100.0,
        5 => 500.0,
        6 => 1000.0,
        7 => 5000.0,
        8 => 10000.0,
        9 => 60000.0,
        10 => 300000.0,
        11 => 600000.0,
        12 => 3000000.0,
        13 => 6000000.0,
        _ => 0.0,
    }
}

/// `linear = page*128 + offset` (bank 0) — matches `sfp.py:linear_to_bpo` inverse.
const fn cmis_linear(page: usize, offset: usize) -> usize {
    if page == 0 && offset < 128 {
        offset
    } else {
        page * 128 + offset
    }
}

// =====================================================================================
// BridgeCmisApi — production impl over a bridge HAL Sfp handle.
// =====================================================================================

/// Production [`CmisApi`] backed by a HAL [`Sfp`]: decode reads come from the Python
/// `get_transceiver_status()`/`get_transceiver_info()` getters; control writes are the raw
/// page-10h register writes (`c_cmis.py` encodings) via `write_eeprom`.
pub struct BridgeCmisApi {
    sfp: Box<dyn Sfp>,
}

impl BridgeCmisApi {
    pub fn new(sfp: Box<dyn Sfp>) -> Self {
        BridgeCmisApi { sfp }
    }

    fn status(&self) -> Value {
        self.sfp.get_transceiver_status().unwrap_or_else(|_| json!({}))
    }

    fn info(&self) -> Value {
        self.sfp.get_transceiver_info().unwrap_or_else(|_| json!({}))
    }

    fn read_byte(&self, linear: usize) -> Option<u8> {
        self.sfp.read_eeprom(linear, 1).ok().flatten().and_then(|v| v.first().copied())
    }

    /// Read a page-01h duration advertisement nibble and map it to milliseconds via
    /// `DP_PATH_TIMINGS`. `high_nibble` selects bits 7:4 (the odd field of the pair) vs
    /// bits 3:0. `None` mirrors `c_cmis`'s unreadable-advertisement → 0 behaviour.
    fn duration_code_ms(&self, offset: usize, high_nibble: bool) -> Option<f64> {
        let byte = self.read_byte(cmis_linear(DURATIONS_PAGE, offset))?;
        let code = if high_nibble { byte >> 4 } else { byte & 0x0f };
        Some(dp_path_timing_ms(code))
    }

    fn write_byte(&self, linear: usize, byte: u8) -> bool {
        self.sfp.write_eeprom(linear, &[byte]).unwrap_or(false)
    }

    /// Read a page-01h SI-advertisement bit (`true` = the module accepts that SI control).
    /// A failed read → `false` (skip the control), mirroring `xcvr_eeprom.read()` → `None`.
    fn si_support_bit(&self, offset: usize, bit: u8) -> bool {
        self.read_byte(cmis_linear(SI_ADVT_PAGE, offset))
            .map(|b| (b >> bit) & 1 != 0)
            .unwrap_or(false)
    }

    /// Read a page-01h 4-bit max-value nibble (`hi` selects bits 7:4 else 3:0). `None` on a
    /// failed read — the caller treats that as "no max advertised" and aborts (c_cmis returns
    /// `False` when `get_*_max_val()` is `None`).
    fn si_max_nibble(&self, offset: usize, hi: bool) -> Option<u8> {
        let byte = self.read_byte(cmis_linear(SI_ADVT_PAGE, offset))?;
        Some(if hi { byte >> 4 } else { byte & 0x0f })
    }

    /// Stage a per-lane SI control into the page-10h Staged Control Set: for each masked host
    /// lane read the base byte, replace the lane's `bits_per_lane`-wide field with its value,
    /// and write it back (the read/modify/write `xcvr_eeprom.write(si_param_lane, val)` does).
    /// `max` bounds each value (`None` = unbounded, e.g. CDR-enable). Returns `false` if a lane
    /// value is missing/non-integer/out-of-range or an EEPROM access fails.
    fn write_packed_si(
        &self,
        base_offset: usize,
        bits_per_lane: usize,
        host_lanes_mask: u32,
        si_param: &str,
        lane_vals: &Value,
        max: Option<u8>,
    ) -> bool {
        let lanes_per_byte = 8 / bits_per_lane;
        let field_mask: u32 = (1u32 << bits_per_lane) - 1;
        for lane in 0..CMIS_MAX_HOST_LANES_USIZE {
            if (host_lanes_mask >> lane) & 1 == 0 {
                continue;
            }
            let lane1 = lane + 1;
            let key = format!("{si_param}{lane1}");
            let val = match lane_vals.get(&key).and_then(|v| v.as_i64()) {
                Some(v) if v >= 0 => v as u32,
                _ => return false,
            };
            if let Some(m) = max {
                if val > m as u32 {
                    return false;
                }
            }
            if val > field_mask {
                return false;
            }
            let byte_off = base_offset + (lane1 - 1) / lanes_per_byte;
            let bitpos = ((lane1 - 1) % lanes_per_byte) * bits_per_lane;
            let lin = cmis_linear(SCS0_PAGE, byte_off);
            let cur = self.read_byte(lin).unwrap_or(0) as u32;
            let updated = (cur & !(field_mask << bitpos)) | ((val & field_mask) << bitpos);
            if !self.write_byte(lin, updated as u8) {
                return false;
            }
        }
        true
    }

    /// Stage one RX SI parameter (`c_cmis.stage_rx_si_settings` per-param branch): gate on the
    /// page-01h RX advertisement, validate against the advertised max, then write page-10h.
    fn stage_rx_si_param(&self, si_param: &str, host_lanes_mask: u32, lane_vals: &Value) -> bool {
        match si_param {
            "OutputEqPreCursorTargetRx" => {
                if !self.si_support_bit(RX_SI_CTRL_ADVT_OFFSET, 3) {
                    return true;
                }
                match self.si_max_nibble(SI_RX_EQ_MAX_OFFSET, false) {
                    Some(m) => self.write_packed_si(
                        SI_OUTPUT_EQ_PRE_RX_OFFSET, 4, host_lanes_mask, si_param, lane_vals, Some(m),
                    ),
                    None => false,
                }
            }
            "OutputEqPostCursorTargetRx" => {
                if !self.si_support_bit(RX_SI_CTRL_ADVT_OFFSET, 4) {
                    return true;
                }
                match self.si_max_nibble(SI_RX_EQ_MAX_OFFSET, true) {
                    Some(m) => self.write_packed_si(
                        SI_OUTPUT_EQ_POST_RX_OFFSET, 4, host_lanes_mask, si_param, lane_vals, Some(m),
                    ),
                    None => false,
                }
            }
            "OutputAmplitudeTargetRx" => {
                if !self.si_support_bit(RX_SI_CTRL_ADVT_OFFSET, 2) {
                    return true;
                }
                match self.si_max_nibble(SI_TX_MAX_OFFSET, true) {
                    Some(m) => self.write_packed_si(
                        SI_OUTPUT_AMP_RX_OFFSET, 4, host_lanes_mask, si_param, lane_vals, Some(m),
                    ),
                    None => false,
                }
            }
            "CDREnableRx" => {
                if !self.si_support_bit(RX_SI_CTRL_ADVT_OFFSET, 0) {
                    return true;
                }
                self.write_packed_si(
                    SI_CDR_ENABLE_RX_OFFSET, 1, host_lanes_mask, si_param, lane_vals, None,
                )
            }
            _ => false,
        }
    }

    /// Stage one TX SI parameter (`c_cmis.stage_tx_si_settings` per-param branch).
    fn stage_tx_si_param(&self, si_param: &str, host_lanes_mask: u32, lane_vals: &Value) -> bool {
        match si_param {
            "FixedInputEqTargetTx" => {
                if !self.si_support_bit(TX_SI_CTRL_ADVT_OFFSET, 2) {
                    return true;
                }
                match self.si_max_nibble(SI_TX_MAX_OFFSET, false) {
                    Some(m) => self.write_packed_si(
                        SI_FIXED_INPUT_EQ_TX_OFFSET, 4, host_lanes_mask, si_param, lane_vals, Some(m),
                    ),
                    None => false,
                }
            }
            "AdaptiveInputEqRecalledTx" => {
                if !(self.si_support_bit(TX_SI_CTRL_ADVT_OFFSET, 5)
                    || self.si_support_bit(TX_SI_CTRL_ADVT_OFFSET, 6))
                {
                    return true;
                }
                self.write_packed_si(
                    SI_ADAPTIVE_RECALL_TX_OFFSET, 2, host_lanes_mask, si_param, lane_vals, None,
                )
            }
            "AdaptiveInputEqEnableTx" => {
                if !self.si_support_bit(TX_SI_CTRL_ADVT_OFFSET, 3) {
                    return true;
                }
                self.write_packed_si(
                    SI_ADAPTIVE_ENABLE_TX_OFFSET, 1, host_lanes_mask, si_param, lane_vals, None,
                )
            }
            "CDREnableTx" => {
                if !self.si_support_bit(TX_SI_CTRL_ADVT_OFFSET, 0) {
                    return true;
                }
                self.write_packed_si(
                    SI_CDR_ENABLE_TX_OFFSET, 1, host_lanes_mask, si_param, lane_vals, None,
                )
            }
            _ => false,
        }
    }

    /// Read a big-endian signed 16-bit value (`>h`) at `linear`. `None` on a HAL error —
    /// the coherent tuning reads treat that as the cleared/unprovisioned default.
    fn read_i16(&self, linear: usize) -> Option<i16> {
        let bytes = self.sfp.read_eeprom(linear, 2).ok().flatten()?;
        if bytes.len() < 2 {
            return None;
        }
        Some(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Write a big-endian signed 16-bit value (`>h`) at `linear`.
    fn write_i16(&self, linear: usize, value: i16) -> bool {
        self.sfp.write_eeprom(linear, &value.to_be_bytes()).unwrap_or(false)
    }

    /// CMIS major revision (defaults to 5 — the emulator/testbed is CMIS 5.x — when the
    /// register read fails; picks the v4+ deinit/init bit polarity).
    fn cmis_major(&self) -> u8 {
        self.read_byte(CMIS_REV_LINEAR).map(|b| b >> 4).unwrap_or(5)
    }
}

impl CmisApi for BridgeCmisApi {
    fn is_flat_memory(&self) -> bool {
        // `c_cmis.CmisApi.is_flat_memory` reads CMIS 00h:2 bit 7 (FlatMem), in lower memory
        // (accessible even on a flat module). Fall back to a heuristic only when the raw read
        // is unavailable.
        if let Some(b) = self.read_byte(CMIS_FLAT_MEM_LINEAR) {
            return b & CMIS_FLAT_MEM_BIT != 0;
        }
        self.status().get("module_state").is_none() && self.info().get("cmis_rev").is_none()
    }

    fn is_coherent_module(&self) -> bool {
        // c_cmis.CCmisApi.is_coherent_module = 'ZR' in get_module_media_interface(). Only a
        // coherent module is served by CCmisApi, whose get_transceiver_info() adds the
        // coherent-only markers (supported_max_laser_freq …).
        self.info().get("supported_max_laser_freq").is_some()
    }

    fn get_module_type_abbreviation(&self) -> Option<String> {
        self.info()
            .get("type_abbrv_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    fn get_module_state(&self) -> String {
        self.status()
            .get("module_state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn get_cmis_rev(&self) -> String {
        self.info()
            .get("cmis_rev")
            .and_then(|v| v.as_str())
            .unwrap_or("5.0")
            .to_string()
    }

    fn get_manufacturer(&self) -> Option<String> {
        // TRANSCEIVER_INFO carries the module's advertised vendor name/part number
        // (get_transceiver_info() -> manufacturer/model). "N/A" means unreadable → None.
        self.info()
            .get("manufacturer")
            .and_then(|v| v.as_str())
            .filter(|s| *s != "N/A")
            .map(|s| s.to_string())
    }

    fn get_model(&self) -> Option<String> {
        self.info()
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| *s != "N/A")
            .map(|s| s.to_string())
    }

    fn get_application_advertisement(&self) -> Value {
        // TRANSCEIVER_INFO carries the advertisement as a Python-dict *repr* string
        // (`str(get_application_advertisement())`); parse it back to JSON keyed by app index.
        match self.info().get("application_advertisement") {
            Some(Value::String(s)) if s != "N/A" => py_repr_to_json(s).unwrap_or_else(|| json!({})),
            _ => json!({}),
        }
    }

    fn get_host_lane_assignment_option(&self, appl: u32) -> u32 {
        advert_field_u32(&self.get_application_advertisement(), appl, "host_lane_assignment_options", 0)
    }

    fn get_media_lane_count(&self, appl: u32) -> u32 {
        advert_field_u32(&self.get_application_advertisement(), appl, "media_lane_count", 1)
    }

    fn get_media_lane_assignment_option(&self, appl: u32) -> u32 {
        advert_field_u32(&self.get_application_advertisement(), appl, "media_lane_assignment_options", 1)
    }

    fn get_datapath_state(&self) -> Value {
        let st = self.status();
        let mut m = serde_json::Map::new();
        for n in 1..=CMIS_MAX_HOST_LANES_USIZE {
            let v = st.get(format!("DP{n}State")).cloned().unwrap_or(Value::Null);
            m.insert(format!("DP{n}State"), v);
        }
        Value::Object(m)
    }

    fn get_config_datapath_hostlane_status(&self) -> Value {
        let st = self.status();
        let mut m = serde_json::Map::new();
        for n in 1..=CMIS_MAX_HOST_LANES_USIZE {
            let v = st
                .get(format!("config_state_hostlane{n}"))
                .cloned()
                .unwrap_or(Value::Null);
            m.insert(format!("ConfigStatusLane{n}"), v);
        }
        Value::Object(m)
    }

    fn get_dpinit_pending(&self) -> Value {
        let st = self.status();
        let mut m = serde_json::Map::new();
        for n in 1..=CMIS_MAX_HOST_LANES_USIZE {
            let v = st
                .get(format!("dpinit_pending_hostlane{n}"))
                .cloned()
                .unwrap_or(Value::Bool(false));
            m.insert(format!("DPInitPending{n}"), v);
        }
        Value::Object(m)
    }

    fn get_active_apsel_hostlane(&self) -> ApiResult<Value> {
        // c_cmis.get_active_apsel_hostlane: read the Active Control Set DPConfigLane bytes
        // (page 11h:206..213); the ApSel code applied to host lane <n> is the upper nibble
        // (bits 7:4). A failed read defaults to 0 ("no active app").
        let mut m = serde_json::Map::new();
        for lane in 0..CMIS_MAX_HOST_LANES_USIZE {
            let lin = cmis_linear(ACS_PAGE, ACS_DPCONFIG_BASE_OFFSET + lane);
            let apsel = self.read_byte(lin).map(|b| u64::from(b >> 4)).unwrap_or(0);
            m.insert(format!("ActiveAppSelLane{}", lane + 1), Value::from(apsel));
        }
        Ok(Value::Object(m))
    }

    fn get_application(&self, lane: u32) -> u32 {
        let lin = cmis_linear(ACS_PAGE, ACS_DPCONFIG_BASE_OFFSET + lane as usize);
        self.read_byte(lin).map(|b| u32::from(b >> 4)).unwrap_or(0)
    }

    fn get_datapath_init_duration(&self) -> f64 {
        if self.is_flat_memory() {
            return 0.0;
        }
        match self.duration_code_ms(DP_INIT_DEINIT_OFFSET, false) {
            None => 0.0,
            Some(v) if v <= DATAPATH_INIT_DURATION_OVERRIDE_THRESHOLD => {
                v * DATAPATH_INIT_DURATION_MULTIPLIER
            }
            Some(v) => v,
        }
    }
    fn get_datapath_deinit_duration(&self) -> f64 {
        if self.is_flat_memory() {
            return 0.0;
        }
        self.duration_code_ms(DP_INIT_DEINIT_OFFSET, true).unwrap_or(0.0)
    }
    fn get_datapath_tx_turnon_duration(&self) -> f64 {
        if self.is_flat_memory() {
            return 0.0;
        }
        self.duration_code_ms(DP_TX_TURN_OFFSET, false).unwrap_or(0.0)
    }
    fn get_datapath_tx_turnoff_duration(&self) -> f64 {
        if self.is_flat_memory() {
            return 0.0;
        }
        self.duration_code_ms(DP_TX_TURN_OFFSET, true).unwrap_or(0.0)
    }
    fn get_module_pwr_up_duration(&self) -> f64 {
        if self.is_flat_memory() {
            return 0.0;
        }
        self.duration_code_ms(MODULE_PWR_OFFSET, false).unwrap_or(0.0)
    }
    fn get_module_pwr_down_duration(&self) -> f64 {
        if self.is_flat_memory() {
            return 0.0;
        }
        self.duration_code_ms(MODULE_PWR_OFFSET, true).unwrap_or(0.0)
    }

    fn set_datapath_deinit(&self, host_lanes_mask: u32) -> bool {
        // c_cmis.set_datapath_deinit: read DataPathDeinit; v4+ SET the masked lane bits
        // (v3 clears). Emulator is CMIS 5.x → v4+.
        let lin = cmis_linear(SCS0_PAGE, DPDEINIT_OFFSET);
        let mut data = self.read_byte(lin).unwrap_or(0);
        for lane in 0..CMIS_MAX_HOST_LANES_USIZE {
            if (host_lanes_mask >> lane) & 1 == 0 {
                continue;
            }
            if self.cmis_major() >= 4 {
                data |= 1 << lane;
            } else {
                data &= !(1 << lane);
            }
        }
        self.write_byte(lin, data)
    }

    fn set_datapath_init(&self, host_lanes_mask: u32) -> bool {
        // c_cmis.set_datapath_init: v4+ CLEARS the masked DataPathDeinit lane bits.
        let lin = cmis_linear(SCS0_PAGE, DPDEINIT_OFFSET);
        let mut data = self.read_byte(lin).unwrap_or(0);
        for lane in 0..CMIS_MAX_HOST_LANES_USIZE {
            if (host_lanes_mask >> lane) & 1 == 0 {
                continue;
            }
            if self.cmis_major() >= 4 {
                data &= !(1 << lane);
            } else {
                data |= 1 << lane;
            }
        }
        self.write_byte(lin, data)
    }

    fn tx_disable_channel(&self, media_lanes_mask: u32, disable: bool) -> bool {
        // c_cmis.tx_disable_channel: read OutputDisableTx, set/clear masked lane bits.
        let lin = cmis_linear(SCS0_PAGE, OUTPUT_DISABLE_TX_OFFSET);
        let mut state = self.read_byte(lin).unwrap_or(0);
        for i in 0..CMIS_MAX_HOST_LANES_USIZE {
            if (media_lanes_mask >> i) & 1 == 0 {
                continue;
            }
            if disable {
                state |= 1 << i;
            } else {
                state &= !(1 << i);
            }
        }
        self.write_byte(lin, state)
    }

    fn set_lpmode(&self, lpmode: bool, _wait_state_change: bool) -> bool {
        // Delegate to the bridge's own set_lpmode (Python CMIS decode owns the bit math).
        self.sfp.set_lpmode(lpmode).unwrap_or(false)
    }

    fn set_application(&self, host_lanes_mask: u32, appl: u32, ec: u32) -> bool {
        // c_cmis.set_application: for each masked host lane, write DPConfigLane =
        // (appl<<4) | (lane_first<<1) | ec, where lane_first is the first masked lane.
        let mut lane_first: i32 = -1;
        let mut ok = true;
        for lane in 0..CMIS_MAX_HOST_LANES_USIZE {
            if (host_lanes_mask >> lane) & 1 == 0 {
                continue;
            }
            if lane_first < 0 {
                lane_first = lane as i32;
            }
            let data = ((appl << 4) | ((lane_first as u32) << 1) | ec) as u8;
            let lin = cmis_linear(SCS0_PAGE, DPCONFIG_BASE_OFFSET + lane);
            ok &= self.write_byte(lin, data);
        }
        ok
    }

    fn scs_apply_datapath_init(&self, host_lanes_mask: u32) -> bool {
        // c_cmis.scs_apply_datapath_init: write ApplyDPInitLane = mask.
        let lin = cmis_linear(SCS0_PAGE, APPLY_DPINIT_OFFSET);
        self.write_byte(lin, host_lanes_mask as u8)
    }

    fn stage_custom_si_settings(&self, host_lanes_mask: u32, optics_si_dict: &Value) -> bool {
        // c_cmis.stage_custom_si_settings: split the SI dict into Tx/Rx params (by the
        // parameter-name suffix), then stage each into the page-10h Staged Control Set —
        // but only for the controls the module advertises support for (page-01h). A value
        // above the advertised max, or a missing/unreadable advertisement, aborts the apply.
        let obj = match optics_si_dict.as_object() {
            Some(o) => o,
            None => return true,
        };
        for (si_param, lane_vals) in obj {
            let ok = if si_param.ends_with("Rx") {
                self.stage_rx_si_param(si_param, host_lanes_mask, lane_vals)
            } else if si_param.ends_with("Tx") {
                self.stage_tx_si_param(si_param, host_lanes_mask, lane_vals)
            } else {
                // Neither Tx nor Rx: c_cmis's split silently drops it (staged as neither).
                continue;
            };
            if !ok {
                return false;
            }
        }
        true
    }

    fn get_supported_power_config(&self) -> (f64, f64) {
        // c_cmis.get_supported_power_config: (MIN_PROG_OUTPUT_POWER, MAX_PROG_OUTPUT_POWER),
        // both signed shorts scaled ×100 → dBm (page 04h Laser Capabilities).
        let min_p = self
            .read_i16(cmis_linear(LASER_CAP_PAGE, MIN_PROG_POWER_OFFSET))
            .map(|v| v as f64 / COHERENT_POWER_SCALE)
            .unwrap_or(0.0);
        let max_p = self
            .read_i16(cmis_linear(LASER_CAP_PAGE, MAX_PROG_POWER_OFFSET))
            .map(|v| v as f64 / COHERENT_POWER_SCALE)
            .unwrap_or(0.0);
        (min_p, max_p)
    }

    fn get_tx_config_power(&self) -> f64 {
        // c_cmis.get_tx_config_power: TX_CONFIG_POWER (page 12h), signed short ×100 → dBm.
        self.read_i16(cmis_linear(TUNE_PAGE, TX_CONFIG_POWER_OFFSET))
            .map(|v| v as f64 / COHERENT_POWER_SCALE)
            .unwrap_or(0.0)
    }

    fn set_tx_power(&self, tx_power: f64) -> bool {
        // c_cmis.set_tx_power: write TX_CONFIG_POWER (page 12h), dBm encoded as a ×100 short.
        let raw = (tx_power * COHERENT_POWER_SCALE).round() as i16;
        self.write_i16(cmis_linear(TUNE_PAGE, TX_CONFIG_POWER_OFFSET), raw)
    }

    fn get_supported_freq_config(&self) -> (u32, i64, i64, i64, i64) {
        // c_cmis.get_supported_freq_config: SUPPORT_GRID byte + LOW/HIGH channel shorts,
        // frequencies derived as 193100 + ch*25 GHz.
        let grid = self.read_byte(cmis_linear(LASER_CAP_PAGE, SUPPORT_GRID_OFFSET)).unwrap_or(0) as u32;
        let low_ch = self.read_i16(cmis_linear(LASER_CAP_PAGE, LOW_CHANNEL_OFFSET)).unwrap_or(0) as i64;
        let hi_ch = self.read_i16(cmis_linear(LASER_CAP_PAGE, HIGH_CHANNEL_OFFSET)).unwrap_or(0) as i64;
        let low_freq = FREQ_BASE_GHZ + low_ch * FREQ_75GHZ_STEP_GHZ;
        let high_freq = FREQ_BASE_GHZ + hi_ch * FREQ_75GHZ_STEP_GHZ;
        (grid, low_ch, hi_ch, low_freq, high_freq)
    }

    fn get_laser_config_freq(&self) -> i64 {
        // c_cmis.get_laser_config_freq: decode GRID_SPACING → grid, then LASER_CONFIG_CHANNEL.
        let grid_byte = self.read_byte(cmis_linear(TUNE_PAGE, GRID_SPACING_OFFSET)).unwrap_or(0);
        let freq_grid: i64 = match grid_byte {
            0x70 => 75,
            0x50 => 100,
            0x80 => 150,
            other => other as i64,
        };
        let channel = self.read_i16(cmis_linear(TUNE_PAGE, LASER_CONFIG_CHANNEL_OFFSET)).unwrap_or(0) as i64;
        match freq_grid {
            75 => FREQ_BASE_GHZ + channel * FREQ_75GHZ_STEP_GHZ,
            150 => FREQ_BASE_GHZ + (channel + 3) * FREQ_75GHZ_STEP_GHZ,
            g => FREQ_BASE_GHZ + channel * g,
        }
    }

    fn set_laser_freq(&self, freq: i64, grid: u32) -> bool {
        // c_cmis.set_laser_freq: pick the grid byte + channel, write GRID_SPACING then
        // LASER_CONFIG_CHANNEL (page 12h). Python `assert`/`raise` become `false` returns
        // here so a bad frequency never aborts the daemon thread.
        let (grid_supported, low_ch, hi_ch, _, _) = self.get_supported_freq_config();
        let grid_75 = (grid_supported >> 7) & 0x1 == 1;
        let grid_100 = (grid_supported >> 5) & 0x1 == 1;
        let (freq_grid, channel): (u8, i64) = match grid {
            75 => {
                if !grid_75 {
                    return false;
                }
                let ch = ((freq - FREQ_BASE_GHZ) as f64 / 25.0).round() as i64;
                if ch % 3 != 0 {
                    return false;
                }
                (0x70, ch)
            }
            100 => {
                if !grid_100 {
                    return false;
                }
                let ch = ((freq - FREQ_BASE_GHZ) as f64 / 100.0).round() as i64;
                (0x50, ch)
            }
            150 => {
                let ch = ((freq - FREQ_BASE_GHZ) as f64 / 25.0).round() as i64 - 3;
                if ch % 6 != 0 {
                    return false;
                }
                (0x80, ch)
            }
            _ => return false,
        };
        self.write_byte(cmis_linear(TUNE_PAGE, GRID_SPACING_OFFSET), freq_grid);
        if channel > hi_ch || channel < low_ch {
            return false;
        }
        self.write_i16(cmis_linear(TUNE_PAGE, LASER_CONFIG_CHANNEL_OFFSET), channel as i16)
    }

    fn get_tuning_in_progress(&self) -> bool {
        // c_cmis reads consts.TUNING_IN_PROGRESS (a media-lane status latch) only to emit a
        // warning; the xcvr emulator never sets it, so a `false` read is observably correct.
        false
    }
}

/// Coerce a JSON scalar (number or numeric string) to `u32` — used to read the
/// `ActiveAppSelLane*` values, which the bridge may report as ints or strings.
pub(crate) fn json_as_u32(v: &Value) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    if let Some(n) = v.as_i64() {
        return u32::try_from(n).ok();
    }
    if let Some(s) = v.as_str() {
        return s.trim().parse::<u32>().ok();
    }
    None
}

fn advert_field_u32(advert: &Value, appl: u32, field: &str, default: u32) -> u32 {
    advert
        .get(appl.to_string())
        .and_then(|app| app.get(field))
        .and_then(json_as_u32)
        .unwrap_or(default)
}

/// Convert a Python dict *repr* (from `str(get_application_advertisement())`) to JSON.
/// The emulator output is well-formed: single-quoted strings, integer app keys, integer
/// counts, `True`/`False`/`None` literals. Best-effort — used only by [`BridgeCmisApi`].
fn py_repr_to_json(s: &str) -> Option<Value> {
    let mut dq = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        if c == '\'' {
            dq.push('"');
        } else {
            dq.push(c);
        }
    }
    let dq = dq.replace("True", "true").replace("False", "false").replace("None", "null");
    let json = quote_int_keys(&dq);
    serde_json::from_str(&json).ok()
}

fn quote_int_keys(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 16);
    let mut i = 0usize;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '"' {
            in_str = !in_str;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_str && c.is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                j += 1;
            }
            let mut k = j;
            while k < bytes.len() && (bytes[k] as char) == ' ' {
                k += 1;
            }
            let prev = out.trim_end().chars().last();
            let is_key = matches!(prev, Some('{') | Some(','))
                && k < bytes.len()
                && (bytes[k] as char) == ':';
            if is_key {
                out.push('"');
                out.push_str(&s[i..j]);
                out.push('"');
            } else {
                out.push_str(&s[i..j]);
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

// =====================================================================================
// MockCmisApi — test double (canned decode + settable/sequenced state + counters).
// =====================================================================================

struct MockInner {
    is_flat_memory: bool,
    is_coherent_module: bool,
    module_type_abbreviation: Option<String>,
    module_state: String,
    cmis_rev: String,
    application_advertisement: Value,
    datapath_state: Value,
    config_status: Value,
    dpinit_pending: Value,
    active_apsel: Value,
    active_apsel_queue: VecDeque<ApiResult<Value>>,
    application_by_lane: u32,
    application_by_lane_map: HashMap<u32, u32>,
    media_lane_count_override: Option<u32>,
    dp_init_dur: f64,
    dp_deinit_dur: f64,
    dp_txon_dur: f64,
    dp_txoff_dur: f64,
    pwr_up_dur: f64,
    pwr_down_dur: f64,
    calls: HashMap<String, usize>,
    last_set_application_ec: u32,
    last_deinit_mask: u32,
    last_tx_disable_mask: u32,
    tx_disable_result: bool,
    scs_apply_result: bool,
    supported_power_config: (f64, f64),
    tx_config_power: f64,
    set_tx_power_result: bool,
    last_set_tx_power: Option<f64>,
    supported_freq_config: (u32, i64, i64, i64, i64),
    laser_config_freq: i64,
    set_laser_freq_result: bool,
    last_set_laser_freq: Option<(i64, u32)>,
    tuning_in_progress: bool,
    manufacturer: Option<String>,
    model: Option<String>,
    stage_si_result: bool,
    last_staged_si: Option<Value>,
    last_staged_si_mask: u32,
}

impl Default for MockInner {
    fn default() -> Self {
        MockInner {
            is_flat_memory: false,
            is_coherent_module: false,
            module_type_abbreviation: Some("QSFP-DD".to_string()),
            module_state: "ModuleReady".to_string(),
            cmis_rev: "5.0".to_string(),
            application_advertisement: json!({}),
            datapath_state: json!({}),
            config_status: json!({}),
            dpinit_pending: json!({}),
            active_apsel: json!({}),
            active_apsel_queue: VecDeque::new(),
            application_by_lane: 0,
            application_by_lane_map: HashMap::new(),
            media_lane_count_override: None,
            dp_init_dur: 5_000.0,
            dp_deinit_dur: 1.0,
            dp_txon_dur: 1.0,
            dp_txoff_dur: 1.0,
            pwr_up_dur: 1.0,
            pwr_down_dur: 1.0,
            calls: HashMap::new(),
            last_set_application_ec: 0,
            last_deinit_mask: 0,
            last_tx_disable_mask: 0,
            tx_disable_result: true,
            scs_apply_result: true,
            supported_power_config: (0.0, 0.0),
            tx_config_power: 0.0,
            set_tx_power_result: true,
            last_set_tx_power: None,
            supported_freq_config: (0, 0, 0, 0, 0),
            laser_config_freq: 0,
            set_laser_freq_result: true,
            last_set_laser_freq: None,
            tuning_in_progress: false,
            manufacturer: Some("Credo".to_string()),
            model: Some("CAC82X321HW".to_string()),
            stage_si_result: true,
            last_staged_si: None,
            last_staged_si_mask: 0,
        }
    }
}

/// Mock [`CmisApi`] — the Rust analogue of the Python tests' `MagicMock()` xcvr
/// api. Interior-mutable + `Clone` (shares one `Arc<Mutex>`), so the api the state
/// machine obtains and the handle the test drives observe the same call counters and
/// sequenced/settable return values.
#[derive(Clone)]
pub struct MockCmisApi {
    inner: Arc<Mutex<MockInner>>,
}

impl Default for MockCmisApi {
    fn default() -> Self {
        MockCmisApi {
            inner: Arc::new(Mutex::new(MockInner::default())),
        }
    }
}

impl MockCmisApi {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&self, name: &str) {
        let mut g = self.inner.lock().unwrap();
        *g.calls.entry(name.to_string()).or_insert(0) += 1;
    }

    /// How many times `method` was invoked on this api (across clones).
    pub fn call_count(&self, method: &str) -> usize {
        *self.inner.lock().unwrap().calls.get(method).unwrap_or(&0)
    }

    // --- builders / setters ---
    pub fn set_flat_memory(&self, v: bool) {
        self.inner.lock().unwrap().is_flat_memory = v;
    }
    pub fn set_coherent_module(&self, v: bool) {
        self.inner.lock().unwrap().is_coherent_module = v;
    }
    pub fn set_module_type_abbreviation(&self, v: Option<&str>) {
        self.inner.lock().unwrap().module_type_abbreviation = v.map(|s| s.to_string());
    }
    pub fn set_module_state(&self, v: &str) {
        self.inner.lock().unwrap().module_state = v.to_string();
    }
    pub fn set_cmis_rev(&self, v: &str) {
        self.inner.lock().unwrap().cmis_rev = v.to_string();
    }
    pub fn set_application_advertisement(&self, v: Value) {
        self.inner.lock().unwrap().application_advertisement = v;
    }
    pub fn set_datapath_state_value(&self, v: Value) {
        self.inner.lock().unwrap().datapath_state = v;
    }
    pub fn set_config_status(&self, v: Value) {
        self.inner.lock().unwrap().config_status = v;
    }
    pub fn set_dpinit_pending(&self, v: Value) {
        self.inner.lock().unwrap().dpinit_pending = v;
    }
    pub fn set_active_apsel(&self, v: Value) {
        self.inner.lock().unwrap().active_apsel = v;
    }
    /// Queue a sequenced `get_active_apsel_hostlane` result (`MagicMock(side_effect=[...])`).
    pub fn push_active_apsel_result(&self, r: ApiResult<Value>) {
        self.inner.lock().unwrap().active_apsel_queue.push_back(r);
    }
    pub fn set_application_by_lane(&self, appl: u32) {
        self.inner.lock().unwrap().application_by_lane = appl;
    }
    pub fn set_application_for_lane(&self, lane: u32, appl: u32) {
        self.inner.lock().unwrap().application_by_lane_map.insert(lane, appl);
    }
    pub fn set_media_lane_count_override(&self, v: Option<u32>) {
        self.inner.lock().unwrap().media_lane_count_override = v;
    }
    /// The explicit-control (`ec`) argument captured by the last `set_application` call.
    pub fn last_set_application_ec(&self) -> u32 {
        self.inner.lock().unwrap().last_set_application_ec
    }
    /// The host-lanes mask captured by the last `set_datapath_deinit` call.
    pub fn last_deinit_mask(&self) -> u32 {
        self.inner.lock().unwrap().last_deinit_mask
    }
    /// The media-lanes mask captured by the last `tx_disable_channel` call.
    pub fn last_tx_disable_mask(&self) -> u32 {
        self.inner.lock().unwrap().last_tx_disable_mask
    }
    /// Make the next `tx_disable_channel` return `false` (staging failure path).
    pub fn set_tx_disable_result(&self, v: bool) {
        self.inner.lock().unwrap().tx_disable_result = v;
    }
    /// Make the next `scs_apply_datapath_init` return `false`.
    pub fn set_scs_apply_result(&self, v: bool) {
        self.inner.lock().unwrap().scs_apply_result = v;
    }
    /// Override the advertised state-machine durations (milliseconds).
    pub fn set_durations_ms(
        &self,
        dp_init: f64,
        dp_deinit: f64,
        dp_txon: f64,
        dp_txoff: f64,
        pwr_up: f64,
        pwr_down: f64,
    ) {
        let mut g = self.inner.lock().unwrap();
        g.dp_init_dur = dp_init;
        g.dp_deinit_dur = dp_deinit;
        g.dp_txon_dur = dp_txon;
        g.dp_txoff_dur = dp_txoff;
        g.pwr_up_dur = pwr_up;
        g.pwr_down_dur = pwr_down;
    }

    // --- coherent (ZR) tuning setters / recorders ---
    pub fn set_supported_power_config(&self, min: f64, max: f64) {
        self.inner.lock().unwrap().supported_power_config = (min, max);
    }
    pub fn set_tx_config_power(&self, v: f64) {
        self.inner.lock().unwrap().tx_config_power = v;
    }
    /// Make the next `set_tx_power` return this result (staging-failure path).
    pub fn set_set_tx_power_result(&self, v: bool) {
        self.inner.lock().unwrap().set_tx_power_result = v;
    }
    /// The Tx power captured by the last `set_tx_power` call.
    pub fn last_set_tx_power(&self) -> Option<f64> {
        self.inner.lock().unwrap().last_set_tx_power
    }
    pub fn set_supported_freq_config(&self, grid: u32, low_ch: i64, hi_ch: i64, low_f: i64, hi_f: i64) {
        self.inner.lock().unwrap().supported_freq_config = (grid, low_ch, hi_ch, low_f, hi_f);
    }
    pub fn set_laser_config_freq(&self, v: i64) {
        self.inner.lock().unwrap().laser_config_freq = v;
    }
    /// Make the next `set_laser_freq` return this result.
    pub fn set_set_laser_freq_result(&self, v: bool) {
        self.inner.lock().unwrap().set_laser_freq_result = v;
    }
    /// The `(freq, grid)` captured by the last `set_laser_freq` call.
    pub fn last_set_laser_freq(&self) -> Option<(i64, u32)> {
        self.inner.lock().unwrap().last_set_laser_freq
    }
    pub fn set_tuning_in_progress(&self, v: bool) {
        self.inner.lock().unwrap().tuning_in_progress = v;
    }

    /// Set the mocked vendor name (`get_manufacturer`); `None` = unreadable field.
    pub fn set_manufacturer(&self, v: Option<&str>) {
        self.inner.lock().unwrap().manufacturer = v.map(|s| s.to_string());
    }
    /// Set the mocked part number (`get_model`); `None` = unreadable field.
    pub fn set_model(&self, v: Option<&str>) {
        self.inner.lock().unwrap().model = v.map(|s| s.to_string());
    }
    /// Make `stage_custom_si_settings` return this result (staging-failure path).
    pub fn set_stage_si_result(&self, v: bool) {
        self.inner.lock().unwrap().stage_si_result = v;
    }
    /// The optics-SI dict captured by the last `stage_custom_si_settings` call.
    pub fn last_staged_si(&self) -> Option<Value> {
        self.inner.lock().unwrap().last_staged_si.clone()
    }
    /// The host-lanes mask captured by the last `stage_custom_si_settings` call.
    pub fn last_staged_si_mask(&self) -> u32 {
        self.inner.lock().unwrap().last_staged_si_mask
    }
}

impl CmisApi for MockCmisApi {
    fn is_flat_memory(&self) -> bool {
        self.inner.lock().unwrap().is_flat_memory
    }
    fn is_coherent_module(&self) -> bool {
        self.inner.lock().unwrap().is_coherent_module
    }
    fn get_module_type_abbreviation(&self) -> Option<String> {
        self.inner.lock().unwrap().module_type_abbreviation.clone()
    }
    fn get_module_state(&self) -> String {
        self.inner.lock().unwrap().module_state.clone()
    }
    fn get_cmis_rev(&self) -> String {
        self.inner.lock().unwrap().cmis_rev.clone()
    }
    fn get_manufacturer(&self) -> Option<String> {
        self.bump("get_manufacturer");
        self.inner.lock().unwrap().manufacturer.clone()
    }
    fn get_model(&self) -> Option<String> {
        self.bump("get_model");
        self.inner.lock().unwrap().model.clone()
    }
    fn get_application_advertisement(&self) -> Value {
        self.inner.lock().unwrap().application_advertisement.clone()
    }
    fn get_host_lane_assignment_option(&self, appl: u32) -> u32 {
        advert_field_u32(
            &self.inner.lock().unwrap().application_advertisement,
            appl,
            "host_lane_assignment_options",
            0,
        )
    }
    fn get_media_lane_count(&self, appl: u32) -> u32 {
        let g = self.inner.lock().unwrap();
        if let Some(v) = g.media_lane_count_override {
            return v;
        }
        advert_field_u32(&g.application_advertisement, appl, "media_lane_count", 1)
    }
    fn get_media_lane_assignment_option(&self, appl: u32) -> u32 {
        advert_field_u32(
            &self.inner.lock().unwrap().application_advertisement,
            appl,
            "media_lane_assignment_options",
            1,
        )
    }
    fn get_datapath_state(&self) -> Value {
        self.inner.lock().unwrap().datapath_state.clone()
    }
    fn get_config_datapath_hostlane_status(&self) -> Value {
        self.inner.lock().unwrap().config_status.clone()
    }
    fn get_dpinit_pending(&self) -> Value {
        self.inner.lock().unwrap().dpinit_pending.clone()
    }
    fn get_active_apsel_hostlane(&self) -> ApiResult<Value> {
        self.bump("get_active_apsel_hostlane");
        let mut g = self.inner.lock().unwrap();
        if let Some(r) = g.active_apsel_queue.pop_front() {
            return r;
        }
        Ok(g.active_apsel.clone())
    }
    fn get_application(&self, lane: u32) -> u32 {
        let g = self.inner.lock().unwrap();
        if g.application_by_lane_map.is_empty() {
            g.application_by_lane
        } else {
            g.application_by_lane_map.get(&lane).copied().unwrap_or(0)
        }
    }

    fn get_datapath_init_duration(&self) -> f64 {
        self.inner.lock().unwrap().dp_init_dur
    }
    fn get_datapath_deinit_duration(&self) -> f64 {
        self.inner.lock().unwrap().dp_deinit_dur
    }
    fn get_datapath_tx_turnon_duration(&self) -> f64 {
        self.inner.lock().unwrap().dp_txon_dur
    }
    fn get_datapath_tx_turnoff_duration(&self) -> f64 {
        self.inner.lock().unwrap().dp_txoff_dur
    }
    fn get_module_pwr_up_duration(&self) -> f64 {
        self.inner.lock().unwrap().pwr_up_dur
    }
    fn get_module_pwr_down_duration(&self) -> f64 {
        self.inner.lock().unwrap().pwr_down_dur
    }

    fn set_datapath_deinit(&self, host_lanes_mask: u32) -> bool {
        self.bump("set_datapath_deinit");
        self.inner.lock().unwrap().last_deinit_mask = host_lanes_mask;
        true
    }
    fn set_datapath_init(&self, _host_lanes_mask: u32) -> bool {
        self.bump("set_datapath_init");
        true
    }
    fn tx_disable_channel(&self, media_lanes_mask: u32, _disable: bool) -> bool {
        self.bump("tx_disable_channel");
        let mut g = self.inner.lock().unwrap();
        g.last_tx_disable_mask = media_lanes_mask;
        g.tx_disable_result
    }
    fn set_lpmode(&self, _lpmode: bool, _wait_state_change: bool) -> bool {
        self.bump("set_lpmode");
        true
    }
    fn set_application(&self, _host_lanes_mask: u32, _appl: u32, ec: u32) -> bool {
        self.bump("set_application");
        self.inner.lock().unwrap().last_set_application_ec = ec;
        true
    }
    fn scs_apply_datapath_init(&self, _host_lanes_mask: u32) -> bool {
        self.bump("scs_apply_datapath_init");
        self.inner.lock().unwrap().scs_apply_result
    }
    fn stage_custom_si_settings(&self, host_lanes_mask: u32, optics_si_dict: &Value) -> bool {
        self.bump("stage_custom_si_settings");
        let mut g = self.inner.lock().unwrap();
        g.last_staged_si = Some(optics_si_dict.clone());
        g.last_staged_si_mask = host_lanes_mask;
        g.stage_si_result
    }

    fn get_supported_power_config(&self) -> (f64, f64) {
        self.bump("get_supported_power_config");
        self.inner.lock().unwrap().supported_power_config
    }
    fn get_tx_config_power(&self) -> f64 {
        self.bump("get_tx_config_power");
        self.inner.lock().unwrap().tx_config_power
    }
    fn set_tx_power(&self, tx_power: f64) -> bool {
        self.bump("set_tx_power");
        let mut g = self.inner.lock().unwrap();
        g.last_set_tx_power = Some(tx_power);
        g.set_tx_power_result
    }
    fn get_supported_freq_config(&self) -> (u32, i64, i64, i64, i64) {
        self.bump("get_supported_freq_config");
        self.inner.lock().unwrap().supported_freq_config
    }
    fn get_laser_config_freq(&self) -> i64 {
        self.bump("get_laser_config_freq");
        self.inner.lock().unwrap().laser_config_freq
    }
    fn set_laser_freq(&self, freq: i64, grid: u32) -> bool {
        self.bump("set_laser_freq");
        let mut g = self.inner.lock().unwrap();
        g.last_set_laser_freq = Some((freq, grid));
        g.set_laser_freq_result
    }
    fn get_tuning_in_progress(&self) -> bool {
        self.bump("get_tuning_in_progress");
        self.inner.lock().unwrap().tuning_in_progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_repr_to_json_parses_advertisement() {
        let s = "{1: {'host_electrical_interface_id': '400GAUI-8 C2M (Annex 120E)', \
                 'host_lane_count': 8, 'media_lane_count': 4, 'host_lane_assignment_options': 1}, \
                 2: {'host_electrical_interface_id': 'CAUI-4 C2M (Annex 83E)', 'host_lane_count': 4, \
                 'media_lane_count': 4, 'host_lane_assignment_options': 17}}";
        let v = py_repr_to_json(s).expect("parse");
        assert_eq!(v["1"]["host_lane_count"], json!(8));
        assert_eq!(v["1"]["host_electrical_interface_id"], json!("400GAUI-8 C2M (Annex 120E)"));
        assert_eq!(v["2"]["host_lane_assignment_options"], json!(17));
    }

    #[test]
    fn cmis_linear_matches_optoe_inverse() {
        assert_eq!(cmis_linear(SCS0_PAGE, DPDEINIT_OFFSET), 2176);
        assert_eq!(cmis_linear(SCS0_PAGE, OUTPUT_DISABLE_TX_OFFSET), 2178);
        assert_eq!(cmis_linear(SCS0_PAGE, APPLY_DPINIT_OFFSET), 2191);
        assert_eq!(cmis_linear(SCS0_PAGE, DPCONFIG_BASE_OFFSET), 2193);
        assert_eq!(cmis_linear(ACS_PAGE, ACS_DPCONFIG_BASE_OFFSET), 2382);
        assert_eq!(cmis_linear(0, 26), 26);
    }

    /// `BridgeCmisApi` sources the active AppSel from the module's Active Control Set
    /// (page 11h:206.., AppSelCode = upper nibble), NOT from a TRANSCEIVER_INFO field.
    #[test]
    fn bridge_active_apsel_reads_acs_register_upper_nibble() {
        use crate::mock::MockSfp;
        let base = cmis_linear(ACS_PAGE, ACS_DPCONFIG_BASE_OFFSET);
        let mut sfp = MockSfp::present();
        // Host lanes 1-4 provisioned to AppSelCode 1 (upper nibble) + DataPathID in the low
        // nibble; lanes 5-8 left unseeded -> read miss -> default 0.
        for lane in 0..4u8 {
            sfp = sfp.with_eeprom(base + lane as usize, 0x10 | (lane << 1));
        }
        let api = BridgeCmisApi::new(Box::new(sfp));
        let apsel = api.get_active_apsel_hostlane().expect("apsel");
        assert_eq!(apsel["ActiveAppSelLane1"], json!(1));
        assert_eq!(apsel["ActiveAppSelLane4"], json!(1));
        assert_eq!(apsel["ActiveAppSelLane5"], json!(0));
        assert_eq!(api.get_application(0), 1);
        assert_eq!(api.get_application(4), 0);
    }

    /// `set_datapath_deinit` (CMIS v5) SETs the masked host-lane bits in DataPathDeinit
    /// (10h:128); `set_datapath_init` CLEARs them. `set_application` writes the DPConfigLane
    /// byte `(appl<<4)|(lane_first<<1)|ec` per masked lane.
    #[test]
    fn bridge_control_writes_encode_registers() {
        use crate::mock::MockSfp;
        let deinit_lin = cmis_linear(SCS0_PAGE, DPDEINIT_OFFSET);
        // CMIS rev 5.x so v4+ polarity.
        let sfp = MockSfp::present().with_eeprom(CMIS_REV_LINEAR, 0x50);
        let api = BridgeCmisApi::new(Box::new(sfp.clone()));
        assert!(api.set_datapath_deinit(0x0f));
        assert_eq!(sfp.eeprom.borrow().get(&deinit_lin).copied(), Some(0x0f));
        assert!(api.set_datapath_init(0x03));
        assert_eq!(sfp.eeprom.borrow().get(&deinit_lin).copied(), Some(0x0c));

        let apply_lin = cmis_linear(SCS0_PAGE, APPLY_DPINIT_OFFSET);
        assert!(api.scs_apply_datapath_init(0x0f));
        assert_eq!(sfp.eeprom.borrow().get(&apply_lin).copied(), Some(0x0f));

        let dpc0 = cmis_linear(SCS0_PAGE, DPCONFIG_BASE_OFFSET);
        assert!(api.set_application(0x0f, 1, 0));
        // lane_first = 0 → (1<<4)|(0<<1)|0 = 0x10 for every masked lane.
        assert_eq!(sfp.eeprom.borrow().get(&dpc0).copied(), Some(0x10));
    }

    /// `stage_custom_si_settings` for the CDR-enable controls (1 bit / host lane) — the
    /// exact case the optics-SI provisioning uses (CDREnableTx/Rx = 1 on 4 host lanes). Gated
    /// on the page-01h TX/RX SI advertisement (161/162 bit0); the packed page-10h Staged
    /// Control Set bytes (10h:160 CDREnableTx, 10h:161 CDREnableRx) get bits 0-3 set → 0x0f.
    #[test]
    fn bridge_stage_custom_si_settings_writes_cdr_page10h() {
        use crate::mock::MockSfp;
        // Advertise TX + RX CDR support (01h:161 bit0, 01h:162 bit0).
        let sfp = MockSfp::present()
            .with_eeprom(cmis_linear(SI_ADVT_PAGE, TX_SI_CTRL_ADVT_OFFSET), 0x01)
            .with_eeprom(cmis_linear(SI_ADVT_PAGE, RX_SI_CTRL_ADVT_OFFSET), 0x01);
        let api = BridgeCmisApi::new(Box::new(sfp.clone()));
        let dict = json!({
            "CDREnableTx": {"CDREnableTx1": 1, "CDREnableTx2": 1, "CDREnableTx3": 1, "CDREnableTx4": 1},
            "CDREnableRx": {"CDREnableRx1": 1, "CDREnableRx2": 1, "CDREnableRx3": 1, "CDREnableRx4": 1},
        });
        assert!(api.stage_custom_si_settings(0x0f, &dict));
        let tx_lin = cmis_linear(SCS0_PAGE, SI_CDR_ENABLE_TX_OFFSET);
        let rx_lin = cmis_linear(SCS0_PAGE, SI_CDR_ENABLE_RX_OFFSET);
        assert_eq!(sfp.eeprom.borrow().get(&tx_lin).copied(), Some(0x0f));
        assert_eq!(sfp.eeprom.borrow().get(&rx_lin).copied(), Some(0x0f));
    }

    /// A control the module does not advertise (page-01h support bit clear) is silently
    /// skipped (`c_cmis` stages it as a no-op): `stage_custom_si_settings` still returns
    /// `true` but writes nothing to the page-10h Staged Control Set.
    #[test]
    fn bridge_stage_custom_si_settings_skips_unadvertised() {
        use crate::mock::MockSfp;
        // No advertisement seeded → si_support_bit reads a miss → false → skip.
        let sfp = MockSfp::present();
        let api = BridgeCmisApi::new(Box::new(sfp.clone()));
        let dict = json!({ "CDREnableTx": {"CDREnableTx1": 1, "CDREnableTx2": 1} });
        assert!(api.stage_custom_si_settings(0x03, &dict));
        let tx_lin = cmis_linear(SCS0_PAGE, SI_CDR_ENABLE_TX_OFFSET);
        assert!(sfp.eeprom.borrow().get(&tx_lin).is_none());
    }

    /// A 4-bit-per-lane control (RX output pre-cursor) packs two host lanes per byte and is
    /// bounded by the page-01h advertised max. Value ≤ max packs (`lane1=3` in bits 3:0,
    /// `lane2=5` in bits 7:4 → 0x53); a value above the advertised max aborts the whole
    /// apply (`stage_custom_si_settings` → `false`).
    #[test]
    fn bridge_stage_custom_si_settings_four_bit_pack_and_max() {
        use crate::mock::MockSfp;
        let advt_lin = cmis_linear(SI_ADVT_PAGE, RX_SI_CTRL_ADVT_OFFSET);
        let max_lin = cmis_linear(SI_ADVT_PAGE, SI_RX_EQ_MAX_OFFSET);
        let pre_lin = cmis_linear(SCS0_PAGE, SI_OUTPUT_EQ_PRE_RX_OFFSET);

        // Advertise RX pre-cursor (01h:162 bit3) with a generous max nibble (lo = 0x0f).
        let sfp = MockSfp::present()
            .with_eeprom(advt_lin, 0x08)
            .with_eeprom(max_lin, 0x0f);
        let api = BridgeCmisApi::new(Box::new(sfp.clone()));
        let dict = json!({ "OutputEqPreCursorTargetRx": {"OutputEqPreCursorTargetRx1": 3, "OutputEqPreCursorTargetRx2": 5} });
        assert!(api.stage_custom_si_settings(0x03, &dict));
        assert_eq!(sfp.eeprom.borrow().get(&pre_lin).copied(), Some(0x53));

        // Same control advertised but with max = 2: a lane value of 3 exceeds it → reject.
        let sfp2 = MockSfp::present()
            .with_eeprom(advt_lin, 0x08)
            .with_eeprom(max_lin, 0x02);
        let api2 = BridgeCmisApi::new(Box::new(sfp2));
        let dict2 = json!({ "OutputEqPreCursorTargetRx": {"OutputEqPreCursorTargetRx1": 3, "OutputEqPreCursorTargetRx2": 1} });
        assert!(!api2.stage_custom_si_settings(0x03, &dict2));
    }
}
