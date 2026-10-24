//! RAC_056905_WW on ThinQ1 — JSON status over the monitoring socket.
//!
//! The same model id on ThinQ2 is the TLV handler in `rac_056905_ww`. This
//! module is only registered on the ThinQ1 factory.

use crate::device_trait::DeviceHandler;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rethink_core::device_base::default_config;
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq1Device;
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const MODES: &[(i64, &str)] = &[
    (0, "cool"),
    (1, "dry"),
    (2, "fan_only"),
    (4, "heat"),
    (6, "auto"),
];

const FAN_MODES: &[(i64, &str)] = &[
    (2, "very low"),
    (3, "low"),
    (4, "medium"),
    (5, "high"),
    (6, "very high"),
    (8, "auto"),
];

const SWING_MODES: &[(i64, &str)] = &[
    (1, "1"),
    (2, "2"),
    (3, "3"),
    (4, "4"),
    (5, "5"),
    (6, "6"),
    (100, "on"),
    (0, "off"),
];

const SWING_H_MODES: &[(i64, &str)] = &[
    (1, "1"),
    (2, "2"),
    (3, "3"),
    (4, "4"),
    (5, "5"),
    (13, "1-3"),
    (35, "3-5"),
    (100, "on"),
    (0, "off"),
];

pub struct Device {
    id: String,
    model_id: String,
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    publish_cache: Mutex<HashMap<String, String>>,
    last_status: Mutex<Option<Value>>,
    reported: Mutex<HashSet<String>>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq1Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let id = thinq.id().to_string();
        let mut config = default_config(&meta, Some(json!({"name": "LG Air Conditioner"})));
        config.components = [
            (
                "climate".into(),
                json!({
                    "platform": "climate",
                    "unique_id": "$deviceid-climate",
                    "name": null,
                    "temperature_unit": "C",
                    "temp_step": 1,
                    "precision": 0.5,
                    "min_temp": 18,
                    "max_temp": 30,
                    "modes": ["off", "cool", "dry", "fan_only", "heat", "auto"],
                    "mode_state_topic": "$this/mode",
                    "mode_command_topic": "$this/mode/set",
                    "current_temperature_topic": "$this/current_temperature",
                    "temperature_state_topic": "$this/temperature",
                    "temperature_command_topic": "$this/temperature/set",
                    "fan_modes": ["very low", "low", "medium", "high", "very high", "auto"],
                    "fan_mode_state_topic": "$this/fan_mode",
                    "fan_mode_command_topic": "$this/fan_mode/set",
                    "swing_modes": ["1", "2", "3", "4", "5", "6", "on", "off"],
                    "swing_mode_state_topic": "$this/swing_mode",
                    "swing_mode_command_topic": "$this/swing_mode/set",
                    "swing_horizontal_modes": ["1", "2", "3", "4", "5", "1-3", "3-5", "on", "off"],
                    "swing_horizontal_mode_state_topic": "$this/swing_horizontal_mode",
                    "swing_horizontal_mode_command_topic": "$this/swing_horizontal_mode/set",
                }),
            ),
            (
                "jet".into(),
                json!({
                    "platform": "switch",
                    "unique_id": "$deviceid-jet",
                    "state_topic": "$this/jet",
                    "command_topic": "$this/jet/set",
                    "name": "Jet",
                    "icon": "mdi:wind-power",
                    "entity_category": "config",
                    "optimistic": true,
                }),
            ),
            (
                "airclean".into(),
                json!({
                    "platform": "switch",
                    "unique_id": "$deviceid-airclean",
                    "state_topic": "$this/airclean",
                    "command_topic": "$this/airclean/set",
                    "name": "Air purify",
                    "icon": "mdi:air-purifier",
                    "entity_category": "config",
                    "optimistic": true,
                }),
            ),
        ]
        .into_iter()
        .collect();

        ha.publish_property(&id, "availability", "online".into());
        ha.publish_config(&id, &config);

        let this = Arc::new(Self {
            id,
            model_id: meta.model_id.clone(),
            ha,
            thinq: thinq.clone(),
            publish_cache: Mutex::new(HashMap::new()),
            last_status: Mutex::new(None),
            reported: Mutex::new(HashSet::new()),
        });
        let handler = this.clone();
        thinq.on_data(Box::new(move |buf| handler.on_status(buf)));
        this
    }

    fn publish_property(&self, prop: &str, value: PropertyValue) {
        {
            let mut cache = self.publish_cache.lock();
            if let Some(prev) = cache.get(prop) {
                if value.eq_str(prev) {
                    return;
                }
            }
            cache.insert(prop.to_string(), value.as_string());
        }
        self.ha.publish_property(&self.id, prop, value);
    }

    fn publish_label(&self, prop: &str, label: Option<&str>) {
        // An unmapped code is published as "None" (HA shows unknown).
        self.publish_property(prop, label.unwrap_or("None").into());
    }

    fn on_status(&self, packet: &[u8]) {
        let status: Value = match serde_json::from_slice(packet) {
            Ok(v) => v,
            Err(_) => return,
        };
        let Some(operation) = status.get("Operation").and_then(Value::as_str) else {
            return;
        };
        // The first pushes after subscribing have every field zeroed; a setpoint of 0 cannot occur otherwise.
        if status.get("TempCfg").and_then(Value::as_str) == Some("0") {
            return;
        }
        let operation = operation.to_string();
        *self.last_status.lock() = Some(status.clone());

        let running = operation != "0";
        let mode = if running {
            self.label(MODES, json_str(&status, "OpMode"), "OpMode")
        } else {
            Some("off")
        };
        self.publish_label("mode", mode);
        self.publish_property(
            "current_temperature",
            js_number(status.get("TempCur")).into(),
        );
        self.publish_property("temperature", js_number(status.get("TempCfg")).into());
        self.publish_label(
            "fan_mode",
            self.label(FAN_MODES, json_str(&status, "WindStrength"), "WindStrength"),
        );
        self.publish_label(
            "swing_mode",
            self.label(SWING_MODES, json_str(&status, "WDirVStep"), "WDirVStep"),
        );
        self.publish_label(
            "swing_horizontal_mode",
            self.label(SWING_H_MODES, json_str(&status, "WDirHStep"), "WDirHStep"),
        );
        let jet = json_str(&status, "Jet") == Some("1");
        let airclean = json_str(&status, "AirClean") == Some("1");
        self.publish_property("jet", if jet { "ON" } else { "OFF" }.into());
        self.publish_property("airclean", if airclean { "ON" } else { "OFF" }.into());
    }

    fn label(
        &self,
        table: &'static [(i64, &'static str)],
        raw: Option<&str>,
        field: &str,
    ) -> Option<&'static str> {
        let raw = raw?;
        let parsed: f64 = match raw.parse() {
            Ok(n) => n,
            Err(_) => {
                self.note_unmapped(field, raw);
                return None;
            }
        };
        if let Some((_, label)) = table.iter().find(|(code, _)| *code as f64 == parsed) {
            return Some(*label);
        }
        self.note_unmapped(field, raw);
        None
    }

    fn note_unmapped(&self, field: &str, raw: &str) {
        let key = format!("{field}{raw}");
        let mut reported = self.reported.lock();
        if reported.insert(key) {
            tracing::debug!(model = %self.model_id, "unmapped {field} value {raw}");
        }
    }

    fn control(&self, key: &str, value: impl ToString) {
        self.thinq.send(json!({
            "Cmd": "Control",
            "CmdOpt": "Set",
            "Value": { key: value.to_string() },
            "Format": "B64",
            "Data": ""
        }));
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "mode" => {
                if mqtt_value == "off" {
                    self.control("Operation", 0);
                    return;
                }
                let Some(mode) = unmap(MODES, mqtt_value) else {
                    return;
                };
                // OpMode alone is ignored while the unit is off.
                let was_off = {
                    let status = self.last_status.lock();
                    status
                        .as_ref()
                        .and_then(|s| s.get("Operation"))
                        .and_then(Value::as_str)
                        == Some("0")
                };
                if was_off {
                    self.control("Operation", 1);
                }
                self.control("OpMode", mode);
            }
            "temperature" => self.control("TempCfg", js_round_string(mqtt_value)),
            "fan_mode" => {
                if let Some(fan) = unmap(FAN_MODES, mqtt_value) {
                    self.control("WindStrength", fan);
                }
            }
            "swing_mode" => {
                if let Some(step) = unmap(SWING_MODES, mqtt_value) {
                    self.control("WDirVStep", step);
                }
            }
            "swing_horizontal_mode" => {
                if let Some(step) = unmap(SWING_H_MODES, mqtt_value) {
                    self.control("WDirHStep", step);
                }
            }
            "jet" => self.control("Jet", if mqtt_value == "ON" { 1 } else { 0 }),
            "airclean" => self.control("AirClean", if mqtt_value == "ON" { 1 } else { 0 }),
            _ => tracing::warn!(prop, "unknown property"),
        }
    }
}

fn json_str<'a>(status: &'a Value, key: &str) -> Option<&'a str> {
    status.get(key).and_then(Value::as_str)
}

fn js_number(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::String(s)) => s.parse().unwrap_or(f64::NAN),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

/// `Math.round` for a finite MQTT number; non-numeric input becomes `"NaN"`.
fn js_round_string(raw: &str) -> String {
    match raw.parse::<f64>() {
        Ok(n) if n.is_finite() => {
            let rounded = (n + 0.5).floor() as i64;
            rounded.to_string()
        }
        _ => "NaN".into(),
    }
}

