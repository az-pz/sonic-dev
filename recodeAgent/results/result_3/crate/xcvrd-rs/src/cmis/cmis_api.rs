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
//!     [`crate::hal::SfpHandle::write_eeprom`] with the upstream `c_cmis.py` encodings.
//!     `set_lpmode` delegates to the bridge's own `set_lpmode` (Python decode).
//!
//! [`MockCmisApi`] is the Part-B double (canned decode + settable/sequenced dynamic
//! state + write call-counters), the analogue of the Python tests' `MagicMock()` api.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::error::Result;
use crate::hal::SfpHandle;

/// The CMIS control/decode surface the [`super::cmis_manager_task::CmisManagerTask`]
/// bring-up state machine drives (`api.*` in `cmis_manager_task.py`). Split from
/// [`SfpHandle`] so Part-B tests inject [`MockCmisApi`]; production wraps a bridge
/// handle in [`BridgeCmisApi`].
pub trait CmisApi {
    // --- decode reads (stay in Python via the bridge) ---
    fn is_flat_memory(&self) -> bool;
    fn is_coherent_module(&self) -> bool;
    fn get_module_type_abbreviation(&self) -> Option<String>;
    fn get_module_state(&self) -> String;
    fn get_cmis_rev(&self) -> String;
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
    fn get_active_apsel_hostlane(&self) -> Result<Value>;
    /// The application code currently applied to host `lane` (0-based).
    fn get_application(&self, lane: u32) -> u32;

    // --- durations, milliseconds (caller divides by 1000 for seconds) ---
    fn get_datapath_init_duration(&self) -> f64;
    fn get_datapath_deinit_duration(&self) -> f64;
    fn get_datapath_tx_turnon_duration(&self) -> f64;
    fn get_datapath_tx_turnoff_duration(&self) -> f64;
    fn get_module_pwr_up_duration(&self) -> f64;
    fn get_module_pwr_down_duration(&self) -> f64;

    // --- coherent/ZR tuning (M8; skipped for non-coherent modules) ---
    fn get_tx_config_power(&self) -> f64;
    fn get_laser_config_freq(&self) -> i64;
    /// `(grid_bitmask, _, _, low_freq, high_freq)` (`c_cmis.get_supported_freq_config`).
    fn get_supported_freq_config(&self) -> (i64, i64, i64, i64, i64);
    fn get_supported_power_config(&self) -> (f64, f64);
    fn get_tuning_in_progress(&self) -> bool;
    fn get_manufacturer(&self) -> String;
    fn get_model(&self) -> String;

    // --- control writes (raw page-10h register writes / bridge set_lpmode) ---
    fn set_datapath_deinit(&self, host_lanes_mask: u32) -> bool;
    fn set_datapath_init(&self, host_lanes_mask: u32) -> bool;
    fn tx_disable_channel(&self, media_lanes_mask: u32, disable: bool) -> bool;
    fn set_lpmode(&self, lpmode: bool, wait_state_change: bool) -> bool;
    fn set_application(&self, host_lanes_mask: u32, appl: u32, ec: u32) -> bool;
    fn scs_apply_datapath_init(&self, host_lanes_mask: u32) -> bool;
    /// Stage per-vendor custom Signal-Integrity settings into the page-10h Staged
    /// Control Set (`c_cmis.stage_custom_si_settings`): split `optics_si_dict` by the
    /// `Tx`/`Rx` key suffix, stage RX controls then TX controls, per-lane, gated by the
    /// module's advertised SI-control support. `true` on success (or nothing to stage).
    fn stage_custom_si_settings(&self, host_lanes_mask: u32, optics_si_dict: &Value) -> bool;
    fn set_tx_power(&self, tx_power: f64) -> bool;
    fn set_laser_freq(&self, freq: i64, grid: u32) -> bool;
}

pub const CMIS_MAX_HOST_LANES_USIZE: usize = 8;

// =====================================================================================
// BridgeCmisApi — production impl over a bridge SfpHandle.
// =====================================================================================

