//! RC90U2_WW — EU heat-pump dryer (RC9DN9029).
//!
//! 25-byte class `0x30` records, same AABB dialect as the US dryers. Also
//! serves RC90V9_WW and RC90V9_AUS. `auto_ack` stays off, matching the
//! upstream handler.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_status;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x30;
const STATUS_LENGTH: usize = 27;
const RECORD_MARKER: u8 = 0x19;

const STATE_OFF: u8 = 0x00;
const STATE_RUNNING: u8 = 2;
const STATE_PAUSED: u8 = 3;

const OPT1_RESERVATION: u8 = 0x01;
const OPT1_ANTI_CREASE: u8 = 0x02;
const OPT1_CHILD_LOCK: u8 = 0x10;
const OPT1_DAMP_DRY_BEEP: u8 = 0x40;
const OPT1_HAND_IRON: u8 = 0x80;

const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Ready"),
    (2, "Running"),
    (3, "Paused"),
    (4, "End"),
    (5, "Error"),
    (100, "Delayed"),
];

const COURSES: &[(u8, &str)] = &[
    (0x02, "Cotton Soft"),
    (0x04, "Duvet"),
    (0x05, "Easy Care"),
    (0x06, "Mixed Fabric"),
    (0x07, "Cotton"),
    (0x08, "Sports Wear"),
    (0x09, "Quick Dry"),
    (0x0a, "Delicate"),
    (0x0b, "Wool"),
    (0x0c, "Rack Dry"),
    (0x0d, "Cool Air"),
    (0x0e, "Warm Air"),
    (0x10, "Allergy Care"),
    (0x65, "Baby Wear"),
    (0x66, "Gym Clothes"),
    (0x67, "Blanket"),
    (0x68, "Blanket Refresh"),
    (0x69, "Rainy Season"),
    (0x6a, "Single Garments"),
    (0x6b, "Deodorization"),
    (0x6c, "Small Load"),
    (0x6d, "Lingerie"),
    (0x6e, "Easy Iron"),
    (0x6f, "Super Dry"),
    (0x70, "Economic Dry"),
    (0x71, "Big Size Item"),
    (0x72, "Minimize Wrinkles"),
    (0x73, "Shoes/Fabric Doll"),
    (0x74, "Full Size Load"),
];

const DRY_LEVELS: &[(u8, &str)] = &[(1, "Iron"), (3, "Cupboard"), (4, "Extra")];
const ECO_HYBRID: &[(u8, &str)] = &[(1, "Eco"), (2, "Normal"), (3, "Time")];

const ERRORS: &[(u8, &str)] = &[
    (0, "OK"),
    (1, "Temperature sensor error (tE1)"),
    (2, "Temperature sensor error (tE2)"),
    (7, "Condenser error (CE1)"),
    (13, "Drain motor error"),
    (14, "Empty water container"),
    (15, "Door open"),
    (17, "Filter missing"),
    (19, "Unknown error (F1)"),
    (20, "Motor error (LE2)"),
    (21, "Unknown error (AE)"),
    (30, "Motor error (LE1)"),
    (37, "Door sensor error (dE4)"),
];

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
        let mut config = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(status) = process_common_status(buf, CLASS_BYTE, STATUS_LENGTH) {
            self.process_status(status);
        }
    }

    fn process_status(&self, status: &[u8]) {
        if status.get(1) != Some(&RECORD_MARKER) || status.len() < 2 + 21 {
            return;
        }
        let rec = &status[2..];
        let state = rec[0];
        let is_off = state == STATE_OFF;
        let is_active = state == STATE_RUNNING || state == STATE_PAUSED;
        let error = rec[6];
        let option1 = rec[14];

        self.core.publish_on_off("power", !is_off);
        self.core.publish_property(
            "error_message",
            named(ERRORS, error).unwrap_or("None").into(),
        );
        self.core.publish_on_off("error", error != 0);
        self.core
            .publish_property("status", named(STATES, state).unwrap_or("None").into());
        // The sub-phase byte keeps a stale value while idle.
        let phase = if is_active { phase_label(rec[9]) } else { None };
        self.core
            .publish_property("phase", phase.unwrap_or("None").into());
        let course = if is_off {
            None
        } else {
            named(COURSES, rec[20]).or_else(|| named(COURSES, rec[5]))
        };
        self.core
            .publish_property("course", course.unwrap_or("None").into());
        let dry = if is_off {
            None
        } else {
            named(DRY_LEVELS, rec[7])
        };
        self.core
            .publish_property("dry_level", dry.unwrap_or("None").into());
        let eco = if is_off {
            None
        } else {
            named(ECO_HYBRID, rec[8])
        };
        self.core
            .publish_property("eco_hybrid", eco.unwrap_or("None").into());
        self.core.publish_property(
            "remaining_time",
            PropertyValue::Int(if is_off { 0 } else { minutes(rec[1], rec[2]) }),
        );
        self.core.publish_property(
            "initial_time",
            PropertyValue::Int(if is_off { 0 } else { minutes(rec[3], rec[4]) }),
        );
        self.core.publish_property(
            "reserve_time",
            PropertyValue::Int(if is_off { 0 } else { minutes(rec[12], rec[13]) }),
        );
        self.core
            .publish_on_off("delay", option1 & OPT1_RESERVATION != 0);
        self.core
            .publish_on_off("anti_crease", option1 & OPT1_ANTI_CREASE != 0);
        self.core
            .publish_on_off("child_lock", option1 & OPT1_CHILD_LOCK != 0);
        self.core
            .publish_on_off("damp_dry_beep", option1 & OPT1_DAMP_DRY_BEEP != 0);
        self.core
            .publish_on_off("hand_iron", option1 & OPT1_HAND_IRON != 0);
    }
}

