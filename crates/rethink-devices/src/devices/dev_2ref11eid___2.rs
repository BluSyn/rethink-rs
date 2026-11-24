//! 2REF11EID___2 — LG LFXC22526S refrigerator (ThinQ1).
//!
//! Port of upstream PR #320. Decodes the 15-byte ThinQ1 binary status payload
//! (`STATUS_FIELDS` layout), dynamically discovers optional entities once the
//! appliance reports defined values (non-`0xFF`), and sends `ControlData`
//! (`Cmd: "Control", CmdOpt: "Set", Value: "ControlData", Format: "B64"`) writes
//! with `0xFF` in IGNORE slots 8..12.

use crate::device_trait::DeviceHandler;
use crate::fridge_common::{status_get, unpack_status, Status, TemperatureUnit, STATUS_FIELDS};
use base64::Engine;
use rethink_core::device_base::default_config;
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq1Device;
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FRIDGE_F: &[(u8, i64)] = &[
    (1, 46), (2, 45), (3, 44), (4, 43), (5, 42), (6, 41), (7, 40),
    (8, 39), (9, 38), (10, 37), (11, 36), (12, 35), (13, 34), (14, 33),
];
const FREEZER_F: &[(u8, i64)] = &[
    (1, 8), (2, 6), (3, 5), (4, 4), (5, 3), (6, 2), (7, 1),
    (8, 0), (9, -1), (10, -2), (11, -3), (12, -4), (13, -6),
];
const FRIDGE_C: &[(u8, i64)] = &[
    (1, 8), (3, 7), (5, 6), (7, 5), (9, 4), (10, 3), (12, 2), (14, 1),
];
const FREEZER_C: &[(u8, i64)] = &[
    (1, -13), (2, -14), (3, -15), (5, -16), (7, -17),
    (9, -18), (11, -19), (12, -20), (13, -21),
];

const CONTROL_MAX_AGE: Duration = Duration::from_secs(12);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(12);

const EXTRA_KEYS: &[&str] = &[
    "ice_plus",
    "fresh_air_filter_mode",
    "fresh_air_filter_status",
    "fresh_air_filter_replace",
    "water_filter_used_months",
    "water_filter_remaining_months",
    "water_filter_replace",
    "smart_saving_mode",
    "smart_saving_running",
    "display_lock",
    "sabbath_mode",
];

fn temp_table(unit: TemperatureUnit, fridge: bool) -> &'static [(u8, i64)] {
    match (unit, fridge) {
        (TemperatureUnit::F, true) => FRIDGE_F,
        (TemperatureUnit::F, false) => FREEZER_F,
        (TemperatureUnit::C, true) => FRIDGE_C,
        (TemperatureUnit::C, false) => FREEZER_C,
    }
}

fn lookup(table: &[(u8, i64)], raw: u8) -> Option<i64> {
    table.iter().find(|(c, _)| *c == raw).map(|(_, v)| *v)
}

fn inverse(table: &[(u8, i64)], val: i64) -> Option<u8> {
    table.iter().find(|(_, v)| *v == val).map(|(c, _)| *c)
}

fn range_ends(table: &[(u8, i64)]) -> (i64, i64) {
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for &(_, v) in table {
        min = min.min(v);
        max = max.max(v);
    }
    (min, max)
}

fn air_mode_name(code: u8) -> Option<&'static str> {
    match code {
        2 => Some("Auto"),
        3 => Some("Power"),
        _ => None,
    }
}

fn air_status_name(code: u8) -> Option<&'static str> {
    match code {
        2 => Some("Auto"),
        3 => Some("Power"),
        4 => Some("Replace"),
        _ => None,
    }
}

fn saving_mode_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Off"),
        2 => Some("Delay defrost"),
        3 => Some("Demand response"),
        _ => None,
    }
}

