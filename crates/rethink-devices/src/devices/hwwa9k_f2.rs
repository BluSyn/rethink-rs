//! HWWA9K_F2 — LG CordZero A9 stick vacuum (AABB class 0xD2).
//!
//! Port of upstream PR #116. Offsets from single-byte injection vs LG cloud decode.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_TAG: u8 = 0xd2;
const RECORD_LEN: usize = 14;
const FIELD_COUNT: u8 = 12;

const OFF_MON: usize = 2;
const OFF_CLEAN: usize = 3;
const OFF_FILTER: usize = 4;
const OFF_PASSAGE: usize = 5;
const OFF_NOZZLE: usize = 6;
const OFF_BATTERY: usize = 7;
const OFF_MOP: usize = 8;
const OFF_COMPLETE: usize = 9;
const OFF_SUCTION: usize = 10;
const OFF_MELODY: usize = 11;
const OFF_VOLUME: usize = 12;
const OFF_BRIGHT: usize = 13;

const CTRL_MOP: u8 = 0x01;
const CTRL_SUCTION: u8 = 0x02;
const CTRL_MELODY: u8 = 0x03;
const CTRL_VOLUME: u8 = 0x04;
const CTRL_BRIGHT: u8 = 0x05;

fn mon_status() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "Standby"),
        (2, "Cleaning"),
        (3, "Charging"),
        (4, "Charging complete"),
    ])
}
fn clean_mode() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "Off"),
        (2, "Standard"),
        (3, "High"),
        (4, "Turbo"),
        (5, "Mop"),
        (6, "Auto"),
    ])
}
fn filter_state() -> HashMap<u8, &'static str> {
    HashMap::from([(1, "Normal"), (2, "Cleaning needed")])
}
fn passage() -> HashMap<u8, &'static str> {
    HashMap::from([(1, "Normal"), (2, "Check for debris")])
}
fn nozzle() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0, "Auxiliary inlet"),
        (1, "PowerDrive Floor"),
        (2, "PowerDrive Carpet"),
        (3, "PowerDrive Mop"),
    ])
}
fn battery() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0, "Off"),
        (1, "High"),
        (2, "Mid"),
        (3, "Low"),
        (4, "Warning"),
    ])
}
fn mop_with() -> HashMap<u8, &'static str> {
    HashMap::from([(1, "Mop only"), (2, "Mop and suction together")])
}
fn suction() -> HashMap<u8, &'static str> {
    HashMap::from([(1, "Standard"), (2, "High"), (3, "Turbo")])
}
fn melody() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "Lucky"),
        (2, "Marble"),
        (3, "Ice"),
        (4, "Breeze"),
        (5, "Nebula"),
    ])
}
fn volume() -> HashMap<u8, &'static str> {
    HashMap::from([(1, "High"), (2, "Normal"), (3, "Low")])
}
fn brightness() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "Very bright"),
        (2, "Bright"),
        (3, "Normal"),
        (4, "Dim"),
        (5, "Off"),
    ])
}

