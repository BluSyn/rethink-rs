//! TLVE5BB top-load washer (LG WT21BWV6). Long AABB envelope, 50-byte records.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const RECORD_LEN: usize = 50;
const STATUS_REQUEST: &str = "F0ED112101000000180411120000";

const STATE: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Standby"),
    (27, "Standby"),
    (2, "Paused"),
    (28, "Paused"),
    (3, "Detecting"),
    (5, "Filling"),
    (6, "Detergent amount"),
    (7, "Delayed"),
    (8, "Soaking"),
    (9, "Pre-wash"),
    (11, "Washing"),
    (29, "Washing"),
    (12, "Rinsing"),
    (13, "Rinse hold"),
    (14, "Spinning"),
    (15, "Drying"),
    (16, "End"),
    (21, "Refreshing"),
    (23, "Error"),
    (34, "Audible diagnosis"),
    (35, "Auto DT open pause"),
    (36, "Confirm start for control"),
    (37, "Clothing recognition"),
    (38, "Detergent input"),
    (39, "Softener input"),
    (40, "Pollution detecting"),
    (41, "Tub cleaning"),
    (42, "End remote maintain on"),
    (43, "Steam"),
    (47, "Laundry care"),
    (48, "EZDispense cleaning"),
    (52, "Dry cooling"),
];

const SOIL: &[(u8, &str)] = &[
    (1, "Light"),
    (2, "Light/Normal"),
    (3, "Normal"),
    (4, "Normal/Heavy"),
    (5, "Heavy"),
    (6, "Pre-wash"),
    (7, "Soaking"),
    (8, "TurboWash"),
    (9, "Time save"),
    (10, "Intensive"),
];

const SPIN: &[(u8, &str)] = &[
    (13, "Low"),
    (14, "Medium"),
    (15, "High"),
    (16, "Extra High"),
    (17, "Dry Intensive"),
    (18, "Delicate"),
];

const TEMP: &[(u8, &str)] = &[(8, "Cold"), (9, "Warm"), (10, "Hot"), (12, "Cold/Hot")];

const BUZZER: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Low"),
    (2, "Medium"),
    (3, "High"),
    (4, "Very loud"),
];

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
        .unwrap_or("None")
}