// CMIS page-10h control-register *linear* (optoe) offsets. `linear = page*128 + offset`
// (bank 0), the inverse of `sfp.py:linear_to_bpo`. Page 0x10 (SCS0) upper memory.
const SCS0_PAGE: usize = 0x10;
const DPDEINIT_OFFSET: usize = 128; // 10h:128 DataPathDeinit (1 bit / host lane)
const OUTPUT_DISABLE_TX_OFFSET: usize = 130; // 10h:130 OutputDisableTx (1 bit / lane)
const APPLY_DPINIT_OFFSET: usize = 143; // 10h:143 ApplyDPInitLane trigger
const DPCONFIG_BASE_OFFSET: usize = 145; // 10h:145..152 DPConfigLane (one byte / lane)
const CMIS_REV_LINEAR: usize = 1; // 00h:1 CMIS revision (high nibble = major)
const CMIS_FLAT_MEM_LINEAR: usize = 2; // 00h:2 status; bit7 = FlatMem (c_cmis.is_flat_memory)
const CMIS_FLAT_MEM_BIT: u8 = 0x80;

// Page-10h Staged Control Set 0 Signal-Integrity control offsets (CMIS v5.2 §8.8.1,
// Table 8-121). Each control packs `bits_per_lane` bits per host lane, LSB-first across
// its byte range: host lane L (1-based) → bit range [(L-1)*bpl .. L*bpl). TX controls
// occupy 153-160, RX controls 161-175 (mirrors the e2e `SCS0_SI_CONTROL_RANGE`). These
// are the byte offsets `xcvr_eeprom.write("<Param><lane>", v)` resolves to.
const SI_ADAPTIVE_INPUT_EQ_ENABLE_TX_OFFSET: usize = 153; // 1 bit/lane
const SI_ADAPTIVE_INPUT_EQ_RECALLED_TX_OFFSET: usize = 154; // 2 bits/lane (154-155)
const SI_FIXED_INPUT_EQ_TARGET_TX_OFFSET: usize = 156; // 4 bits/lane (156-159)
const SI_CDR_ENABLE_TX_OFFSET: usize = 160; // 1 bit/lane
const SI_CDR_ENABLE_RX_OFFSET: usize = 161; // 1 bit/lane
const SI_OUTPUT_EQ_PRE_CURSOR_TARGET_RX_OFFSET: usize = 162; // 4 bits/lane (162-165)
const SI_OUTPUT_EQ_POST_CURSOR_TARGET_RX_OFFSET: usize = 166; // 4 bits/lane (166-169)
const SI_OUTPUT_AMPLITUDE_TARGET_RX_OFFSET: usize = 170; // 4 bits/lane (170-173)

// Page-01h Supported Signal-Integrity Controls Advertisement (CMIS §8.4.7). xcvrd only
// stages a control the module advertises support for; the CDR support bits are the ones
// the e2e drives (`SI_ADV_TX_CDR_OFFSET`/`SI_ADV_RX_CDR_OFFSET`, bit 0).
const SI_ADV_TX_CDR_OFFSET: usize = 161; // 01h:161 bit0 TXCDRSupported
const SI_ADV_RX_CDR_OFFSET: usize = 162; // 01h:162 bit0 RxCDRSupported

// `codes_cmis`/`consts` SI parameter names — the `optics_si_settings.json` top-level keys.
const CDR_ENABLE_TX: &str = "CDREnableTx";
const CDR_ENABLE_RX: &str = "CDREnableRx";
const OUTPUT_EQ_PRE_CURSOR_TARGET_RX: &str = "OutputEqPreCursorTargetRx";
const OUTPUT_EQ_POST_CURSOR_TARGET_RX: &str = "OutputEqPostCursorTargetRx";
const OUTPUT_AMPLITUDE_TARGET_RX: &str = "OutputAmplitudeTargetRx";
const FIXED_INPUT_EQ_TARGET_TX: &str = "FixedInputEqTargetTx";
const ADAPTIVE_INPUT_EQ_RECALLED_TX: &str = "AdaptiveInputEqRecalledTx";
const ADAPTIVE_INPUT_EQ_ENABLE_TX: &str = "AdaptiveInputEqEnableTx";

// CMIS page-11h Active Control Set. `get_active_apsel_hostlane`/`get_application` read
// the *provisioned* (active) DPConfigLane bytes (11h:206..213); the AppSelCode is the
// upper nibble (bits 7:4) of each host lane's byte. These are read-only and reflect the
// application the module actually applied (updated by ApplyDPInit), which is what
// `c_cmis.CmisApi.get_active_apsel_hostlane` reads — NOT a `get_transceiver_info` field.
const ACS_PAGE: usize = 0x11;
const ACS_DPCONFIG_BASE_OFFSET: usize = 206; // 11h:206..213 ActiveControlSet DPConfigLane