fn extra_component(key: &str) -> Value {
    let avail = json!([
        {"topic": "$this/availability"},
        {"topic": "$rethink/availability"},
        {"topic": format!("$this/{key}/availability")},
    ]);
    match key {
        "ice_plus" => json!({"platform":"switch","name":"Ice Plus","icon":"mdi:snowflake","unique_id":"$deviceid-ice_plus","state_topic":"$this/ice_plus","command_topic":"$this/ice_plus/set","optimistic":false,"availability":avail,"availability_mode":"all"}),
        "fresh_air_filter_mode" => json!({"platform":"select","name":"Fresh air filter mode","options":["Auto","Power"],"icon":"mdi:air-filter","unique_id":"$deviceid-fresh_air_filter_mode","state_topic":"$this/fresh_air_filter_mode","command_topic":"$this/fresh_air_filter_mode/set","optimistic":false,"availability":avail,"availability_mode":"all"}),
        "fresh_air_filter_status" => json!({"platform":"sensor","name":"Fresh air filter status","device_class":"enum","options":["Auto","Power","Replace"],"entity_category":"diagnostic","unique_id":"$deviceid-fresh_air_filter_status","state_topic":"$this/fresh_air_filter_status","availability":avail,"availability_mode":"all"}),
        "fresh_air_filter_replace" => json!({"platform":"binary_sensor","name":"Fresh air filter replacement needed","device_class":"problem","entity_category":"diagnostic","unique_id":"$deviceid-fresh_air_filter_replace","state_topic":"$this/fresh_air_filter_replace","availability":avail,"availability_mode":"all"}),
        "water_filter_used_months" => json!({"platform":"sensor","name":"Water filter age","unit_of_measurement":"months","suggested_display_precision":0,"entity_category":"diagnostic","unique_id":"$deviceid-water_filter_used_months","state_topic":"$this/water_filter_used_months","availability":avail,"availability_mode":"all"}),
        "water_filter_remaining_months" => json!({"platform":"sensor","name":"Water filter remaining","unit_of_measurement":"months","suggested_display_precision":0,"entity_category":"diagnostic","unique_id":"$deviceid-water_filter_remaining_months","state_topic":"$this/water_filter_remaining_months","availability":avail,"availability_mode":"all"}),
        "water_filter_replace" => json!({"platform":"binary_sensor","name":"Water filter replacement needed","device_class":"problem","entity_category":"diagnostic","unique_id":"$deviceid-water_filter_replace","state_topic":"$this/water_filter_replace","availability":avail,"availability_mode":"all"}),
        "smart_saving_mode" => json!({"platform":"select","name":"Smart saving mode","options":["Off","Delay defrost","Demand response"],"icon":"mdi:leaf","unique_id":"$deviceid-smart_saving_mode","state_topic":"$this/smart_saving_mode","command_topic":"$this/smart_saving_mode/set","optimistic":false,"availability":avail,"availability_mode":"all"}),
        "smart_saving_running" => json!({"platform":"binary_sensor","name":"Smart saving running","entity_category":"diagnostic","unique_id":"$deviceid-smart_saving_running","state_topic":"$this/smart_saving_running","availability":avail,"availability_mode":"all"}),
        "display_lock" => json!({"platform":"sensor","name":"Display lock","device_class":"enum","options":["Unlocked","Locked"],"entity_category":"diagnostic","unique_id":"$deviceid-display_lock","state_topic":"$this/display_lock","availability":avail,"availability_mode":"all"}),
        "sabbath_mode" => json!({"platform":"switch","name":"Sabbath mode","icon":"mdi:candle","unique_id":"$deviceid-sabbath_mode","state_topic":"$this/sabbath_mode","command_topic":"$this/sabbath_mode/set","optimistic":false,"availability":avail,"availability_mode":"all"}),
        _ => json!({}),
    }
}