fn minutes(hour: u8, min: u8) -> i64 {
    hour as i64 * 60 + min as i64
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn phase_label(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Detecting"),
        1 => Some("Steam"),
        2 | 3 | 4 => Some("Drying"),
        5 => Some("Cooling"),
        6 => Some("Anti-crease"),
        7 => Some("End"),
        _ => None,
    }
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn components() -> std::collections::HashMap<String, serde_json::Value> {
    let phase_options = [
        "Detecting",
        "Steam",
        "Drying",
        "Cooling",
        "Anti-crease",
        "End",
    ];
    [
        (
            "power".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "name": "Power",
                "icon": "mdi:tumble-dryer",
                "device_class": "running",
            }),
        ),
        (
            "status".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-status",
                "state_topic": "$this/status",
                "name": "Status",
                "icon": "mdi:state-machine",
                "device_class": "enum",
                "options": labels(STATES),
            }),
        ),
        (
            "phase".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-phase",
                "state_topic": "$this/phase",
                "name": "Phase",
                "icon": "mdi:progress-clock",
                "device_class": "enum",
                "options": phase_options,
            }),
        ),
        (
            "error".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-error",
                "state_topic": "$this/error",
                "name": "Error",
                "icon": "mdi:check-circle",
                "device_class": "problem",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "error_message".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-error-message",
                "state_topic": "$this/error_message",
                "name": "Error message",
                "icon": "mdi:alert-circle-outline",
                "device_class": "enum",
                "entity_category": "diagnostic",
                "options": labels(ERRORS),
            }),
        ),
        (
            "course".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-course",
                "state_topic": "$this/course",
                "name": "Course",
                "icon": "mdi:pin-outline",
                "device_class": "enum",
                "options": labels(COURSES),
            }),
        ),
        (
            "dry_level".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-dry_level",
                "state_topic": "$this/dry_level",
                "name": "Dry level",
                "icon": "mdi:water-percent",
                "device_class": "enum",
                "options": labels(DRY_LEVELS),
            }),
        ),
        (
            "eco_hybrid".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-eco_hybrid",
                "state_topic": "$this/eco_hybrid",
                "name": "Eco Hybrid",
                "icon": "mdi:leaf",
                "device_class": "enum",
                "options": labels(ECO_HYBRID),
            }),
        ),
        (
            "remaining_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-remaining_time",
                "state_topic": "$this/remaining_time",
                "name": "Remaining time",
                "icon": "mdi:timer-outline",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        ),
        (
            "initial_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-initial_time",
                "state_topic": "$this/initial_time",
                "name": "Initial time",
                "icon": "mdi:clock-outline",
                "device_class": "duration",
                "unit_of_measurement": "min",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "reserve_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-reserve_time",
                "state_topic": "$this/reserve_time",
                "name": "Delay end",
                "icon": "mdi:timer-sand",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        ),
        (
            "delay".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-delay",
                "state_topic": "$this/delay",
                "name": "Delay end set",
                "icon": "mdi:timer-sand",
            }),
        ),
        (
            "anti_crease".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-anti_crease",
                "state_topic": "$this/anti_crease",
                "name": "Anti crease",
                "icon": "mdi:iron-outline",
            }),
        ),
        (
            "child_lock".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-child_lock",
                "state_topic": "$this/child_lock",
                "name": "Child lock",
                "icon": "mdi:lock-outline",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "damp_dry_beep".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-damp_dry_beep",
                "state_topic": "$this/damp_dry_beep",
                "name": "Damp dry beep",
                "icon": "mdi:water-alert-outline",
            }),
        ),
        (
            "hand_iron".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-hand_iron",
                "state_topic": "$this/hand_iron",
                "name": "Hand iron",
                "icon": "mdi:iron",
            }),
        ),
    ]
    .into_iter()
    .collect()
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        request_status(&self.core);
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";

    fn meta_id(model: &str) -> Metadata {
        Metadata::new(model, model, "2.9.61")
    }

    fn make(model: &str) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta_id(model));
        let dev = Device::new(ha.clone(), thinq.clone(), meta_id(model));
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

    fn empty(ha: &MockHaConnection) -> bool {
        ha.device(DEVICE_ID).unwrap().properties.is_empty()
    }

    #[test]
    fn aliases_share_this_handler() {
        let a = crate::t2_factory("RC90U2_WW").unwrap();
        let b = crate::t2_factory("RC90V9_WW").unwrap();
        let c = crate::t2_factory("RC90V9_AUS").unwrap();
        assert!(std::ptr::fn_addr_eq(a, b));
        assert!(std::ptr::fn_addr_eq(a, c));
    }

    #[test]
    fn start_asks_for_a_status_snapshot() {
        let (_ha, thinq, dev) = make("RC90U2_WW");
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        let packet = thinq.outbox();
        assert_eq!(packet.len(), 1);
        assert_eq!(hex_encode(&packet[0]), "AA0EF0ED1121010000001800B5BB");
    }

    #[test]
    fn eb_ready_cotton() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "aa2130eb001901021e00000700030102000000000804000000000000006b00f1bb",
        ));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Ready".into());
        assert_eq!(prop(&ha, "phase"), "None".into());
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Eco".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(150));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "reserve_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "error"), "OFF".into());
        assert_eq!(prop(&ha, "error_message"), "OK".into());
        assert_eq!(prop(&ha, "delay"), "OFF".into());
        assert_eq!(prop(&ha, "anti_crease"), "OFF".into());
        assert_eq!(prop(&ha, "child_lock"), "OFF".into());
        assert_eq!(prop(&ha, "damp_dry_beep"), "OFF".into());
        assert_eq!(prop(&ha, "hand_iron"), "OFF".into());
    }

    #[test]
    fn power_off_clears_course_fields() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021E00000700030102000000000804000000000000006B00001900000000000000000000000000000804000000010000006B0004BB",
        ));
        assert_eq!(prop(&ha, "power"), "OFF".into());
        assert_eq!(prop(&ha, "status"), "Off".into());
        assert_eq!(prop(&ha, "course"), "None".into());
        assert_eq!(prop(&ha, "dry_level"), "None".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "None".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(0));
    }

    #[test]
    fn power_on_without_a_course() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001900000000000000000000000000000804000000010000006B00001901000000000000000000000000000804000000000000006B0071BB",
        ));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Ready".into());
        assert_eq!(prop(&ha, "course"), "None".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(0));
    }

    #[test]
    fn course_knob_mixed_and_duvet() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021E00000700030102000000000804000000000000006B00001901010A00000600030302000000000804000000000000006B003FBB",
        ));
        assert_eq!(prop(&ha, "course"), "Mixed Fabric".into());
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(70));

        thinq.emit_data(&hex_decode(
            "AA3C30EC001901010A00000600030302000000000804000000000000006B00001901023700000400000302000000000804000000000000006B002ABB",
        ));
        assert_eq!(prop(&ha, "course"), "Duvet".into());
        assert_eq!(prop(&ha, "dry_level"), "None".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(175));
    }

    #[test]
    fn timed_downloaded_and_cool_air() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901023700000400000302000000000004000000000000006B00001901030000001000000302000000000004000000000000006B003BBB",
        ));
        assert_eq!(prop(&ha, "course"), "Allergy Care".into());
        assert_eq!(prop(&ha, "dry_level"), "None".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(180));

        thinq.emit_data(&hex_decode(
            "AA3C30EC001901001E00000900000302000000000004000000000000006B00001901002700000100000304000000000004000000006B00006B008FBB",
        ));
        assert_eq!(prop(&ha, "course"), "Deodorization".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(39));

        thinq.emit_data(&hex_decode(
            "AA3C30EC001901010F00000E00000102000000000004000000000000006B00001901010000000D00000005000000000004000000000000006B001DBB",
        ));
        assert_eq!(prop(&ha, "course"), "Cool Air".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "None".into());
        assert_eq!(prop(&ha, "phase"), "None".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(60));
    }

    #[test]
    fn delay_end_sets_reserve_and_the_option_bit() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021E00000700030102000000000804000000000000006B00001901021E00000700030102030003000904000000000000006B00D0BB",
        ));
        assert_eq!(prop(&ha, "delay"), "ON".into());
        assert_eq!(prop(&ha, "reserve_time"), PropertyValue::Int(180));
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "anti_crease"), "OFF".into());
        assert_eq!(prop(&ha, "child_lock"), "OFF".into());
    }

    #[test]
    fn dry_level_iron() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021E00000700030102000000000804000000000000006B00001901011E00000700010302000000000804000000000000006B0028BB",
        ));
        assert_eq!(prop(&ha, "dry_level"), "Iron".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(90));
    }

    #[test]
    fn start_cool_air_publishes_cooling() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901001E00000D00000005000000000004000000000000006B00001902001E001E0D00000005000000000005000000010000006B00C0BB",
        ));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Running".into());
        assert_eq!(prop(&ha, "phase"), "Cooling".into());
        assert_eq!(prop(&ha, "course"), "Cool Air".into());
        assert_eq!(prop(&ha, "dry_level"), "None".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "None".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(30));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(30));
        assert_eq!(prop(&ha, "delay"), "OFF".into());
        assert_eq!(prop(&ha, "error"), "OFF".into());
    }

    #[test]
    fn serial_power_and_start_events_are_ignored() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3730310201534141333939333530303900007FD200008000000000000253414133393933343931320000A36700004000000000000FBB",
        ));
        thinq.emit_data(&hex_decode("AA07301900AFBB"));
        thinq.emit_data(&hex_decode("AA09307200C9004BBB"));
        assert!(empty(&ha));
    }

    #[test]
    fn wrong_length_ec_is_rejected() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode("aa0830ec0019010000bb"));
        assert!(empty(&ha));
    }

    #[test]
    fn rc90v9_cycle_and_child_lock() {
        let (ha, thinq, _dev) = make("RC90V9_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021400000200000302000000000804000000000000006B00001901001E00000900000302000000000804000000000000006B0038BB",
        ));
        assert_eq!(prop(&ha, "status"), "Ready".into());
        assert_eq!(prop(&ha, "course"), "Quick Dry".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(30));

        thinq.emit_data(&hex_decode(
            "AA3C30EC0019020224030007000401020000000000050000AD0100000071000019020224030007000401030000000000050000AE010000007100A5BB",
        ));
        assert_eq!(prop(&ha, "status"), "Running".into());
        assert_eq!(prop(&ha, "phase"), "Drying".into());
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "dry_level"), "Extra".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(156));

        thinq.emit_data(&hex_decode(
            "AA3C30EC00190200030300070004010400000000000500043F010000007100001902000203000700040105000000002005000444010000007100A8BB",
        ));
        assert_eq!(prop(&ha, "phase"), "Cooling".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(2));

        thinq.emit_data(&hex_decode(
            "AA3C30EC00190200010300070004010500000000200500044801000000710000190400010300070004010700000000000400044902000000710058BB",
        ));
        assert_eq!(prop(&ha, "status"), "End".into());

        thinq.emit_data(&hex_decode(
            "AA3C30EC001901000000000000000000000000000004000000000000007100001901000000000000000000000000001004700000000000007100F5BB",
        ));
        assert_eq!(prop(&ha, "child_lock"), "ON".into());
    }
}
