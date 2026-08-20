//! 1WPU4CIGCR__2 — LG PuriCare water purifier (AABB class 0x12).
//!
//! Port of upstream PR #118. Record map from modelJSON + LG-cloud oracle injections.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_TAG: u8 = 0x12;
const RECORD_LEN: usize = 26;
const USAGE_BODY_LEN: usize = 14;
const IGNORE: u8 = 0xff;

const OFF_MON: usize = 0;
const OFF_COCK: usize = 1;
const OFF_WATER: usize = 2;
const OFF_AMOUNT: usize = 3;
const OFF_TEMP_UNIT: usize = 4;
const OFF_AMT_UNIT: usize = 5;
const OFF_HOT: usize = 6;
const OFF_NOT_USE: usize = 8;
const OFF_DEFAULT_WATER: usize = 10;
const OFF_DEFAULT_AMOUNT: usize = 11;
const OFF_BUTTON: usize = 12;
const OFF_STER_MON: usize = 15;
const OFF_STER_DAY: usize = 16;
const OFF_STER_HOUR: usize = 17;
const OFF_STER_MIN: usize = 18;
const OFF_AUTO_CARE: usize = 20;
const OFF_REFRESH: usize = 22;
const OFF_STER_STATE: usize = 23;
const OFF_FILTER: usize = 24;
const OFF_APP: usize = 25;

fn mon_status() -> HashMap<u8, &'static str> {
    HashMap::from([(0, "Fault"), (1, "Not operating"), (2, "Normal")])
}
fn cock_state() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0, "Standby"),
        (1, "UVnano Sterilizing"),
        (2, "Manual dispensing"),
    ])
}
fn water_sel() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "Hot water"),
        (2, "Purified"),
        (3, "Cold water"),
        (4, "Sterilized water"),
    ])
}
fn water_amount() -> HashMap<u8, &'static str> {
    HashMap::from([(1, "120"), (2, "250"), (3, "500"), (4, "Continuous")])
}
fn hot_temp() -> HashMap<u8, i64> {
    HashMap::from([(1, 40), (2, 75), (3, 85)])
}
fn default_water() -> HashMap<u8, &'static str> {
    HashMap::from([(1, "Last used"), (2, "Purified"), (3, "Cold water")])
}
fn sterilize() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0, "Standby"),
        (1, "Water line sterilizing"),
        (2, "Water line sterilizing"),
        (3, "Water line sterilizing"),
        (4, "Outlet sterilizing"),
        (5, "Cancelling sterilize"),
    ])
}
fn filter_flush() -> HashMap<u8, &'static str> {
    HashMap::from([(0, "Off"), (1, "Filter replacement")])
}
fn temp_unit() -> HashMap<u8, &'static str> {
    HashMap::from([(0, "°F"), (1, "°C")])
}
fn amount_unit() -> HashMap<u8, &'static str> {
    HashMap::from([(0, "oz"), (1, "mL")])
}

fn enum_of(map: &HashMap<u8, &'static str>, raw: u8) -> String {
    map.get(&raw)
        .copied()
        .map(str::to_string)
        .unwrap_or_else(|| format!("Code {raw}"))
}

fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

