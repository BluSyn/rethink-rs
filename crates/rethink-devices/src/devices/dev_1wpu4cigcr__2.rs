//! 1WPU4CIGCR__2 — LG PuriCare water purifier (ATOM-U, deviceType 103).
//!
//! AABB class 0x12. A state report is one or two 26-byte records, the trailing one current. The
//! usage report (`12 1f 00`, then six big-endian counters) carries per-report dispense deltas,
//! accumulated here into process-local `total_increasing` counters in litres; Home Assistant
//! handles the reset when rethink restarts. The sterilization reservation is reported as month,
//! day, hour and minute only, so it is published as a timestamp in the current UTC year.
//! Read-only.
//!
//! Port of upstream PR #118.

use crate::device_trait::DeviceHandler;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_TAG: u8 = 0x12;
const HEADER_LEN: usize = 2;
const RECORD_LEN: usize = 26;
const USAGE_BODY_LEN: usize = 14;

const OFF_MON_STATUS: usize = 0;
const OFF_COCK_STATE: usize = 1;
const OFF_WATER_SELECTION: usize = 2;
const OFF_WATER_AMOUNT_MODE: usize = 3;
const OFF_AMOUNT_UNIT: usize = 5;
const OFF_HOT_WATER_TEMP: usize = 6;
const OFF_STERILIZE_INIT_MONTH: usize = 15;
const OFF_STERILIZE_INIT_DAY: usize = 16;
const OFF_STERILIZE_INIT_HOUR: usize = 17;
const OFF_STERILIZE_INIT_MIN: usize = 18;
const OFF_MON_DATA_REFRESH: usize = 22;
const OFF_HIGH_STERILIZE_STATE: usize = 23;
const OFF_FILTER_FLUSHING_STATE: usize = 24;
const OFF_APP_VERSION: usize = 25;

// modelJSON `MonitoringValue.<field>.valueMapping`, with English labels from the model's language
// pack, or the mapping's own `_comment` where LG left the label blank.

const MON_STATUS: &[(u8, &str)] = &[(0, "Fault"), (1, "Not operating"), (2, "Normal")];

/// COCK_MANUAL_ON is the manual-dispense hold.
const COCK_STATE: &[(u8, &str)] = &[
    (0, "Standby"),
    (1, "UVnano Sterilizing"),
    (2, "Manual dispensing"),
];

/// Which tap was used last.
const WATER_SELECTION: &[(u8, &str)] = &[
    (1, "Hot water"),
    (2, "Purified"),
    (3, "Cold water"),
    (4, "Sterilized water"),
];

/// In the unit the amount unit field reports.
const WATER_AMOUNT: &[(u8, &str)] = &[(1, "120"), (2, "250"), (3, "500"), (4, "Continuous")];

/// A code, not a temperature; LG's `hotWaterTemp_C` maps it.
const HOT_WATER_TEMP: &[(u8, u32)] = &[(1, 40), (2, 75), (3, 85)];

const STERILIZE_STATE: &[(u8, &str)] = &[
    (0, "Standby"),
    (1, "Water line sterilizing"),
    (2, "Water line sterilizing"),
    (3, "Water line sterilizing"),
    (4, "Outlet sterilizing"),
    (5, "Cancelling sterilize"),
];

const FILTER_FLUSH: &[(u8, &str)] = &[(0, "Off"), (1, "Filter replacement")];

/// A setting the panel owns; it renders the live dispense volume.
const AMOUNT_UNIT: &[(u8, &str)] = &[(0, "oz"), (1, "mL")];

/// Settings and diagnostics earlier versions of this handler published. HA removes a component
/// whose discovery carries only `platform`.
const WITHDRAWN: &[(&str, &str)] = &[
    ("default_water", "select"),
    ("default_water_amount", "select"),
    ("auto_care", "switch"),
    ("not_use_notice", "switch"),
    ("button_sound", "sensor"),
    ("temp_unit", "sensor"),
    ("amount_unit", "sensor"),
];

fn lookup<T: Copy>(table: &[(u8, T)], raw: u8) -> Option<T> {
    table.iter().find(|(code, _)| *code == raw).map(|(_, v)| *v)
}