fn u16_le(rec: &[u8], offset: usize) -> i64 {
    u16::from_le_bytes([rec[offset], rec[offset + 1]]) as i64
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });
        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        let mut components = Map::new();
        let add = |components: &mut Map<String, Value>, key: &str, body: Value| {
            components.insert(key.into(), body);
        };
        add(&mut components, "power", json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:washing-machine","device_class":"running"}));
        add(&mut components, "status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":labels(STATE)}));
        add(&mut components, "remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"}));
        add(&mut components, "initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"}));
        add(&mut components, "energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","unit_of_measurement":"Wh","state_class":"total_increasing"}));
        add(&mut components, "soil", json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:liquid-spot","device_class":"enum","options":labels(SOIL)}));
        add(&mut components, "temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer","device_class":"enum","options":labels(TEMP)}));
        add(&mut components, "spin", json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","device_class":"enum","options":labels(SPIN)}));
        add(&mut components, "rinses_remaining", json!({"platform":"sensor","unique_id":"$deviceid-rinses_remaining","state_topic":"$this/rinses_remaining","name":"Rinses remaining","icon":"mdi:water-sync","state_class":"measurement"}));
        add(&mut components, "tub_clean_count", json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since drum clean","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"}));
        add(&mut components, "buzzer", json!({"platform":"sensor","unique_id":"$deviceid-buzzer","state_topic":"$this/buzzer","name":"Buzzer","icon":"mdi:volume-high","device_class":"enum","options":labels(BUZZER),"entity_category":"diagnostic"}));
        add(&mut components, "lid_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-lid_lock","state_topic":"$this/lid_lock","name":"Lid lock","icon":"mdi:lock","entity_category":"diagnostic"}));
        base.components = components.into_iter().collect();
        core.set_config(base);

        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_enveloped_status(buf, CLASS_BYTE, RECORD_LEN) {
            self.process_status(rec);
        }
    }

    fn process_status(&self, rec: &[u8]) {
        if rec.len() < RECORD_LEN {
            return;
        }
        let off = rec[21] == 0;
        self.core.publish_on_off("power", !off);
        self.core
            .publish_property("status", named(STATE, rec[21]).into());
        self.core.publish_property(
            "remaining_time",
            if off { 0 } else { u16_le(rec, 14) }.into(),
        );
        self.core.publish_property(
            "initial_time",
            if off { 0 } else { u16_le(rec, 16) }.into(),
        );
        self.core
            .publish_property("energy", u16_le(rec, 18).into());
        self.core
            .publish_property("soil", named(SOIL, rec[1]).into());
        self.core
            .publish_property("temp", named(TEMP, rec[2]).into());
        self.core
            .publish_property("spin", named(SPIN, rec[4]).into());
        self.core
            .publish_property("rinses_remaining", i64::from(rec[3]).into());
        self.core
            .publish_property("tub_clean_count", i64::from(rec[28]).into());
        self.core
            .publish_property("buzzer", named(BUZZER, rec[29]).into());
        self.core.publish_on_off("lid_lock", rec[38] != 0);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode(STATUS_REQUEST));
    }
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const DEVICE_ID: &str = "test-id";
    const RUNNING: &str = "aa ff 20 0a 00 76 00 9c 86 00 01 00 ec 00 64 00 05 08 03 0e 09 00 00 09 06 00 00 00 00 57 00 6a 00 15 00 2e 0b 0b 6f 00 00 00 03 2d 04 00 00 00 00 00 00 00 10 01 00 30 00 00 00 00 00 00 00 00 00 00 05 08 03 0e 09 00 00 09 06 00 00 00 00 56 00 6a 00 17 00 2e 0b 0b 6f 00 00 00 03 2d 04 00 00 00 00 00 00 00 10 01 00 30 00 00 00 00 00 00 00 00 00 17 c9 bb";
    const END: &str = "aaff200a007600a0cd000100ec0064000508000e09000009060000000001006a0050002e0e0e6f000000002d040000000000000010010030000000000000000000000508000e09000009060000000001006a0055002e10106f000000002d0400000000000000000100300000000000000000001e67bb";
    const END_UNLOCKED: &str = "aaff200a007600a0d4000100ec0064000508000e09000009060000000001006a0055002e10106f000000002d040000000000000000010030000000000000000000000508000e09000009060000000001006a0055002e10106f000000002d0400000000000000000000300000000000000000003a09bb";
    const OFF: &str = "aaff200a007600a0dd000100ec0064000508000e09000009060000000001006a0055002e10106f000000002d040000000000000000000030000000000000000000000508000e09000009060000000001006a0055002e00006f000000002d04000000000000000000003000000000000000000033b6bb";
    const OFF_SNAPSHOT: &str = "aaff200a004400a0fa000100eb0032000508000e09000009060000000001006a0055002e00006f000000002d040000000000000000000030000000000000000000d5a3bb";

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn feed(frame: &str) -> std::sync::Arc<MockHaConnection> {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, Metadata::new("TLVE5BB", "TLVE5BB", "0.0.0"));
        let _dev = Device::new(ha.clone(), thinq.clone(), Metadata::new("TLVE5BB", "TLVE5BB", "0.0.0"));
        thinq.emit_data(&hex_decode(frame));
        ha
    }

    #[test]
    fn decodes_the_current_record_from_a_paired_running_update() {
        let ha = feed(RUNNING);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("86"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("106"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("23"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Heavy"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Cold"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "rinses_remaining").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("45"));
        assert_eq!(prop(&ha, "buzzer").as_deref(), Some("Very loud"));
        assert_eq!(prop(&ha, "lid_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn reports_the_real_end_of_cycle_transition() {
        let ha = feed(END);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("85"));
        assert_eq!(prop(&ha, "rinses_remaining").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "lid_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn tracks_the_lid_unlocking_while_the_washer_remains_in_end() {
        let ha = feed(END_UNLOCKED);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "lid_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn reports_off_and_clears_the_stale_remaining_minute() {
        let ha = feed(OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("85"));
        assert_eq!(prop(&ha, "lid_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn decodes_the_single_record_snapshot_sent_after_power_off() {
        let ha = feed(OFF_SNAPSHOT);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("85"));
        assert_eq!(prop(&ha, "lid_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn rejects_frames_whose_declared_record_length_does_not_match() {
        let mut malformed = hex_decode(RUNNING);
        malformed[13] = 0;
        malformed[14] = 99;
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, Metadata::new("TLVE5BB", "TLVE5BB", "0.0.0"));
        let _dev = Device::new(ha.clone(), thinq.clone(), Metadata::new("TLVE5BB", "TLVE5BB", "0.0.0"));
        thinq.emit_data(&malformed);
        assert!(prop(&ha, "status").is_none());
    }

    #[test]
    fn start_requests_status() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, Metadata::new("TLVE5BB", "TLVE5BB", "0.0.0"));
        let dev = Device::new(ha, thinq.clone(), Metadata::new("TLVE5BB", "TLVE5BB", "0.0.0"));
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            hex_encode(&rethink_core::device_base::wrap_aabb(&hex_decode(STATUS_REQUEST)))
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("TLVE5BB").is_some());
    }
}