struct Usage {
    total: i64,
    normal: i64,
    cold: i64,
    hot: i64,
    sterilization: i64,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    usage: Mutex<Usage>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            usage: Mutex::new(Usage {
                total: 0,
                normal: 0,
                cold: 0,
                hot: 0,
                sterilization: 0,
            }),
        });

        let mut base = default_config(&meta, Some(json!({"name": "LG Water Purifier"})));
        let vol = |id, name| {
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{id}"),
                "state_topic": format!("$this/{id}"),
                "name": name,
                "device_class": "volume",
                "unit_of_measurement": "L",
                "state_class": "total_increasing",
                "suggested_display_precision": 1,
            })
        };
        let sensor = |id, name, icon| {
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{id}"),
                "state_topic": format!("$this/{id}"),
                "name": name,
                "icon": icon,
            })
        };
        base.components = [
            ("status", sensor("status", "State", "mdi:water-check")),
            ("water_selection", sensor("water_selection", "Last dispense", "mdi:water")),
            ("water_amount", sensor("water_amount", "Dispense volume", "mdi:cup-water")),
            ("hot_water_temp", json!({"platform":"sensor","unique_id":"$deviceid-hot_water_temp","state_topic":"$this/hot_water_temp","name":"Hot water temp","device_class":"temperature","unit_of_measurement":"°C","suggested_display_precision":0,"value_template":"{{ value if value | is_number else 'None' }}"})),
            ("cock_state", sensor("cock_state", "Outlet state", "mdi:shield-sun-outline")),
            ("sterilize_state", sensor("sterilize_state", "Sterilize state", "mdi:shield-sun")),
            ("filter_flushing", sensor("filter_flushing", "Filter cleaning", "mdi:air-filter")),
            ("sterilize_reserved_at", sensor("sterilize_reserved_at", "Sterilize schedule", "mdi:calendar-clock")),
            ("default_water", json!({"platform":"select","unique_id":"$deviceid-default_water","state_topic":"$this/default_water","command_topic":"$this/default_water/set","name":"Default water","options":["Last used","Purified","Cold water"]})),
            ("default_water_amount", json!({"platform":"select","unique_id":"$deviceid-default_water_amount","state_topic":"$this/default_water_amount","command_topic":"$this/default_water_amount/set","name":"Default volume","options":["120 mL","250 mL","500 mL"]})),
            ("auto_care", json!({"platform":"switch","unique_id":"$deviceid-auto_care","state_topic":"$this/auto_care","command_topic":"$this/auto_care/set","name":"Auto care"})),
            ("not_use_notice", json!({"platform":"switch","unique_id":"$deviceid-not_use_notice","state_topic":"$this/not_use_notice","command_topic":"$this/not_use_notice/set","name":"Disuse alert"})),
            ("button_sound", json!({"platform":"sensor","unique_id":"$deviceid-button_sound","state_topic":"$this/button_sound","name":"Button sound","entity_category":"diagnostic"})),
            ("temp_unit", json!({"platform":"sensor","unique_id":"$deviceid-temp_unit","state_topic":"$this/temp_unit","name":"Temperature unit","entity_category":"diagnostic"})),
            ("amount_unit", json!({"platform":"sensor","unique_id":"$deviceid-amount_unit","state_topic":"$this/amount_unit","name":"Amount unit","entity_category":"diagnostic"})),
            ("app_version", json!({"platform":"sensor","unique_id":"$deviceid-app_version","state_topic":"$this/app_version","name":"App version","entity_category":"diagnostic"})),
            ("data_refresh", json!({"platform":"sensor","unique_id":"$deviceid-data_refresh","state_topic":"$this/data_refresh","name":"Data refresh interval","entity_category":"diagnostic"})),
            ("water_usage_today", vol("water_usage_today", "Water dispensed")),
            ("water_usage_normal", vol("water_usage_normal", "Purified dispensed")),
            ("water_usage_cold", vol("water_usage_cold", "Cold water dispensed")),
            ("water_usage_hot", vol("water_usage_hot", "Hot water dispensed")),
            ("water_usage_sterilization", vol("water_usage_sterilization", "Rinse water dispensed")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.is_empty() || buf[0] != CLASS_TAG {
            return;
        }
        if self.process_usage(buf) {
            return;
        }
        if buf.len() < RECORD_LEN + 2 || (buf.len() - 2) % RECORD_LEN != 0 {
            return;
        }
        let record = buf.len() - RECORD_LEN;
        let rec = &buf[record..];
        let at = |o: usize| rec[o];

        self.core
            .publish_property("status", enum_of(&mon_status(), at(OFF_MON)).into());
        self.core
            .publish_property("cock_state", enum_of(&cock_state(), at(OFF_COCK)).into());
        self.core.publish_property(
            "water_selection",
            enum_of(&water_sel(), at(OFF_WATER)).into(),
        );
        self.core.publish_property(
            "sterilize_state",
            enum_of(&sterilize(), at(OFF_STER_STATE)).into(),
        );
        self.core.publish_property(
            "filter_flushing",
            enum_of(&filter_flush(), at(OFF_FILTER)).into(),
        );
        self.core.publish_property(
            "default_water",
            enum_of(&default_water(), at(OFF_DEFAULT_WATER)).into(),
        );
        self.core
            .publish_property("temp_unit", enum_of(&temp_unit(), at(OFF_TEMP_UNIT)).into());
        self.core.publish_property(
            "amount_unit",
            enum_of(&amount_unit(), at(OFF_AMT_UNIT)).into(),
        );

        let unit = amount_unit().get(&at(OFF_AMT_UNIT)).copied().unwrap_or("mL");
        let amount = water_amount().get(&at(OFF_AMOUNT)).copied();
        let water_amount_s = match amount {
            Some("Continuous") => "Continuous".into(),
            Some(n) => format!("{n} {unit}"),
            None => "Unknown".into(),
        };
        self.core
            .publish_property("water_amount", water_amount_s.into());
        let dflt = match at(OFF_DEFAULT_AMOUNT) {
            1 => "120 mL",
            2 => "250 mL",
            3 => "500 mL",
            _ => "Unknown",
        };
        self.core
            .publish_property("default_water_amount", dflt.into());

        if let Some(hot) = hot_temp().get(&at(OFF_HOT)) {
            self.core.publish_property("hot_water_temp", (*hot).into());
        } else {
            self.core.publish_property("hot_water_temp", "".into());
        }

        let bin = |raw: u8| match raw {
            0 => "OFF",
            1 => "ON",
            _ => "",
        };
        self.core
            .publish_property("button_sound", bin(at(OFF_BUTTON)).into());
        self.core
            .publish_property("auto_care", bin(at(OFF_AUTO_CARE)).into());
        self.core
            .publish_property("not_use_notice", bin(at(OFF_NOT_USE)).into());

        let month = at(OFF_STER_MON);
        let day = at(OFF_STER_DAY);
        let hour = at(OFF_STER_HOUR);
        let minute = at(OFF_STER_MIN);
        let set = ![month, day, hour, minute].contains(&IGNORE);
        if set {
            self.core.publish_property(
                "sterilize_reserved_at",
                format!("{month}.{day} {hour:02}:{minute:02}").into(),
            );
        } else {
            self.core
                .publish_property("sterilize_reserved_at", "Not set".into());
        }
        self.core
            .publish_property("app_version", (at(OFF_APP) as i64).into());
        self.core
            .publish_property("data_refresh", (at(OFF_REFRESH) as i64).into());
    }

    fn process_usage(&self, buf: &[u8]) -> bool {
        if buf.len() != USAGE_BODY_LEN || buf[1] != 0x1f || buf[2] != 0x00 {
            return false;
        }
        let normal = buf[3] as i64;
        let hot = be16(buf, 4) as i64;
        let cold = be16(buf, 6) as i64;
        let sterilization = be16(buf, 12) as i64;
        let total = normal + hot + cold + be16(buf, 8) as i64 + be16(buf, 10) as i64 + sterilization;
        if total == 0 {
            return true;
        }
        let mut u = self.usage.lock();
        u.total += total;
        u.normal += normal;
        u.cold += cold;
        u.hot += hot;
        u.sterilization += sterilization;
        self.core
            .publish_property("water_usage_today", u.total.into());
        self.core
            .publish_property("water_usage_normal", u.normal.into());
        self.core
            .publish_property("water_usage_cold", u.cold.into());
        self.core.publish_property("water_usage_hot", u.hot.into());
        self.core
            .publish_property("water_usage_sterilization", u.sterilization.into());
        true
    }

    fn set_field(&self, offset: usize, value: u8) {
        let mut record = vec![IGNORE; RECORD_LEN];
        record[offset] = value;
        let mut pkt = vec![0xf0, 0x17];
        pkt.extend(record);
        self.core.send(&pkt);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "default_water" => {
                if let Some((code, _)) = default_water().into_iter().find(|(_, l)| *l == mqtt_value) {
                    self.set_field(OFF_DEFAULT_WATER, code);
                }
            }
            "default_water_amount" => {
                let code = match mqtt_value {
                    "120 mL" => Some(1),
                    "250 mL" => Some(2),
                    "500 mL" => Some(3),
                    _ => None,
                };
                if let Some(c) = code {
                    self.set_field(OFF_DEFAULT_AMOUNT, c);
                }
            }
            "auto_care" => {
                if mqtt_value == "ON" || mqtt_value == "OFF" {
                    self.set_field(OFF_AUTO_CARE, u8::from(mqtt_value == "ON"));
                }
            }
            "not_use_notice" => {
                if mqtt_value == "ON" || mqtt_value == "OFF" {
                    self.set_field(OFF_NOT_USE, u8::from(mqtt_value == "ON"));
                }
            }
            _ => {}
        }
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
    fn set_property(&self, prop: &str, value: &str) {
        Device::set_property(self, prop, value);
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
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, wrap_aabb, Thinq2Device};

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("1WPU4CIGCR__2", Device::new)
    }

    const STATE_SHORT: &str = "aa2012eb020002010101ffff00ff010101ffff071c121eff01ff0300000177bb";

    fn with_byte(frame: &str, index: usize, value: u8) -> Vec<u8> {
        let mut p = hex_decode(frame);
        p[index] = value;
        // recompute checksum like wrap_aabb from inner
        let inner = p[2..p.len() - 2].to_vec();
        wrap_aabb(&inner)
    }

    #[test]
    fn config_has_status_and_usage() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        assert_eq!(comps["status"]["platform"], "sensor");
        assert!(comps.contains_key("water_usage_today"));
        assert!(comps.contains_key("default_water"));
        assert!(comps.contains_key("auto_care"));
    }

    #[test]
    fn real_state_frame_matches_lg_cloud() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE_SHORT));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "cock_state").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "water_selection").as_deref(), Some("Purified"));
        assert_eq!(prop(&ha, "water_amount").as_deref(), Some("120 mL"));
        assert_eq!(prop(&ha, "default_water").as_deref(), Some("Last used"));
        assert_eq!(prop(&ha, "default_water_amount").as_deref(), Some("120 mL"));
        assert_eq!(prop(&ha, "hot_water_temp").as_deref(), Some(""));
        assert_eq!(prop(&ha, "temp_unit").as_deref(), Some("°C"));
        assert_eq!(prop(&ha, "amount_unit").as_deref(), Some("mL"));
        assert_eq!(prop(&ha, "button_sound").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "auto_care").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "not_use_notice").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "sterilize_state").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "filter_flushing").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "app_version").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "data_refresh").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "sterilize_reserved_at").as_deref(), Some("7.28 18:30"));
    }

    #[test]
    fn oracle_single_byte_changes() {
        let cases = [
            (6usize, 3u8, "water_selection", "Cold water"),
            (8, 0, "temp_unit", "°F"),
            (14, 3, "default_water", "Cold water"),
            (27, 3, "sterilize_state", "Water line sterilizing"),
        ];
        for (idx, val, key, want) in cases {
            let (ha, thinq, _) = make();
            thinq.emit_data(&with_byte(STATE_SHORT, idx, val));
            assert_eq!(prop(&ha, key).as_deref(), Some(want), "{key}");
        }
        let (ha, thinq, _) = make();
        thinq.emit_data(&with_byte(STATE_SHORT, 10, 3));
        assert_eq!(prop(&ha, "hot_water_temp").as_deref(), Some("85"));
    }

    fn usage_frame(normal: u8, hot: u16, cold: u16, soda: u16, mineral: u16, ster: u16) -> Vec<u8> {
        let mut inner = vec![CLASS_TAG, 0x1f, 0x00, normal];
        inner.extend(hot.to_be_bytes());
        inner.extend(cold.to_be_bytes());
        inner.extend(soda.to_be_bytes());
        inner.extend(mineral.to_be_bytes());
        inner.extend(ster.to_be_bytes());
        wrap_aabb(&inner)
    }

    #[test]
    fn usage_deltas_accumulate() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&usage_frame(3, 1, 2, 0, 0, 4));
        assert_eq!(prop(&ha, "water_usage_normal").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "water_usage_hot").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "water_usage_cold").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "water_usage_sterilization").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "water_usage_today").as_deref(), Some("10"));
        thinq.emit_data(&usage_frame(5, 0, 0, 2, 1, 0));
        assert_eq!(prop(&ha, "water_usage_normal").as_deref(), Some("8"));
        assert_eq!(prop(&ha, "water_usage_today").as_deref(), Some("18"));
    }

    #[test]
    fn zero_usage_frame_changes_nothing() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&usage_frame(4, 0, 0, 0, 0, 0));
        assert_eq!(prop(&ha, "water_usage_today").as_deref(), Some("4"));
        thinq.emit_data(&usage_frame(0, 0, 0, 0, 0, 0));
        assert_eq!(prop(&ha, "water_usage_today").as_deref(), Some("4"));
    }

    #[test]
    fn usage_frame_does_not_clobber_state() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE_SHORT));
        thinq.emit_data(&usage_frame(0, 0, 5, 0, 0, 0));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "water_usage_cold").as_deref(), Some("5"));
    }

    #[test]
    fn default_water_write_touches_only_that_offset() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("default_water", "Cold water");
        let pkt = thinq.outbox();
        assert_eq!(pkt.len(), 1);
        let inner = &pkt[0][2..pkt[0].len() - 2];
        assert_eq!(inner[0], 0xf0);
        assert_eq!(inner[1], 0x17);
        let record = &inner[2..];
        assert_eq!(record.len(), RECORD_LEN);
        assert_eq!(record[OFF_DEFAULT_WATER], 3);
        assert!(record
            .iter()
            .enumerate()
            .all(|(i, b)| i == OFF_DEFAULT_WATER || *b == IGNORE));
    }

    #[test]
    fn read_only_writes_send_nothing() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("status", "Normal");
        dev.set_property("sterilize_reserved_at", "9.9 09:09");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn unset_reservation_reads_not_set() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&with_byte(STATE_SHORT, 19, IGNORE));
        assert_eq!(prop(&ha, "sterilize_reserved_at").as_deref(), Some("Not set"));
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("1WPU4CIGCR__2").is_some());
    }
}
