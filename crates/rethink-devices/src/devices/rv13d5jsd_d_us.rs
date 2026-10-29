//! RV13D5JSD_D_US — LG DLEX4200B electric dryer.
//!
//! 29-byte class `0x30` records, same framing as the RV13B6 dryers.
//! `auto_ack` is on. Completion stays latched through the automatic power-off.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x30;
const STATUS_LENGTH: usize = 29;
const STATUS_REQUEST: &[u8] = &[0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x00];
const POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];

const PHASE_OFF: u8 = 0x00;
const PHASE_END: u8 = 0x04;
const PHASE_WRINKLE: u8 = 0x38;

const STATUS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Initial"),
    (0x03, "Pause"),
    (0x04, "End"),
    (0x32, "Drying"),
    (0x33, "Cooling"),
    (0x38, "Wrinkle Care"),
];
const COURSES: &[(u8, &str)] = &[
    (0x01, "Heavy Duty"),
    (0x02, "Towels"),
    (0x03, "Normal"),
    (0x04, "Perm Press"),
    (0x05, "Delicates"),
    (0x07, "Bedding"),
    (0x08, "Antibacterial"),
    (0x09, "Small Load"),
    (0x0b, "Sportswear"),
    (0x10, "Speed Dry"),
    (0x11, "Air Dry"),
    (0x12, "Time Dry"),
    (0x15, "Steam Fresh"),
    (0x16, "Steam Sanitary"),
];
const DRY_LEVELS: &[(u8, &str)] = &[(1, "Damp"), (2, "Less"), (3, "Normal"), (4, "More"), (5, "Very")];
const TEMPS: &[(u8, &str)] = &[
    (1, "Ultra Low"),
    (2, "Low"),
    (3, "Medium"),
    (4, "Mid High"),
    (5, "High"),
];
const SIGNALS: &[(u8, &str)] = &[(0, "Off"), (1, "Low"), (4, "High")];

pub struct Device {
    core: Arc<AabbDeviceCore>,
    dry_completed_published: Mutex<bool>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            dry_completed_published: Mutex::new(false),
        });
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
        if status.len() < 2 || status[1] != 0x1b {
            return;
        }
        let rec = &status[1..];
        let phase = rec[1];
        let is_off = phase == PHASE_OFF;
        let cycle_complete = phase == PHASE_END || phase == PHASE_WRINKLE;
        self.core.publish_on_off("power", !is_off);
        self.core
            .publish_property("status", named(STATUS, phase).unwrap_or("None").into());
        self.core.publish_property(
            "course",
            if is_off {
                "None"
            } else {
                named(COURSES, rec[6]).unwrap_or("None")
            }
            .into(),
        );
        let remain = if is_off || cycle_complete {
            0
        } else {
            rec[2] as i64 * 60 + rec[3] as i64
        };
        let initial = if is_off || cycle_complete {
            0
        } else {
            rec[4] as i64 * 60 + rec[5] as i64
        };
        self.core
            .publish_property("remaining_time", PropertyValue::Int(remain));
        self.core
            .publish_property("initial_time", PropertyValue::Int(initial));
        self.core.publish_property(
            "dry_level",
            named(DRY_LEVELS, rec[8]).unwrap_or("None").into(),
        );
        self.core
            .publish_property("temp", named(TEMPS, rec[9]).unwrap_or("None").into());
        self.core.publish_property(
            "signal",
            named(SIGNALS, rec[11]).unwrap_or("None").into(),
        );
        let adjust = if is_off { 0 } else { rec[12] as i8 as i64 };
        self.core
            .publish_property("more_less_time", PropertyValue::Int(adjust));
        self.core
            .publish_property("load_item", PropertyValue::Int(rec[23] as i64));
        let time_dry = if rec[6] == 0x12 && (1..=5).contains(&rec[10]) {
            (rec[10] as i64 + 1) * 10
        } else {
            0
        };
        self.core
            .publish_property("time_dry_setting", PropertyValue::Int(time_dry));
        let flags = rec[15];
        self.core.publish_on_off("reduce_static", flags & 0x02 != 0);
        self.core.publish_on_off("damp_dry_signal", flags & 0x08 != 0);
        self.core.publish_on_off("wrinkle_care", flags & 0x10 != 0);
        let opt2 = rec[16];
        self.core.publish_on_off("energy_saver", opt2 & 0x02 != 0);
        self.core.publish_on_off("turbo_steam", opt2 & 0x04 != 0);
        if cycle_complete {
            self.core.publish_on_off("dry_completed", true);
            *self.dry_completed_published.lock() = true;
        } else if !is_off || !*self.dry_completed_published.lock() {
            self.core.publish_on_off("dry_completed", false);
            *self.dry_completed_published.lock() = true;
        }
    }
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}
fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, n)| *n).collect()
}