// CMIS page-01h State Machine Durations Advertising (CMIS v5.2 §8.3.7 / §8.4.7). Each
// max-duration is a 4-bit code (Table 8-43) packed two per byte. `c_cmis.py` decodes the
// code with `CmisCodes.DP_PATH_TIMINGS` (code -> milliseconds); we read the raw page-01h
// byte and re-apply that same fixed lookup — no module-specific interpretation, so CMIS
// decode still effectively lives "in Python". These were previously hard-coded to the CMIS
// spec *maximum* (dp_deinit=600s, pwr_up/down=70s, dp_init=60s, tx_on/off=5s); that made the
// AP_CONF timer (max(pwr_up, dp_deinit)) 600s, so a stalled datapath could not complete the
// CMIS_MAX_RETRIES retries within the e2e budget before latching cmis_state=FAILED. The
// module advertises far shorter durations; read and honour them.
const DURATIONS_PAGE: usize = 0x01;
const DP_INIT_DEINIT_OFFSET: usize = 144; // 01h:144 hi=MaxDurationDPDeinit, lo=MaxDurationDPInit
const MODULE_PWR_OFFSET: usize = 167; // 01h:167 hi=MaxDurationModulePwrDn, lo=MaxDurationModulePwrUp
const DP_TX_TURN_OFFSET: usize = 168; // 01h:168 hi=MaxDurationDPTxTurnOff, lo=MaxDurationDPTxTurnOn

// `c_cmis.get_datapath_init_duration` scales a short (<=1000 ms) advertised DPInit value ×10.
const DATAPATH_INIT_DURATION_MULTIPLIER: f64 = 10.0;
const DATAPATH_INIT_DURATION_OVERRIDE_THRESHOLD: f64 = 1000.0;

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

/// Production [`CmisApi`] backed by a bridge [`SfpHandle`]: decode reads come from the
/// Python `get_transceiver_status()`/`get_transceiver_info()` getters; control writes are
/// the raw page-10h register writes (`c_cmis.py` encodings) via `write_eeprom`.
pub struct BridgeCmisApi {
    sfp: Box<dyn SfpHandle>,
}