fn extra_values(status: &Status) -> HashMap<&'static str, PropertyValue> {
    let mut out = HashMap::new();
    match status_get(status, "expressFreeze") {
        1 => { out.insert("ice_plus", "OFF".into()); }
        2 => { out.insert("ice_plus", "ON".into()); }
        _ => {}
    }
    let air = status_get(status, "freshAirFilter");
    if let Some(m) = air_mode_name(air) {
        out.insert("fresh_air_filter_mode", m.into());
    }
    if let Some(st) = air_status_name(air) {
        out.insert("fresh_air_filter_status", st.into());
        out.insert("fresh_air_filter_replace", if air == 4 { "ON" } else { "OFF" }.into());
    }
    let wf = status_get(status, "waterFilter");
    if wf <= 12 {
        out.insert("water_filter_used_months", PropertyValue::Int(i64::from(wf)));
        out.insert(
            "water_filter_remaining_months",
            PropertyValue::Int(i64::from(6u8.saturating_sub(wf))),
        );
        out.insert("water_filter_replace", if wf >= 6 { "ON" } else { "OFF" }.into());
    }
    if let Some(sm) = saving_mode_name(status_get(status, "smartSaving")) {
        out.insert("smart_saving_mode", sm.into());
    }
    match status_get(status, "smartSavingRun") {
        0 => { out.insert("smart_saving_running", "OFF".into()); }
        1 => { out.insert("smart_saving_running", "ON".into()); }
        _ => {}
    }
    match status_get(status, "displayLock") {
        1 => { out.insert("display_lock", "Unlocked".into()); }
        2 => { out.insert("display_lock", "Locked".into()); }
        _ => {}
    }
    match status_get(status, "sabbathMode") {
        0 => { out.insert("sabbath_mode", "OFF".into()); }
        1 => { out.insert("sabbath_mode", "ON".into()); }
        _ => {}
    }
    out
}

fn known_control(field: &str, status: &Status, unit: TemperatureUnit) -> bool {
    let raw = status_get(status, field);
    match field {
        "fridgeSetpoint" => lookup(temp_table(unit, true), raw).is_some(),
        "freezerSetpoint" => lookup(temp_table(unit, false), raw).is_some(),
        "expressFreeze" => matches!(raw, 1 | 2),
        "freshAirFilter" => matches!(raw, 2 | 3 | 4),
        "smartSaving" => saving_mode_name(raw).is_some(),
        "sabbathMode" => matches!(raw, 0 | 1),
        _ => false,
    }
}

struct State {
    unit: Option<TemperatureUnit>,
    status: Option<Vec<u8>>,
    status_at: Option<Instant>,
    supported: BTreeSet<&'static str>,
    pending: HashMap<&'static str, (u8, Instant)>,
    config: Option<DeviceDiscovery>,
    publish_cache: HashMap<String, String>,
}