fn unmap(table: &'static [(i64, &'static str)], label: &str) -> Option<i64> {
    table
        .iter()
        .find(|(_, name)| *name == label)
        .map(|(code, _)| *code)
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.id
    }
    fn start(&self) {
        self.thinq.send(json!({ "Cmd": "Mon", "CmdOpt": "Start" }));
    }
    fn drop_device(&self) {
        self.ha
            .publish_property(&self.id, "availability", "offline".into());
    }
    fn set_property(&self, prop: &str, value: &str) {
        Device::set_property(self, prop, value);
    }
    fn publish_config(&self) {}
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{MockHaConnection, MockThinq1Device};

    const SAMPLE_OFF: &str = "eyJPcGVyYXRpb24iOiIwIiwiT3BNb2RlIjoiMiIsIldpbmRTdHJlbmd0aCI6IjIiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjIxIiwiVGVtcENmZyI6IjI0IiwiR3JvdXBUeXBlIjoiMSIsIlNsZWVwVGltZSI6IjAiLCJPblRpbWUiOiIwIiwiT2ZmVGltZSI6IjAiLCJSYWNBZGRGdW5jIjoiTlMiLCJFeHRyYU9wIjoiMCIsIkRpYWdDb2RlIjoiMDAiLCJUaW1lQnNPbiI6IjAiLCJUaW1lQnNPZmYiOiIwIiwiQWlyQ2xlYW4iOiIwIiwiQXV0b0RyeSI6IjAiLCJQb3dlclNhdmUiOiIwIiwiV0RpclZTdGVwIjoiMCIsIldEaXJIU3RlcCI6IjMiLCJUZW1wTGltaXRNYXgiOiIwIiwiVGVtcExpbWl0TWluIjoiMCIsIkR1Y3Rab25lVHlwZSI6IjAiLCJab25lQ29udHJvbCI6IjAiLCJEUkVEIjoiMCIsIlNlbnNvclBNMSI6IjAiLCJTZW5zb3JQTTIiOiIwIiwiU2Vuc29yUE0xMCI6IjAiLCJBaXJQb2x1dGlvbiI6IjAiLCJIdW1pZGl0eUNmZyI6IjAiLCJXYXRlclRlbXBDb29sTWluIjoiMCIsIldhdGVyVGVtcENvb2xNYXgiOiIwIiwiV2F0ZXJUZW1wSGVhdE1pbiI6IjAiLCJXYXRlclRlbXBIZWF0TWF4IjoiMCIsIkhvdFdhdGVyVGVtcE1pbiI6IjAiLCJIb3RXYXRlclRlbXBNYXgiOiIwIiwiU2Vuc29ySHVtaWRpdHkiOiIwIiwiVG90YWxBaXJQb2x1dGlvbiI6IjAiLCJTZW5zb3JNb24iOiIwIiwiQ2xlYW5EcnkiOiIwIiwiUHJvZHVjdFN0YXR1cyI6IjAiLCJBaXJNb25pdG9yaW5nIjoiMCIsIkh1bWlkaWZpY2F0aW9uIjoiMCIsIkFpckZhc3QiOiIwIiwiQWlyUmVtb3ZhbCI6IjAiLCJBaXJVVkRpc2luZmVjdGlvbiI6IjAiLCJXYXRlcnRhbmtMaWdodCI6IjAiLCJTaWduYWxMaWdodGluZyI6IjAiLCJXRGlyVXBEb3duIjoiMCIsIldEaXJMZWZ0UmlnaHQiOiIwIiwiV1N3aXJsIjoiMCIsIkpldCI6IjAiLCJMb3dIZWF0aW5nIjoiMCIsIkNpcmN1bGF0ZVN0cmVuZ3RoIjoiMCIsIkNpcmN1bGF0ZURpciI6IjAiLCJBbnRpQnVncyI6IjAiLCJJY2VWYWxsZXkiOiIwIiwiSHVtc2F2ZSI6IjAiLCJXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ3VyIjoiMCIsIkhvdFdhdGVyVGVtcENmZyI6IjAiLCJIb3RXYXRlck1vZGUiOiIwIiwiSG90V2F0ZXIiOiIwIiwiQVdIUFRlbXBDZmdTd2l0Y2giOiIwIiwiQWlyVGVtcENvb2xNaW4iOiIwIiwiQWlyVGVtcENvb2xNYXgiOiIwIiwiQWlyVGVtcEhlYXRNaW4iOiIwIiwiQWlyVGVtcEhlYXRNYXgiOiIwIiwiV2F0ZXJJblRlbXBDdXIiOiIwIiwiQVdIUFdBVGVtcENvbnRyb2xTdGEiOiIwIiwiRGlzcGxheUNvbnRyb2wiOiIwIiwiU21hcnRDYXJlIjoiMCIsIlR3b1NldENvb2xUZW1wIjoiMCIsIlR3b1NldEhlYXRUZW1wIjoiMCIsIlR3b1NldENvb2xVU0wiOiIwIiwiVHdvU2V0Q29vbExTTCI6IjAiLCJUd29TZXRIZWF0VVNMIjoiMCIsIlR3b1NldEhlYXRMU0wiOiIwIiwiVHdvU2V0QUNPU3RhdGUiOiIwIiwiVHdvU2V0TW9kZURlYWRiYW5kIjoiMCIsIlR3b1NldFN0YXRlIjoiMCJ9";
    const SAMPLE_COOL_23_HIGH: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiMCIsIldpbmRTdHJlbmd0aCI6IjYiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjIxIiwiVGVtcENmZyI6IjIzIiwiR3JvdXBUeXBlIjoiMSIsIlNsZWVwVGltZSI6IjAiLCJPblRpbWUiOiIwIiwiT2ZmVGltZSI6IjAiLCJSYWNBZGRGdW5jIjoiTlMiLCJFeHRyYU9wIjoiMCIsIkRpYWdDb2RlIjoiMDAiLCJUaW1lQnNPbiI6IjAiLCJUaW1lQnNPZmYiOiIwIiwiQWlyQ2xlYW4iOiIwIiwiQXV0b0RyeSI6IjAiLCJQb3dlclNhdmUiOiIwIiwiV0RpclZTdGVwIjoiMTAwIiwiV0RpckhTdGVwIjoiMyIsIlRlbXBMaW1pdE1heCI6IjAiLCJUZW1wTGltaXRNaW4iOiIwIiwiRHVjdFpvbmVUeXBlIjoiMCIsIlpvbmVDb250cm9sIjoiMCIsIkRSRUQiOiIwIiwiU2Vuc29yUE0xIjoiMCIsIlNlbnNvclBNMiI6IjAiLCJTZW5zb3JQTTEwIjoiMCIsIkFpclBvbHV0aW9uIjoiMCIsIkh1bWlkaXR5Q2ZnIjoiMCIsIldhdGVyVGVtcENvb2xNaW4iOiIwIiwiV2F0ZXJUZW1wQ29vbE1heCI6IjAiLCJXYXRlclRlbXBIZWF0TWluIjoiMCIsIldhdGVyVGVtcEhlYXRNYXgiOiIwIiwiSG90V2F0ZXJUZW1wTWluIjoiMCIsIkhvdFdhdGVyVGVtcE1heCI6IjAiLCJTZW5zb3JIdW1pZGl0eSI6IjAiLCJUb3RhbEFpclBvbHV0aW9uIjoiMCIsIlNlbnNvck1vbiI6IjAiLCJDbGVhbkRyeSI6IjAiLCJQcm9kdWN0U3RhdHVzIjoiMCIsIkFpck1vbml0b3JpbmciOiIwIiwiSHVtaWRpZmljYXRpb24iOiIwIiwiQWlyRmFzdCI6IjAiLCJBaXJSZW1vdmFsIjoiMCIsIkFpclVWRGlzaW5mZWN0aW9uIjoiMCIsIldhdGVydGFua0xpZ2h0IjoiMCIsIlNpZ25hbExpZ2h0aW5nIjoiMCIsIldEaXJVcERvd24iOiIwIiwiV0RpckxlZnRSaWdodCI6IjAiLCJXU3dpcmwiOiIwIiwiSmV0IjoiMCIsIkxvd0hlYXRpbmciOiIwIiwiQ2lyY3VsYXRlU3RyZW5ndGgiOiIwIiwiQ2lyY3VsYXRlRGlyIjoiMCIsIkFudGlCdWdzIjoiMCIsIkljZVZhbGxleSI6IjAiLCJIdW1zYXZlIjoiMCIsIldhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ2ZnIjoiMCIsIkhvdFdhdGVyTW9kZSI6IjAiLCJIb3RXYXRlciI6IjAiLCJBV0hQVGVtcENmZ1N3aXRjaCI6IjAiLCJBaXJUZW1wQ29vbE1pbiI6IjAiLCJBaXJUZW1wQ29vbE1heCI6IjAiLCJBaXJUZW1wSGVhdE1pbiI6IjAiLCJBaXJUZW1wSGVhdE1heCI6IjAiLCJXYXRlckluVGVtcEN1ciI6IjAiLCJBV0hQV0FUZW1wQ29udHJvbFN0YSI6IjAiLCJEaXNwbGF5Q29udHJvbCI6IjAiLCJTbWFydENhcmUiOiIwIiwiVHdvU2V0Q29vbFRlbXAiOiIwIiwiVHdvU2V0SGVhdFRlbXAiOiIwIiwiVHdvU2V0Q29vbFVTTCI6IjAiLCJUd29TZXRDb29sTFNMIjoiMCIsIlR3b1NldEhlYXRVU0wiOiIwIiwiVHdvU2V0SGVhdExTTCI6IjAiLCJUd29TZXRBQ09TdGF0ZSI6IjAiLCJUd29TZXRNb2RlRGVhZGJhbmQiOiIwIiwiVHdvU2V0U3RhdGUiOiIwIn0=";
    const SAMPLE_FAN_ONLY_LOW: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiMiIsIldpbmRTdHJlbmd0aCI6IjIiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjIxIiwiVGVtcENmZyI6IjI0IiwiR3JvdXBUeXBlIjoiMSIsIlNsZWVwVGltZSI6IjAiLCJPblRpbWUiOiIwIiwiT2ZmVGltZSI6IjAiLCJSYWNBZGRGdW5jIjoiTlMiLCJFeHRyYU9wIjoiMCIsIkRpYWdDb2RlIjoiMDAiLCJUaW1lQnNPbiI6IjAiLCJUaW1lQnNPZmYiOiIwIiwiQWlyQ2xlYW4iOiIwIiwiQXV0b0RyeSI6IjAiLCJQb3dlclNhdmUiOiIwIiwiV0RpclZTdGVwIjoiMCIsIldEaXJIU3RlcCI6IjMiLCJUZW1wTGltaXRNYXgiOiIwIiwiVGVtcExpbWl0TWluIjoiMCIsIkR1Y3Rab25lVHlwZSI6IjAiLCJab25lQ29udHJvbCI6IjAiLCJEUkVEIjoiMCIsIlNlbnNvclBNMSI6IjAiLCJTZW5zb3JQTTIiOiIwIiwiU2Vuc29yUE0xMCI6IjAiLCJBaXJQb2x1dGlvbiI6IjAiLCJIdW1pZGl0eUNmZyI6IjAiLCJXYXRlclRlbXBDb29sTWluIjoiMCIsIldhdGVyVGVtcENvb2xNYXgiOiIwIiwiV2F0ZXJUZW1wSGVhdE1pbiI6IjAiLCJXYXRlclRlbXBIZWF0TWF4IjoiMCIsIkhvdFdhdGVyVGVtcE1pbiI6IjAiLCJIb3RXYXRlclRlbXBNYXgiOiIwIiwiU2Vuc29ySHVtaWRpdHkiOiIwIiwiVG90YWxBaXJQb2x1dGlvbiI6IjAiLCJTZW5zb3JNb24iOiIwIiwiQ2xlYW5EcnkiOiIwIiwiUHJvZHVjdFN0YXR1cyI6IjAiLCJBaXJNb25pdG9yaW5nIjoiMCIsIkh1bWlkaWZpY2F0aW9uIjoiMCIsIkFpckZhc3QiOiIwIiwiQWlyUmVtb3ZhbCI6IjAiLCJBaXJVVkRpc2luZmVjdGlvbiI6IjAiLCJXYXRlcnRhbmtMaWdodCI6IjAiLCJTaWduYWxMaWdodGluZyI6IjAiLCJXRGlyVXBEb3duIjoiMCIsIldEaXJMZWZ0UmlnaHQiOiIwIiwiV1N3aXJsIjoiMCIsIkpldCI6IjAiLCJMb3dIZWF0aW5nIjoiMCIsIkNpcmN1bGF0ZVN0cmVuZ3RoIjoiMCIsIkNpcmN1bGF0ZURpciI6IjAiLCJBbnRpQnVncyI6IjAiLCJJY2VWYWxsZXkiOiIwIiwiSHVtc2F2ZSI6IjAiLCJXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ3VyIjoiMCIsIkhvdFdhdGVyVGVtcENmZyI6IjAiLCJIb3RXYXRlck1vZGUiOiIwIiwiSG90V2F0ZXIiOiIwIiwiQVdIUFRlbXBDZmdTd2l0Y2giOiIwIiwiQWlyVGVtcENvb2xNaW4iOiIwIiwiQWlyVGVtcENvb2xNYXgiOiIwIiwiQWlyVGVtcEhlYXRNaW4iOiIwIiwiQWlyVGVtcEhlYXRNYXgiOiIwIiwiV2F0ZXJJblRlbXBDdXIiOiIwIiwiQVdIUFdBVGVtcENvbnRyb2xTdGEiOiIwIiwiRGlzcGxheUNvbnRyb2wiOiIwIiwiU21hcnRDYXJlIjoiMCIsIlR3b1NldENvb2xUZW1wIjoiMCIsIlR3b1NldEhlYXRUZW1wIjoiMCIsIlR3b1NldENvb2xVU0wiOiIwIiwiVHdvU2V0Q29vbExTTCI6IjAiLCJUd29TZXRIZWF0VVNMIjoiMCIsIlR3b1NldEhlYXRMU0wiOiIwIiwiVHdvU2V0QUNPU3RhdGUiOiIwIiwiVHdvU2V0TW9kZURlYWRiYW5kIjoiMCIsIlR3b1NldFN0YXRlIjoiMCJ9";
    const SAMPLE_DRY: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiMSIsIldpbmRTdHJlbmd0aCI6IjIiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjE5LjUiLCJUZW1wQ2ZnIjoiMTkiLCJHcm91cFR5cGUiOiIxIiwiU2xlZXBUaW1lIjoiMCIsIk9uVGltZSI6IjAiLCJPZmZUaW1lIjoiMCIsIlJhY0FkZEZ1bmMiOiJOUyIsIkV4dHJhT3AiOiIwIiwiRGlhZ0NvZGUiOiIwMCIsIlRpbWVCc09uIjoiMCIsIlRpbWVCc09mZiI6IjAiLCJBaXJDbGVhbiI6IjAiLCJBdXRvRHJ5IjoiMCIsIlBvd2VyU2F2ZSI6IjAiLCJXRGlyVlN0ZXAiOiIwIiwiV0RpckhTdGVwIjoiMyIsIlRlbXBMaW1pdE1heCI6IjAiLCJUZW1wTGltaXRNaW4iOiIwIiwiRHVjdFpvbmVUeXBlIjoiMCIsIlpvbmVDb250cm9sIjoiMCIsIkRSRUQiOiIwIiwiU2Vuc29yUE0xIjoiMCIsIlNlbnNvclBNMiI6IjAiLCJTZW5zb3JQTTEwIjoiMCIsIkFpclBvbHV0aW9uIjoiMCIsIkh1bWlkaXR5Q2ZnIjoiMCIsIldhdGVyVGVtcENvb2xNaW4iOiIwIiwiV2F0ZXJUZW1wQ29vbE1heCI6IjAiLCJXYXRlclRlbXBIZWF0TWluIjoiMCIsIldhdGVyVGVtcEhlYXRNYXgiOiIwIiwiSG90V2F0ZXJUZW1wTWluIjoiMCIsIkhvdFdhdGVyVGVtcE1heCI6IjAiLCJTZW5zb3JIdW1pZGl0eSI6IjAiLCJUb3RhbEFpclBvbHV0aW9uIjoiMCIsIlNlbnNvck1vbiI6IjAiLCJDbGVhbkRyeSI6IjAiLCJQcm9kdWN0U3RhdHVzIjoiMCIsIkFpck1vbml0b3JpbmciOiIwIiwiSHVtaWRpZmljYXRpb24iOiIwIiwiQWlyRmFzdCI6IjAiLCJBaXJSZW1vdmFsIjoiMCIsIkFpclVWRGlzaW5mZWN0aW9uIjoiMCIsIldhdGVydGFua0xpZ2h0IjoiMCIsIlNpZ25hbExpZ2h0aW5nIjoiMCIsIldEaXJVcERvd24iOiIwIiwiV0RpckxlZnRSaWdodCI6IjAiLCJXU3dpcmwiOiIwIiwiSmV0IjoiMCIsIkxvd0hlYXRpbmciOiIwIiwiQ2lyY3VsYXRlU3RyZW5ndGgiOiIwIiwiQ2lyY3VsYXRlRGlyIjoiMCIsIkFudGlCdWdzIjoiMCIsIkljZVZhbGxleSI6IjAiLCJIdW1zYXZlIjoiMCIsIldhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ2ZnIjoiMCIsIkhvdFdhdGVyTW9kZSI6IjAiLCJIb3RXYXRlciI6IjAiLCJBV0hQVGVtcENmZ1N3aXRjaCI6IjAiLCJBaXJUZW1wQ29vbE1pbiI6IjAiLCJBaXJUZW1wQ29vbE1heCI6IjAiLCJBaXJUZW1wSGVhdE1pbiI6IjAiLCJBaXJUZW1wSGVhdE1heCI6IjAiLCJXYXRlckluVGVtcEN1ciI6IjAiLCJBV0hQV0FUZW1wQ29udHJvbFN0YSI6IjAiLCJEaXNwbGF5Q29udHJvbCI6IjAiLCJTbWFydENhcmUiOiIwIiwiVHdvU2V0Q29vbFRlbXAiOiIwIiwiVHdvU2V0SGVhdFRlbXAiOiIwIiwiVHdvU2V0Q29vbFVTTCI6IjAiLCJUd29TZXRDb29sTFNMIjoiMCIsIlR3b1NldEhlYXRVU0wiOiIwIiwiVHdvU2V0SGVhdExTTCI6IjAiLCJUd29TZXRBQ09TdGF0ZSI6IjAiLCJUd29TZXRNb2RlRGVhZGJhbmQiOiIwIiwiVHdvU2V0U3RhdGUiOiIwIn0=";
    const SAMPLE_HEAT: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiNCIsIldpbmRTdHJlbmd0aCI6IjIiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjE5LjUiLCJUZW1wQ2ZnIjoiMzAiLCJHcm91cFR5cGUiOiIxIiwiU2xlZXBUaW1lIjoiMCIsIk9uVGltZSI6IjAiLCJPZmZUaW1lIjoiMCIsIlJhY0FkZEZ1bmMiOiJOUyIsIkV4dHJhT3AiOiIwIiwiRGlhZ0NvZGUiOiIwMCIsIlRpbWVCc09uIjoiMCIsIlRpbWVCc09mZiI6IjAiLCJBaXJDbGVhbiI6IjAiLCJBdXRvRHJ5IjoiMCIsIlBvd2VyU2F2ZSI6IjAiLCJXRGlyVlN0ZXAiOiIwIiwiV0RpckhTdGVwIjoiMyIsIlRlbXBMaW1pdE1heCI6IjAiLCJUZW1wTGltaXRNaW4iOiIwIiwiRHVjdFpvbmVUeXBlIjoiMCIsIlpvbmVDb250cm9sIjoiMCIsIkRSRUQiOiIwIiwiU2Vuc29yUE0xIjoiMCIsIlNlbnNvclBNMiI6IjAiLCJTZW5zb3JQTTEwIjoiMCIsIkFpclBvbHV0aW9uIjoiMCIsIkh1bWlkaXR5Q2ZnIjoiMCIsIldhdGVyVGVtcENvb2xNaW4iOiIwIiwiV2F0ZXJUZW1wQ29vbE1heCI6IjAiLCJXYXRlclRlbXBIZWF0TWluIjoiMCIsIldhdGVyVGVtcEhlYXRNYXgiOiIwIiwiSG90V2F0ZXJUZW1wTWluIjoiMCIsIkhvdFdhdGVyVGVtcE1heCI6IjAiLCJTZW5zb3JIdW1pZGl0eSI6IjAiLCJUb3RhbEFpclBvbHV0aW9uIjoiMCIsIlNlbnNvck1vbiI6IjAiLCJDbGVhbkRyeSI6IjAiLCJQcm9kdWN0U3RhdHVzIjoiMCIsIkFpck1vbml0b3JpbmciOiIwIiwiSHVtaWRpZmljYXRpb24iOiIwIiwiQWlyRmFzdCI6IjAiLCJBaXJSZW1vdmFsIjoiMCIsIkFpclVWRGlzaW5mZWN0aW9uIjoiMCIsIldhdGVydGFua0xpZ2h0IjoiMCIsIlNpZ25hbExpZ2h0aW5nIjoiMCIsIldEaXJVcERvd24iOiIwIiwiV0RpckxlZnRSaWdodCI6IjAiLCJXU3dpcmwiOiIwIiwiSmV0IjoiMCIsIkxvd0hlYXRpbmciOiIwIiwiQ2lyY3VsYXRlU3RyZW5ndGgiOiIwIiwiQ2lyY3VsYXRlRGlyIjoiMCIsIkFudGlCdWdzIjoiMCIsIkljZVZhbGxleSI6IjAiLCJIdW1zYXZlIjoiMCIsIldhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ2ZnIjoiMCIsIkhvdFdhdGVyTW9kZSI6IjAiLCJIb3RXYXRlciI6IjAiLCJBV0hQVGVtcENmZ1N3aXRjaCI6IjAiLCJBaXJUZW1wQ29vbE1pbiI6IjAiLCJBaXJUZW1wQ29vbE1heCI6IjAiLCJBaXJUZW1wSGVhdE1pbiI6IjAiLCJBaXJUZW1wSGVhdE1heCI6IjAiLCJXYXRlckluVGVtcEN1ciI6IjAiLCJBV0hQV0FUZW1wQ29udHJvbFN0YSI6IjAiLCJEaXNwbGF5Q29udHJvbCI6IjAiLCJTbWFydENhcmUiOiIwIiwiVHdvU2V0Q29vbFRlbXAiOiIwIiwiVHdvU2V0SGVhdFRlbXAiOiIwIiwiVHdvU2V0Q29vbFVTTCI6IjAiLCJUd29TZXRDb29sTFNMIjoiMCIsIlR3b1NldEhlYXRVU0wiOiIwIiwiVHdvU2V0SGVhdExTTCI6IjAiLCJUd29TZXRBQ09TdGF0ZSI6IjAiLCJUd29TZXRNb2RlRGVhZGJhbmQiOiIwIiwiVHdvU2V0U3RhdGUiOiIwIn0=";
    const SAMPLE_AUTO: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiNiIsIldpbmRTdHJlbmd0aCI6IjIiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjE5LjUiLCJUZW1wQ2ZnIjoiMjIiLCJHcm91cFR5cGUiOiIxIiwiU2xlZXBUaW1lIjoiMCIsIk9uVGltZSI6IjAiLCJPZmZUaW1lIjoiMCIsIlJhY0FkZEZ1bmMiOiJOUyIsIkV4dHJhT3AiOiIwIiwiRGlhZ0NvZGUiOiIwMCIsIlRpbWVCc09uIjoiMCIsIlRpbWVCc09mZiI6IjAiLCJBaXJDbGVhbiI6IjAiLCJBdXRvRHJ5IjoiMCIsIlBvd2VyU2F2ZSI6IjAiLCJXRGlyVlN0ZXAiOiIwIiwiV0RpckhTdGVwIjoiMyIsIlRlbXBMaW1pdE1heCI6IjAiLCJUZW1wTGltaXRNaW4iOiIwIiwiRHVjdFpvbmVUeXBlIjoiMCIsIlpvbmVDb250cm9sIjoiMCIsIkRSRUQiOiIwIiwiU2Vuc29yUE0xIjoiMCIsIlNlbnNvclBNMiI6IjAiLCJTZW5zb3JQTTEwIjoiMCIsIkFpclBvbHV0aW9uIjoiMCIsIkh1bWlkaXR5Q2ZnIjoiMCIsIldhdGVyVGVtcENvb2xNaW4iOiIwIiwiV2F0ZXJUZW1wQ29vbE1heCI6IjAiLCJXYXRlclRlbXBIZWF0TWluIjoiMCIsIldhdGVyVGVtcEhlYXRNYXgiOiIwIiwiSG90V2F0ZXJUZW1wTWluIjoiMCIsIkhvdFdhdGVyVGVtcE1heCI6IjAiLCJTZW5zb3JIdW1pZGl0eSI6IjAiLCJUb3RhbEFpclBvbHV0aW9uIjoiMCIsIlNlbnNvck1vbiI6IjAiLCJDbGVhbkRyeSI6IjAiLCJQcm9kdWN0U3RhdHVzIjoiMCIsIkFpck1vbml0b3JpbmciOiIwIiwiSHVtaWRpZmljYXRpb24iOiIwIiwiQWlyRmFzdCI6IjAiLCJBaXJSZW1vdmFsIjoiMCIsIkFpclVWRGlzaW5mZWN0aW9uIjoiMCIsIldhdGVydGFua0xpZ2h0IjoiMCIsIlNpZ25hbExpZ2h0aW5nIjoiMCIsIldEaXJVcERvd24iOiIwIiwiV0RpckxlZnRSaWdodCI6IjAiLCJXU3dpcmwiOiIwIiwiSmV0IjoiMCIsIkxvd0hlYXRpbmciOiIwIiwiQ2lyY3VsYXRlU3RyZW5ndGgiOiIwIiwiQ2lyY3VsYXRlRGlyIjoiMCIsIkFudGlCdWdzIjoiMCIsIkljZVZhbGxleSI6IjAiLCJIdW1zYXZlIjoiMCIsIldhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ2ZnIjoiMCIsIkhvdFdhdGVyTW9kZSI6IjAiLCJIb3RXYXRlciI6IjAiLCJBV0hQVGVtcENmZ1N3aXRjaCI6IjAiLCJBaXJUZW1wQ29vbE1pbiI6IjAiLCJBaXJUZW1wQ29vbE1heCI6IjAiLCJBaXJUZW1wSGVhdE1pbiI6IjAiLCJBaXJUZW1wSGVhdE1heCI6IjAiLCJXYXRlckluVGVtcEN1ciI6IjAiLCJBV0hQV0FUZW1wQ29udHJvbFN0YSI6IjAiLCJEaXNwbGF5Q29udHJvbCI6IjAiLCJTbWFydENhcmUiOiIwIiwiVHdvU2V0Q29vbFRlbXAiOiIwIiwiVHdvU2V0SGVhdFRlbXAiOiIwIiwiVHdvU2V0Q29vbFVTTCI6IjAiLCJUd29TZXRDb29sTFNMIjoiMCIsIlR3b1NldEhlYXRVU0wiOiIwIiwiVHdvU2V0SGVhdExTTCI6IjAiLCJUd29TZXRBQ09TdGF0ZSI6IjAiLCJUd29TZXRNb2RlRGVhZGJhbmQiOiIwIiwiVHdvU2V0U3RhdGUiOiIwIn0=";
    const SAMPLE_FAN_AUTO: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiMiIsIldpbmRTdHJlbmd0aCI6IjgiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjE5LjUiLCJUZW1wQ2ZnIjoiMjQiLCJHcm91cFR5cGUiOiIxIiwiU2xlZXBUaW1lIjoiMCIsIk9uVGltZSI6IjAiLCJPZmZUaW1lIjoiMCIsIlJhY0FkZEZ1bmMiOiJOUyIsIkV4dHJhT3AiOiIwIiwiRGlhZ0NvZGUiOiIwMCIsIlRpbWVCc09uIjoiMCIsIlRpbWVCc09mZiI6IjAiLCJBaXJDbGVhbiI6IjAiLCJBdXRvRHJ5IjoiMCIsIlBvd2VyU2F2ZSI6IjAiLCJXRGlyVlN0ZXAiOiIwIiwiV0RpckhTdGVwIjoiMyIsIlRlbXBMaW1pdE1heCI6IjAiLCJUZW1wTGltaXRNaW4iOiIwIiwiRHVjdFpvbmVUeXBlIjoiMCIsIlpvbmVDb250cm9sIjoiMCIsIkRSRUQiOiIwIiwiU2Vuc29yUE0xIjoiMCIsIlNlbnNvclBNMiI6IjAiLCJTZW5zb3JQTTEwIjoiMCIsIkFpclBvbHV0aW9uIjoiMCIsIkh1bWlkaXR5Q2ZnIjoiMCIsIldhdGVyVGVtcENvb2xNaW4iOiIwIiwiV2F0ZXJUZW1wQ29vbE1heCI6IjAiLCJXYXRlclRlbXBIZWF0TWluIjoiMCIsIldhdGVyVGVtcEhlYXRNYXgiOiIwIiwiSG90V2F0ZXJUZW1wTWluIjoiMCIsIkhvdFdhdGVyVGVtcE1heCI6IjAiLCJTZW5zb3JIdW1pZGl0eSI6IjAiLCJUb3RhbEFpclBvbHV0aW9uIjoiMCIsIlNlbnNvck1vbiI6IjAiLCJDbGVhbkRyeSI6IjAiLCJQcm9kdWN0U3RhdHVzIjoiMCIsIkFpck1vbml0b3JpbmciOiIwIiwiSHVtaWRpZmljYXRpb24iOiIwIiwiQWlyRmFzdCI6IjAiLCJBaXJSZW1vdmFsIjoiMCIsIkFpclVWRGlzaW5mZWN0aW9uIjoiMCIsIldhdGVydGFua0xpZ2h0IjoiMCIsIlNpZ25hbExpZ2h0aW5nIjoiMCIsIldEaXJVcERvd24iOiIwIiwiV0RpckxlZnRSaWdodCI6IjAiLCJXU3dpcmwiOiIwIiwiSmV0IjoiMCIsIkxvd0hlYXRpbmciOiIwIiwiQ2lyY3VsYXRlU3RyZW5ndGgiOiIwIiwiQ2lyY3VsYXRlRGlyIjoiMCIsIkFudGlCdWdzIjoiMCIsIkljZVZhbGxleSI6IjAiLCJIdW1zYXZlIjoiMCIsIldhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ2ZnIjoiMCIsIkhvdFdhdGVyTW9kZSI6IjAiLCJIb3RXYXRlciI6IjAiLCJBV0hQVGVtcENmZ1N3aXRjaCI6IjAiLCJBaXJUZW1wQ29vbE1pbiI6IjAiLCJBaXJUZW1wQ29vbE1heCI6IjAiLCJBaXJUZW1wSGVhdE1pbiI6IjAiLCJBaXJUZW1wSGVhdE1heCI6IjAiLCJXYXRlckluVGVtcEN1ciI6IjAiLCJBV0hQV0FUZW1wQ29udHJvbFN0YSI6IjAiLCJEaXNwbGF5Q29udHJvbCI6IjAiLCJTbWFydENhcmUiOiIwIiwiVHdvU2V0Q29vbFRlbXAiOiIwIiwiVHdvU2V0SGVhdFRlbXAiOiIwIiwiVHdvU2V0Q29vbFVTTCI6IjAiLCJUd29TZXRDb29sTFNMIjoiMCIsIlR3b1NldEhlYXRVU0wiOiIwIiwiVHdvU2V0SGVhdExTTCI6IjAiLCJUd29TZXRBQ09TdGF0ZSI6IjAiLCJUd29TZXRNb2RlRGVhZGJhbmQiOiIwIiwiVHdvU2V0U3RhdGUiOiIwIn0=";
    const SAMPLE_VSWING_ON: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiMiIsIldpbmRTdHJlbmd0aCI6IjIiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjIxIiwiVGVtcENmZyI6IjI0IiwiR3JvdXBUeXBlIjoiMSIsIlNsZWVwVGltZSI6IjAiLCJPblRpbWUiOiIwIiwiT2ZmVGltZSI6IjAiLCJSYWNBZGRGdW5jIjoiTlMiLCJFeHRyYU9wIjoiMCIsIkRpYWdDb2RlIjoiMDAiLCJUaW1lQnNPbiI6IjAiLCJUaW1lQnNPZmYiOiIwIiwiQWlyQ2xlYW4iOiIwIiwiQXV0b0RyeSI6IjAiLCJQb3dlclNhdmUiOiIwIiwiV0RpclZTdGVwIjoiMTAwIiwiV0RpckhTdGVwIjoiMyIsIlRlbXBMaW1pdE1heCI6IjAiLCJUZW1wTGltaXRNaW4iOiIwIiwiRHVjdFpvbmVUeXBlIjoiMCIsIlpvbmVDb250cm9sIjoiMCIsIkRSRUQiOiIwIiwiU2Vuc29yUE0xIjoiMCIsIlNlbnNvclBNMiI6IjAiLCJTZW5zb3JQTTEwIjoiMCIsIkFpclBvbHV0aW9uIjoiMCIsIkh1bWlkaXR5Q2ZnIjoiMCIsIldhdGVyVGVtcENvb2xNaW4iOiIwIiwiV2F0ZXJUZW1wQ29vbE1heCI6IjAiLCJXYXRlclRlbXBIZWF0TWluIjoiMCIsIldhdGVyVGVtcEhlYXRNYXgiOiIwIiwiSG90V2F0ZXJUZW1wTWluIjoiMCIsIkhvdFdhdGVyVGVtcE1heCI6IjAiLCJTZW5zb3JIdW1pZGl0eSI6IjAiLCJUb3RhbEFpclBvbHV0aW9uIjoiMCIsIlNlbnNvck1vbiI6IjAiLCJDbGVhbkRyeSI6IjAiLCJQcm9kdWN0U3RhdHVzIjoiMCIsIkFpck1vbml0b3JpbmciOiIwIiwiSHVtaWRpZmljYXRpb24iOiIwIiwiQWlyRmFzdCI6IjAiLCJBaXJSZW1vdmFsIjoiMCIsIkFpclVWRGlzaW5mZWN0aW9uIjoiMCIsIldhdGVydGFua0xpZ2h0IjoiMCIsIlNpZ25hbExpZ2h0aW5nIjoiMCIsIldEaXJVcERvd24iOiIwIiwiV0RpckxlZnRSaWdodCI6IjAiLCJXU3dpcmwiOiIwIiwiSmV0IjoiMCIsIkxvd0hlYXRpbmciOiIwIiwiQ2lyY3VsYXRlU3RyZW5ndGgiOiIwIiwiQ2lyY3VsYXRlRGlyIjoiMCIsIkFudGlCdWdzIjoiMCIsIkljZVZhbGxleSI6IjAiLCJIdW1zYXZlIjoiMCIsIldhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ2ZnIjoiMCIsIkhvdFdhdGVyTW9kZSI6IjAiLCJIb3RXYXRlciI6IjAiLCJBV0hQVGVtcENmZ1N3aXRjaCI6IjAiLCJBaXJUZW1wQ29vbE1pbiI6IjAiLCJBaXJUZW1wQ29vbE1heCI6IjAiLCJBaXJUZW1wSGVhdE1pbiI6IjAiLCJBaXJUZW1wSGVhdE1heCI6IjAiLCJXYXRlckluVGVtcEN1ciI6IjAiLCJBV0hQV0FUZW1wQ29udHJvbFN0YSI6IjAiLCJEaXNwbGF5Q29udHJvbCI6IjAiLCJTbWFydENhcmUiOiIwIiwiVHdvU2V0Q29vbFRlbXAiOiIwIiwiVHdvU2V0SGVhdFRlbXAiOiIwIiwiVHdvU2V0Q29vbFVTTCI6IjAiLCJUd29TZXRDb29sTFNMIjoiMCIsIlR3b1NldEhlYXRVU0wiOiIwIiwiVHdvU2V0SGVhdExTTCI6IjAiLCJUd29TZXRBQ09TdGF0ZSI6IjAiLCJUd29TZXRNb2RlRGVhZGJhbmQiOiIwIiwiVHdvU2V0U3RhdGUiOiIwIn0=";
    const SAMPLE_VSWING_STEP1: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiMiIsIldpbmRTdHJlbmd0aCI6IjIiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjE5LjUiLCJUZW1wQ2ZnIjoiMjMiLCJHcm91cFR5cGUiOiIxIiwiU2xlZXBUaW1lIjoiMCIsIk9uVGltZSI6IjAiLCJPZmZUaW1lIjoiMCIsIlJhY0FkZEZ1bmMiOiJOUyIsIkV4dHJhT3AiOiIwIiwiRGlhZ0NvZGUiOiIwMCIsIlRpbWVCc09uIjoiMCIsIlRpbWVCc09mZiI6IjAiLCJBaXJDbGVhbiI6IjAiLCJBdXRvRHJ5IjoiMCIsIlBvd2VyU2F2ZSI6IjAiLCJXRGlyVlN0ZXAiOiIxIiwiV0RpckhTdGVwIjoiMyIsIlRlbXBMaW1pdE1heCI6IjAiLCJUZW1wTGltaXRNaW4iOiIwIiwiRHVjdFpvbmVUeXBlIjoiMCIsIlpvbmVDb250cm9sIjoiMCIsIkRSRUQiOiIwIiwiU2Vuc29yUE0xIjoiMCIsIlNlbnNvclBNMiI6IjAiLCJTZW5zb3JQTTEwIjoiMCIsIkFpclBvbHV0aW9uIjoiMCIsIkh1bWlkaXR5Q2ZnIjoiMCIsIldhdGVyVGVtcENvb2xNaW4iOiIwIiwiV2F0ZXJUZW1wQ29vbE1heCI6IjAiLCJXYXRlclRlbXBIZWF0TWluIjoiMCIsIldhdGVyVGVtcEhlYXRNYXgiOiIwIiwiSG90V2F0ZXJUZW1wTWluIjoiMCIsIkhvdFdhdGVyVGVtcE1heCI6IjAiLCJTZW5zb3JIdW1pZGl0eSI6IjAiLCJUb3RhbEFpclBvbHV0aW9uIjoiMCIsIlNlbnNvck1vbiI6IjAiLCJDbGVhbkRyeSI6IjAiLCJQcm9kdWN0U3RhdHVzIjoiMCIsIkFpck1vbml0b3JpbmciOiIwIiwiSHVtaWRpZmljYXRpb24iOiIwIiwiQWlyRmFzdCI6IjAiLCJBaXJSZW1vdmFsIjoiMCIsIkFpclVWRGlzaW5mZWN0aW9uIjoiMCIsIldhdGVydGFua0xpZ2h0IjoiMCIsIlNpZ25hbExpZ2h0aW5nIjoiMCIsIldEaXJVcERvd24iOiIwIiwiV0RpckxlZnRSaWdodCI6IjAiLCJXU3dpcmwiOiIwIiwiSmV0IjoiMCIsIkxvd0hlYXRpbmciOiIwIiwiQ2lyY3VsYXRlU3RyZW5ndGgiOiIwIiwiQ2lyY3VsYXRlRGlyIjoiMCIsIkFudGlCdWdzIjoiMCIsIkljZVZhbGxleSI6IjAiLCJIdW1zYXZlIjoiMCIsIldhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ2ZnIjoiMCIsIkhvdFdhdGVyTW9kZSI6IjAiLCJIb3RXYXRlciI6IjAiLCJBV0hQVGVtcENmZ1N3aXRjaCI6IjAiLCJBaXJUZW1wQ29vbE1pbiI6IjAiLCJBaXJUZW1wQ29vbE1heCI6IjAiLCJBaXJUZW1wSGVhdE1pbiI6IjAiLCJBaXJUZW1wSGVhdE1heCI6IjAiLCJXYXRlckluVGVtcEN1ciI6IjAiLCJBV0hQV0FUZW1wQ29udHJvbFN0YSI6IjAiLCJEaXNwbGF5Q29udHJvbCI6IjAiLCJTbWFydENhcmUiOiIwIiwiVHdvU2V0Q29vbFRlbXAiOiIwIiwiVHdvU2V0SGVhdFRlbXAiOiIwIiwiVHdvU2V0Q29vbFVTTCI6IjAiLCJUd29TZXRDb29sTFNMIjoiMCIsIlR3b1NldEhlYXRVU0wiOiIwIiwiVHdvU2V0SGVhdExTTCI6IjAiLCJUd29TZXRBQ09TdGF0ZSI6IjAiLCJUd29TZXRNb2RlRGVhZGJhbmQiOiIwIiwiVHdvU2V0U3RhdGUiOiIwIn0=";
    const SAMPLE_HSWING_STEP4: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiMiIsIldpbmRTdHJlbmd0aCI6IjIiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjE5LjUiLCJUZW1wQ2ZnIjoiMjMiLCJHcm91cFR5cGUiOiIxIiwiU2xlZXBUaW1lIjoiMCIsIk9uVGltZSI6IjAiLCJPZmZUaW1lIjoiMCIsIlJhY0FkZEZ1bmMiOiJOUyIsIkV4dHJhT3AiOiIwIiwiRGlhZ0NvZGUiOiIwMCIsIlRpbWVCc09uIjoiMCIsIlRpbWVCc09mZiI6IjAiLCJBaXJDbGVhbiI6IjAiLCJBdXRvRHJ5IjoiMCIsIlBvd2VyU2F2ZSI6IjAiLCJXRGlyVlN0ZXAiOiIwIiwiV0RpckhTdGVwIjoiNCIsIlRlbXBMaW1pdE1heCI6IjAiLCJUZW1wTGltaXRNaW4iOiIwIiwiRHVjdFpvbmVUeXBlIjoiMCIsIlpvbmVDb250cm9sIjoiMCIsIkRSRUQiOiIwIiwiU2Vuc29yUE0xIjoiMCIsIlNlbnNvclBNMiI6IjAiLCJTZW5zb3JQTTEwIjoiMCIsIkFpclBvbHV0aW9uIjoiMCIsIkh1bWlkaXR5Q2ZnIjoiMCIsIldhdGVyVGVtcENvb2xNaW4iOiIwIiwiV2F0ZXJUZW1wQ29vbE1heCI6IjAiLCJXYXRlclRlbXBIZWF0TWluIjoiMCIsIldhdGVyVGVtcEhlYXRNYXgiOiIwIiwiSG90V2F0ZXJUZW1wTWluIjoiMCIsIkhvdFdhdGVyVGVtcE1heCI6IjAiLCJTZW5zb3JIdW1pZGl0eSI6IjAiLCJUb3RhbEFpclBvbHV0aW9uIjoiMCIsIlNlbnNvck1vbiI6IjAiLCJDbGVhbkRyeSI6IjAiLCJQcm9kdWN0U3RhdHVzIjoiMCIsIkFpck1vbml0b3JpbmciOiIwIiwiSHVtaWRpZmljYXRpb24iOiIwIiwiQWlyRmFzdCI6IjAiLCJBaXJSZW1vdmFsIjoiMCIsIkFpclVWRGlzaW5mZWN0aW9uIjoiMCIsIldhdGVydGFua0xpZ2h0IjoiMCIsIlNpZ25hbExpZ2h0aW5nIjoiMCIsIldEaXJVcERvd24iOiIwIiwiV0RpckxlZnRSaWdodCI6IjAiLCJXU3dpcmwiOiIwIiwiSmV0IjoiMCIsIkxvd0hlYXRpbmciOiIwIiwiQ2lyY3VsYXRlU3RyZW5ndGgiOiIwIiwiQ2lyY3VsYXRlRGlyIjoiMCIsIkFudGlCdWdzIjoiMCIsIkljZVZhbGxleSI6IjAiLCJIdW1zYXZlIjoiMCIsIldhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ2ZnIjoiMCIsIkhvdFdhdGVyTW9kZSI6IjAiLCJIb3RXYXRlciI6IjAiLCJBV0hQVGVtcENmZ1N3aXRjaCI6IjAiLCJBaXJUZW1wQ29vbE1pbiI6IjAiLCJBaXJUZW1wQ29vbE1heCI6IjAiLCJBaXJUZW1wSGVhdE1pbiI6IjAiLCJBaXJUZW1wSGVhdE1heCI6IjAiLCJXYXRlckluVGVtcEN1ciI6IjAiLCJBV0hQV0FUZW1wQ29udHJvbFN0YSI6IjAiLCJEaXNwbGF5Q29udHJvbCI6IjAiLCJTbWFydENhcmUiOiIwIiwiVHdvU2V0Q29vbFRlbXAiOiIwIiwiVHdvU2V0SGVhdFRlbXAiOiIwIiwiVHdvU2V0Q29vbFVTTCI6IjAiLCJUd29TZXRDb29sTFNMIjoiMCIsIlR3b1NldEhlYXRVU0wiOiIwIiwiVHdvU2V0SGVhdExTTCI6IjAiLCJUd29TZXRBQ09TdGF0ZSI6IjAiLCJUd29TZXRNb2RlRGVhZGJhbmQiOiIwIiwiVHdvU2V0U3RhdGUiOiIwIn0=";
    const SAMPLE_JET_ON: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiMCIsIldpbmRTdHJlbmd0aCI6IjYiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjI2IiwiVGVtcENmZyI6IjE4IiwiR3JvdXBUeXBlIjoiMSIsIlNsZWVwVGltZSI6IjAiLCJPblRpbWUiOiIwIiwiT2ZmVGltZSI6IjAiLCJSYWNBZGRGdW5jIjoiTlMiLCJFeHRyYU9wIjoiMCIsIkRpYWdDb2RlIjoiMDAiLCJUaW1lQnNPbiI6IjAiLCJUaW1lQnNPZmYiOiIwIiwiQWlyQ2xlYW4iOiIwIiwiQXV0b0RyeSI6IjAiLCJQb3dlclNhdmUiOiIwIiwiV0RpclZTdGVwIjoiMCIsIldEaXJIU3RlcCI6IjMiLCJUZW1wTGltaXRNYXgiOiIwIiwiVGVtcExpbWl0TWluIjoiMCIsIkR1Y3Rab25lVHlwZSI6IjAiLCJab25lQ29udHJvbCI6IjAiLCJEUkVEIjoiMCIsIlNlbnNvclBNMSI6IjAiLCJTZW5zb3JQTTIiOiIwIiwiU2Vuc29yUE0xMCI6IjAiLCJBaXJQb2x1dGlvbiI6IjAiLCJIdW1pZGl0eUNmZyI6IjAiLCJXYXRlclRlbXBDb29sTWluIjoiMCIsIldhdGVyVGVtcENvb2xNYXgiOiIwIiwiV2F0ZXJUZW1wSGVhdE1pbiI6IjAiLCJXYXRlclRlbXBIZWF0TWF4IjoiMCIsIkhvdFdhdGVyVGVtcE1pbiI6IjAiLCJIb3RXYXRlclRlbXBNYXgiOiIwIiwiU2Vuc29ySHVtaWRpdHkiOiIwIiwiVG90YWxBaXJQb2x1dGlvbiI6IjAiLCJTZW5zb3JNb24iOiIwIiwiQ2xlYW5EcnkiOiIwIiwiUHJvZHVjdFN0YXR1cyI6IjAiLCJBaXJNb25pdG9yaW5nIjoiMCIsIkh1bWlkaWZpY2F0aW9uIjoiMCIsIkFpckZhc3QiOiIwIiwiQWlyUmVtb3ZhbCI6IjAiLCJBaXJVVkRpc2luZmVjdGlvbiI6IjAiLCJXYXRlcnRhbmtMaWdodCI6IjAiLCJTaWduYWxMaWdodGluZyI6IjAiLCJXRGlyVXBEb3duIjoiMCIsIldEaXJMZWZ0UmlnaHQiOiIwIiwiV1N3aXJsIjoiMCIsIkpldCI6IjEiLCJMb3dIZWF0aW5nIjoiMCIsIkNpcmN1bGF0ZVN0cmVuZ3RoIjoiMCIsIkNpcmN1bGF0ZURpciI6IjAiLCJBbnRpQnVncyI6IjAiLCJJY2VWYWxsZXkiOiIwIiwiSHVtc2F2ZSI6IjAiLCJXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ3VyIjoiMCIsIkhvdFdhdGVyVGVtcENmZyI6IjAiLCJIb3RXYXRlck1vZGUiOiIwIiwiSG90V2F0ZXIiOiIwIiwiQVdIUFRlbXBDZmdTd2l0Y2giOiIwIiwiQWlyVGVtcENvb2xNaW4iOiIwIiwiQWlyVGVtcENvb2xNYXgiOiIwIiwiQWlyVGVtcEhlYXRNaW4iOiIwIiwiQWlyVGVtcEhlYXRNYXgiOiIwIiwiV2F0ZXJJblRlbXBDdXIiOiIwIiwiQVdIUFdBVGVtcENvbnRyb2xTdGEiOiIwIiwiRGlzcGxheUNvbnRyb2wiOiIwIiwiU21hcnRDYXJlIjoiMCIsIlR3b1NldENvb2xUZW1wIjoiMCIsIlR3b1NldEhlYXRUZW1wIjoiMCIsIlR3b1NldENvb2xVU0wiOiIwIiwiVHdvU2V0Q29vbExTTCI6IjAiLCJUd29TZXRIZWF0VVNMIjoiMCIsIlR3b1NldEhlYXRMU0wiOiIwIiwiVHdvU2V0QUNPU3RhdGUiOiIwIiwiVHdvU2V0TW9kZURlYWRiYW5kIjoiMCIsIlR3b1NldFN0YXRlIjoiMCJ9";
    const SAMPLE_AIRCLEAN_ON: &str = "eyJPcGVyYXRpb24iOiIxIiwiT3BNb2RlIjoiMCIsIldpbmRTdHJlbmd0aCI6IjYiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjI1LjUiLCJUZW1wQ2ZnIjoiMTgiLCJHcm91cFR5cGUiOiIxIiwiU2xlZXBUaW1lIjoiMCIsIk9uVGltZSI6IjAiLCJPZmZUaW1lIjoiMCIsIlJhY0FkZEZ1bmMiOiJOUyIsIkV4dHJhT3AiOiIwIiwiRGlhZ0NvZGUiOiIwMCIsIlRpbWVCc09uIjoiMCIsIlRpbWVCc09mZiI6IjAiLCJBaXJDbGVhbiI6IjEiLCJBdXRvRHJ5IjoiMCIsIlBvd2VyU2F2ZSI6IjAiLCJXRGlyVlN0ZXAiOiIwIiwiV0RpckhTdGVwIjoiMyIsIlRlbXBMaW1pdE1heCI6IjAiLCJUZW1wTGltaXRNaW4iOiIwIiwiRHVjdFpvbmVUeXBlIjoiMCIsIlpvbmVDb250cm9sIjoiMCIsIkRSRUQiOiIwIiwiU2Vuc29yUE0xIjoiMCIsIlNlbnNvclBNMiI6IjAiLCJTZW5zb3JQTTEwIjoiMCIsIkFpclBvbHV0aW9uIjoiMCIsIkh1bWlkaXR5Q2ZnIjoiMCIsIldhdGVyVGVtcENvb2xNaW4iOiIwIiwiV2F0ZXJUZW1wQ29vbE1heCI6IjAiLCJXYXRlclRlbXBIZWF0TWluIjoiMCIsIldhdGVyVGVtcEhlYXRNYXgiOiIwIiwiSG90V2F0ZXJUZW1wTWluIjoiMCIsIkhvdFdhdGVyVGVtcE1heCI6IjAiLCJTZW5zb3JIdW1pZGl0eSI6IjAiLCJUb3RhbEFpclBvbHV0aW9uIjoiMCIsIlNlbnNvck1vbiI6IjAiLCJDbGVhbkRyeSI6IjAiLCJQcm9kdWN0U3RhdHVzIjoiMCIsIkFpck1vbml0b3JpbmciOiIwIiwiSHVtaWRpZmljYXRpb24iOiIwIiwiQWlyRmFzdCI6IjAiLCJBaXJSZW1vdmFsIjoiMCIsIkFpclVWRGlzaW5mZWN0aW9uIjoiMCIsIldhdGVydGFua0xpZ2h0IjoiMCIsIlNpZ25hbExpZ2h0aW5nIjoiMCIsIldEaXJVcERvd24iOiIwIiwiV0RpckxlZnRSaWdodCI6IjAiLCJXU3dpcmwiOiIwIiwiSmV0IjoiMSIsIkxvd0hlYXRpbmciOiIwIiwiQ2lyY3VsYXRlU3RyZW5ndGgiOiIwIiwiQ2lyY3VsYXRlRGlyIjoiMCIsIkFudGlCdWdzIjoiMCIsIkljZVZhbGxleSI6IjAiLCJIdW1zYXZlIjoiMCIsIldhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDdXIiOiIwIiwiSG90V2F0ZXJUZW1wQ2ZnIjoiMCIsIkhvdFdhdGVyTW9kZSI6IjAiLCJIb3RXYXRlciI6IjAiLCJBV0hQVGVtcENmZ1N3aXRjaCI6IjAiLCJBaXJUZW1wQ29vbE1pbiI6IjAiLCJBaXJUZW1wQ29vbE1heCI6IjAiLCJBaXJUZW1wSGVhdE1pbiI6IjAiLCJBaXJUZW1wSGVhdE1heCI6IjAiLCJXYXRlckluVGVtcEN1ciI6IjAiLCJBV0hQV0FUZW1wQ29udHJvbFN0YSI6IjAiLCJEaXNwbGF5Q29udHJvbCI6IjAiLCJTbWFydENhcmUiOiIwIiwiVHdvU2V0Q29vbFRlbXAiOiIwIiwiVHdvU2V0SGVhdFRlbXAiOiIwIiwiVHdvU2V0Q29vbFVTTCI6IjAiLCJUd29TZXRDb29sTFNMIjoiMCIsIlR3b1NldEhlYXRVU0wiOiIwIiwiVHdvU2V0SGVhdExTTCI6IjAiLCJUd29TZXRBQ09TdGF0ZSI6IjAiLCJUd29TZXRNb2RlRGVhZGJhbmQiOiIwIiwiVHdvU2V0U3RhdGUiOiIwIn0=";
    const SAMPLE_PLACEHOLDER: &str = "eyJPcGVyYXRpb24iOiIwIiwiT3BNb2RlIjoiMCIsIldpbmRTdHJlbmd0aCI6IjAiLCJUZW1wVW5pdCI6Ik5TIiwiVGVtcEN1ciI6IjAiLCJUZW1wQ2ZnIjoiMCIsIkdyb3VwVHlwZSI6IjEiLCJTbGVlcFRpbWUiOiIwIiwiT25UaW1lIjoiMCIsIk9mZlRpbWUiOiIwIiwiUmFjQWRkRnVuYyI6Ik5TIiwiRXh0cmFPcCI6IjAiLCJEaWFnQ29kZSI6IjAwIiwiVGltZUJzT24iOiIwIiwiVGltZUJzT2ZmIjoiMCIsIkFpckNsZWFuIjoiMCIsIkF1dG9EcnkiOiIwIiwiUG93ZXJTYXZlIjoiMCIsIldEaXJWU3RlcCI6IjAiLCJXRGlySFN0ZXAiOiIwIiwiVGVtcExpbWl0TWF4IjoiMCIsIlRlbXBMaW1pdE1pbiI6IjAiLCJEdWN0Wm9uZVR5cGUiOiIwIiwiWm9uZUNvbnRyb2wiOiIwIiwiRFJFRCI6IjAiLCJTZW5zb3JQTTEiOiIwIiwiU2Vuc29yUE0yIjoiMCIsIlNlbnNvclBNMTAiOiIwIiwiQWlyUG9sdXRpb24iOiIwIiwiSHVtaWRpdHlDZmciOiIwIiwiV2F0ZXJUZW1wQ29vbE1pbiI6IjAiLCJXYXRlclRlbXBDb29sTWF4IjoiMCIsIldhdGVyVGVtcEhlYXRNaW4iOiIwIiwiV2F0ZXJUZW1wSGVhdE1heCI6IjAiLCJIb3RXYXRlclRlbXBNaW4iOiIwIiwiSG90V2F0ZXJUZW1wTWF4IjoiMCIsIlNlbnNvckh1bWlkaXR5IjoiMCIsIlRvdGFsQWlyUG9sdXRpb24iOiIwIiwiU2Vuc29yTW9uIjoiMCIsIkNsZWFuRHJ5IjoiMCIsIlByb2R1Y3RTdGF0dXMiOiIwIiwiQWlyTW9uaXRvcmluZyI6IjAiLCJIdW1pZGlmaWNhdGlvbiI6IjAiLCJBaXJGYXN0IjoiMCIsIkFpclJlbW92YWwiOiIwIiwiQWlyVVZEaXNpbmZlY3Rpb24iOiIwIiwiV2F0ZXJ0YW5rTGlnaHQiOiIwIiwiU2lnbmFsTGlnaHRpbmciOiIwIiwiV0RpclVwRG93biI6IjAiLCJXRGlyTGVmdFJpZ2h0IjoiMCIsIldTd2lybCI6IjAiLCJKZXQiOiIwIiwiTG93SGVhdGluZyI6IjAiLCJDaXJjdWxhdGVTdHJlbmd0aCI6IjAiLCJDaXJjdWxhdGVEaXIiOiIwIiwiQW50aUJ1Z3MiOiIwIiwiSWNlVmFsbGV5IjoiMCIsIkh1bXNhdmUiOiIwIiwiV2F0ZXJUZW1wQ3VyIjoiMCIsIkhvdFdhdGVyVGVtcEN1ciI6IjAiLCJIb3RXYXRlclRlbXBDZmciOiIwIiwiSG90V2F0ZXJNb2RlIjoiMCIsIkhvdFdhdGVyIjoiMCIsIkFXSFBUZW1wQ2ZnU3dpdGNoIjoiMCIsIkFpclRlbXBDb29sTWluIjoiMCIsIkFpclRlbXBDb29sTWF4IjoiMCIsIkFpclRlbXBIZWF0TWluIjoiMCIsIkFpclRlbXBIZWF0TWF4IjoiMCIsIldhdGVySW5UZW1wQ3VyIjoiMCIsIkFXSFBXQVRlbXBDb250cm9sU3RhIjoiMCIsIkRpc3BsYXlDb250cm9sIjoiMCIsIlNtYXJ0Q2FyZSI6IjAiLCJUd29TZXRDb29sVGVtcCI6IjAiLCJUd29TZXRIZWF0VGVtcCI6IjAiLCJUd29TZXRDb29sVVNMIjoiMCIsIlR3b1NldENvb2xMU0wiOiIwIiwiVHdvU2V0SGVhdFVTTCI6IjAiLCJUd29TZXRIZWF0TFNMIjoiMCIsIlR3b1NldEFDT1N0YXRlIjoiMCIsIlR3b1NldE1vZGVEZWFkYmFuZCI6IjAiLCJUd29TZXRTdGF0ZSI6IjAifQ==";

    const DEVICE_ID: &str = "test-id";

    fn meta() -> Metadata {
        Metadata::new("RAC_056905_WW", "RAC_056905_WW", "2.6.7_RTOS_3K")
    }

    fn packet(b64: &str) -> Vec<u8> {
        STANDARD.decode(b64).unwrap()
    }

    fn make_device() -> (Arc<MockHaConnection>, Arc<MockThinq1Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq1Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        let d = dev.clone();
        ha.on_set_property(move |id, prop, value| {
            if id == DEVICE_ID {
                d.set_property(prop, value);
            }
        });
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> PropertyValue {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .get(name)
            .cloned()
            .unwrap()
    }

    fn control(key: &str, value: &str) -> Value {
        json!({
            "Cmd": "Control",
            "CmdOpt": "Set",
            "Value": { key: value },
            "Format": "B64",
            "Data": ""
        })
    }

    #[test]
    fn registry_is_thinq1_and_leaves_the_tlv_handler() {
        assert!(crate::t1_factory("RAC_056905_WW").is_some());
        assert!(crate::t2_factory("RAC_056905_WW").is_some());
        let (ha, thinq, _dev) = make_device();
        let cfg = ha.device(DEVICE_ID).unwrap().config.unwrap();
        assert!(cfg.components.contains_key("climate"));
        assert!(cfg.components.contains_key("jet"));
        assert!(cfg.components.contains_key("airclean"));
        assert_eq!(
            cfg.components["climate"]["mode_command_topic"],
            "$this/mode/set"
        );
        thinq.reset_recorder();
        let factory = crate::t1_factory("RAC_056905_WW").unwrap();
        let created = factory(ha.clone(), thinq.clone(), meta());
        created.start();
        assert_eq!(thinq.sent(), vec![json!({"Cmd": "Mon", "CmdOpt": "Start"})]);
    }

    #[test]
    fn start_subscribes_to_monitoring() {
        let (_ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(thinq.sent(), vec![json!({"Cmd": "Mon", "CmdOpt": "Start"})]);
    }

    #[test]
    fn off_folds_operation_into_mode_off() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&packet(SAMPLE_OFF));
        assert_eq!(prop(&ha, "mode"), "off".into());
        assert_eq!(prop(&ha, "current_temperature"), PropertyValue::Int(21));
        assert_eq!(prop(&ha, "temperature"), PropertyValue::Int(24));
        assert_eq!(prop(&ha, "fan_mode"), "very low".into());
        assert_eq!(prop(&ha, "swing_mode"), "off".into());
        assert_eq!(prop(&ha, "swing_horizontal_mode"), "3".into());
        assert_eq!(prop(&ha, "jet"), "OFF".into());
        assert_eq!(prop(&ha, "airclean"), "OFF".into());
    }

    #[test]
    fn cool_23_very_high_swing_on() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&packet(SAMPLE_COOL_23_HIGH));
        assert_eq!(prop(&ha, "mode"), "cool".into());
        assert_eq!(prop(&ha, "temperature"), PropertyValue::Int(23));
        assert_eq!(prop(&ha, "fan_mode"), "very high".into());
        assert_eq!(prop(&ha, "swing_mode"), "on".into());
    }

    #[test]
    fn fan_only_very_low_and_horizontal_step() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&packet(SAMPLE_FAN_ONLY_LOW));
        assert_eq!(prop(&ha, "mode"), "fan_only".into());
        assert_eq!(prop(&ha, "fan_mode"), "very low".into());
        assert_eq!(prop(&ha, "swing_mode"), "off".into());
        assert_eq!(prop(&ha, "swing_horizontal_mode"), "3".into());
    }

    #[test]
    fn dry_heat_auto_modes() {
        for (sample, mode) in [
            (SAMPLE_DRY, "dry"),
            (SAMPLE_HEAT, "heat"),
            (SAMPLE_AUTO, "auto"),
        ] {
            let (ha, thinq, _dev) = make_device();
            thinq.emit_data(&packet(sample));
            assert_eq!(prop(&ha, "mode"), mode.into());
        }
    }

    #[test]
    fn half_degree_current_temperature_and_auto_fan() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&packet(SAMPLE_FAN_AUTO));
        assert_eq!(prop(&ha, "current_temperature"), PropertyValue::Num(19.5));
        assert_eq!(prop(&ha, "fan_mode"), "auto".into());
    }

    #[test]
    fn swing_steps_decode_to_numbered_labels() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&packet(SAMPLE_VSWING_ON));
        assert_eq!(prop(&ha, "swing_mode"), "on".into());
        thinq.emit_data(&packet(SAMPLE_VSWING_STEP1));
        assert_eq!(prop(&ha, "swing_mode"), "1".into());
        thinq.emit_data(&packet(SAMPLE_HSWING_STEP4));
        assert_eq!(prop(&ha, "swing_horizontal_mode"), "4".into());
    }

    #[test]
    fn jet_and_air_purify_read_back_on() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&packet(SAMPLE_JET_ON));
        assert_eq!(prop(&ha, "jet"), "ON".into());
        assert_eq!(prop(&ha, "airclean"), "OFF".into());
        thinq.emit_data(&packet(SAMPLE_AIRCLEAN_ON));
        assert_eq!(prop(&ha, "jet"), "ON".into());
        assert_eq!(prop(&ha, "airclean"), "ON".into());
        assert_eq!(prop(&ha, "temperature"), PropertyValue::Int(18));
    }

    #[test]
    fn writes_use_the_set_envelope() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&packet(SAMPLE_COOL_23_HIGH));
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "climate", "temperature_command", "21.0");
        ha.set_property(DEVICE_ID, "climate", "fan_mode_command", "high");
        ha.set_property(DEVICE_ID, "climate", "swing_mode_command", "on");
        ha.set_property(DEVICE_ID, "climate", "swing_horizontal_mode_command", "1-3");
        ha.set_property(DEVICE_ID, "jet", "command", "ON");
        ha.set_property(DEVICE_ID, "airclean", "command", "OFF");
        assert_eq!(
            thinq.sent(),
            vec![
                control("TempCfg", "21"),
                control("WindStrength", "5"),
                control("WDirVStep", "100"),
                control("WDirHStep", "13"),
                control("Jet", "1"),
                control("AirClean", "0"),
            ]
        );
    }

    #[test]
    fn mode_from_off_powers_on_first() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&packet(SAMPLE_OFF));
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "climate", "mode_command", "cool");
        assert_eq!(
            thinq.sent(),
            vec![control("Operation", "1"), control("OpMode", "0")]
        );

        thinq.emit_data(&packet(SAMPLE_COOL_23_HIGH));
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "climate", "mode_command", "heat");
        assert_eq!(thinq.sent(), vec![control("OpMode", "4")]);

        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "climate", "mode_command", "off");
        assert_eq!(thinq.sent(), vec![control("Operation", "0")]);
    }

    #[test]
    fn zeroed_placeholder_pushes_are_dropped() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&packet(SAMPLE_PLACEHOLDER));
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());

        thinq.emit_data(&packet(SAMPLE_COOL_23_HIGH));
        thinq.emit_data(&packet(SAMPLE_PLACEHOLDER));
        assert_eq!(prop(&ha, "mode"), "cool".into());
        assert_eq!(prop(&ha, "temperature"), PropertyValue::Int(23));
        assert_eq!(prop(&ha, "fan_mode"), "very high".into());
    }

    #[test]
    fn non_json_and_unknown_codes_are_tolerated() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(b"not json at all");
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());

        let unknown = br#"{"Operation":"1","OpMode":"9","WindStrength":"2"}"#;
        thinq.emit_data(unknown);
        assert_eq!(prop(&ha, "mode"), "None".into());
        assert_eq!(prop(&ha, "fan_mode"), "very low".into());
    }
}