impl BridgeCmisApi {
    pub fn new(sfp: Box<dyn SfpHandle>) -> Self {
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

    /// Is a page-01h advertisement bit set (`byte & 0x01`)? Unreadable → not supported.
    fn si_adv_bit0(&self, offset: usize) -> bool {
        self.read_byte(cmis_linear(DURATIONS_PAGE, offset)).map(|b| b & 0x01 != 0).unwrap_or(false)
    }

    /// Stage one SI control (`c_cmis.scs_lane_write` + the per-control `stage_*`): for
    /// each masked host lane, pack the vendor value for `<param><lane>` (1-based) into the
    /// control's page-10h byte range at `bits_per_lane` LSB-first, then write the affected
    /// bytes. `false` if a masked lane's value is missing/null (mirrors the reference
    /// `si_param_lane_val is None → return False`).
    fn stage_si_param(
        &self,
        base: usize,
        bits_per_lane: u32,
        host_lanes_mask: u32,
        sub: &Value,
        param: &str,
    ) -> bool {
        let field_mask: u32 = (1u32 << bits_per_lane) - 1;
        let mut bytes: std::collections::BTreeMap<usize, u8> = std::collections::BTreeMap::new();
        for lane in 0..CMIS_MAX_HOST_LANES_USIZE {
            if (host_lanes_mask >> lane) & 1 == 0 {
                continue;
            }
            let key = format!("{param}{}", lane + 1);
            let Some(val) = sub.get(&key).and_then(json_as_u32) else {
                return false;
            };
            let bit_start = lane as u32 * bits_per_lane;
            let off = base + (bit_start / 8) as usize;
            let shift = bit_start % 8;
            let cur = bytes
                .entry(off)
                .or_insert_with(|| self.read_byte(cmis_linear(SCS0_PAGE, off)).unwrap_or(0));
            *cur &= !((field_mask as u8) << shift);
            *cur |= ((val & field_mask) as u8) << shift;
        }
        let mut ok = true;
        for (off, byte) in bytes {
            ok &= self.write_byte(cmis_linear(SCS0_PAGE, off), byte);
        }
        ok
    }

    /// `stage_rx_si_settings` for a single RX control — gated by the module's advertised
    /// support. Unknown RX params return `false` (the reference `else: return False`).
    fn stage_rx_si_param(&self, param: &str, host_lanes_mask: u32, sub: &Value) -> bool {
        match param {
            CDR_ENABLE_RX => {
                if self.si_adv_bit0(SI_ADV_RX_CDR_OFFSET) {
                    self.stage_si_param(SI_CDR_ENABLE_RX_OFFSET, 1, host_lanes_mask, sub, param)
                } else {
                    true
                }
            }
            // The emulator advertises only CDR support; the remaining RX controls
            // (output EQ pre/post, amplitude) are applied when present. Their support
            // advertisement offsets are not exercised by the DUT, so they are staged
            // unconditionally here (documented deviation from the per-control gate).
            OUTPUT_EQ_PRE_CURSOR_TARGET_RX => self.stage_si_param(
                SI_OUTPUT_EQ_PRE_CURSOR_TARGET_RX_OFFSET,
                4,
                host_lanes_mask,
                sub,
                param,
            ),
            OUTPUT_EQ_POST_CURSOR_TARGET_RX => self.stage_si_param(
                SI_OUTPUT_EQ_POST_CURSOR_TARGET_RX_OFFSET,
                4,
                host_lanes_mask,
                sub,
                param,
            ),
            OUTPUT_AMPLITUDE_TARGET_RX => self.stage_si_param(
                SI_OUTPUT_AMPLITUDE_TARGET_RX_OFFSET,
                4,
                host_lanes_mask,
                sub,
                param,
            ),
            _ => false,
        }
    }

    /// `stage_tx_si_settings` for a single TX control — gated (CDR) / applied (others).
    fn stage_tx_si_param(&self, param: &str, host_lanes_mask: u32, sub: &Value) -> bool {
        match param {
            CDR_ENABLE_TX => {
                if self.si_adv_bit0(SI_ADV_TX_CDR_OFFSET) {
                    self.stage_si_param(SI_CDR_ENABLE_TX_OFFSET, 1, host_lanes_mask, sub, param)
                } else {
                    true
                }
            }
            FIXED_INPUT_EQ_TARGET_TX => self.stage_si_param(
                SI_FIXED_INPUT_EQ_TARGET_TX_OFFSET,
                4,
                host_lanes_mask,
                sub,
                param,
            ),
            ADAPTIVE_INPUT_EQ_RECALLED_TX => self.stage_si_param(
                SI_ADAPTIVE_INPUT_EQ_RECALLED_TX_OFFSET,
                2,
                host_lanes_mask,
                sub,
                param,
            ),
            ADAPTIVE_INPUT_EQ_ENABLE_TX => self.stage_si_param(
                SI_ADAPTIVE_INPUT_EQ_ENABLE_TX_OFFSET,
                1,
                host_lanes_mask,
                sub,
                param,
            ),
            _ => false,
        }
    }

    /// CMIS major revision (defaults to 5 — the emulator/testbed is CMIS 5.x — when the
    /// register read fails; picks the v4+ deinit/init bit polarity).
    fn cmis_major(&self) -> u8 {
        self.read_byte(CMIS_REV_LINEAR).map(|b| b >> 4).unwrap_or(5)
    }
}

impl CmisApi for BridgeCmisApi {
    fn is_flat_memory(&self) -> bool {
        // `c_cmis.CmisApi.is_flat_memory` reads CMIS 00h:2 bit 7 (FlatMem). That byte
        // lives in lower memory, which is accessible even on a flat module (one that by
        // definition has no paged upper memory), so read the raw bit through the bridge —
        // the same 00h:2.7 test the flat-memory e2e performs (`emu.read(idx,0,0,2,1) &
        // FLAT_MEM_BIT`). A flat CMIS module still decodes its lower-memory identity, so
        // its `get_transceiver_info()` carries `cmis_rev` and `get_transceiver_status()`
        // may carry `module_state`; the heuristic below therefore cannot distinguish a
        // flat CMIS module from a paged one and is kept only as a fallback for the rare
        // case the register read itself is unavailable.
        if let Some(b) = self.read_byte(CMIS_FLAT_MEM_LINEAR) {
            return b & CMIS_FLAT_MEM_BIT != 0;
        }
        self.status().get("module_state").is_none() && self.info().get("cmis_rev").is_none()
    }

