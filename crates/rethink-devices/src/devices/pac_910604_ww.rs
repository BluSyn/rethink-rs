//! PAC_910604_WW — LG FQ18PADNQN floor-standing air conditioner (deviceType 401).
//!
//! Same 0xA7 TLV envelope as RAC_056905_WW, but an incompatible field set: the fan level is
//! duplicated across both bytes (level × 0x0101), horizontal swing is a two-vane field (low byte
//! right, high byte left), and human sense, the wind modes and the air-quality readings are new
//! tags. The unit reports state unprompted, so nothing is ever queried and discovery is published
//! at construction from a fixed field map. Only user-initiated control writes are sent, each
//! reproducing a frame the LG app was captured sending. Wind mode is one selector over two tags:
//! coolpower is a direct 0x236 write, while longpower and off are climate bundles with the fan
//! forced to max or high. The filter counters (0x355 hours used, 0x356 rated hours) come in
//! ordinary state frames; no PAC reset command is known, so no reset is offered.
//! Port of upstream PR #170.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, ReadXform, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::tlv::Tlv;
use serde_json::{json, Value};
use std::sync::Arc;

const TAG_POWER: u16 = 0x1f7;
const TAG_MODE: u16 = 0x1f9;
const TAG_FAN: u16 = 0x1fa;
const TAG_CURRENT_TEMP: u16 = 0x1fd;
const TAG_TARGET_TEMP: u16 = 0x1fe;
const TAG_SWING_V: u16 = 0x205;
const TAG_SWING_H: u16 = 0x206;
const TAG_HUMAN_SENSE: u16 = 0x208;
const TAG_LONG_POWER: u16 = 0x209;
const TAG_ECO: u16 = 0x20d;
const TAG_AUTO_DRY: u16 = 0x20e;
const TAG_AIR_CLEAN: u16 = 0x20f;
const TAG_SLEEP_TIMER: u16 = 0x21a;
const TAG_STOP_TIMER: u16 = 0x21b;
const TAG_START_TIMER: u16 = 0x21c;
const TAG_ERROR: u16 = 0x221;
const TAG_AUTO_DRY_REMAIN: u16 = 0x225;
const TAG_COOL_POWER: u16 = 0x236;
const TAG_SMART_CARE: u16 = 0x23e;
const TAG_SMART_GUIDE: u16 = 0x23f;
const TAG_AIR_QUALITY: u16 = 0x240;
const TAG_POWER_DRAW: u16 = 0x2b3;
const TAG_PM1: u16 = 0x333;
const TAG_PM25: u16 = 0x334;
const TAG_PM10: u16 = 0x335;
const TAG_HUMIDITY: u16 = 0x336;
const TAG_FILTER_USED: u16 = 0x355;
const TAG_FILTER_LIFE: u16 = 0x356;

const MODES: &[(&str, u32)] = &[("cool", 0), ("dry", 1)];

// 7 has only been seen under coolpower and 9 only under longpower.
const FAN_LEVELS: &[(&str, u32)] = &[
    ("auto", 8),
    ("low", 2),
    ("medium", 4),
    ("high", 6),
    ("turbo", 7),
    ("max", 9),
];

// Plain on/off here, not a vane angle as on the RAC units.
const SWING_V: &[(&str, u32)] = &[("off", 0), ("on", 1)];

const SWING_H: &[(&str, u32)] = &[("off", 0), ("right", 1), ("left", 256), ("both", 257)];

const HUMAN_SENSE: &[(&str, u32)] = &[("off", 0), ("direct", 1), ("indirect", 2)];

// 1 and 4 were captured; 2 and 3 follow the four-grade scale shown on the panel.
const AIR_QUALITY: &[(&str, u32)] = &[("good", 1), ("moderate", 2), ("bad", 3), ("very_bad", 4)];

const WIND_MODES: [&str; 3] = ["off", "coolpower", "longpower"];

/// Wind-mode write codes. Only coolpower is written to 0x236; the others send a climate bundle.
const WIND_OFF: u32 = 0;
const WIND_COOLPOWER: u32 = 1;
const WIND_LONGPOWER: u32 = 2;

/// Published by the earlier port of this model (from upstream PR #122). A component reduced to its
/// platform makes Home Assistant remove the entity.
const RETIRED_COMPONENTS: &[(&str, &str)] = &[
    ("energy_current", "sensor"),
    ("jet", "switch"),
    ("quiet", "switch"),
    ("uvnano", "switch"),
    ("childlock", "switch"),
    ("spacefit", "switch"),
    ("display", "switch"),
    ("beep", "switch"),
    ("hxclean", "switch"),
    ("allclean", "switch"),
    ("autodrylevel", "select"),
    ("autodryremain", "sensor"),
    ("autodryrunning", "binary_sensor"),
    ("autodrycancel", "button"),
    ("onesidewind", "select"),
    ("humiditydisplay", "select"),
    ("tempstep", "select"),
    ("filter_life", "sensor"),
    ("filter_used", "sensor"),
    ("filterreset", "button"),
];