fn components() -> std::collections::HashMap<String, serde_json::Value> {
    [
        ("power".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:tumble-dryer","device_class":"running"})),
        ("power_off".into(), json!({"platform":"button","unique_id":"$deviceid-power_off","command_topic":"$this/power_off/set","payload_press":"PRESS","name":"Power off","icon":"mdi:power-off","availability":[{"topic":"$this/power","payload_available":"ON","payload_not_available":"OFF"}]})),
        ("status".into(), json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Run state","icon":"mdi:state-machine","device_class":"enum","options":labels(STATUS)})),
        ("dry_completed".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-dry_completed","state_topic":"$this/dry_completed","name":"Dry completed","icon":"mdi:check-circle"})),
        ("course".into(), json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Current course","icon":"mdi:format-list-bulleted"})),
        ("remaining_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
        ("dry_level".into(), json!({"platform":"sensor","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","name":"Dry level","icon":"mdi:water-percent","device_class":"enum","options":labels(DRY_LEVELS)})),
        ("temp".into(), json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer","device_class":"enum","options":labels(TEMPS)})),
        ("time_dry_setting".into(), json!({"platform":"sensor","unique_id":"$deviceid-time_dry_setting","state_topic":"$this/time_dry_setting","name":"Time Dry setting","icon":"mdi:timer-cog-outline","unit_of_measurement":"min","entity_category":"diagnostic"})),
        ("more_less_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-more_less_time","state_topic":"$this/more_less_time","name":"More/Less time","icon":"mdi:plus-minus-variant","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
        ("signal".into(), json!({"platform":"sensor","unique_id":"$deviceid-signal","state_topic":"$this/signal","name":"Signal","icon":"mdi:bell-outline","device_class":"enum","options":labels(SIGNALS),"entity_category":"diagnostic"})),
        ("reduce_static".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-reduce_static","state_topic":"$this/reduce_static","name":"Reduce static","icon":"mdi:flash-off-outline"})),
        ("damp_dry_signal".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-damp_dry_signal","state_topic":"$this/damp_dry_signal","name":"Damp Dry Signal","icon":"mdi:water-alert-outline"})),
        ("energy_saver".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-energy_saver","state_topic":"$this/energy_saver","name":"Energy Saver","icon":"mdi:leaf"})),
        ("turbo_steam".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-turbo_steam","state_topic":"$this/turbo_steam","name":"TurboSteam","icon":"mdi:kettle-steam"})),
        ("wrinkle_care".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Wrinkle Care","icon":"mdi:tshirt-crew-outline"})),
        ("load_item".into(), json!({"platform":"sensor","unique_id":"$deviceid-load_item","state_topic":"$this/load_item","name":"Load items","icon":"mdi:tshirt-crew-outline","entity_category":"diagnostic"})),
    ]
    .into_iter()
    .collect()
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(STATUS_REQUEST);
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        if prop == "power_off" && value == "PRESS" {
            self.core.send(POWER_OFF);
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const EB_OFF: &str = "AA2330EB001B000039003900000000000400000000A800000001000000CA000000B9BB";
    const NORMAL: &str = "AA4030EC001B010001000100000000000400000040A800000000000000CA000000001B010039003903000304000400000040AA00000000000000CA0000007FBB";
    const SMALL_LOAD: &str = "AA4030EC001B010037003707000303000400000040A800000000000000CA000000001B01001E001E09000305000400000040A800000000000000CA00000027BB";
    const SPORTS: &str = "AA4030EC001B010019001910000005000400000040A800000000000000CA000000001B01001B001B0B000303000400000040A800000000000000CA0000006DBB";
    const ANTI: &str = "AA4030EC001B01001E001E09000305000400000040A800000000000000CA000000001B01010A010A08000505000400000040A800000000000000CA0000004ABB";
    const TIME60: &str = "AA4030EC001B010028002812000003030400000040A800000000000002CA000000001B010100010012000005050400000040A800000000000000CA00000067BB";
    const TIME20: &str = "AA4030EC001B010100010012000005050400000040A800000000000000CA000000001B010014001412000005010400000040A800000000000000CA0000005DBB";
    const ULTRA: &str = "AA4030EC001B010028002812000005030400000040A800000000000000CA000000001B010028002812000001030400000040A800000000000000CA0000002FBB";
    const LESS: &str = "AA4030EC001B010014001412000005010400000040A800000000000000CA000000001B01000F000F120000050104FB000040A800000000000000CA0000004EBB";
    const MORE: &str = "AA4030EC001B01000F000F120000050104FB000040A800000000000000CA000000001B010014001412000005010400000040A800000000000000CA0000004EBB";
    const SIG_OFF: &str = "AA4030EC001B010014001412000005010400000040A800000000000000CA000000001B010014001412000005010000000040A800000000000000CA00000073BB";
    const SIG_HIGH: &str = "AA4030EC001B010014001412000005010000000040A800000000000000CA000000001B010014001412000005010400000040A800000000000000CA00000073BB";
    const WRINKLE_ON: &str = "AA4030EC001B010014001412000005010400000040A800000000000000CA000000001B010014001412000005010400000050A800000000000000CA0000006FBB";
    const DAMP_ON: &str = "AA4030EC001B010036003601000305000400000040A800000000000000CA000000001B010036003601000305000400000048A800000000000000CA000000C9BB";
    const STATIC_ON: &str = "AA4030EC001B010036003601000305000400000040A800000000000000CA000000001B010034003401000305000400000042A800000000000005CA000000C2BB";
    const TURBO_ON: &str = "AA4030EC001B010036003601000305000400000040A800000000000002CA000000001B010036003601000305000400000040AC00000000000000CA000000CFBB";
    const DRYING: &str = "AA4030EC001B000001000100000000000400000040A80001A738000000CA000000001B320039003903000304000400000000AB00000001000000CA000000A9BB";
    const COOLING: &str = "AA4030EC001B320006010303000304000400000000AB00068801000000CA000000001B330005010203000304000400000000AB0006B032000000CA00000065BB";
    const END: &str = "AA4030EC001B330001010903000304000400000000AB0006D632000000CA000000001B040001010903000304000400000040AA0006D633000000CA000000BEBB";
    const WRINKLE_RUN: &str = "AA4030EC001B040001000A15000004000400000050A80001A733000002CA000000001B380001000115000004000400000050A90001A704000002CA0000009ABB";
    const SETTLES: &str = "AA4030EC001B040001010903000304000400000040AA0006D633000000CA000000001B000001000100000000000400000040A80006D604000000CA000000E1BB";
    const IDENTITY: &str = "AA3730310201534141333839363434333200006322000080000000000002534141333839363433313400001FF2000080000000000081BB";

    fn meta() -> Metadata {
        Metadata::new("RV13D5JSD_D_US", "RV13D5JSD_D_US", "0.0.0")
    }
    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn feed(frames: &[&str]) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        ha
    }
    fn prop(ha: &MockHaConnection, name: &str) -> String {
        ha.device(ID).unwrap().properties.get(name).unwrap().as_string()
    }

    #[test]
    fn identity_is_acked_and_status_is_not() {
        let (_ha, thinq, _) = make();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(IDENTITY));
        let acks: Vec<String> = thinq.sent().into_iter().filter(|m| m.cmd == "ack").map(|m| m.data.as_str().unwrap_or("").to_string()).collect();
        assert_eq!(acks, vec!["AA08F000310482BB"]);
        assert!(thinq.outbox().is_empty());
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(EB_OFF));
        thinq.emit_data(&hex_decode(NORMAL));
        assert!(thinq.sent().iter().all(|m| m.cmd != "ack"));
    }

    #[test]
    fn unknown_phase_clears_status() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(NORMAL));
        assert_eq!(prop(&ha, "status"), "Initial");
        let mut unknown = hex_decode(NORMAL);
        unknown[35] = 0x7f;
        thinq.emit_data(&unknown);
        assert_eq!(prop(&ha, "status"), "None");
    }

    #[test]
    fn power_off_and_start_and_dial() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power_off", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F0240101009CBB");
        dev.set_property("power_off", "not-a-press");
        assert_eq!(thinq.outbox().len(), 1);
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1121010000001800B5BB");

        let ha = feed(&[EB_OFF]);
        assert_eq!(prop(&ha, "power"), "OFF");
        assert_eq!(prop(&ha, "status"), "Off");
        assert_eq!(prop(&ha, "course"), "None");
        assert_eq!(prop(&ha, "dry_completed"), "OFF");

        let ha = feed(&[NORMAL]);
        assert_eq!(prop(&ha, "course"), "Normal");
        assert_eq!(prop(&ha, "remaining_time"), "57");
        assert_eq!(prop(&ha, "initial_time"), "57");
        assert_eq!(prop(&ha, "dry_level"), "Normal");
        assert_eq!(prop(&ha, "temp"), "Mid High");
        assert_eq!(prop(&ha, "energy_saver"), "ON");
        let ha = feed(&[SMALL_LOAD]);
        assert_eq!(prop(&ha, "course"), "Small Load");
        assert_eq!(prop(&ha, "remaining_time"), "30");
        assert_eq!(prop(&ha, "temp"), "High");
        let ha = feed(&[SPORTS]);
        assert_eq!(prop(&ha, "course"), "Sportswear");
        assert_eq!(prop(&ha, "remaining_time"), "27");
        assert_eq!(prop(&ha, "temp"), "Medium");
        let ha = feed(&[ANTI]);
        assert_eq!(prop(&ha, "course"), "Antibacterial");
        assert_eq!(prop(&ha, "remaining_time"), "70");
        assert_eq!(prop(&ha, "dry_level"), "Very");
    }

    #[test]
    fn time_dry_more_less_and_options() {
        let ha = feed(&[TIME60]);
        assert_eq!(prop(&ha, "course"), "Time Dry");
        assert_eq!(prop(&ha, "remaining_time"), "60");
        assert_eq!(prop(&ha, "time_dry_setting"), "60");
        assert_eq!(prop(&ha, "temp"), "High");
        let ha = feed(&[TIME60, TIME20]);
        assert_eq!(prop(&ha, "remaining_time"), "20");
        assert_eq!(prop(&ha, "time_dry_setting"), "20");
        let ha = feed(&[ULTRA]);
        assert_eq!(prop(&ha, "temp"), "Ultra Low");
        let ha = feed(&[LESS]);
        assert_eq!(prop(&ha, "remaining_time"), "15");
        assert_eq!(prop(&ha, "more_less_time"), "-5");
        let ha = feed(&[LESS, MORE]);
        assert_eq!(prop(&ha, "remaining_time"), "20");
        assert_eq!(prop(&ha, "more_less_time"), "0");
        assert_eq!(prop(&feed(&[SIG_OFF]), "signal"), "Off");
        assert_eq!(prop(&feed(&[SIG_OFF, SIG_HIGH]), "signal"), "High");
        assert_eq!(prop(&feed(&[WRINKLE_ON]), "wrinkle_care"), "ON");
        assert_eq!(prop(&feed(&[DAMP_ON]), "damp_dry_signal"), "ON");
        assert_eq!(prop(&feed(&[STATIC_ON]), "reduce_static"), "ON");
        assert_eq!(prop(&feed(&[STATIC_ON]), "load_item"), "5");
        assert_eq!(prop(&feed(&[TURBO_ON]), "turbo_steam"), "ON");
    }

    #[test]
    fn completion_latches_through_power_off() {
        let ha = feed(&[DRYING]);
        assert_eq!(prop(&ha, "status"), "Drying");
        assert_eq!(prop(&ha, "dry_completed"), "OFF");
        let ha = feed(&[DRYING, COOLING, END]);
        assert_eq!(prop(&ha, "status"), "End");
        assert_eq!(prop(&ha, "dry_completed"), "ON");
        assert_eq!(prop(&ha, "remaining_time"), "0");
        let ha = feed(&[DRYING, COOLING, END, SETTLES]);
        assert_eq!(prop(&ha, "status"), "Off");
        assert_eq!(prop(&ha, "power"), "OFF");
        assert_eq!(prop(&ha, "dry_completed"), "ON");
        let ha = feed(&[DRYING, COOLING, END, SETTLES, NORMAL]);
        assert_eq!(prop(&ha, "dry_completed"), "OFF");
        let ha = feed(&[END, WRINKLE_RUN]);
        assert_eq!(prop(&ha, "status"), "Wrinkle Care");
        assert_eq!(prop(&ha, "power"), "ON");
        assert_eq!(prop(&ha, "remaining_time"), "0");
        assert_eq!(prop(&ha, "dry_completed"), "ON");
    }

    #[test]
    fn junk_frames_publish_nothing() {
        for junk in ["AA09307200C80048BB", "AA0A30EC001B010029BB", "001122"] {
            let (ha, thinq, _) = make();
            thinq.emit_data(&hex_decode(junk));
            assert!(ha.device(ID).unwrap().properties.is_empty(), "{junk}");
        }
        assert!(crate::t2_factory("RV13D5JSD_D_US").is_some());
    }
}