/// The reservation in `year`, as JavaScript's `toISOString()` writes it. A field left at 0xff
/// (not set) or an impossible date gives `None`.
fn reservation_timestamp(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> Option<String> {
    let at = Utc
        .with_ymd_and_hms(
            year,
            month.into(),
            day.into(),
            hour.into(),
            minute.into(),
            0,
        )
        .single()?;
    Some(at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

fn sensor(id: &str, name: &str, extra: Value) -> Value {
    let mut component = json!({
        "platform": "sensor",
        "unique_id": format!("$deviceid-{id}"),
        "state_topic": format!("$this/{id}"),
        "name": name,
    });
    if let (Value::Object(component), Value::Object(extra)) = (&mut component, extra) {
        component.extend(extra);
    }
    component
}

fn volume(id: &str, name: &str) -> Value {
    sensor(
        id,
        name,
        json!({
            "device_class": "volume",
            "unit_of_measurement": "L",
            "state_class": "total_increasing",
            "suggested_display_precision": 1,
        }),
    )
}

fn components() -> HashMap<String, Value> {
    let mut components: HashMap<String, Value> = [
        ("status", sensor("status", "State", json!({"icon": "mdi:water-check"}))),
        ("water_selection", sensor("water_selection", "Last dispense", json!({"icon": "mdi:water"}))),
        ("water_amount", sensor("water_amount", "Dispense volume", json!({"icon": "mdi:cup-water"}))),
        ("hot_water_temp", sensor("hot_water_temp", "Hot water temp", json!({"device_class": "temperature", "unit_of_measurement": "°C", "suggested_display_precision": 0, "value_template": "{{ value if value | is_number else 'None' }}"}))),
        ("cock_state", sensor("cock_state", "Outlet state", json!({"icon": "mdi:shield-sun-outline"}))),
        ("sterilize_state", sensor("sterilize_state", "Sterilize state", json!({"icon": "mdi:shield-sun"}))),
        ("filter_flushing", sensor("filter_flushing", "Filter cleaning", json!({"icon": "mdi:air-filter"}))),
        ("sterilize_reserved_at", sensor("sterilize_reserved_at", "Sterilize schedule", json!({"icon": "mdi:calendar-clock", "device_class": "timestamp"}))),
        ("app_version", sensor("app_version", "App version", json!({"icon": "mdi:tag", "entity_category": "diagnostic"}))),
        ("data_refresh", sensor("data_refresh", "Data refresh interval", json!({"icon": "mdi:timer-outline", "entity_category": "diagnostic"}))),
        ("water_usage_today", volume("water_usage_today", "Water dispensed")),
        ("water_usage_normal", volume("water_usage_normal", "Purified dispensed")),
        ("water_usage_cold", volume("water_usage_cold", "Cold water dispensed")),
        ("water_usage_hot", volume("water_usage_hot", "Hot water dispensed")),
        ("water_usage_sterilization", volume("water_usage_sterilization", "Rinse water dispensed")),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect();
    for (key, platform) in WITHDRAWN {
        components.insert(key.to_string(), json!({ "platform": platform }));
    }
    components
}

/// Running dispense counters in mL, summed from the per-report deltas.
#[derive(Default)]
struct Usage {
    total: u64,
    normal: u64,
    cold: u64,
    hot: u64,
    sterilization: u64,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    usage: Mutex<Usage>,
    now: fn() -> DateTime<Utc>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        Self::with_clock(ha, thinq, meta, Utc::now)
    }

    fn with_clock(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        now: fn() -> DateTime<Utc>,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            usage: Mutex::new(Usage::default()),
            now,
        });

        let mut config = default_config(&meta, Some(json!({"name": "LG Water Purifier"})));
        config.components = components();
        core.set_config(config);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first() != Some(&CLASS_TAG) || self.process_usage(buf) {
            return;
        }
        let payload = buf.len().saturating_sub(HEADER_LEN);
        if payload == 0 || !payload.is_multiple_of(RECORD_LEN) {
            return;
        }
        // The trailing record is the current state; a leading one, when present, is the previous.
        let rec = &buf[buf.len() - RECORD_LEN..];

        let publish_enum = |prop: &str, table: &[(u8, &'static str)], raw: u8| {
            self.core
                .publish_property(prop, lookup(table, raw).unwrap_or("None").into());
        };
        publish_enum("status", MON_STATUS, rec[OFF_MON_STATUS]);
        publish_enum("cock_state", COCK_STATE, rec[OFF_COCK_STATE]);
        publish_enum("water_selection", WATER_SELECTION, rec[OFF_WATER_SELECTION]);
        publish_enum(
            "sterilize_state",
            STERILIZE_STATE,
            rec[OFF_HIGH_STERILIZE_STATE],
        );
        publish_enum(
            "filter_flushing",
            FILTER_FLUSH,
            rec[OFF_FILTER_FLUSHING_STATE],
        );

        // The volume reads in whatever unit the appliance reports, so the unit travels with it.
        let unit = lookup(AMOUNT_UNIT, rec[OFF_AMOUNT_UNIT]).unwrap_or("mL");
        let amount = match lookup(WATER_AMOUNT, rec[OFF_WATER_AMOUNT_MODE]) {
            Some("Continuous") => "Continuous".to_string(),
            Some(amount) => format!("{amount} {unit}"),
            None => "None".to_string(),
        };
        self.core.publish_property("water_amount", amount.into());

        // 0xff: this unit is not reporting one.
        let hot = lookup(HOT_WATER_TEMP, rec[OFF_HOT_WATER_TEMP]);
        self.core.publish_property(
            "hot_water_temp",
            hot.map_or_else(|| "None".into(), PropertyValue::from),
        );

        let reserved = reservation_timestamp(
            (self.now)().year(),
            rec[OFF_STERILIZE_INIT_MONTH],
            rec[OFF_STERILIZE_INIT_DAY],
            rec[OFF_STERILIZE_INIT_HOUR],
            rec[OFF_STERILIZE_INIT_MIN],
        );
        self.core.publish_property(
            "sterilize_reserved_at",
            reserved.map_or_else(|| "None".into(), PropertyValue::from),
        );

        self.core
            .publish_property("app_version", u32::from(rec[OFF_APP_VERSION]).into());
        self.core
            .publish_property("data_refresh", u32::from(rec[OFF_MON_DATA_REFRESH]).into());
    }

    /// The usage report: `12 1f 00`, then six big-endian dispense counters (deltas since the last
    /// report). Returns true when it consumed the frame.
    fn process_usage(&self, buf: &[u8]) -> bool {
        if buf.len() != USAGE_BODY_LEN || buf[1] != 0x1f || buf[2] != 0x00 {
            return false;
        }
        let be16 = |off: usize| u64::from(u16::from_be_bytes([buf[off], buf[off + 1]]));
        let normal = u64::from(buf[3]);
        let hot = be16(4);
        let cold = be16(6);
        let sterilization = be16(12);
        let total = normal + hot + cold + be16(8) + be16(10) + sterilization;
        if total == 0 {
            return true;
        }

        let totals = {
            let mut usage = self.usage.lock();
            usage.total += total;
            usage.normal += normal;
            usage.cold += cold;
            usage.hot += hot;
            usage.sterilization += sterilization;
            [
                ("water_usage_today", usage.total),
                ("water_usage_normal", usage.normal),
                ("water_usage_cold", usage.cold),
                ("water_usage_hot", usage.hot),
                ("water_usage_sterilization", usage.sterilization),
            ]
        };
        for (prop, ml) in totals {
            self.core
                .publish_property(prop, (ml as f64 / 1000.0).into());
        }
        true
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {}
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, _prop: &str, _value: &str) {}
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
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, wrap_aabb, MockHaConnection, MockThinq2Device};

    const MODEL_ID: &str = "1WPU4CIGCR__2";

    // A real single-record frame captured from the appliance on 2026-07-28. The variants are that
    // frame with one byte changed and the checksum recomputed; each was injected into LG's cloud
    // that day, so the expected value is LG's own decode of that exact frame.
    const STATE_SHORT: &str = "aa2012eb020002010101ffff00ff010101ffff071c121eff01ff0300000177bb";

    const USAGE_FRAMES: [&str; 6] = [
        "aa12121f00fc00000000000000000000bcbb",
        "aa12121f0032000000000000000000004abb",
        "aa12121f00fc00000000000000000000bcbb",
        "aa12121f0000000000b0000000000000c8bb",
        "aa12121f000000fc0000000000000000bcbb",
        "aa12121f0000007100000000000000000bbb",
    ];

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).unwrap()
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2(MODEL_ID, |ha, thinq, meta| {
            Device::with_clock(ha, thinq, meta, fixed_now)
        })
    }

    fn components(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID).unwrap().config.unwrap().components
    }

    fn with_byte(frame: &str, index: usize, value: u8) -> Vec<u8> {
        let mut packet = hex_decode(frame);
        packet[index] = value;
        wrap_aabb(&packet[2..packet.len() - 2])
    }

    #[test]
    fn a_real_state_frame_decodes_to_ha_friendly_values() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE_SHORT));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "cock_state").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "water_selection").as_deref(), Some("Purified"));
        assert_eq!(prop(&ha, "water_amount").as_deref(), Some("120 mL"));
        // 0xff: not reported.
        assert_eq!(prop(&ha, "hot_water_temp").as_deref(), Some("None"));
        assert_eq!(
            prop(&ha, "sterilize_reserved_at").as_deref(),
            Some("2026-07-28T18:30:00.000Z")
        );
        assert_eq!(prop(&ha, "sterilize_state").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "filter_flushing").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "app_version").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "data_refresh").as_deref(), Some("3"));
    }

    #[test]
    fn captured_byte_changes_still_decode_the_right_live_values() {
        for (index, value, key, want) in [
            (6, 3, "water_selection", "Cold water"),
            (9, 0, "water_amount", "120 oz"),
            (27, 3, "sterilize_state", "Water line sterilizing"),
            (10, 3, "hot_water_temp", "85"),
        ] {
            let (ha, thinq, _) = make();
            thinq.emit_data(&with_byte(STATE_SHORT, index, value));
            assert_eq!(prop(&ha, key).as_deref(), Some(want), "{key}");
        }
    }

    #[test]
    fn real_usage_captures_accumulate_into_litres() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(USAGE_FRAMES[0]));
        assert_eq!(prop(&ha, "water_usage_today").as_deref(), Some("0.252"));
        assert_eq!(prop(&ha, "water_usage_normal").as_deref(), Some("0.252"));

        thinq.emit_data(&hex_decode(USAGE_FRAMES[1]));
        assert_eq!(prop(&ha, "water_usage_today").as_deref(), Some("0.302"));
        assert_eq!(prop(&ha, "water_usage_normal").as_deref(), Some("0.302"));

        for frame in &USAGE_FRAMES[2..] {
            thinq.emit_data(&hex_decode(frame));
        }
        assert_eq!(prop(&ha, "water_usage_today").as_deref(), Some("1.095"));
        assert_eq!(prop(&ha, "water_usage_normal").as_deref(), Some("0.554"));
        assert_eq!(prop(&ha, "water_usage_cold").as_deref(), Some("0.176"));
        assert_eq!(prop(&ha, "water_usage_hot").as_deref(), Some("0.365"));
        assert_eq!(prop(&ha, "water_usage_sterilization").as_deref(), Some("0"));
    }

    #[test]
    fn config_only_exposes_the_verified_read_only_entities() {
        let (ha, _, _) = make();
        let components = components(&ha);
        let mut entities: Vec<&str> = components
            .iter()
            .filter(|(_, c)| c.get("unique_id").is_some())
            .map(|(k, _)| k.as_str())
            .collect();
        entities.sort_unstable();
        assert_eq!(
            entities,
            [
                "app_version",
                "cock_state",
                "data_refresh",
                "filter_flushing",
                "hot_water_temp",
                "status",
                "sterilize_reserved_at",
                "sterilize_state",
                "water_amount",
                "water_selection",
                "water_usage_cold",
                "water_usage_hot",
                "water_usage_normal",
                "water_usage_sterilization",
                "water_usage_today",
            ]
        );
        for (name, comp) in &components {
            assert!(
                comp.get("command_topic").is_none(),
                "{name} stays read-only"
            );
        }
        assert_eq!(
            components["sterilize_reserved_at"]["device_class"],
            "timestamp"
        );
        for key in [
            "water_usage_today",
            "water_usage_normal",
            "water_usage_cold",
            "water_usage_hot",
            "water_usage_sterilization",
        ] {
            assert_eq!(components[key]["state_class"], "total_increasing", "{key}");
            assert_eq!(components[key]["unit_of_measurement"], "L", "{key}");
        }
        for gone in [
            "default_water",
            "default_water_amount",
            "button_sound",
            "auto_care",
            "not_use_notice",
            "temp_unit",
            "amount_unit",
            "sterilize_time",
        ] {
            // Absent, or only a removal tombstone.
            assert!(
                components
                    .get(gone)
                    .is_none_or(|c| c.get("unique_id").is_none()),
                "{gone}"
            );
        }
    }

    #[test]
    fn withdrawn_components_are_removal_tombstones() {
        let (ha, _, _) = make();
        let components = components(&ha);
        for (key, platform) in [
            ("default_water", "select"),
            ("default_water_amount", "select"),
            ("auto_care", "switch"),
            ("not_use_notice", "switch"),
            ("button_sound", "sensor"),
            ("temp_unit", "sensor"),
            ("amount_unit", "sensor"),
        ] {
            assert_eq!(components[key], json!({ "platform": platform }), "{key}");
        }
        assert!(!components.contains_key("sterilize_time"));
    }

    #[test]
    fn unknown_codes_publish_none() {
        for (index, key) in [
            (4, "status"),
            (5, "cock_state"),
            (6, "water_selection"),
            (7, "water_amount"),
            (27, "sterilize_state"),
            (28, "filter_flushing"),
        ] {
            let (ha, thinq, _) = make();
            thinq.emit_data(&with_byte(STATE_SHORT, index, 9));
            assert_eq!(prop(&ha, key).as_deref(), Some("None"), "{key}");
        }
    }

    #[test]
    fn an_unset_or_impossible_reservation_publishes_none() {
        for (index, value) in [(22, 0xff), (21, 24), (22, 60), (19, 13), (20, 0)] {
            let (ha, thinq, _) = make();
            thinq.emit_data(&with_byte(STATE_SHORT, index, value));
            assert_eq!(prop(&ha, "sterilize_reserved_at").as_deref(), Some("None"));
        }
        // 29 February does not exist in 2026.
        let mut packet = hex_decode(STATE_SHORT);
        packet[19] = 2;
        packet[20] = 29;
        let (ha, thinq, _) = make();
        thinq.emit_data(&wrap_aabb(&packet[2..packet.len() - 2]));
        assert_eq!(prop(&ha, "sterilize_reserved_at").as_deref(), Some("None"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&with_byte(STATE_SHORT, 19, 12));
        assert_eq!(
            prop(&ha, "sterilize_reserved_at").as_deref(),
            Some("2026-12-28T18:30:00.000Z")
        );
    }

    #[test]
    fn a_zero_usage_frame_changes_nothing() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(USAGE_FRAMES[0]));
        let mut zero = [0; USAGE_BODY_LEN];
        zero[..2].copy_from_slice(&[CLASS_TAG, 0x1f]);
        thinq.emit_data(&wrap_aabb(&zero));
        assert_eq!(prop(&ha, "water_usage_today").as_deref(), Some("0.252"));
        assert_eq!(prop(&ha, "status"), None);
    }

    #[test]
    fn a_usage_frame_does_not_clobber_state() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE_SHORT));
        thinq.emit_data(&hex_decode(USAGE_FRAMES[3]));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "water_usage_cold").as_deref(), Some("0.176"));
    }

    #[test]
    fn writes_send_nothing() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("status", "Normal");
        dev.set_property("default_water", "Cold water");
        dev.set_property("auto_care", "ON");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory(MODEL_ID).is_some());
    }
}