fn options(table: &[(&'static str, u32)]) -> Vec<&'static str> {
    table.iter().map(|(l, _)| *l).collect()
}

fn label(table: &[(&'static str, u32)], code: u32) -> Option<&'static str> {
    table.iter().find(|(_, c)| *c == code).map(|(l, _)| *l)
}

fn code(table: &[(&str, u32)], label: &str) -> Option<u32> {
    table.iter().find(|(l, _)| *l == label).map(|(_, c)| *c)
}

/// While eco engages, one frame carries 0xFF in one byte; the other still holds the level.
fn fan_level(raw: u32) -> u32 {
    let lo = raw & 0xff;
    let hi = (raw >> 8) & 0xff;
    if lo == 0xff {
        hi
    } else {
        lo
    }
}

fn on_off(raw: u32) -> Option<PropertyValue> {
    Some(if raw != 0 { "ON" } else { "OFF" }.into())
}

fn on_off_write(val: &str) -> Option<PropertyValue> {
    Some(PropertyValue::Int(i64::from(val == "ON")))
}

fn half(raw: u32) -> Option<PropertyValue> {
    Some(PropertyValue::from(f64::from(raw) / 2.0))
}

/// `Math.round(Number(val) * scale)`; a payload that is not a number writes nothing.
fn rounded(val: &str, scale: f64) -> Option<PropertyValue> {
    crate::js::number(val)
        .map(|n| (n * scale).round())
        .filter(|n| n.is_finite())
        .map(PropertyValue::from)
}

/// Same minimum-count guard as RAC_056905_WW: a power-only notification is not a full report.
fn is_values_response(tlv: &[Tlv]) -> bool {
    tlv.len() >= 10 && tlv.iter().any(|e| e.t == TAG_POWER)
}

pub struct Device {
    pub core: Arc<TlvDeviceCore>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = TlvDeviceCore::with_caps_query(ha, thinq, false);
        *core.is_values_response.lock() = Some(Box::new(is_values_response));

        let mut config = default_config(&meta, Some(json!({"name": "LG Stand Air Conditioner"})));
        let mut modes = vec!["off"];
        modes.extend(options(MODES));
        config.components.insert(
            "climate".into(),
            json!({
                "platform": "climate",
                "unique_id": "$deviceid-climate",
                "name": null,
                "temperature_unit": "C",
                "temp_step": 1,
                "precision": 1,
                "current_humidity_topic": "$this/humidity-",
                "min_temp": 18,
                "max_temp": 30,
                "modes": modes,
                "fan_modes": options(FAN_LEVELS),
                "swing_modes": options(SWING_V),
                "swing_horizontal_modes": options(SWING_H),
            }),
        );
        add_climate_fields(&core, &mut config);
        add_feature_fields(&core, &mut config);
        add_sensor_fields(&core, &mut config);
        for (key, platform) in RETIRED_COMPONENTS {
            config
                .components
                .insert((*key).into(), json!({ "platform": platform }));
        }
        core.set_config(config);
        Arc::new(Self { core })
    }
}

fn add_climate_fields(core: &Arc<TlvDeviceCore>, config: &mut DeviceDiscovery) {
    let mut f = FieldDefinition::new("climate", "current_temperature")
        .with_id(TAG_CURRENT_TEMP)
        .read_only();
    f.state_topic = Some("topic".into());
    f.read_xform = Some(Box::new(half));
    core.add_field(config, f, true);

    let mut f = FieldDefinition::new("climate", "temperature").with_id(TAG_TARGET_TEMP);
    f.read_xform = Some(Box::new(half));
    f.write_xform = Some(Box::new(|val| rounded(val, 2.0)));
    // The app never sends a setpoint alone; it repeats the mode and fan it believes are current.
    f.write_attach_static = Some(vec![TAG_MODE, TAG_FAN]);
    core.add_field(config, f, true);

    let core_cb = core.clone();
    let mut f = FieldDefinition::new("climate", "power")
        .with_id(TAG_POWER)
        .write_only();
    f.write_xform = Some(Box::new(on_off_write));
    // Switching on restores mode, fan and setpoint in one frame, as the app does.
    f.write_attach = Some(Box::new(|raw| {
        if raw != 0 {
            vec![TAG_MODE, TAG_FAN, TAG_TARGET_TEMP]
        } else {
            vec![]
        }
    }));
    f.read_xform = Some(Box::new(on_off));
    f.read_callback = Some(Box::new(move |val| {
        core_cb
            .ha
            .publish_property(&core_cb.id, "power-", val.clone());
        refresh_mode(&core_cb);
        false
    }));
    core.add_field(config, f, true);

    let core_r = core.clone();
    let core_w = core.clone();
    let mut f = FieldDefinition::new("climate", "mode").with_id(TAG_MODE);
    f.read_xform = Some(Box::new(move |raw| {
        if core_r.get_raw(TAG_POWER) == Some(0) {
            return Some("off".into());
        }
        label(MODES, raw).map(PropertyValue::from)
    }));
    f.write_xform = Some(Box::new(move |val| {
        // Not a wire mode: Home Assistant sends it to mean power off.
        if val == "off" {
            core_w.set_property("climate-power", "OFF");
            return None;
        }
        // A write while off is ignored by the unit.
        core_w.set_raw(TAG_POWER, 1);
        code(MODES, val).map(PropertyValue::from)
    }));
    f.write_attach_static = Some(vec![TAG_POWER, TAG_FAN, TAG_TARGET_TEMP]);
    core.add_field(config, f, true);

    let core_w = core.clone();
    let mut f = FieldDefinition::new("climate", "fan_mode").with_id(TAG_FAN);
    f.read_xform = Some(Box::new(|raw| {
        label(FAN_LEVELS, fan_level(raw)).map(PropertyValue::from)
    }));
    f.write_xform = Some(Box::new(move |val| {
        let level = code(FAN_LEVELS, val)?;
        core_w.set_raw(TAG_POWER, 1);
        Some(PropertyValue::from(level * 0x0101))
    }));
    f.write_attach_static = Some(vec![TAG_POWER, TAG_MODE, TAG_TARGET_TEMP]);
    core.add_field(config, f, true);

    // An unseen combination reads "off" rather than leaving a stale value.
    for (id, name, table) in [
        (TAG_SWING_V, "swing_mode", SWING_V),
        (TAG_SWING_H, "swing_horizontal_mode", SWING_H),
    ] {
        let mut f = FieldDefinition::new("climate", name).with_id(id);
        f.read_xform = Some(Box::new(move |raw| {
            Some(label(table, raw).unwrap_or("off").into())
        }));
        f.write_xform = Some(Box::new(move |val| {
            code(table, val).map(PropertyValue::from)
        }));
        core.add_field(config, f, true);
    }
}

/// Re-evaluates the mode so it flips between "off" and the set mode.
fn refresh_mode(core: &TlvDeviceCore) {
    match core.get_raw(TAG_MODE) {
        Some(mode) => core.process_key_value(TAG_MODE, mode),
        // Upstream re-reads an unknown mode as well, which still reads "off" while powered down.
        None if core.get_raw(TAG_POWER) == Some(0) => {
            let previous = core
                .publish_cache
                .lock()
                .insert("climate-mode".into(), "off".into());
            if previous.as_deref() != Some("off") {
                core.ha
                    .publish_property(&core.id, "climate-mode", "off".into());
            }
        }
        None => {}
    }
}

fn add_feature_fields(core: &Arc<TlvDeviceCore>, config: &mut DeviceDiscovery) {
    config.components.insert(
        "power".into(),
        json!({
            "platform": "binary_sensor",
            "unique_id": "$deviceid-power",
            "name": "Power",
            "state_topic": "$this/power-",
            "entity_category": "diagnostic",
        }),
    );
    add_sensor(
        core,
        config,
        TAG_SMART_GUIDE,
        "smartguide",
        json!({
            "platform": "binary_sensor",
            "unique_id": "$deviceid-smartguide",
            "name": "Smart guide",
            "entity_category": "diagnostic",
        }),
        Some(Box::new(on_off)),
    );

    for (id, comp, name, icon) in [
        (TAG_ECO, "eco", "Energy saving", "mdi:flower"),
        (TAG_AIR_CLEAN, "airclean", "Air purify", "mdi:air-purifier"),
        (TAG_SMART_CARE, "smartcare", "Smart care", "mdi:chip"),
    ] {
        config.components.insert(
            comp.into(),
            json!({
                "platform": "switch",
                "unique_id": format!("$deviceid-{comp}"),
                "name": name,
                "icon": icon,
                "entity_category": "config",
            }),
        );
        let mut f = FieldDefinition::new(comp, "").with_id(id);
        f.read_xform = Some(Box::new(on_off));
        f.write_xform = Some(Box::new(on_off_write));
        core.add_field(config, f, true);
    }

    // No 0x236=0 or 0x209=0 write was ever observed: off is the app's shared off bundle.
    config.components.insert(
        "wind_mode".into(),
        json!({
            "platform": "select",
            "unique_id": "$deviceid-wind_mode",
            "name": "Wind mode",
            "icon": "mdi:wind-power",
            "options": WIND_MODES,
            "entity_category": "config",
        }),
    );
    let core_cb = core.clone();
    let core_w = core.clone();
    let mut f = FieldDefinition::new("wind_mode", "").with_id(TAG_COOL_POWER);
    f.read_callback = Some(Box::new(move |_| {
        publish_wind_mode(&core_cb);
        false
    }));
    f.write_xform = Some(Box::new(|val| {
        Some(PropertyValue::from(match val {
            "coolpower" => WIND_COOLPOWER,
            "longpower" => WIND_LONGPOWER,
            _ => WIND_OFF,
        }))
    }));
    f.write_callback = Some(Box::new(move |val| {
        if val == WIND_COOLPOWER {
            return true;
        }
        let (Some(mode), Some(target)) =
            (core_w.get_raw(TAG_MODE), core_w.get_raw(TAG_TARGET_TEMP))
        else {
            return false;
        };
        // The fan is forced to max (9) for longpower and to high (6) for off, as the app sent them.
        let level: u32 = if val == WIND_LONGPOWER { 9 } else { 6 };
        let fan = level * 0x0101;
        core_w.set_raw(TAG_FAN, fan);
        core_w.send(
            &[1, 1, 2, 1, 1],
            &[
                Tlv::new(TAG_MODE, mode),
                Tlv::new(TAG_FAN, fan),
                Tlv::new(TAG_TARGET_TEMP, target),
            ],
        );
        false
    }));
    core.add_field(config, f, true);

    let core_cb = core.clone();
    let mut f = FieldDefinition::new("wind_mode", "windlong").with_id(TAG_LONG_POWER);
    f.readable = false;
    // Upstream gives it no write_xform, which makes it unwritable.
    f.writable = false;
    f.read_callback = Some(Box::new(move |_| {
        publish_wind_mode(&core_cb);
        false
    }));
    core.add_field(config, f, false);

    config.components.insert(
        "human_sense".into(),
        json!({
            "platform": "select",
            "unique_id": "$deviceid-human_sense",
            "name": "Human sense",
            "icon": "mdi:human",
            "options": options(HUMAN_SENSE),
            "entity_category": "config",
        }),
    );
    let mut f = FieldDefinition::new("human_sense", "").with_id(TAG_HUMAN_SENSE);
    f.read_xform = Some(Box::new(|raw| {
        label(HUMAN_SENSE, raw).map(PropertyValue::from)
    }));
    f.write_xform = Some(Box::new(|val| {
        code(HUMAN_SENSE, val).map(PropertyValue::from)
    }));
    core.add_field(config, f, true);

    // Minutes, counting down; 0 cancels.
    for (id, comp, name, icon, max, step) in [
        (
            TAG_SLEEP_TIMER,
            "sleep_timer",
            "Sleep timer",
            "mdi:bed-clock",
            420,
            10,
        ),
        (
            TAG_START_TIMER,
            "start_timer",
            "Turn-on timer",
            "mdi:timer-play",
            1440,
            60,
        ),
        (
            TAG_STOP_TIMER,
            "stop_timer",
            "Turn-off timer",
            "mdi:timer-stop",
            1440,
            60,
        ),
    ] {
        config.components.insert(
            comp.into(),
            json!({
                "platform": "number",
                "unique_id": format!("$deviceid-{comp}"),
                "name": name,
                "icon": icon,
                "device_class": "duration",
                "unit_of_measurement": "min",
                "min": 0,
                "max": max,
                "step": step,
                "mode": "box",
            }),
        );
        let mut f = FieldDefinition::new(comp, "").with_id(id);
        f.write_xform = Some(Box::new(|val| rounded(val, 1.0)));
        core.add_field(config, f, true);
    }
}

/// Coolpower wins over longpower.
fn publish_wind_mode(core: &TlvDeviceCore) {
    let mode = if core.get_raw(TAG_COOL_POWER).unwrap_or(0) != 0 {
        "coolpower"
    } else if core.get_raw(TAG_LONG_POWER).unwrap_or(0) != 0 {
        "longpower"
    } else {
        "off"
    };
    core.ha
        .publish_property(&core.id, "wind_mode-", mode.into());
}

fn add_sensor_fields(core: &Arc<TlvDeviceCore>, config: &mut DeviceDiscovery) {
    for (id, comp, name) in [
        (TAG_PM1, "pm1", "PM1.0"),
        (TAG_PM25, "pm25", "PM2.5"),
        (TAG_PM10, "pm10", "PM10"),
    ] {
        add_sensor(
            core,
            config,
            id,
            comp,
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{comp}"),
                "name": name,
                "device_class": comp,
                "unit_of_measurement": "µg/m³",
                "state_class": "measurement",
                "suggested_display_precision": 0,
            }),
            None,
        );
    }
    // A one-to-one percentage on this model, unlike the cassette unit's tenths.
    add_sensor(
        core,
        config,
        TAG_HUMIDITY,
        "humidity",
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-humidity",
            "name": "Humidity",
            "device_class": "humidity",
            "unit_of_measurement": "%",
            "state_class": "measurement",
            "suggested_display_precision": 0,
        }),
        None,
    );
    add_sensor(
        core,
        config,
        TAG_AIR_QUALITY,
        "air_quality",
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-air_quality",
            "name": "Air quality",
            "icon": "mdi:weather-hazy",
            "device_class": "enum",
            "options": options(AIR_QUALITY),
        }),
        Some(Box::new(|raw| {
            label(AIR_QUALITY, raw).map(PropertyValue::from)
        })),
    );
    add_sensor(
        core,
        config,
        TAG_AUTO_DRY,
        "autodry",
        json!({
            "platform": "binary_sensor",
            "unique_id": "$deviceid-autodry",
            "name": "Auto dry",
            "icon": "mdi:hair-dryer",
            "entity_category": "diagnostic",
        }),
        Some(Box::new(on_off)),
    );
    add_sensor(
        core,
        config,
        TAG_AUTO_DRY_REMAIN,
        "dry_remain",
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-dry_remain",
            "name": "Auto dry remaining",
            "icon": "mdi:hair-dryer-outline",
            "device_class": "duration",
            "unit_of_measurement": "min",
            "entity_category": "diagnostic",
        }),
        None,
    );

    for (comp, name) in [
        ("filter_used_time", "Filter used time"),
        ("filter_life_time", "Filter life time"),
        ("filter_remaining", "Filter remaining"),
    ] {
        config.components.insert(
            comp.into(),
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{comp}"),
                "name": name,
                "icon": "mdi:air-filter",
                "device_class": "duration",
                "unit_of_measurement": "h",
                "entity_category": "diagnostic",
            }),
        );
        core.add_field(config, FieldDefinition::new(comp, "").read_only(), true);
    }
    for (id, name) in [(TAG_FILTER_USED, ""), (TAG_FILTER_LIFE, "max")] {
        let core_cb = core.clone();
        let mut f = FieldDefinition::new("filter_used_time", name)
            .with_id(id)
            .read_only();
        f.read_callback = Some(Box::new(move |_| {
            publish_filter(&core_cb);
            false
        }));
        core.add_field(config, f, false);
    }

    // Published raw: readings as low as 32 W occur while running, so the RAC -60 does not apply.
    add_sensor(
        core,
        config,
        TAG_POWER_DRAW,
        "power_draw",
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-power_draw",
            "name": "Power",
            "device_class": "power",
            "unit_of_measurement": "W",
            "state_class": "measurement",
            "suggested_display_precision": 0,
        }),
        None,
    );
    add_sensor(
        core,
        config,
        TAG_ERROR,
        "error",
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-error",
            "name": "Error code",
            "icon": "mdi:alert",
            "entity_category": "diagnostic",
        }),
        None,
    );
}

