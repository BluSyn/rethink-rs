//! 2REF11EIDA__4 fridge — 68-byte status, F/C unit detection, flex drawer.

use crate::device_trait::DeviceHandler;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    TemperatureUnit,
};
use rethink_util::sync::Mutex;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use serde_json::json;
use std::sync::Arc;

const FLEX_OPTIONS: &[&str] = &[
    "Chilled Wine",
    "Deli/Snacks",
    "Cold Drink",
    "Meat/Seafood",
    "Freezer",
];

pub struct Device {
    core: Arc<AabbDeviceCore>,
    temperature_unit: Mutex<Option<TemperatureUnit>>,
    meta: Metadata,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core,
            temperature_unit: Mutex::new(None),
            meta,
        });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn set_temperature_unit(&self, unit: TemperatureUnit) {
        if *self.temperature_unit.lock() == Some(unit) {
            return;
        }
        *self.temperature_unit.lock() = Some(unit);
        let (fu, fmin, fmax) = fridge_range(unit);
        let (zu, zmin, zmax) = freezer_range(unit);
        let mut config = default_config(&self.meta, Some(json!({"name": "LG Fridge"})));
        config.components = [
            (
                "fridge_setpoint".into(),
                json!({
                    "platform": "number", "device_class": "temperature",
                    "unique_id": "$deviceid-fridge_setpoint",
                    "state_topic": "$this/fridge_setpoint",
                    "command_topic": "$this/fridge_setpoint/set",
                    "name": "Fridge temperature",
                    "unit_of_measurement": fu, "min": fmin, "max": fmax,
                }),
            ),
            (
                "freezer_setpoint".into(),
                json!({
                    "platform": "number", "device_class": "temperature",
                    "unique_id": "$deviceid-freezer_setpoint",
                    "state_topic": "$this/freezer_setpoint",
                    "command_topic": "$this/freezer_setpoint/set",
                    "name": "Freezer temperature",
                    "unit_of_measurement": zu, "min": zmin, "max": zmax,
                }),
            ),
            (
                "flex_setpoint".into(),
                json!({
                    "platform": "select", "icon": "mdi:thermometer",
                    "unique_id": "$deviceid-flex_setpoint",
                    "state_topic": "$this/flex_setpoint",
                    "command_topic": "$this/flex_setpoint/set",
                    "name": "Convertible",
                    "options": FLEX_OPTIONS,
                }),
            ),
            (
                "door".into(),
                json!({
                    "platform": "binary_sensor", "device_class": "door",
                    "unique_id": "$deviceid-door", "state_topic": "$this/door", "name": "Door",
                }),
            ),
        ]
        .into_iter()
        .collect();
        // Door is a binary_sensor above — automate on state open/closed.
        self.core.set_config(config);
    }

    fn process_status(&self, cur: &[u8]) {
        if cur.len() < 14 {
            return;
        }
        let unit = if cur[8] != 0 {
            TemperatureUnit::C
        } else {
            TemperatureUnit::F
        };
        self.set_temperature_unit(unit);
        let setpoint_fridge = convert_fridge_temperature(unit, cur[1] as i32);
        let setpoint_freezer = convert_freezer_temperature(unit, cur[2] as i32);
        let any_door_open = cur[7];
        let setpoint_flex = usize::from(cur[13])
            .checked_sub(1)
            .and_then(|index| FLEX_OPTIONS.get(index))
            .copied()
            .unwrap_or("None");
        self.core.publish_door_with_trigger(any_door_open == 1);
        self.core
            .publish_property("fridge_setpoint", PropertyValue::Int(setpoint_fridge as i64));
        self.core
            .publish_property("freezer_setpoint", PropertyValue::Int(setpoint_freezer as i64));
        self.core
            .publish_property("flex_setpoint", setpoint_flex.into());
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 2 + 68 * 2 && buf[0] == 0x10 && buf[1] == 0xec {
            self.process_status(&buf[2 + 68..2 + 68 + 68]);
        }
        // Note: TS has a bug `subarray(2, 2 + 68 + 68)` but only 68-byte buffer for 10EB;
        // processStatus only reads first 14+ bytes so we take 68 correctly.
        if buf.len() == 2 + 68 && buf[0] == 0x10 && buf[1] == 0xeb {
            self.process_status(&buf[2..2 + 68]);
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        crate::devices::washer_ctrl::request_fridge_status(&self.core);
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, mqtt_value: &str) {
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::C);
        let mut base = rethink_util::hex::decode(
            "F017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        )
        .unwrap();
        base[2 + 8] = if unit == TemperatureUnit::C { 1 } else { 0 };
        match prop {
            "fridge_setpoint" => {
                let n: i32 = mqtt_value.parse().unwrap_or(0);
                base[2 + 1] = convert_fridge_temperature(unit, n) as u8;
                self.core.send(&base);
            }
            "freezer_setpoint" => {
                let n: i32 = mqtt_value.parse().unwrap_or(0);
                base[2 + 2] = convert_freezer_temperature(unit, n) as u8;
                self.core.send(&base);
            }
            "flex_setpoint" => {
                if let Some(index) = FLEX_OPTIONS.iter().position(|o| *o == mqtt_value) {
                    base[2 + 13] = (1 + index) as u8;
                    self.core.send(&base);
                }
            }
            _ => eprintln!("Unknown property {prop}"),
        }
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
    use rethink_core::ha::DeviceDiscovery;
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DEVICE_ID: &str = "test-id";
    // fridge 35F, freezer 0F, flex Cold Drink, doors closed
    const SAMPLE_INITIAL: &str = "AA4A10EB0209060202020400000001FFFF0300FFFF00FFFFFFFFFFFFFF020001010100000102FF6161FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000079BB";
    // cur[7] = 1: door open
    const SAMPLE_DELTA_DOOR_OPEN: &str = "AA8E10EC0209060202020400000001FFFF0300FFFF00FFFFFFFFFFFFFF020001010100000102FF6161FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000209060202020401000001FFFF0300FFFF00FFFFFFFFFFFFFF020001010100000102FF6161FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF0000FABB";
    // cur[1] = 2: fridge 42F
    const SAMPLE_DELTA_FRIDGE_42F: &str = "AA8E10EC0201060202020401000001FFFF0300FFFF00FFFFFFFFFFFFFF020001010100000102FF6161FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000202060202020401000001FFFF0300FFFF00FFFFFFFFFFFFFF020001010100000102FF6161FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF0000F4BB";
    // cur[13] = 5: convertible drawer set to Freezer
    const SAMPLE_DELTA_FLEX_FREEZER: &str = "AA8E10EC0209060202020401000001FFFF0300FFFF00FFFFFFFFFFFFFF020001010100000102FF6161FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000209060202020401000001FFFF0500FFFF00FFFFFFFFFFFFFF020001010100000102FF6161FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF0000E7BB";

    fn meta() -> Metadata {
        Metadata::new("2REF11EIDA__4", "2REF11EIDA__4", "1.0")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta()))
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?
            .properties
            .get(name)
            .map(|p| p.as_string())
    }

    /// SAMPLE_INITIAL with one status byte replaced.
    fn initial_with(index: usize, value: u8) -> Vec<u8> {
        let frame = hex_decode(SAMPLE_INITIAL);
        let mut inner = frame[2..frame.len() - 2].to_vec();
        inner[2 + index] = value;
        wrap_aabb(&inner)
    }

    /// Counts every property publish that reaches HA.
    struct CountingHa {
        inner: Arc<MockHaConnection>,
        publishes: AtomicUsize,
    }

    impl HaConnection for CountingHa {
        fn publish_config(&self, id: &str, config: &DeviceDiscovery) {
            self.inner.publish_config(id, config);
        }
        fn publish_property(&self, id: &str, property: &str, value: PropertyValue) {
            self.publishes.fetch_add(1, Ordering::SeqCst);
            self.inner.publish_property(id, property, value);
        }
        fn publish_event(&self, id: &str, topic_suffix: &str, payload: &str) {
            self.inner.publish_event(id, topic_suffix, payload);
        }
        fn is_connected(&self) -> bool {
            true
        }
    }

    #[test]
    fn config_waits_for_a_status_frame() {
        let (ha, _, _) = make();
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn initial_status_publishes_fahrenheit_config_and_setpoints() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], json!("°F"));
        assert_eq!(comps["fridge_setpoint"]["min"], json!(33));
        assert_eq!(comps["fridge_setpoint"]["max"], json!(43));
        assert_eq!(
            comps["freezer_setpoint"]["unit_of_measurement"],
            json!("°F")
        );
        assert_eq!(comps["freezer_setpoint"]["min"], json!(-7));
        assert_eq!(comps["freezer_setpoint"]["max"], json!(5));
        assert_eq!(
            comps["flex_setpoint"]["options"],
            json!([
                "Chilled Wine",
                "Deli/Snacks",
                "Cold Drink",
                "Meat/Seafood",
                "Freezer"
            ])
        );
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("35"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "flex_setpoint").as_deref(), Some("Cold Drink"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    #[test]
    fn door_open_delta_publishes_door_on() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DELTA_DOOR_OPEN));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("35"));
    }

    #[test]
    fn fridge_42f_delta_decodes() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DELTA_FRIDGE_42F));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("42"));
    }

    #[test]
    fn convertible_freezer_delta_publishes_flex_setpoint() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DELTA_FLEX_FREEZER));
        assert_eq!(prop(&ha, "flex_setpoint").as_deref(), Some("Freezer"));
    }

    #[test]
    fn unknown_convertible_code_publishes_none() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        thinq.emit_data(&initial_with(13, 6));
        assert_eq!(prop(&ha, "flex_setpoint").as_deref(), Some("None"));
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        thinq.emit_data(&initial_with(13, 0));
        assert_eq!(prop(&ha, "flex_setpoint").as_deref(), Some("None"));
    }

    #[test]
    fn unchanged_status_is_not_republished() {
        let ha = Arc::new(CountingHa {
            inner: MockHaConnection::new(),
            publishes: AtomicUsize::new(0),
        });
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let _dev = Device::new(ha.clone(), thinq.clone(), meta());
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        ha.publishes.store(0, Ordering::SeqCst);
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        assert_eq!(ha.publishes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn frames_outside_the_aa_bb_envelope_are_ignored() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("001122"));
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn unrecognised_inner_shape_is_ignored() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA08109901020304BB"));
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn start_sends_the_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1211010000010400EBBB"
        );
    }

    #[test]
    fn write_fridge_42f() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "42");
        let pkt = thinq.outbox()[0].clone();
        assert_eq!(pkt[2], 0xf0);
        assert_eq!(pkt[3], 0x17);
        assert_eq!(pkt[5], 2);
        assert_eq!(pkt[2 + 2 + 8], 0);
    }

    #[test]
    fn write_freezer_minus_5f() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-5");
        let pkt = thinq.outbox()[0].clone();
        assert_eq!(pkt[2], 0xf0);
        assert_eq!(pkt[3], 0x17);
        assert_eq!(pkt[2 + 2 + 2], 11);
    }

    #[test]
    fn write_flex_freezer() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        thinq.reset_recorder();
        dev.set_property("flex_setpoint", "Freezer");
        assert_eq!(thinq.outbox()[0][2 + 2 + 13], 5);
    }

    #[test]
    fn write_unknown_flex_value_sends_nothing() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        thinq.reset_recorder();
        dev.set_property("flex_setpoint", "NotARealOption");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn write_unknown_property_sends_nothing() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        thinq.reset_recorder();
        dev.set_property("does-not-exist", "1");
        assert!(thinq.outbox().is_empty());
    }
}