    fn is_coherent_module(&self) -> bool {
        // Coherent/ZR tuning is M8; a non-coherent module skips those branches. The
        // bridge does not surface the coherent flag, so report false here.
        false
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

    fn get_application_advertisement(&self) -> Value {
        // TRANSCEIVER_INFO carries the advertisement as a Python-dict *repr* string
        // (`str(get_application_advertisement())`); parse it back to JSON keyed by app
        // index. CMIS decode itself stayed in Python; this only re-inflates its output.
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

    fn get_active_apsel_hostlane(&self) -> Result<Value> {
        // c_cmis.get_active_apsel_hostlane: read the Active Control Set DPConfigLane bytes
        // (page 11h:206..213); the ApSel code applied to host lane <n> is the upper nibble
        // (bits 7:4). A fresh module already advertises its default application here, so a
        // matching port needs no decommission; after ApplyDPInit these reflect the applied
        // app (→ the golden `active_apsel_hostlaneN`). A failed read defaults to 0 ("no
        // active app"), which is the safe value: it never spuriously forces a decommission.
        let mut m = serde_json::Map::new();
        for lane in 0..CMIS_MAX_HOST_LANES_USIZE {
            let lin = cmis_linear(ACS_PAGE, ACS_DPCONFIG_BASE_OFFSET + lane);
            let apsel = self.read_byte(lin).map(|b| u64::from(b >> 4)).unwrap_or(0);
            m.insert(format!("ActiveAppSelLane{}", lane + 1), Value::from(apsel));
        }
        Ok(Value::Object(m))
    }

    fn get_application(&self, lane: u32) -> u32 {
        // The app applied to host `lane` == the Active Control Set AppSelCode (upper nibble
        // of page 11h:206+lane) — same source as `get_active_apsel_hostlane`.
        let lin = cmis_linear(ACS_PAGE, ACS_DPCONFIG_BASE_OFFSET + lane as usize);
        self.read_byte(lin).map(|b| u32::from(b >> 4)).unwrap_or(0)
    }

    fn get_datapath_init_duration(&self) -> f64 {
        // c_cmis.get_datapath_init_duration: flat memory → 0; else DP_PATH_TIMINGS(01h:144 lo),
        // with a short (<=1000 ms) advertised window scaled ×10 (some modules under-report it).
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

    fn get_tx_config_power(&self) -> f64 {
        0.0
    }
    fn get_laser_config_freq(&self) -> i64 {
        0
    }
    fn get_supported_freq_config(&self) -> (i64, i64, i64, i64, i64) {
        (0, 0, 0, 0, 0)
    }
    fn get_supported_power_config(&self) -> (f64, f64) {
        (0.0, 0.0)
    }
    fn get_tuning_in_progress(&self) -> bool {
        false
    }
    fn get_manufacturer(&self) -> String {
        self.info().get("manufacturer").and_then(|v| v.as_str()).unwrap_or("").to_string()
    }
    fn get_model(&self) -> String {
        self.info().get("model").and_then(|v| v.as_str()).unwrap_or("").to_string()
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
        // Delegate to the bridge's own set_lpmode (Python CMIS decode owns the
        // MODULE_LEVEL_CONTROL bit math + power-up/down wait).
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
        let Some(obj) = optics_si_dict.as_object() else {
            return true;
        };
        // c_cmis.stage_custom_si_settings: split by the Tx/Rx suffix, then stage RX
        // controls before TX controls. Insertion order preserved (serde_json
        // preserve_order) so a vendor file's parameter ordering is honoured.
        for (param, sub) in obj {
            if param.ends_with("Rx") && !self.stage_rx_si_param(param, host_lanes_mask, sub) {
                return false;
            }
        }
        for (param, sub) in obj {
            if param.ends_with("Tx") && !self.stage_tx_si_param(param, host_lanes_mask, sub) {
                return false;
            }
        }
        true
    }

    fn set_tx_power(&self, _tx_power: f64) -> bool {
        // Coherent tuning is M8; non-coherent modules never reach here.
        true
    }
    fn set_laser_freq(&self, _freq: i64, _grid: u32) -> bool {
        true
    }
}

fn json_as_u32(v: &Value) -> Option<u32> {
    v.as_u64()
        .map(|n| n as u32)
        .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
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
/// counts, `True`/`False`/`None` literals. Best-effort — used only by [`BridgeCmisApi`]
/// (e2e), never by the mock-driven unit tests.
fn py_repr_to_json(s: &str) -> Option<Value> {
    // 1) single quotes → double quotes.
    let mut dq = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        if c == '\'' {
            dq.push('"');
        } else {
            dq.push(c);
        }
    }
    // 2) Python literals → JSON (safe for the fixed advertisement field set).
    let dq = dq.replace("True", "true").replace("False", "false").replace("None", "null");
    // 3) quote bare integer *keys* (`{1:` / `, 2:` → `{"1":` / `, "2":`).
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
// MockCmisApi — Part-B double (canned decode + settable/sequenced state + counters).
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
    active_apsel_queue: VecDeque<Result<Value>>,
    application_by_lane: u32,
    application_by_lane_map: HashMap<u32, u32>,
    media_lane_count_override: Option<u32>,
    dp_init_dur: f64,
    dp_deinit_dur: f64,
    dp_txon_dur: f64,
    dp_txoff_dur: f64,
    pwr_up_dur: f64,
    pwr_down_dur: f64,
    tx_config_power: f64,
    laser_config_freq: i64,
    tuning_in_progress: bool,
    supported_freq_config: (i64, i64, i64, i64, i64),
    supported_power_config: (f64, f64),
    manufacturer: String,
    model: String,
    calls: HashMap<String, usize>,
    captured_optics_si: Value,
    stage_custom_si_result: bool,
    last_set_application_ec: u32,
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
            tx_config_power: 0.0,
            laser_config_freq: 0,
            tuning_in_progress: false,
            supported_freq_config: (0, 0, 0, 0, 0),
            supported_power_config: (-40.0, 10.0),
            manufacturer: String::new(),
            model: String::new(),
            calls: HashMap::new(),
            captured_optics_si: json!({}),
            stage_custom_si_result: true,
            last_set_application_ec: 0,
        }
    }
}

/// Part-B mock [`CmisApi`] — the Rust analogue of the Python tests' `MagicMock()` xcvr
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