/// Published together, once both counters are known and the rated life is not zero.
fn publish_filter(core: &TlvDeviceCore) {
    let (Some(used), Some(life)) = (core.get_raw(TAG_FILTER_USED), core.get_raw(TAG_FILTER_LIFE))
    else {
        return;
    };
    if life == 0 {
        return;
    }
    for (prop, value) in [
        ("filter_used_time-", used),
        ("filter_life_time-", life),
        ("filter_remaining-", life.saturating_sub(used)),
    ] {
        core.ha.publish_property(&core.id, prop, value.into());
    }
}

fn add_sensor(
    core: &TlvDeviceCore,
    config: &mut DeviceDiscovery,
    id: u16,
    comp: &str,
    component: Value,
    read_xform: Option<ReadXform>,
) {
    config.components.insert(comp.into(), component);
    let mut f = FieldDefinition::new(comp, "").with_id(id).read_only();
    f.read_xform = read_xform;
    core.add_field(config, f, true);
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {}
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        self.core.set_property(prop, value);
    }
    fn publish_config(&self) {
        self.core.republish_config();
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};
    use rethink_util::crc16::crc16;
    use rethink_util::tlv;
    use std::collections::HashMap;

    const DEVICE_ID: &str = "test-id";
    const MODEL_ID: &str = "PAC_910604_WW";

    // Real captures from owner-operated sessions (deviceType 401, 0xA7 TLV). POWER_OFF_HEX and
    // BASE_HEX are full state frames: off before the first power-on, then on / cool / fan medium
    // / 25 °C room / 26 °C target / swings off / PM 8,8,10 / humidity 62 / filter 12 of 720 h /
    // 32 W. The rest are short delta frames carrying only the changed tags (named after them),
    // and AIR_QUALITY_VERY_BAD_HEX is the grade going 1 -> 4 after an aerosol spray. The *_STATE
    // frames and the CMD_* command frames come from a second session in which each control was
    // toggled in the ThinQ app; every control write must reproduce the TLVs of its CMD_* frame.
    // "both" horizontal swing (257) was only ever seen as state.
    const POWER_OFF_HEX: &str = concat!(
        "000004000000A70204448491009283918091C09300A300A3407DC07E407F90247EA006067F50328200814081808",
        "08080C082408D8082C083008340838183C09080868086C087009F4087C38780938193C094029440948090409000",
        "D20089408840ACC08E808E408E0090C0CCC0CD00CD40CD9039D5A002D0D54CCDC0C8C08F808FC08F40AB40AB00D1",
        "009780C49081A551",
    );
    const BASE_HEX: &str = concat!(
        "000004000000A70204F48591009280918091C09300A300A3407DC17E407F90347EA004047F50328200814081808",
        "08080C082408D8082C083008340838183C09080868086C087009F4087C38780938193C094029440948090409001",
        "D20089408840ACD0208E808E408E0090C0CCC8CD08CD4ACD903ED5A002D0D54CCDC1C8C08F808FC08F41AB40AB00",
        "D1009780C4908241AD",
    );
    const FAN_LOW_HEX: &str = "000004000000A702047B0F92C17E417F90307EA00202D201C48D320C";
    const AIR_QUALITY_VERY_BAD_HEX: &str = "000004000000A70204F80D9004CCD01DCD106ECD5091C48BFB58";
    const HUMAN_AUTO_HEX: &str = "000004000000A70204520C92C27EA0080882018180C48A1D97";
    const HUMAN_INDIRECT_HEX: &str = "000004000000A70204660692C28202C484189F";
    const COOLPOWER_HEX: &str =
        "000004000000A70204CD1492C27F90247EA0070781A001018D81CD48C490118313";
    const LONGPOWER_HEX: &str = "000004000000A70204D20E92C27EA00909818082418D80C48C1BE5";
    const ECO_OVERRIDE_HEX: &str = "000004000000A702045F0D92C27F90347EA0FF048341C48B622A";
    const ECO_OVERRIDE_REVERSED_HEX: &str = "000004000000A702049C0A92C27EA004FF8341C4881A00";
    const SLEEP_HEX: &str = "000004000000A70204A70892C286A0012CC4862AD4";
    const DRY_HEX: &str = "000004000000A70204E205895019C4839773";
    const ECO_ON_HEX: &str = "000004000000A702043F0C92C283418C90298CD05AC48AA44B";
    const AIRCLEAN_ON_HEX: &str = "000004000000A70204150692C283C1C48469F5";
    const SMARTGUIDE_HEX: &str = "000004000000A70204740A92C2CD4B8FE01000C48839F2";
    const SWING_V_HEX: &str = "000004000000A70204B50692C18141C484D7FC";
    const SWING_L_HEX: &str = "000004000000A70204560892C281A00100C48645C2";
    const SWING_R_HEX: &str = "000004000000A702049A0692C28181C484BD73";
    const SWING_BOTH_HEX: &str = "000004000000A70204F70892C281A00101C486C08E";
    const SMARTCARE_ON_HEX: &str =
        "000004000000A70204A91492C27F90327EA0080881A001018F819782C4901142D5";
    const SMARTCARE_OFF_HEX: &str = "000004000000A70204B80E92C27EA0040481808F809780C48CF29B";
    const AUTODRY_OFF_HEX: &str = "000004000000a70204c80492c28380647f";
    const AUTODRY_ON_HEX: &str = "000004000000a70204df0692c28381c484ee75";
    const TRUNCATED_HEX: &str = "000004000000A70204F48591009280";
    const BAD_LENGTH_HEX: &str = concat!(
        "000004000000A70204FF8591009280918091C09300A300A3407DC17E407F90347EA004047F50328200814081808",
        "0808080C082408D8082C083008340838183C09",
    );
    const TIMER_ON_STATE_HEX: &str = "000004000000a702043f0792c187103cc485187b";
    const TIMER_OFF_STATE_HEX: &str = "000004000000a70204500892c186e005a0c48640bb";
    const SLEEP_420_STATE_HEX: &str = "000004000000a70204630e92c18140818086a001a48f40c48cf807";
    const HUMAN_DIRECT_STATE_HEX: &str = "000004000000a70204720e92c17ea008ff82018341cd48c48cc406";
    const HUMAN_INDIRECT_STATE_HEX: &str = "000004000000a702047c2092c17ea00808820283408bc08c10008c408bc08c008c408bc08c008c40c4901daef4";
    const COOLPOWER_STATE_HEX: &str =
        "000004000000a70204b01492c17f90247ea0070781a0010182408d81c49011475a";
    const CMD_AIRCLEAN_OFF: &str = "010104000000650201000283c0430c";
    const CMD_AIRCLEAN_ON: &str = "010104000000650201000283c1532d";
    const CMD_ECO_OFF: &str = "01010400000065020100028340d284";
    const CMD_ECO_ON: &str = "01010400000065020100028341c2a5";
    const CMD_SMARTCARE_ON: &str = "01010400000065020100028f815e84";
    const CMD_SMARTCARE_OFF: &str = "01010400000065020100028f804ea5";
    const CMD_SLEEP_420: &str = "010104000000650201000486a001a4b5e7";
    const CMD_SLEEP_0: &str = "01010400000065020100028680f43d";
    const CMD_START_60: &str = "010104000000650201000387103c3c2b";
    const CMD_START_0: &str = "010104000000650201000287005684";
    const CMD_STOP_1440: &str = "010104000000650201000486e005a0240a";
    const CMD_STOP_0: &str = "010104000000650201000286c0bcf9";
    const CMD_HUMAN_DIRECT: &str = "01010400000065020100028201b950";
    const CMD_HUMAN_INDIRECT: &str = "010104000000650201000282028933";
    const CMD_HUMAN_OFF: &str = "01010400000065020100028200a971";
    const CMD_COOLPOWER_ON: &str = "01010400000065020100028d8138e6";
    const CMD_WIND_LONG: &str = "01010400000065020100097e407ea009097f9034d41d";
    const CMD_WIND_OFF: &str = "01010400000065020100097e407ea006067f9034650a";
    const CMD_COOL_HIGH_26: &str = "01010400000065020100097e407ea006067f9034650a";
    const CMD_COOL_MED_26: &str = "01010400000065020100097e407ea004047f9034cce1";
    const CMD_ON_COOL_HIGH_18: &str = "010104000000650201000b7dc17e407ea006067f90246e65";
    const CMD_DRY_LOW_24: &str = "01010400000065020100097e417ea002027f903021aa";
    const CMD_SWING_V_ON: &str = "01010400000065020100028141a4c7";
    const SWING_LEFT_ON_STATE_HEX: &str = "000004000000a702041a0892c281a00100c486d0a6";
    const SWING_LEFT_OFF_STATE_HEX: &str = "000004000000a702041e0692c28180c48478af";
    const SWING_RIGHT_ON_STATE_HEX: &str = "000004000000a702042e0692c28181c4840943";

    fn meta() -> Metadata {
        Metadata::new(MODEL_ID, "TEST", "1.0")
    }

    fn make_device() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    /// A device that has seen the base full frame, with the recorder cleared.
    fn build_ready_device() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let (ha, thinq, dev) = make_device();
        thinq.emit_data(&hex_decode(BASE_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn components(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID)
            .and_then(|d| d.config)
            .expect("HA configuration published")
            .components
    }

    fn state(ha: &MockHaConnection, comp: &str, topic_id: &str) -> Option<PropertyValue> {
        ha.get_property(DEVICE_ID, comp, topic_id)
    }

    fn int(v: i64) -> Option<PropertyValue> {
        Some(PropertyValue::Int(v))
    }

    fn text(v: &str) -> Option<PropertyValue> {
        Some(v.into())
    }

    /// TLVs of a frame, as {tag: value}.
    fn frame_tlvs(frame: &[u8]) -> HashMap<u16, u32> {
        tlv::parse(&frame[11..11 + frame[10] as usize])
            .into_iter()
            .map(|e| (e.t, e.v))
            .collect()
    }

    fn hex_tlvs(hex: &str) -> HashMap<u16, u32> {
        frame_tlvs(&hex_decode(hex))
    }

    /// TLVs of the single frame the handler just sent.
    fn sent_tlvs(thinq: &MockThinq2Device) -> HashMap<u16, u32> {
        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 1, "exactly one frame should be sent");
        frame_tlvs(&outbox[0])
    }

    #[test]
    fn every_declared_capture_fixture_has_a_valid_envelope_length_and_crc() {
        for frame in [
            POWER_OFF_HEX,
            BASE_HEX,
            FAN_LOW_HEX,
            AIR_QUALITY_VERY_BAD_HEX,
            HUMAN_AUTO_HEX,
            HUMAN_INDIRECT_HEX,
            COOLPOWER_HEX,
            LONGPOWER_HEX,
            ECO_OVERRIDE_HEX,
            ECO_OVERRIDE_REVERSED_HEX,
            SLEEP_HEX,
            DRY_HEX,
            ECO_ON_HEX,
            AIRCLEAN_ON_HEX,
            SMARTGUIDE_HEX,
            SWING_V_HEX,
            SWING_L_HEX,
            SWING_R_HEX,
            SWING_BOTH_HEX,
            SMARTCARE_ON_HEX,
            SMARTCARE_OFF_HEX,
            AUTODRY_OFF_HEX,
            AUTODRY_ON_HEX,
            TIMER_ON_STATE_HEX,
            TIMER_OFF_STATE_HEX,
            SLEEP_420_STATE_HEX,
            HUMAN_DIRECT_STATE_HEX,
            HUMAN_INDIRECT_STATE_HEX,
            COOLPOWER_STATE_HEX,
        ] {
            let packet = hex_decode(frame);
            assert_eq!(packet.len(), packet[10] as usize + 13, "{frame}");
            assert_eq!(crc16(&packet[2..]), 0, "{frame}");
        }
    }

    #[test]
    fn discovery_is_immediate_and_a_delta_first_connection_publishes_state() {
        let (ha, thinq, _dev) = make_device();
        assert!(ha.device(DEVICE_ID).and_then(|d| d.config).is_some());
        thinq.emit_data(&hex_decode(FAN_LOW_HEX));
        assert_eq!(state(&ha, "climate", "mode_state"), text("dry"));
        assert_eq!(state(&ha, "climate", "fan_mode_state"), text("low"));
        assert_eq!(state(&ha, "climate", "temperature_state"), int(24));
        assert!(thinq.outbox().is_empty());
        assert!(thinq.sent().is_empty());
    }

    #[test]
    fn constructor_and_start_send_nothing() {
        let (_ha, thinq, dev) = make_device();
        dev.start();
        assert!(thinq.outbox().is_empty(), "no packets on the wire");
        assert!(thinq.sent().is_empty(), "no ThinQ messages");
    }

    #[test]
    fn base_frame_publishes_config_with_command_topics_only_on_controls() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(BASE_HEX));
        let c = components(&ha);
        assert_eq!(c["climate"]["platform"], "climate");
        assert_eq!(c["climate"]["current_humidity_topic"], "$this/humidity-");
        let controls = [
            "climate",
            "eco",
            "airclean",
            "smartcare",
            "wind_mode",
            "human_sense",
            "sleep_timer",
            "start_timer",
            "stop_timer",
        ];
        for (name, comp) in &c {
            let has_command = comp
                .as_object()
                .unwrap()
                .keys()
                .any(|k| k.ends_with("command_topic"));
            assert_eq!(
                has_command,
                controls.contains(&name.as_str()),
                "{name} command topic"
            );
        }
        assert_eq!(
            c["wind_mode"]["options"],
            json!(["off", "coolpower", "longpower"])
        );
        assert_eq!(
            c["human_sense"]["options"],
            json!(["off", "direct", "indirect"])
        );
        assert_eq!(c["climate"]["swing_modes"], json!(["off", "on"]));
        assert_eq!(
            c["climate"]["swing_horizontal_modes"],
            json!(["off", "right", "left", "both"])
        );
        assert!(!c.contains_key("swing_v"), "no loose vertical swing entity");
        assert!(
            !c.contains_key("swing_h"),
            "no loose horizontal swing entity"
        );
        assert_eq!(c["climate"]["modes"], json!(["off", "cool", "dry"]));
        assert_eq!(
            c["climate"]["fan_modes"],
            json!(["auto", "low", "medium", "high", "turbo", "max"])
        );
    }

    #[test]
    fn base_frame_publishes_expected_state() {
        let (ha, _thinq, _dev) = build_ready_device();
        assert_eq!(state(&ha, "climate", "current_temperature"), int(25));
        assert_eq!(state(&ha, "climate", "current_humidity"), int(62));
        assert_eq!(state(&ha, "climate", "temperature_state"), int(26));
        assert_eq!(state(&ha, "climate", "mode_state"), text("cool"));
        assert_eq!(state(&ha, "climate", "fan_mode_state"), text("medium"));
        assert_eq!(state(&ha, "power", "state"), text("ON"));
        assert_eq!(state(&ha, "pm1", "state"), int(8));
        assert_eq!(state(&ha, "pm25", "state"), int(8));
        assert_eq!(state(&ha, "pm10", "state"), int(10));
        assert_eq!(state(&ha, "humidity", "state"), int(62));
        assert_eq!(state(&ha, "air_quality", "state"), text("good"));
    }

    #[test]
    fn air_quality_grade_decodes_and_is_read_only() {
        let (ha, thinq, dev) = build_ready_device();
        thinq.emit_data(&hex_decode(AIR_QUALITY_VERY_BAD_HEX));
        assert_eq!(state(&ha, "air_quality", "state"), text("very_bad"));

        thinq.reset_recorder();
        dev.set_property("air_quality-", "good");
        assert!(thinq.outbox().is_empty(), "writes must not reach the wire");
        assert!(
            thinq.sent().is_empty(),
            "writes must not send ThinQ messages"
        );
    }

    #[test]
    fn power_off_frame_reports_off() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(POWER_OFF_HEX));
        assert_eq!(state(&ha, "climate", "mode_state"), text("off"));
        assert_eq!(state(&ha, "power", "state"), text("OFF"));
        assert_eq!(state(&ha, "climate", "fan_mode_state"), text("high"));
    }

    #[test]
    fn fan_levels_decode() {
        let (ha, thinq, _dev) = build_ready_device();
        thinq.emit_data(&hex_decode(FAN_LOW_HEX));
        assert_eq!(state(&ha, "climate", "fan_mode_state"), text("low"));
        assert_eq!(state(&ha, "climate", "mode_state"), text("dry"));
        assert_eq!(state(&ha, "climate", "temperature_state"), int(24));
        thinq.emit_data(&hex_decode(HUMAN_AUTO_HEX));
        assert_eq!(state(&ha, "climate", "fan_mode_state"), text("auto"));
        thinq.emit_data(&hex_decode(COOLPOWER_HEX));
        assert_eq!(state(&ha, "climate", "fan_mode_state"), text("turbo"));
        assert_eq!(state(&ha, "climate", "temperature_state"), int(18));
        thinq.emit_data(&hex_decode(LONGPOWER_HEX));
        assert_eq!(state(&ha, "climate", "fan_mode_state"), text("max"));
    }

    #[test]
    fn eco_override_flag_still_reads_the_fan_level() {
        let (ha, thinq, _dev) = build_ready_device();
        // 0x1fa = 0xFF04, then the reversed 0x04FF: the other byte is still medium.
        thinq.emit_data(&hex_decode(ECO_OVERRIDE_HEX));
        assert_eq!(state(&ha, "climate", "fan_mode_state"), text("medium"));
        thinq.emit_data(&hex_decode(ECO_OVERRIDE_REVERSED_HEX));
        assert_eq!(state(&ha, "climate", "fan_mode_state"), text("medium"));
    }

    #[test]
    fn swing_states_decode_onto_the_climate_entity() {
        let (ha, thinq, _dev) = build_ready_device();
        thinq.emit_data(&hex_decode(SWING_V_HEX));
        assert_eq!(state(&ha, "climate", "swing_mode_state"), text("on"));
        for (frame, expected) in [
            (SWING_LEFT_ON_STATE_HEX, "left"),
            (SWING_RIGHT_ON_STATE_HEX, "right"),
            (SWING_BOTH_HEX, "both"),
            (SWING_LEFT_OFF_STATE_HEX, "off"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            assert_eq!(
                state(&ha, "climate", "swing_horizontal_mode_state"),
                text(expected)
            );
        }
    }

    #[test]
    fn human_sense_states_decode() {
        let (ha, thinq, _dev) = build_ready_device();
        thinq.emit_data(&hex_decode(HUMAN_AUTO_HEX));
        assert_eq!(state(&ha, "human_sense", "state"), text("direct"));
        thinq.emit_data(&hex_decode(HUMAN_INDIRECT_HEX));
        assert_eq!(state(&ha, "human_sense", "state"), text("indirect"));
    }

    #[test]
    fn function_toggles_decode() {
        let (ha, thinq, _dev) = build_ready_device();
        thinq.emit_data(&hex_decode(ECO_ON_HEX));
        assert_eq!(state(&ha, "eco", "state"), text("ON"));
        thinq.emit_data(&hex_decode(AIRCLEAN_ON_HEX));
        assert_eq!(state(&ha, "airclean", "state"), text("ON"));
        thinq.emit_data(&hex_decode(SMARTGUIDE_HEX));
        assert_eq!(state(&ha, "smartguide", "state"), text("ON"));
        thinq.emit_data(&hex_decode(COOLPOWER_STATE_HEX));
        assert_eq!(state(&ha, "wind_mode", "state"), text("coolpower"));
    }

    #[test]
    fn smart_care_delta_only_tags_decode() {
        let (ha, thinq, _dev) = build_ready_device();
        thinq.emit_data(&hex_decode(SMARTCARE_ON_HEX));
        assert_eq!(state(&ha, "smartcare", "state"), text("ON"));
        thinq.emit_data(&hex_decode(SMARTCARE_OFF_HEX));
        assert_eq!(state(&ha, "smartcare", "state"), text("OFF"));
    }

    #[test]
    fn timer_sensors_decode_minutes() {
        let (ha, thinq, _dev) = build_ready_device();
        thinq.emit_data(&hex_decode(SLEEP_HEX));
        assert_eq!(state(&ha, "sleep_timer", "state"), int(300));
        thinq.emit_data(&hex_decode(DRY_HEX));
        assert_eq!(state(&ha, "dry_remain", "state"), int(25));
    }

    #[test]
    fn timer_numbers_decode_minutes() {
        let (ha, thinq, _dev) = build_ready_device();
        thinq.emit_data(&hex_decode(SLEEP_420_STATE_HEX));
        assert_eq!(state(&ha, "sleep_timer", "state"), int(420));
        thinq.emit_data(&hex_decode(TIMER_ON_STATE_HEX));
        assert_eq!(state(&ha, "start_timer", "state"), int(60));
        thinq.emit_data(&hex_decode(TIMER_OFF_STATE_HEX));
        assert_eq!(state(&ha, "stop_timer", "state"), int(1440));
    }

    #[test]
    fn wind_mode_derives_from_both_power_tags() {
        let (ha, thinq, _dev) = build_ready_device();
        assert_eq!(state(&ha, "wind_mode", "state"), text("off"));
        thinq.emit_data(&hex_decode(COOLPOWER_STATE_HEX));
        assert_eq!(state(&ha, "wind_mode", "state"), text("coolpower"));
        thinq.emit_data(&hex_decode(LONGPOWER_HEX));
        assert_eq!(state(&ha, "wind_mode", "state"), text("longpower"));
    }

    #[test]
    fn switch_writes_reproduce_the_captured_app_frames() {
        for (comp, on, off) in [
            ("airclean", CMD_AIRCLEAN_ON, CMD_AIRCLEAN_OFF),
            ("eco", CMD_ECO_ON, CMD_ECO_OFF),
            ("smartcare", CMD_SMARTCARE_ON, CMD_SMARTCARE_OFF),
        ] {
            let (_ha, thinq, dev) = build_ready_device();
            dev.set_property(&format!("{comp}-"), "ON");
            assert_eq!(sent_tlvs(&thinq), hex_tlvs(on), "{comp} ON");
            thinq.reset_recorder();
            dev.set_property(&format!("{comp}-"), "OFF");
            assert_eq!(sent_tlvs(&thinq), hex_tlvs(off), "{comp} OFF");
        }
    }

    #[test]
    fn human_sense_select_writes_reproduce_the_captured_app_frames() {
        let (_ha, thinq, dev) = build_ready_device();
        for (val, cmd) in [
            ("direct", CMD_HUMAN_DIRECT),
            ("indirect", CMD_HUMAN_INDIRECT),
            ("off", CMD_HUMAN_OFF),
        ] {
            dev.set_property("human_sense-", val);
            assert_eq!(sent_tlvs(&thinq), hex_tlvs(cmd), "{val}");
            thinq.reset_recorder();
        }
    }

    #[test]
    fn timer_number_writes_reproduce_the_captured_app_frames() {
        let (_ha, thinq, dev) = build_ready_device();
        for (comp, val, cmd) in [
            ("sleep_timer", "420", CMD_SLEEP_420),
            ("sleep_timer", "0", CMD_SLEEP_0),
            ("start_timer", "60", CMD_START_60),
            ("start_timer", "0", CMD_START_0),
            ("stop_timer", "1440", CMD_STOP_1440),
            ("stop_timer", "0", CMD_STOP_0),
        ] {
            dev.set_property(&format!("{comp}-"), val);
            assert_eq!(sent_tlvs(&thinq), hex_tlvs(cmd), "{comp}={val}");
            thinq.reset_recorder();
        }
    }

    #[test]
    fn wind_mode_writes_reproduce_the_captured_app_frames() {
        // BASE is cool / medium / 26 °C, the capture session's state, so the bundles match exactly.
        let (_ha, thinq, dev) = build_ready_device();
        dev.set_property("wind_mode-", "coolpower");
        assert_eq!(sent_tlvs(&thinq), hex_tlvs(CMD_COOLPOWER_ON));
        thinq.reset_recorder();
        dev.set_property("wind_mode-", "longpower");
        assert_eq!(sent_tlvs(&thinq), hex_tlvs(CMD_WIND_LONG));
        thinq.reset_recorder();
        dev.set_property("wind_mode-", "off");
        assert_eq!(sent_tlvs(&thinq), hex_tlvs(CMD_WIND_OFF));
    }

    #[test]
    fn climate_writes_reproduce_the_frames_the_app_itself_sent() {
        // A mode write repeats the fan and setpoint the appliance currently holds.
        let (_ha, thinq, dev) = build_ready_device();
        dev.set_property("climate-mode", "cool");
        let got = sent_tlvs(&thinq);
        let want = hex_tlvs(CMD_COOL_MED_26);
        assert_eq!(got.get(&0x1f9), want.get(&0x1f9));
        assert_eq!(
            got.get(&0x1fa),
            want.get(&0x1fa),
            "fan is duplicated across both bytes"
        );
        assert_eq!(got.get(&0x1fe), want.get(&0x1fe));
        assert_eq!(
            got.get(&0x1f7),
            Some(&1),
            "power is attached; a write while off is ignored"
        );

        let (_ha, thinq, dev) = build_ready_device();
        dev.set_property("climate-fan_mode", "high");
        let got = sent_tlvs(&thinq);
        let want = hex_tlvs(CMD_COOL_HIGH_26);
        assert_eq!(got.get(&0x1fa), want.get(&0x1fa));
        assert_eq!(got.get(&0x1f9), want.get(&0x1f9));
        assert_eq!(got.get(&0x1fe), want.get(&0x1fe));

        let (_ha, thinq, dev) = build_ready_device();
        dev.set_property("climate-mode", "dry");
        dev.set_property("climate-fan_mode", "low");
        thinq.reset_recorder();
        dev.set_property("climate-temperature", "24");
        let got = sent_tlvs(&thinq);
        let want = hex_tlvs(CMD_DRY_LOW_24);
        assert_eq!(got.get(&0x1f9), want.get(&0x1f9));
        assert_eq!(got.get(&0x1fa), want.get(&0x1fa));
        assert_eq!(
            got.get(&0x1fe),
            want.get(&0x1fe),
            "24 °C is written as raw 48"
        );
    }

    #[test]
    fn power_on_restores_mode_fan_and_setpoint_in_one_frame_and_off_sends_power_alone() {
        let (_ha, thinq, dev) = build_ready_device();
        dev.set_property("climate-fan_mode", "high");
        dev.set_property("climate-temperature", "18");
        thinq.reset_recorder();

        dev.set_property("climate-power", "ON");
        let got = sent_tlvs(&thinq);
        let want = hex_tlvs(CMD_ON_COOL_HIGH_18);
        for tag in [0x1f7, 0x1f9, 0x1fa, 0x1fe] {
            assert_eq!(got.get(&tag), want.get(&tag), "tag 0x{tag:x}");
        }

        thinq.reset_recorder();
        dev.set_property("climate-power", "OFF");
        let off = sent_tlvs(&thinq);
        assert_eq!(off.get(&0x1f7), Some(&0));
        assert_eq!(
            off.get(&0x1f9),
            None,
            "nothing is attached when switching off"
        );
        assert_eq!(off.get(&0x1fa), None);
    }

    #[test]
    fn the_off_climate_mode_powers_the_unit_down_instead_of_writing_a_wire_mode() {
        let (_ha, thinq, dev) = build_ready_device();
        dev.set_property("climate-mode", "off");
        let got = sent_tlvs(&thinq);
        assert_eq!(got.get(&0x1f7), Some(&0), "power off");
        assert_eq!(got.get(&0x1f9), None, "'off' is not a wire mode");
    }

    #[test]
    fn swing_writes_reproduce_the_captured_vertical_frame_and_echo_the_driven_horizontal_states() {
        let (_ha, thinq, dev) = build_ready_device();
        dev.set_property("climate-swing_mode", "on");
        assert_eq!(
            sent_tlvs(&thinq).get(&0x205),
            hex_tlvs(CMD_SWING_V_ON).get(&0x205)
        );

        for (value, expected) in [
            ("left", hex_tlvs(SWING_LEFT_ON_STATE_HEX)[&0x206]),
            ("off", hex_tlvs(SWING_LEFT_OFF_STATE_HEX)[&0x206]),
            ("right", hex_tlvs(SWING_RIGHT_ON_STATE_HEX)[&0x206]),
            ("both", 257),
        ] {
            let (_ha, thinq, dev) = build_ready_device();
            dev.set_property("climate-swing_horizontal_mode", value);
            assert_eq!(
                sent_tlvs(&thinq).get(&0x206),
                Some(&expected),
                "swing_horizontal_mode={value}"
            );
        }
    }

    #[test]
    fn power_draw_is_published_in_watts_and_reads_zero_while_off() {
        let (ha, _thinq, dev) = build_ready_device();
        let c = components(&ha);
        assert_eq!(c["power_draw"]["device_class"], "power");
        assert_eq!(c["power_draw"]["unit_of_measurement"], "W");

        // Raw: the RAC -60 correction would make these negative.
        assert_eq!(state(&ha, "power_draw", "state"), int(32));
        dev.core.process_key_value(0x2b3, 578);
        assert_eq!(state(&ha, "power_draw", "state"), int(578));
        dev.core.process_key_value(0x2b3, 0);
        assert_eq!(state(&ha, "power_draw", "state"), int(0));
    }

    #[test]
    fn auto_dry_enable_state_round_trips_on_the_owner_captured_frames() {
        let (ha, thinq, _dev) = build_ready_device();
        thinq.emit_data(&hex_decode(AUTODRY_OFF_HEX));
        assert_eq!(state(&ha, "autodry", "state"), text("OFF"));
        thinq.emit_data(&hex_decode(AUTODRY_ON_HEX));
        assert_eq!(state(&ha, "autodry", "state"), text("ON"));
        assert!(thinq.outbox().is_empty());
        assert!(thinq.sent().is_empty());
    }

    #[test]
    fn filter_counters_expose_used_rated_and_remaining_hours() {
        let (ha, thinq, dev) = build_ready_device();
        // The base frame carried 0x355 = 12 used out of 0x356 = 720 rated hours.
        assert_eq!(state(&ha, "filter_used_time", "state"), int(12));
        assert_eq!(state(&ha, "filter_life_time", "state"), int(720));
        assert_eq!(state(&ha, "filter_remaining", "state"), int(708));
        assert_eq!(state(&ha, "error", "state"), int(0));
        // Upstream asserts these are absent; here they are the earlier port's removal tombstones.
        let c = components(&ha);
        assert_eq!(
            c["filterreset"],
            json!({"platform": "button"}),
            "no reset button without a captured PAC reset command"
        );
        assert_eq!(
            c["filter_used"],
            json!({"platform": "sensor"}),
            "percentage is template-derived"
        );

        dev.core.process_key_value(0x355, 360);
        assert_eq!(state(&ha, "filter_used_time", "state"), int(360));
        assert_eq!(state(&ha, "filter_remaining", "state"), int(360));

        // A zero rated lifetime must not divide or replace the last good values.
        dev.core.process_key_value(0x356, 0);
        dev.core.process_key_value(0x355, 400);
        assert_eq!(state(&ha, "filter_remaining", "state"), int(360));
        dev.core.process_key_value(0x356, 100);
        assert_eq!(state(&ha, "filter_remaining", "state"), int(0));

        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn sensor_writes_still_send_nothing() {
        let (_ha, thinq, dev) = build_ready_device();
        dev.set_property("pm1-", "10");
        dev.set_property("humidity-", "50");
        assert!(thinq.outbox().is_empty(), "writes must not reach the wire");
        assert!(
            thinq.sent().is_empty(),
            "writes must not send ThinQ messages"
        );
    }

    #[test]
    fn appliance_status_is_diagnostic_and_controls_and_room_readings_stay_primary() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(BASE_HEX));
        let c = components(&ha);
        for name in [
            "power",
            "smartguide",
            "autodry",
            "dry_remain",
            "filter_used_time",
            "filter_life_time",
            "filter_remaining",
            "error",
        ] {
            assert_eq!(
                c[name]["entity_category"], "diagnostic",
                "{name} should be diagnostic"
            );
        }
        for name in [
            "climate",
            "sleep_timer",
            "start_timer",
            "stop_timer",
            "pm1",
            "pm25",
            "pm10",
            "humidity",
            "power_draw",
        ] {
            assert!(
                c[name].get("entity_category").is_none(),
                "{name} should stay primary"
            );
        }
        for name in ["eco", "airclean", "smartcare", "wind_mode", "human_sense"] {
            assert_eq!(
                c[name]["entity_category"], "config",
                "{name} should be config"
            );
        }
    }

    #[test]
    fn malformed_frames_are_ignored() {
        let (ha, thinq, _dev) = build_ready_device();
        thinq.emit_data(&hex_decode(TRUNCATED_HEX));
        // Node's Buffer.from(hex) drops the odd trailing digit of this fixture.
        thinq.emit_data(&hex_decode(&BAD_LENGTH_HEX[..BAD_LENGTH_HEX.len() & !1]));
        thinq.emit_data(&[0x00, 0x01, 0x02, 0x03]);
        assert_eq!(state(&ha, "climate", "mode_state"), text("cool"));
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn climate_swing_and_human_sense_options_match_the_published_contract() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(BASE_HEX));
        let c = components(&ha);
        assert_eq!(c["climate"]["swing_modes"], json!(["off", "on"]));
        assert_eq!(
            c["climate"]["swing_horizontal_modes"],
            json!(["off", "right", "left", "both"])
        );
        assert_eq!(c["human_sense"]["platform"], "select");
        assert_eq!(
            c["human_sense"]["options"],
            json!(["off", "direct", "indirect"])
        );
    }

    #[test]
    fn entities_of_the_earlier_port_are_withdrawn_with_tombstones() {
        let (ha, _thinq, _dev) = make_device();
        let c = components(&ha);
        for (key, platform) in [
            ("energy_current", "sensor"),
            ("jet", "switch"),
            ("quiet", "switch"),
            ("uvnano", "switch"),
            ("childlock", "switch"),
            ("spacefit", "switch"),
            ("display", "switch"),
            ("beep", "switch"),
            ("hxclean", "switch"),
            ("allclean", "switch"),
            ("autodrylevel", "select"),
            ("autodryremain", "sensor"),
            ("autodryrunning", "binary_sensor"),
            ("autodrycancel", "button"),
            ("onesidewind", "select"),
            ("humiditydisplay", "select"),
            ("tempstep", "select"),
            ("filter_life", "sensor"),
            ("filter_used", "sensor"),
            ("filterreset", "button"),
        ] {
            assert_eq!(
                c[key],
                json!({ "platform": platform }),
                "{key} is a tombstone only"
            );
        }
        // Kept keys are live entities, not tombstones.
        for key in [
            "climate",
            "airclean",
            "smartcare",
            "autodry",
            "humidity",
            "filter_remaining",
        ] {
            assert!(
                c[key].get("unique_id").is_some(),
                "{key} is still published"
            );
        }
    }

    #[test]
    fn power_off_before_any_mode_still_reads_off() {
        let (ha, thinq, _dev) = make_device();
        let pkt = [Tlv::new(TAG_POWER, 0)];
        let body = tlv::build(&pkt);
        let mut frame = vec![
            0x00,
            0x00,
            0x04,
            0x00,
            0x00,
            0x00,
            0xa7,
            0x02,
            0x04,
            0x00,
            body.len() as u8,
        ];
        frame.extend_from_slice(&body);
        frame.extend_from_slice(&[0x00, 0x00]);
        thinq.emit_data(&frame);
        assert_eq!(state(&ha, "power", "state"), text("OFF"));
        assert_eq!(state(&ha, "climate", "mode_state"), text("off"));
    }

    #[test]
    fn number_payloads_parse_like_javascript_number() {
        for (prop, payload, tag, raw) in [
            ("sleep_timer-", " 30 ", TAG_SLEEP_TIMER, 30),
            ("sleep_timer-", "0x1e", TAG_SLEEP_TIMER, 30),
            ("stop_timer-", "59.6", TAG_STOP_TIMER, 60),
            ("climate-temperature", "24.5", TAG_TARGET_TEMP, 49),
        ] {
            let (_ha, thinq, dev) = build_ready_device();
            dev.set_property(prop, payload);
            assert_eq!(
                sent_tlvs(&thinq).get(&tag),
                Some(&raw),
                "{prop}={payload:?}"
            );
        }
        let (_ha, thinq, dev) = build_ready_device();
        dev.set_property("sleep_timer-", "abc");
        dev.set_property("climate-temperature", "Infinity");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn publish_config_restores_availability_and_discovery() {
        let (ha, _thinq, dev) = make_device();
        dev.drop_device();
        assert_eq!(
            ha.device(DEVICE_ID).unwrap().availability.as_deref(),
            Some("offline")
        );
        dev.publish_config();
        let info = ha.device(DEVICE_ID).unwrap();
        assert_eq!(info.availability.as_deref(), Some("online"));
        assert!(info.config.is_some());
    }

    #[test]
    fn registry_selects_this_handler_for_the_exact_model_id() {
        assert!(crate::registry::t2_factory(MODEL_ID).is_some());
    }
}