fn labels(m: &HashMap<u8, &'static str>) -> Vec<&'static str> {
    let mut v: Vec<_> = m.values().copied().collect();
    v.sort();
    v
}

fn enum_of(map: &HashMap<u8, &'static str>, raw: u8) -> String {
    map.get(&raw)
        .copied()
        .map(str::to_string)
        .unwrap_or_else(|| {
            if raw == 0 || raw == 0xff {
                "Unknown".into()
            } else {
                format!("Code {raw}")
            }
        })
}

fn code_of(map: &HashMap<u8, &'static str>, label: &str) -> Option<u8> {
    map.iter().find(|(_, v)| **v == label).map(|(k, _)| *k)
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });

        let mut base = default_config(&meta, Some(json!({"name": "LG Stick Vacuum"})));
        let suction_opts = labels(&suction());
        let mop_opts = labels(&mop_with());
        let melody_opts = labels(&melody());
        let vol_opts = labels(&volume());
        let bright_opts = labels(&brightness());
        base.components = [
            ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"State","icon":"mdi:robot-vacuum"})),
            ("clean_mode", json!({"platform":"sensor","unique_id":"$deviceid-clean_mode","state_topic":"$this/clean_mode","name":"Operation level","icon":"mdi:fan"})),
            ("suction_force", json!({"platform":"select","unique_id":"$deviceid-suction_force","state_topic":"$this/suction_force","command_topic":"$this/suction_force/set","name":"Default suction power","icon":"mdi:weather-windy","options":suction_opts})),
            ("battery", json!({"platform":"sensor","unique_id":"$deviceid-battery","state_topic":"$this/battery","name":"Battery","icon":"mdi:battery"})),
            ("nozzle", json!({"platform":"sensor","unique_id":"$deviceid-nozzle","state_topic":"$this/nozzle","name":"Attached nozzle","icon":"mdi:vacuum"})),
            ("filter_state", json!({"platform":"sensor","unique_id":"$deviceid-filter_state","state_topic":"$this/filter_state","name":"Filter state","icon":"mdi:air-filter"})),
            ("passage", json!({"platform":"sensor","unique_id":"$deviceid-passage","state_topic":"$this/passage","name":"Inlet blocked","icon":"mdi:pipe-disconnected"})),
            ("mop_with_sucking", json!({"platform":"select","unique_id":"$deviceid-mop_with_sucking","state_topic":"$this/mop_with_sucking","command_topic":"$this/mop_with_sucking/set","name":"Mop usage mode","icon":"mdi:water","options":mop_opts})),
            ("charging_melody", json!({"platform":"select","unique_id":"$deviceid-charging_melody","state_topic":"$this/charging_melody","command_topic":"$this/charging_melody/set","name":"Charging melody","icon":"mdi:music-note","entity_category":"config","options":melody_opts})),
            ("volume", json!({"platform":"select","unique_id":"$deviceid-volume","state_topic":"$this/volume","command_topic":"$this/volume/set","name":"Volume","icon":"mdi:volume-high","entity_category":"config","options":vol_opts})),
            ("brightness", json!({"platform":"select","unique_id":"$deviceid-brightness","state_topic":"$this/brightness","command_topic":"$this/brightness/set","name":"LED Brightness","icon":"mdi:brightness-6","entity_category":"config","options":bright_opts})),
            ("complete_clean", json!({"platform":"binary_sensor","unique_id":"$deviceid-complete-clean","state_topic":"$this/complete_clean","name":"Cleaning complete","icon":"mdi:check-circle-outline","payload_on":"ON","payload_off":"OFF"})),
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
        if buf.len() < RECORD_LEN || buf.len() % RECORD_LEN != 2 {
            return;
        }
        let record = buf.len() - RECORD_LEN;
        if buf[record + 1] != FIELD_COUNT {
            return;
        }
        let rec = &buf[record..];
        let at = |o: usize| rec[o];

        self.core
            .publish_property("status", enum_of(&mon_status(), at(OFF_MON)).into());
        self.core
            .publish_property("clean_mode", enum_of(&clean_mode(), at(OFF_CLEAN)).into());
        self.core
            .publish_property("battery", enum_of(&battery(), at(OFF_BATTERY)).into());
        self.core
            .publish_property("nozzle", enum_of(&nozzle(), at(OFF_NOZZLE)).into());
        self.core.publish_property(
            "filter_state",
            enum_of(&filter_state(), at(OFF_FILTER)).into(),
        );
        self.core
            .publish_property("passage", enum_of(&passage(), at(OFF_PASSAGE)).into());
        self.core
            .publish_on_off("complete_clean", at(OFF_COMPLETE) == 2);

        let opt = |prop: &str, map: HashMap<u8, &'static str>, raw: u8| {
            if let Some(l) = map.get(&raw) {
                self.core.publish_property(prop, (*l).into());
            }
        };
        opt("suction_force", suction(), at(OFF_SUCTION));
        opt("mop_with_sucking", mop_with(), at(OFF_MOP));
        opt("charging_melody", melody(), at(OFF_MELODY));
        opt("volume", volume(), at(OFF_VOLUME));
        opt("brightness", brightness(), at(OFF_BRIGHT));
    }

    fn set_field(&self, typ: u8, value: u8) {
        self.core.send(&[0xf0, 0x24, typ, 0x01, value]);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        let write = |typ: u8, map: HashMap<u8, &'static str>| {
            if let Some(code) = code_of(&map, mqtt_value) {
                self.set_field(typ, code);
                self.core.publish_property(prop, mqtt_value.into());
            }
        };
        match prop {
            "suction_force" => write(CTRL_SUCTION, suction()),
            "mop_with_sucking" => write(CTRL_MOP, mop_with()),
            "charging_melody" => write(CTRL_MELODY, melody()),
            "volume" => write(CTRL_VOLUME, volume()),
            "brightness" => write(CTRL_BRIGHT, brightness()),
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
    use rethink_core::{hex_decode, hex_encode, Thinq2Device};

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("HWWA9K_F2", Device::new)
    }

    const STATE: &str = "aa14d2eb000c04010101ff00010102010103c3bb";
    const MON_CHARGING: &str = "aa14d2eb000c03010101ff00010102010103c0bb";
    const SUCTION_TURBO: &str = "aa14d2eb000c04010101ff00010103010103c2bb";
    const BRIGHT_HIGH: &str = "aa14d2eb000c04010101ff00010102010102c0bb";
    const NOZZLE_MOP: &str = "aa14d2eb000c040101010300010102010103cfbb";
    const CLEAN_DONE: &str = "aa14d2eb000c04010101ff00010202010103c2bb";
    const ECHO_TWO: &str =
        "aa22d2ec000c04010101ff00020101030304000c04010101ff000201020303049ebb";
    const CAPABILITY: &str =
        "aaffd20a002c00005200010a85001a00040456432d31010456432d32010456432d33010456432d34014880bb";

    #[test]
    fn config_has_core_entities() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        for k in [
            "status",
            "clean_mode",
            "suction_force",
            "battery",
            "complete_clean",
        ] {
            assert!(comps.contains_key(k), "missing {k}");
        }
        assert_eq!(comps["suction_force"]["platform"], "select");
        assert!(comps["suction_force"].get("command_topic").is_some());
        assert!(comps["status"].get("command_topic").is_none());
    }

    #[test]
    fn captured_idle_frame_matches_lg_cloud() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Charging complete"));
        assert_eq!(prop(&ha, "clean_mode").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "filter_state").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "passage").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "battery").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "suction_force").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "charging_melody").as_deref(), Some("Lucky"));
        assert_eq!(prop(&ha, "volume").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "brightness").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "complete_clean").as_deref(), Some("OFF"));
    }

    #[test]
    fn single_byte_oracle_frames() {
        let cases = [
            (MON_CHARGING, "status", "Charging"),
            (SUCTION_TURBO, "suction_force", "Turbo"),
            (BRIGHT_HIGH, "brightness", "Bright"),
            (NOZZLE_MOP, "nozzle", "PowerDrive Mop"),
            (CLEAN_DONE, "complete_clean", "ON"),
        ];
        for (frame, key, want) in cases {
            let (ha, thinq, _) = make();
            thinq.emit_data(&hex_decode(frame));
            assert_eq!(prop(&ha, key).as_deref(), Some(want), "{key}");
        }
    }

    #[test]
    fn two_record_echo_reads_trailing_record() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(ECHO_TWO));
        assert_eq!(prop(&ha, "mop_with_sucking").as_deref(), Some("Mop and suction together"));
        assert_eq!(prop(&ha, "suction_force").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "charging_melody").as_deref(), Some("Ice"));
        assert_eq!(prop(&ha, "volume").as_deref(), Some("Low"));
        assert_eq!(prop(&ha, "brightness").as_deref(), Some("Dim"));
    }

    #[test]
    fn capability_frame_ignored() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE));
        thinq.emit_data(&hex_decode(CAPABILITY));
        assert_eq!(prop(&ha, "suction_force").as_deref(), Some("High"));
    }

    #[test]
    fn writes_match_lg_cloud_frames() {
        let cases = [
            ("mop_with_sucking", "Mop only", "AA09F0240101019FBB"),
            ("mop_with_sucking", "Mop and suction together", "AA09F0240101029EBB"),
            ("suction_force", "Standard", "AA09F0240201019EBB"),
            ("suction_force", "Turbo", "AA09F02402010398BB"),
            ("brightness", "Off", "AA09F02405010587BB"),
        ];
        for (prop_n, val, want) in cases {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property(prop_n, val);
            assert_eq!(hex_encode(&thinq.outbox()[0]), want, "{prop_n}={val}");
        }
    }

    #[test]
    fn unknown_write_sends_nothing() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("suction_force", "InvalidForce");
        dev.set_property("status", "Charging");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("HWWA9K_F2").is_some());
    }
}