    // --- builders / setters (chainable) ---
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
    /// Queue a sequenced `get_active_apsel_hostlane` result (the analogue of a
    /// `MagicMock(side_effect=[...])` entry; an `Err` mirrors `NotImplementedError`).
    pub fn push_active_apsel_result(&self, r: Result<Value>) {
        self.inner.lock().unwrap().active_apsel_queue.push_back(r);
    }
    pub fn set_application_by_lane(&self, appl: u32) {
        self.inner.lock().unwrap().application_by_lane = appl;
    }
    /// Set the per-lane application code returned by `get_application(lane)`.
    /// `is_cmis_application_update_required` inspects each host lane's currently-active
    /// application; this lets a test paint a distinct code per lane. When the map is
    /// empty, `get_application` falls back to the flat `application_by_lane` value so
    /// existing single-value tests are unaffected.
    pub fn set_application_for_lane(&self, lane: u32, appl: u32) {
        self.inner.lock().unwrap().application_by_lane_map.insert(lane, appl);
    }
    pub fn set_media_lane_count_override(&self, v: Option<u32>) {
        self.inner.lock().unwrap().media_lane_count_override = v;
    }
    pub fn set_supported_freq_config(&self, v: (i64, i64, i64, i64, i64)) {
        self.inner.lock().unwrap().supported_freq_config = v;
    }
    pub fn set_tx_config_power(&self, v: f64) {
        self.inner.lock().unwrap().tx_config_power = v;
    }
    pub fn set_laser_config_freq(&self, v: i64) {
        self.inner.lock().unwrap().laser_config_freq = v;
    }
    pub fn set_manufacturer(&self, v: &str) {
        self.inner.lock().unwrap().manufacturer = v.to_string();
    }
    pub fn set_model(&self, v: &str) {
        self.inner.lock().unwrap().model = v.to_string();
    }
    /// Make the next `stage_custom_si_settings` return `false` (staging failure path).
    pub fn set_stage_custom_si_result(&self, v: bool) {
        self.inner.lock().unwrap().stage_custom_si_result = v;
    }
    /// The `optics_si_dict` captured by the last `stage_custom_si_settings` call.
    pub fn captured_optics_si(&self) -> Value {
        self.inner.lock().unwrap().captured_optics_si.clone()
    }
    /// The explicit-control (`ec`) argument captured by the last `set_application` call.
    pub fn last_set_application_ec(&self) -> u32 {
        self.inner.lock().unwrap().last_set_application_ec
    }
    /// Override the advertised state-machine durations (milliseconds), the way a test would
    /// stub `api.get_*_duration()`. Lets a test drive timer expiry / retry deterministically.
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
    fn get_active_apsel_hostlane(&self) -> Result<Value> {
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

    fn get_tx_config_power(&self) -> f64 {
        self.inner.lock().unwrap().tx_config_power
    }
    fn get_laser_config_freq(&self) -> i64 {
        self.inner.lock().unwrap().laser_config_freq
    }
    fn get_supported_freq_config(&self) -> (i64, i64, i64, i64, i64) {
        self.inner.lock().unwrap().supported_freq_config
    }
    fn get_supported_power_config(&self) -> (f64, f64) {
        self.inner.lock().unwrap().supported_power_config
    }
    fn get_tuning_in_progress(&self) -> bool {
        self.inner.lock().unwrap().tuning_in_progress
    }
    fn get_manufacturer(&self) -> String {
        self.inner.lock().unwrap().manufacturer.clone()
    }
    fn get_model(&self) -> String {
        self.inner.lock().unwrap().model.clone()
    }

    fn set_datapath_deinit(&self, _host_lanes_mask: u32) -> bool {
        self.bump("set_datapath_deinit");
        true
    }
    fn set_datapath_init(&self, _host_lanes_mask: u32) -> bool {
        self.bump("set_datapath_init");
        true
    }
    fn tx_disable_channel(&self, _media_lanes_mask: u32, _disable: bool) -> bool {
        self.bump("tx_disable_channel");
        true
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
        true
    }
    fn stage_custom_si_settings(&self, _host_lanes_mask: u32, optics_si_dict: &Value) -> bool {
        self.bump("stage_custom_si_settings");
        let mut g = self.inner.lock().unwrap();
        g.captured_optics_si = optics_si_dict.clone();
        g.stage_custom_si_result
    }
    fn set_tx_power(&self, _tx_power: f64) -> bool {
        self.bump("set_tx_power");
        true
    }
    fn set_laser_freq(&self, _freq: i64, _grid: u32) -> bool {
        self.bump("set_laser_freq");
        true
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

    // ---- BridgeCmisApi sources the active AppSel from the module's Active Control Set
    //      (page 11h:206.., AppSelCode = upper nibble), NOT from a TRANSCEIVER_INFO field.
    //      M6 root-cause fix: an unprovisioned lane reads as 0 (not Null) so
    //      is_decommission_required never errors, and an applied lane reports its real app. ----
    #[test]
    fn bridge_active_apsel_reads_acs_register_upper_nibble() {
        use crate::mock::MockSfp;
        let base = cmis_linear(ACS_PAGE, ACS_DPCONFIG_BASE_OFFSET);
        let mut sfp = MockSfp::present();
        // Host lanes 1-4 provisioned to AppSelCode 1 (upper nibble) with their DataPathID in
        // the low nibble; lanes 5-8 left unseeded -> read miss -> default 0.
        for lane in 0..4u8 {
            sfp = sfp.with_eeprom(base + lane as usize, 0x10 | (lane << 1));
        }
        let api = BridgeCmisApi::new(Box::new(sfp));

        let apsel = api.get_active_apsel_hostlane().expect("acs read");
        assert_eq!(apsel["ActiveAppSelLane1"], json!(1));
        assert_eq!(apsel["ActiveAppSelLane4"], json!(1));
        // Unprovisioned lane -> 0 (never Null), so decommission math stays well-defined.
        assert_eq!(apsel["ActiveAppSelLane5"], json!(0));
        assert_eq!(apsel["ActiveAppSelLane8"], json!(0));
        assert_eq!(api.get_application(0), 1);
        assert_eq!(api.get_application(3), 1);
        assert_eq!(api.get_application(4), 0);
    }

    #[test]
    fn mock_counts_calls_and_pops_sequenced_apsel() {
        let api = MockCmisApi::new();
        assert!(api.set_datapath_deinit(0xff));
        assert!(api.set_datapath_deinit(0xff));
        assert_eq!(api.call_count("set_datapath_deinit"), 2);

        api.set_active_apsel(json!({"ActiveAppSelLane1": 1}));
        api.push_active_apsel_result(Ok(json!({"ActiveAppSelLane1": 2})));
        api.push_active_apsel_result(Err(crate::error::XcvrdError::Other("not implemented".into())));
        // queued results pop first, then the settable default.
        assert_eq!(api.get_active_apsel_hostlane().unwrap()["ActiveAppSelLane1"], json!(2));
        assert!(api.get_active_apsel_hostlane().is_err());
        assert_eq!(api.get_active_apsel_hostlane().unwrap()["ActiveAppSelLane1"], json!(1));
    }

    // ---- BridgeCmisApi decodes the state-machine max-durations from the module's page-01h
    //      advertisement via DP_PATH_TIMINGS (mirrors c_cmis.get_*_duration), instead of the
    //      old hard-coded CMIS spec maximums. The emulator advertises DPInit=BETWEEN_1_AND_5_S
    //      (code 7 → 5000 ms) and every other duration LESS_THAN_1_MS (code 0 → 1 ms); an
    //      AP_CONF timer of max(pwr_up, dp_deinit)=1 ms (not 600 s) lets a stalled datapath
    //      cross CMIS_MAX_RETRIES → FAILED inside the e2e budget. ----
    #[test]
    fn bridge_durations_decode_page01h_via_dp_path_timings() {
        use crate::mock::MockSfp;
        // module_state present → not flat memory, so the advertisement is read.
        let paged = |sfp: MockSfp| sfp.with_status(json!({"module_state": "ModuleReady"}));
        let dpinit_deinit = cmis_linear(DURATIONS_PAGE, DP_INIT_DEINIT_OFFSET); // 272
        let pwr = cmis_linear(DURATIONS_PAGE, MODULE_PWR_OFFSET); // 295
        let tx = cmis_linear(DURATIONS_PAGE, DP_TX_TURN_OFFSET); // 296
        assert_eq!((dpinit_deinit, pwr, tx), (272, 295, 296));

        // Emulator layout: 01h:144 = 0x07 (lo DPInit=7, hi DPDeinit=0); 01h:167 = 0; 01h:168 = 0.
        let sfp = paged(MockSfp::present())
            .with_eeprom(dpinit_deinit, 0x07)
            .with_eeprom(pwr, 0x00)
            .with_eeprom(tx, 0x00);
        let api = BridgeCmisApi::new(Box::new(sfp));
        assert_eq!(api.get_datapath_init_duration(), 5000.0); // code 7 → 5000, >1000 no ×10
        assert_eq!(api.get_datapath_deinit_duration(), 1.0); // code 0 → 1 ms
        assert_eq!(api.get_datapath_tx_turnon_duration(), 1.0);
        assert_eq!(api.get_datapath_tx_turnoff_duration(), 1.0);
        assert_eq!(api.get_module_pwr_up_duration(), 1.0);
        assert_eq!(api.get_module_pwr_down_duration(), 1.0);

        // Nibble split + DPInit ×10 override: 01h:144 = 0x63 → DPInit lo=3 (50 ms ≤ 1000 → 500),
        // DPDeinit hi=6 (1000 ms, no override). 01h:168 = 0x9A → TxOn lo=10 (300000), TxOff hi=9 (60000).
        let sfp2 = paged(MockSfp::present())
            .with_eeprom(dpinit_deinit, 0x63)
            .with_eeprom(tx, 0x9A);
        let api2 = BridgeCmisApi::new(Box::new(sfp2));
        assert_eq!(api2.get_datapath_init_duration(), 500.0);
        assert_eq!(api2.get_datapath_deinit_duration(), 1000.0);
        assert_eq!(api2.get_datapath_tx_turnon_duration(), 300000.0);
        assert_eq!(api2.get_datapath_tx_turnoff_duration(), 60000.0);

        // Unreadable advertisement (unseeded page-01h) → 0, mirroring c_cmis's None → 0.
        let api3 = BridgeCmisApi::new(Box::new(paged(MockSfp::present())));
        assert_eq!(api3.get_datapath_deinit_duration(), 0.0);
        assert_eq!(api3.get_datapath_init_duration(), 0.0);

        // Flat memory → 0 (c_cmis short-circuits before the read).
        let api4 = BridgeCmisApi::new(Box::new(
            MockSfp::present().with_eeprom(dpinit_deinit, 0x07),
        ));
        assert!(api4.is_flat_memory());
        assert_eq!(api4.get_datapath_init_duration(), 0.0);
        assert_eq!(api4.get_datapath_deinit_duration(), 0.0);
    }
}