pub struct Device {
    id: String,
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    meta: Metadata,
    state: Mutex<State>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq1Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            id: thinq.id().to_string(),
            ha,
            thinq: thinq.clone(),
            meta,
            state: Mutex::new(State {
                unit: None,
                status: None,
                status_at: None,
                supported: BTreeSet::new(),
                pending: HashMap::new(),
                config: None,
                publish_cache: HashMap::new(),
            }),
        });
        let dev = this.clone();
        thinq.on_data(Box::new(move |buf| dev.on_status(buf)));
        this
    }

    fn publish_property(&self, prop: &str, value: PropertyValue) {
        {
            let mut st = self.state.lock();
            if let Some(prev) = st.publish_cache.get(prop) {
                if value.eq_str(prev) {
                    return;
                }
            }
            st.publish_cache.insert(prop.to_string(), value.as_string());
        }
        self.ha.publish_property(&self.id, prop, value);
    }

    fn build_config(&self, unit: TemperatureUnit, supported: &BTreeSet<&'static str>) -> DeviceDiscovery {
        let uom = if unit == TemperatureUnit::F { "°F" } else { "°C" };
        let (f_min, f_max) = range_ends(temp_table(unit, true));
        let (z_min, z_max) = range_ends(temp_table(unit, false));
        let mut config = default_config(
            &self.meta,
            Some(json!({"name": "LG Refrigerator", "model": "LFXC22526S"})),
        );
        let mut comps: HashMap<String, Value> = [
            ("fridge_setpoint".into(), json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-fridge_setpoint","state_topic":"$this/fridge_setpoint","command_topic":"$this/fridge_setpoint/set","name":"Fridge target temperature","unit_of_measurement":uom,"min":f_min,"max":f_max,"step":1,"optimistic":false})),
            ("freezer_setpoint".into(), json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-freezer_setpoint","state_topic":"$this/freezer_setpoint","command_topic":"$this/freezer_setpoint/set","name":"Freezer target temperature","unit_of_measurement":uom,"min":z_min,"max":z_max,"step":1,"optimistic":false})),
            ("door".into(), json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door"})),
        ]
        .into_iter()
        .collect();
        for &k in supported {
            comps.insert(k.to_string(), extra_component(k));
        }
        config.components = comps;
        config
    }

    fn on_status(&self, buf: &[u8]) {
        if buf.len() < 15 {
            self.invalidate_status();
            return;
        }
        let status = unpack_status(buf);
        let unit = match status_get(&status, "tempUnit") {
            0 => TemperatureUnit::F,
            1 => TemperatureUnit::C,
            _ => {
                self.invalidate_status();
                self.publish_property("fridge_setpoint", "None".into());
                self.publish_property("freezer_setpoint", "None".into());
                return;
            }
        };
        let extras = extra_values(&status);
        let (config_to_publish, supported_snapshot) = {
            let mut st = self.state.lock();
            let mut need_cfg = st.unit != Some(unit);
            if need_cfg {
                st.pending.clear();
            }
            st.unit = Some(unit);
            for &k in EXTRA_KEYS {
                if extras.contains_key(k) && st.supported.insert(k) {
                    need_cfg = true;
                }
            }
            st.status = Some(buf.to_vec());
            st.status_at = Some(Instant::now());
            let sup = st.supported.clone();
            let cfg = if need_cfg {
                let c = self.build_config(unit, &sup);
                st.config = Some(c.clone());
                Some(c)
            } else {
                None
            };
            (cfg, sup)
        };

        if let Some(cfg) = config_to_publish {
            self.ha.publish_config(&self.id, &cfg);
        }
        self.publish_property("availability", "online".into());
        self.publish_property(
            "fridge_setpoint",
            lookup(temp_table(unit, true), status_get(&status, "fridgeSetpoint"))
                .map_or_else(|| "None".into(), PropertyValue::Int),
        );
        self.publish_property(
            "freezer_setpoint",
            lookup(temp_table(unit, false), status_get(&status, "freezerSetpoint"))
                .map_or_else(|| "None".into(), PropertyValue::Int),
        );
        self.publish_property(
            "door",
            match status_get(&status, "anyDoorOpen") {
                0 => "OFF",
                1 => "ON",
                _ => "None",
            }
            .into(),
        );
        for &k in &supported_snapshot {
            let avail_topic = format!("{k}/availability");
            if let Some(v) = extras.get(k) {
                self.publish_property(&avail_topic, "online".into());
                self.publish_property(k, v.clone());
            } else {
                self.publish_property(&avail_topic, "offline".into());
            }
        }
    }

    fn invalidate_status(&self) {
        {
            let mut st = self.state.lock();
            st.status = None;
            st.status_at = None;
            st.pending.clear();
        }
        self.publish_property("availability", "offline".into());
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.id
    }
    fn start(&self) {
        self.thinq.send(json!({"Cmd": "Mon", "CmdOpt": "Start"}));
    }
    fn drop_device(&self) {
        self.invalidate_status();
    }
    fn set_property(&self, prop: &str, value: &str) {
        let mut st = self.state.lock();
        let (Some(raw_status), Some(at), Some(unit)) =
            (st.status.clone(), st.status_at, st.unit)
        else {
            return;
        };
        if at.elapsed() >= CONTROL_MAX_AGE {
            return;
        }
        let status = unpack_status(&raw_status);
        if !known_control("fridgeSetpoint", &status, unit)
            || !known_control("freezerSetpoint", &status, unit)
        {
            return;
        }
        let (field, code): (&'static str, Option<u8>) = match prop {
            "fridge_setpoint" | "freezer_setpoint" => {
                let fridge = prop == "fridge_setpoint";
                let parsed = value.trim().parse::<i64>().ok();
                (
                    if fridge { "fridgeSetpoint" } else { "freezerSetpoint" },
                    parsed.and_then(|v| inverse(temp_table(unit, fridge), v)),
                )
            }
            "ice_plus" => (
                "expressFreeze",
                match value {
                    "OFF" => Some(1),
                    "ON" => Some(2),
                    _ => None,
                },
            ),
            "fresh_air_filter_mode" => (
                "freshAirFilter",
                match value {
                    "Auto" => Some(2),
                    "Power" => Some(3),
                    _ => None,
                },
            ),
            "smart_saving_mode" => (
                "smartSaving",
                match value {
                    "Off" => Some(0),
                    "Delay defrost" => Some(2),
                    "Demand response" => Some(3),
                    _ => None,
                },
            ),
            "sabbath_mode" => (
                "sabbathMode",
                match value {
                    "OFF" => Some(0),
                    "ON" => Some(1),
                    _ => None,
                },
            ),
            _ => return,
        };
        let Some(code) = code else { return };
        if !known_control(field, &status, unit) {
            return;
        }

        let mut data = raw_status[1..15].to_vec();
        data[8..13].fill(0xff);
        let now = Instant::now();
        st.pending.insert(field, (code, now + COMMAND_TIMEOUT));
        st.pending.retain(|&f, &mut (c, exp)| {
            if now < exp && known_control(f, &status, unit) {
                if let Some(idx) = STATUS_FIELDS.iter().position(|&sf| sf == f) {
                    if idx >= 1 && idx - 1 < data.len() {
                        data[idx - 1] = c;
                    }
                }
                true
            } else {
                false
            }
        });
        drop(st);

        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        self.thinq.send(json!({
            "Cmd": "Control",
            "CmdOpt": "Set",
            "Value": "ControlData",
            "Format": "B64",
            "Data": b64,
        }));
    }
    fn publish_config(&self) {
        let (cfg, online) = {
            let st = self.state.lock();
            (st.config.clone(), st.status.is_some())
        };
        if let Some(cfg) = cfg {
            self.ha
                .publish_property(&self.id, "availability", if online { "online" } else { "offline" }.into());
            self.ha.publish_config(&self.id, &cfg);
        }
    }
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
    use rethink_core::{hex_decode, MockHaConnection, MockThinq1Device};

    const ID: &str = "test-fridge";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_live_sample_doors_and_sends_control_data() {
        let meta = Metadata::new("2REF11EID___2", "LFXC22526S", "1.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq1Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        dev.start();
        assert_eq!(thinq.sent()[0], json!({"Cmd": "Mon", "CmdOpt": "Start"}));

        thinq.emit_data(&hex_decode("020d0a01020205000000ffffffff00"));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("34"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-2"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "ice_plus").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fresh_air_filter_mode").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "water_filter_used_months").as_deref(), Some("5"));

        thinq.emit_data(&hex_decode("020d0a01020205010000ffffffff00"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));

        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "37");
        let sent = thinq.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["Cmd"], "Control");
        assert_eq!(sent[0]["CmdOpt"], "Set");
        assert_eq!(sent[0]["Value"], "ControlData");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(sent[0]["Data"].as_str().unwrap())
            .unwrap();
        // Raw byte 0 is fridgeSetpoint (37 F -> code 10 = 0x0a), slots 8..13 are 0xff.
        assert_eq!(raw[0], 0x0a);
        assert_eq!(&raw[8..13], &[0xff; 5]);
    }
}
