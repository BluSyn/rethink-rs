//! RV13U6AM8W_D_US_WIFI dryer (AABB).

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

fn map_status() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x01, "Starting"),
        (0x03, "Paused"),
        (0x32, "Drying"),
        (0x33, "Cooldown"),
        (0x04, "Finishing"),
    ])
}
fn map_cycles() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x01, "Heavy Duty"),
        (0x03, "Normal"),
        (0x04, "Perm. Press"),
        (0x05, "Delicates"),
        (0x07, "Bedding"),
        (0x10, "Speed Dry"),
        (0x11, "Air Dry"),
        (0x12, "Manual"),
    ])
}
fn map_temps() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x01, "Ultra Low"),
        (0x02, "Low"),
        (0x03, "Medium"),
        (0x04, "Med High"),
        (0x05, "High"),
    ])
}
fn map_dry() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "None"),
        (0x01, "Damp"),
        (0x02, "Less"),
        (0x03, "Normal"),
        (0x04, "More"),
        (0x05, "Very"),
    ])
}

fn unique_values(m: &HashMap<u8, &'static str>, order: &[u8]) -> Vec<&'static str> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for k in order {
        if let Some(v) = m.get(k) {
            if seen.insert(*v) {
                out.push(*v);
            }
        }
    }
    out
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });

        let status = map_status();
        let cycles = map_cycles();
        let temps = map_temps();
        let dry = map_dry();

        let mut base = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        let mut components = Map::new();
        components.insert(
            "status".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-status",
                "state_topic": "$this/status",
                "name": "Status",
                "icon": "mdi:state-machine",
                "device_class": "enum",
                "options": unique_values(&status, &[0x00,0x01,0x03,0x32,0x33,0x04]),
            }),
        );
        components.insert(
            "remaining_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-remaining_time",
                "state_topic": "$this/remaining_time",
                "name": "Remaining time",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        );
        components.insert(
            "power".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "name": "Power",
                "icon": "mdi:tumble-dryer",
                "device_class": "running",
            }),
        );
        components.insert(
            "drum_running".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-drum_running",
                "state_topic": "$this/drum_running",
                "name": "Drum running",
                "icon": "mdi:rotate-3d-variant",
            }),
        );
        components.insert(
            "cycle".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-cycle",
                "state_topic": "$this/cycle",
                "name": "Cycle",
                "icon": "mdi:tumble-dryer",
                "device_class": "enum",
                "options": unique_values(&cycles, &[0x01,0x03,0x04,0x05,0x07,0x10,0x11,0x12]),
            }),
        );
        components.insert(
            "temp".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-temp",
                "state_topic": "$this/temp",
                "name": "Temperature",
                "icon": "mdi:thermometer",
                "device_class": "enum",
                "options": unique_values(&temps, &[0x00,0x01,0x02,0x03,0x04,0x05]),
            }),
        );
        components.insert(
            "dry_level".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-dry_level",
                "state_topic": "$this/dry_level",
                "name": "Dry level",
                "icon": "mdi:water-percent",
                "device_class": "enum",
                "options": unique_values(&dry, &[0x00,0x01,0x02,0x03,0x04,0x05]),
            }),
        );
        base.components = components.into_iter().collect();
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_record(&self, rec: &[u8]) {
        if rec.len() < 18 {
            return;
        }
        let phase = rec[2];
        let mins = rec[4] as i64;
        self.core.publish_property(
            "status",
            map_status().get(&phase).copied().unwrap_or("unknown").into(),
        );
        self.core.publish_property("remaining_time", mins.into());
        self.core.publish_on_off("power", phase != 0);
        self.core.publish_on_off("drum_running", rec[17] == 0xa9);
        self.core.publish_property(
            "cycle",
            map_cycles()
                .get(&rec[7])
                .copied()
                .unwrap_or("unknown")
                .into(),
        );
        self.core.publish_property(
            "temp",
            map_temps()
                .get(&rec[10])
                .copied()
                .unwrap_or("unknown")
                .into(),
        );
        self.core.publish_property(
            "dry_level",
            map_dry().get(&rec[9]).copied().unwrap_or("unknown").into(),
        );
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.is_empty() || buf[0] != 0x30 {
            return;
        }
        if buf.len() > 1 && buf[1] == 0xec && buf.len() == 60 {
            self.process_record(&buf[2..31]);
        } else if buf.len() > 1 && buf[1] == 0xeb && buf.len() == 31 {
            self.process_record(&buf[2..31]);
        }
    }

    pub fn set_property(&self, _prop: &str, _mqtt_value: &str) {}
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
    use rethink_core::{Thinq2Device, hex_decode, MockHaConnection, MockThinq2Device, Metadata};

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata { Metadata::new("RV13U6AM8W_D_US_WIFI", "LG DLE7300WE", "1.0") }
    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    #[test]
    fn config_and_status() {
        let (ha, thinq, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        for c in ["status","remaining_time","power","drum_running","cycle","temp","dry_level"] {
            assert!(comps.contains_key(c), "missing {c}");
        }
        thinq.emit_data(&hex_decode("AA2330EB000000000000000000000000000000000000000000000000000000000000BB"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));

        thinq.emit_data(&hex_decode("AA2330EB000032002D00000000000000000000000000000000000000000000000000BB"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("45"));

        thinq.emit_data(&hex_decode(
            "AA4030EC001B320036003601000305000100000000A90000000100000064000000001B320035003601000305000100000000A90000530100000064000000AFBB"
        ));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("54"));
        assert_eq!(prop(&ha, "cycle").as_deref(), Some("Heavy Duty"));
        assert_eq!(prop(&ha, "drum_running").as_deref(), Some("ON"));
    }
}
