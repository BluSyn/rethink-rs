//! WLRGL5825F — LG LRGL5825F gas range (AABB, class `0x40`, 62-byte status, read-only).
//!
//! Port of upstream PR #273. Same 62-byte record size as `wlrel6323s`, with
//! current oven temperature at offset 8 and gas-range cook modes.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{hex_decode, HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x40;
const STATUS_RECORD_LENGTH: usize = 62;
const STATUS_QUERY: &str =
    "f0ed114101000000181a0207080c14191a1e262b30353a00000000000000000000000000";

const OVEN_STATE_OFFSET: usize = 0;
const OVEN_MODE_OFFSET: usize = 1;
const OVEN_SET_TEMPERATURE_OFFSET: usize = 6;
const OVEN_CURRENT_TEMPERATURE_OFFSET: usize = 8;
const OVEN_DOOR_FLAGS_OFFSET: usize = 11;
const OVEN_DOOR_OPEN_MASK: u8 = 0x08;
const COOKTOP_STATE_OFFSETS: &[usize] = &[36, 37, 38, 39];

const OVEN_STATES: &[(u8, &str)] = &[
    (0x00, "Initial"),
    (0x01, "Preheating"),
    (0x02, "Cooking"),
    (0x03, "Done"),
    (0x04, "Cooling"),
    (0x05, "Cleaning"),
    (0x06, "Cleaning Done"),
];

const OVEN_MODES: &[(u8, &str)] = &[
    (0x00, "None"),
    (0x01, "Bake"),
    (0x02, "Roast"),
    (0x03, "Convection Bake"),
    (0x04, "Convection Roast"),
    (0x05, "Crisp Convection"),
    (0x06, "Favorite"),
    (0x07, "Broil"),
    (0x08, "Warm"),
    (0x09, "Proof"),
    (0x0a, "Pizza"),
    (0x0b, "Slow Cook"),
    (0x0c, "Probe"),
    (0x0d, "Easy Clean"),
    (0x0e, "Speed Broil"),
    (0x0f, "Self Clean"),
    (0x10, "Speed Roast"),
    (0x11, "Air Fry"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, n)| *n).collect()
}

fn components() -> HashMap<String, Value> {
    [
        ("cooktop_status", json!({"platform":"binary_sensor","icon":"mdi:stove","unique_id":"$deviceid-cooktop_status","state_topic":"$this/cooktop_status","name":"Cooktop","payload_on":"ON","payload_off":"OFF"})),
        ("oven_status", json!({"platform":"sensor","device_class":"enum","icon":"mdi:stove","unique_id":"$deviceid-oven_status","state_topic":"$this/oven_status","name":"Oven Status","options":labels(OVEN_STATES)})),
        ("oven_mode", json!({"platform":"sensor","device_class":"enum","icon":"mdi:chef-hat","unique_id":"$deviceid-oven_mode","state_topic":"$this/oven_mode","name":"Oven Mode","options":labels(OVEN_MODES)})),
        ("oven_set_temperature", json!({"platform":"sensor","device_class":"temperature","icon":"mdi:thermometer-chevron-up","unique_id":"$deviceid-oven_set_temperature","state_topic":"$this/oven_set_temperature","name":"Oven Set Temperature","unit_of_measurement":"°C"})),
        ("oven_current_temperature", json!({"platform":"sensor","device_class":"temperature","icon":"mdi:thermometer","unique_id":"$deviceid-oven_current_temperature","state_topic":"$this/oven_current_temperature","name":"Oven Current Temperature","unit_of_measurement":"°C"})),
        ("oven_door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-oven_door","state_topic":"$this/oven_door","name":"Oven Door","payload_on":"ON","payload_off":"OFF"})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });
        let mut config = default_config(&meta, Some(json!({"name": "LG Gas Range"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                if let Some(cur) = process_common_status(inner, CLASS_BYTE, STATUS_RECORD_LENGTH) {
                    dev.publish_status(cur);
                }
            }
        }));
        this
    }

    fn publish_status(&self, cur: &[u8]) {
        self.core.publish_property(
            "oven_status",
            named(OVEN_STATES, cur[OVEN_STATE_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "oven_mode",
            named(OVEN_MODES, cur[OVEN_MODE_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        let set_temp = cur[OVEN_SET_TEMPERATURE_OFFSET];
        self.core.publish_property(
            "oven_set_temperature",
            if set_temp == 0 {
                "None".into()
            } else {
                PropertyValue::Int(i64::from(set_temp))
            },
        );
        let cur_temp = cur[OVEN_CURRENT_TEMPERATURE_OFFSET];
        self.core.publish_property(
            "oven_current_temperature",
            if cur_temp == 0 {
                "None".into()
            } else {
                PropertyValue::Int(i64::from(cur_temp))
            },
        );
        self.core.publish_on_off(
            "oven_door",
            cur[OVEN_DOOR_FLAGS_OFFSET] & OVEN_DOOR_OPEN_MASK != 0,
        );
        let cooktop_on = COOKTOP_STATE_OFFSETS.iter().any(|&off| cur[off] != 0);
        self.core.publish_on_off("cooktop_status", cooktop_on);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode(STATUS_QUERY));
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
    use rethink_core::{wrap_aabb, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_62_byte_oven_and_cooktop_status() {
        let meta = Metadata::new("WLRGL5825F", "WLRGL5825F", "1.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let _dev = Device::new(ha.clone(), thinq.clone(), meta);

        let mut payload = vec![0x40, 0xeb];
        let mut rec = vec![0u8; STATUS_RECORD_LENGTH];
        rec[OVEN_STATE_OFFSET] = 0x01; // Preheating
        rec[OVEN_MODE_OFFSET] = 0x01; // Bake
        rec[OVEN_SET_TEMPERATURE_OFFSET] = 175;
        rec[OVEN_CURRENT_TEMPERATURE_OFFSET] = 113;
        rec[OVEN_DOOR_FLAGS_OFFSET] = OVEN_DOOR_OPEN_MASK;
        rec[36] = 1;
        payload.extend_from_slice(&rec);
        thinq.emit_data(&wrap_aabb(&payload));

        assert_eq!(prop(&ha, "oven_status").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "oven_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "oven_set_temperature").as_deref(), Some("175"));
        assert_eq!(prop(&ha, "oven_current_temperature").as_deref(), Some("113"));
        assert_eq!(prop(&ha, "oven_door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("ON"));
    }
}
