//! G2V7_Y___W.B_2QEUK — LG F2NX50S9TKC slim front-loader.
//!
//! 67-byte records inside a class `0x20` envelope. A `0xEB` record answers
//! the status request; later changes arrive as a `0xEC` pair. `auto_ack` is
//! on because an unacked report is repeated.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{request_status, set_power_start_pause};
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const STATUS_LENGTH: usize = 67;

const COURSES: &[(u8, &str)] = &[
    (0x2e, "Cotton"),
    (0x13, "Eco 40-60"),
    (0x2b, "Mixed Fabric"),
    (0x1d, "Easy Care"),
    (0x5e, "Hand/Wool"),
    (0x4f, "Activewear"),
    (0x72, "AI Wash"),
    (0x7a, "TurboWash 39"),
    (0x4b, "Quick 14"),
    (0x04, "Allergy Care"),
    (0x88, "Microplastic Care"),
    (0xff, "Download Cycle"),
];

const TEMPERATURES: &[(u8, &str)] = &[
    (8, "Cold"),
    (1, "20 °C"),
    (2, "30 °C"),
    (3, "40 °C"),
    (5, "60 °C"),
    (6, "95 °C"),
];

const SPINS: &[(u8, &str)] = &[
    (0x0c, "No spin"),
    (1, "400 rpm"),
    (4, "800 rpm"),
    (6, "1000 rpm"),
    (8, "1200 rpm"),
];

const STATES: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Standby"),
    (0x02, "Paused"),
    (0x03, "Detecting"),
    (0x07, "Delayed"),
    (0x0b, "Washing"),
    (0x0c, "Rinsing"),
    (0x0e, "Spinning"),
    (0x10, "End"),
];

const FLAGS_BYTE: usize = 37;
const FLAG_DOOR_CLOSED: u8 = 0x01;
const FLAG_CHILD_LOCK: u8 = 0x20;
const FLAG_REMOTE_START: u8 = 0x10;
const OPTIONS_BYTE: usize = 34;
const OPT_INTENSIVE: u8 = 0x08;
const OPT_TURBOWASH: u8 = 0x20;
const OPT_PREWASH: u8 = 0x40;
const FLAGS2_BYTE: usize = 39;
const FLAG2_RINSE_PLUS: u8 = 0x40;

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self { core: core.clone() });
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
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
        if buf.len() == 4 && buf[0] == 0x20 && buf[1] == 0x00 && buf[3] != 0x00 {
            tracing::debug!("command refused {:02x?}", buf);
            return;
        }
        if let Some(rec) = process_enveloped_status(buf, CLASS_BYTE, STATUS_LENGTH) {
            self.process_status(rec);
        }
    }

    fn process_status(&self, s: &[u8]) {
        if s.len() < STATUS_LENGTH {
            return;
        }
        let state = s[21];
        let on = state != 0;
        self.core.publish_on_off("power", on);
        self.core
            .publish_property("status", named(STATES, state).unwrap_or("None").into());
        self.publish_setting("course", on, named(COURSES, s[5]));
        self.publish_setting("temp", on, named(TEMPERATURES, s[2]));
        self.publish_setting("spin", on, named(SPINS, s[4]));
        self.publish_minutes("initial_time", on, s[16] as i64);
        self.publish_minutes("remaining_time", on, s[14] as i64);
        let delay = u16::from_be_bytes([s[11], s[12]]) as i64;
        self.publish_minutes("delay_end", on, delay);
        let options = s[OPTIONS_BYTE];
        self.publish_flag("rinse_plus", on, s[FLAGS2_BYTE] & FLAG2_RINSE_PLUS != 0);
        self.publish_flag("intensive", on, options & OPT_INTENSIVE != 0);
        self.publish_flag("prewash", on, options & OPT_PREWASH != 0);
        self.publish_flag("turbowash", on, options & OPT_TURBOWASH != 0);
        self.core
            .publish_property("energy", PropertyValue::Int(s[17] as i64 * 256 + s[18] as i64));
        let flags = s[FLAGS_BYTE];
        self.core
            .publish_on_off("door", flags & FLAG_DOOR_CLOSED == 0);
        self.core
            .publish_on_off("child_lock", flags & FLAG_CHILD_LOCK == 0);
        self.core
            .publish_on_off("remote_start", flags & FLAG_REMOTE_START != 0);
        if on {
            self.core
                .publish_property("tub_clean_count", PropertyValue::Int(s[28] as i64));
        }
    }

    fn publish_setting(&self, prop: &str, on: bool, label: Option<&str>) {
        let value = if on { label.unwrap_or("None") } else { "None" };
        self.core.publish_property(prop, value.into());
    }

    fn publish_minutes(&self, prop: &str, on: bool, minutes: i64) {
        if on {
            self.core.publish_property(prop, PropertyValue::Int(minutes));
        } else {
            self.core.publish_property(prop, "None".into());
        }
    }

    fn publish_flag(&self, prop: &str, on: bool, set: bool) {
        if on {
            self.core.publish_on_off(prop, set);
        } else {
            self.core.publish_property(prop, "None".into());
        }
    }
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, name)| *name)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn components() -> std::collections::HashMap<String, serde_json::Value> {
    [
        ("power".into(), json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"","icon":"mdi:washing-machine"})),
        ("start".into(), json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
        ("pause".into(), json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
        ("status".into(), json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":labels(STATES)})),
        ("course".into(), json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline","device_class":"enum","options":labels(COURSES)})),
        ("temp".into(), json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","device_class":"enum","options":labels(TEMPERATURES)})),
        ("spin".into(), json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","device_class":"enum","options":labels(SPINS)})),
        ("initial_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","device_class":"duration","unit_of_measurement":"min","name":"Initial time"})),
        ("remaining_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","device_class":"duration","unit_of_measurement":"min","name":"Remaining time"})),
        ("energy".into(), json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
        ("door".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","device_class":"door"})),
        ("child_lock".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","device_class":"lock"})),
        ("remote_start".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:play-circle-outline"})),
        ("delay_end".into(), json!({"platform":"sensor","unique_id":"$deviceid-delay_end","state_topic":"$this/delay_end","device_class":"duration","unit_of_measurement":"min","name":"Delay end","icon":"mdi:timer-sand"})),
        ("rinse_plus".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-rinse_plus","state_topic":"$this/rinse_plus","name":"Rinse+","icon":"mdi:water-plus"})),
        ("intensive".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-intensive","state_topic":"$this/intensive","name":"Intensive","icon":"mdi:washing-machine"})),
        ("prewash".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-prewash","state_topic":"$this/prewash","name":"Pre-wash","icon":"mdi:water-sync"})),
        ("turbowash".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-turbowash","state_topic":"$this/turbowash","name":"TurboWash","icon":"mdi:rocket-launch"})),
        ("tub_clean_count".into(), json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since drum clean","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"})),
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
    fn set_property(&self, prop: &str, value: &str) {
        set_power_start_pause(&self.core, prop, value);
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
    use rethink_core::device_base::wrap_aabb;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const READY_COTTON: &str = "AAFF200A005500C7F9000100EB00430003030E082E0000000000000000CB00CB0000002E010090000004020504000000000000004000001000000000000004000000000000000000000000000000180000009383BB";
    const PAIR_EASY_CARE: &str = "AAFF200A009800C818000100EC00860003030E082E0000000000000000CB00CB0000002E010090000004020504000000000000004000001000000000000004000000000000000000000000000000180000000003030E081D00000000000000009400940000001D01009000000302050400000000000000400000100000000000000400000000000000000000000000000018000000B3FCBB";
    const PAIR_DOWNLOAD: &str = "AAFF200A009800C84B000100EC00860003030E044F00000000000000003900390000004F010090090002020504000000000000004000001000000000000004000000000000000000000000000000180000000003000E08FF0000000000000000100010000000FF0100900900010105040000000000000040000010000000000000040000000000000000000000000000001800000078E8BB";
    const READY_COLD: &str = "AAFF200A005500C8D3000100EB00430003080E082E00000000000000005D005D0000002E010090000004020504000000000000000000001000000000000004000000000000000000000000000000180000005B9EBB";
    const PAIR_DOOR: &str = "AAFF200A009800CE17000100EC00860003030E062B00000000000000005600560000002B010090000003020504000000000000000000001000000000000004000000000000000000000000000000180000000003030E062B00000000000000005600560000002B01009000000302050400000000000000010000100000000000000400000000000000000000000000000018000000B10DBB";
    const PAUSED_NO_SPIN: &str = "AAFF200A00550005E4000100EB00430003050E0C2E00000000000000004200420000002E020B90010004020404000000000000004101001000000000000004000000000000000000000000000000180000008068BB";
    const PAIR_WASHING: &str = "AAFF200A0098000D67000100EC00860003020E082E00000000000000007100710000002E030190000000020504000000000000009101001000000000000004000000000000000000000000000000180000000003020E082E00000000000000003900390000002E0B039003000002050400000000000000910100100000000000000400000000000000000000000000000018000000AAE0BB";
    const PAIR_RINSING: &str = "AAFF200A0098000ECE000100EC00860003020E082E0000000000000000260039001E002E0B0390030000020504000000000000009101001000000000000004000000000000000000000000000000180000000003000E082E0000000000000000250039001E002E0C0B9003000002050400000000000000110100100000000000000400000000000000000000000000000018000000C9D3BB";
    const PAIR_SPINNING: &str = "AAFF200A009800AE67000100EC00860003000E082E00000000000000000C003A00C8002E0C0B900400010106040000000000000011010010000000000000040000000000000000000000000000001800000000000000082E00000000000000000B003A00C8002E0E0C9004000100060400000000000000110100100000000000000400000000000000000000000000000018000000B40CBB";
    const END: &str = "AAFF200A00550011BD000100EB004300000000002E0000000000000000000039004C002E100E90000001000604000000000000004101001000000000000004000000000000000000000000000000180000008F81BB";
    const PAIR_OFF: &str = "AAFF200A00980011CF000100EC008600000000002E0000000000000000000039004C002E100E900000010006040000000000000041010010000000000000040000000000000000000000000000001800000000000000002E0000000000000000000039004C002E00109000000400060400000000000000400000100000000000000400000000000000000000000000000018000000DDA3BB";
    const PAIR_ENERGY: &str = "AAFF200A00980001A3000100EC00860003030E087A00000000000000001B002700FF007A0B0190090000010B04000000002000001101001000000000000004000000000000000000000000000000180000000003030E087A00000000000000001B00270100007A0B0190090000010B04000000002000001101001000000000000004000000000000000000000000000000180000000421BB";
    const AFTER_CUT: &str = "AAFF200A00550005C9000100EB0043000000000000000000000000000000000000000000000000000000000004000000000000000000201000000000000004000000000000000000000000000000180000007749BB";
    const CHILD_ON: &str = "AAFF200A0098008ED9000100EC00860003010E014B00000000000000000E000E0000004B0B0290090000021504000000002000009101001000000000000004000000000000000000000000000000180000000003010E014B00000000000000000E000E0000004B0B029009000002150400000000200000B101001000000000000004000000000000000000000000000000180000002F90BB";
    const CHILD_OFF: &str = "AAFF200A0098008EEC000100EC00860003010E014B00000000000000000D000E0008004B0B029009000002150400000000200000B101001000000000000004000000000000000000000000000000180000000003010E014B00000000000000000D000E0009004B0B0290090000021504000000002000009101001000000000000004000000000000000000000000000000180000003804BB";
    const REMOTE_ON: &str = "AAFF200A0098008E83000100EC00860003010E014B00000000000000000E000E0000004B010090090001021504000000002000004100001000000000000004000000000000000000000000000000180000000003010E014B00000000000000000E000E0000004B01009009000102150400000000200000510000100000000000000400000000000000000000000000000018000000BE45BB";
    const RINSE_PLUS: &str = "AAFF200A0098009060000100EC00860003030E082E0000000000000000CB00CB0000002E010090000004021604000000000000004000001000000000000004000000000000000000000000000000180000000003030F082E0000000000000000DA00DA0000002E01009000000502160400000000000000400040100000000000000400000000000000000000000000000018000000C1A2BB";
    const INTENSIVE: &str = "AAFF200A0098009068000100EC00860003030E082E0000000000000000CB00CB0000002E01009000000402160400000000000000400000100000000000000400000000000000000000000000000018000000000A030E082E0000000000000000EA00EA0000002E010090000004021604000000000800004000001000000000000004000000000000000000000000000000180000003A49BB";
    const PREWASH: &str = "AAFF200A009800906F000100EC00860003030E082E0000000000000000CB00CB0000002E010090000004021604000000000000004000001000000000000004000000000000000000000000000000180000000003030E082E0000000000000000DC00DC0000002E01009000000502160400000000400000400000100000000000000400000000000000000000000000000018000000C0E0BB";
    const TURBO: &str = "AAFF200A0098009077000100EC00860003030E082E0000000000000000CB00CB0000002E010090000004021604000000000000004000001000000000000004000000000000000000000000000000180000000003030E062E0000000000000000A600A60000002E010090000004021604000000002000004000001000000000000004000000000000000000000000000000180000000D53BB";
    const DELAY_5H: &str = "AAFF200A0098009155000100EC00860003030E082E000000000000B400CB00CB0001002E010090000004021604000000000000004100801000000000000004000000000000000000000000000000180000000003030E082E0000000000012C00CB00CB0001002E010090000004021604000000000000004100801000000000000004000000000000000000000000000000180000005D26BB";
    const DELAYED: &str = "AAFF200A0098009125000100EC00860003030E082E000000000000B4003800380000002E070390010000021604000000000000009101801000000000000004000000000000000000000000000000180000000003030E082E000000000000B3003800380000002E07039001000002160400000000000000910180100000000000000400000000000000000000000000000018000000FA24BB";
    const START_REFUSED: &str = "AA08200024FFA0BB";

    fn meta() -> Metadata {
        Metadata::new("G2V7_Y___W.B_2QEUK", "G2V7_Y___W.B_2QEUK", "2.11.306")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn feed(frame: &str) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(frame));
        ha
    }

    fn prop(ha: &MockHaConnection, name: &str) -> String {
        ha.device(ID).unwrap().properties.get(name).unwrap().as_string()
    }

    #[test]
    fn start_asks_for_status_once() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1121010000001800B5BB"
        );
    }

    #[test]
    fn eb_decodes_the_selected_course() {
        let ha = feed(READY_COTTON);
        assert_eq!(prop(&ha, "power"), "ON");
        assert_eq!(prop(&ha, "status"), "Standby");
        assert_eq!(prop(&ha, "course"), "Cotton");
        assert_eq!(prop(&ha, "temp"), "40 °C");
        assert_eq!(prop(&ha, "spin"), "1200 rpm");
        assert_eq!(prop(&ha, "initial_time"), "203");
        assert_eq!(prop(&ha, "remaining_time"), "203");
        assert_eq!(prop(&ha, "door"), "ON");
        assert_eq!(prop(&ha, "tub_clean_count"), "5");
    }

    #[test]
    fn ec_pair_uses_the_second_record() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(READY_COTTON));
        thinq.emit_data(&hex_decode(PAIR_EASY_CARE));
        assert_eq!(prop(&ha, "course"), "Easy Care");
        assert_eq!(prop(&ha, "initial_time"), "148");
        thinq.emit_data(&hex_decode(PAIR_DOWNLOAD));
        assert_eq!(prop(&ha, "course"), "Download Cycle");
        assert_eq!(prop(&ha, "temp"), "None");
        assert_eq!(prop(&ha, "spin"), "1200 rpm");
        assert_eq!(prop(&ha, "initial_time"), "16");
    }

    #[test]
    fn zero_settings_and_a_wash() {
        let ha = feed(READY_COLD);
        assert_eq!(prop(&ha, "temp"), "Cold");
        let ha = feed(PAUSED_NO_SPIN);
        assert_eq!(prop(&ha, "status"), "Paused");
        assert_eq!(prop(&ha, "temp"), "60 °C");
        assert_eq!(prop(&ha, "spin"), "No spin");

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(PAIR_WASHING));
        assert_eq!(prop(&ha, "status"), "Washing");
        assert_eq!(prop(&ha, "temp"), "30 °C");
        assert_eq!(prop(&ha, "initial_time"), "57");
        thinq.emit_data(&hex_decode(PAIR_RINSING));
        assert_eq!(prop(&ha, "status"), "Rinsing");
        assert_eq!(prop(&ha, "remaining_time"), "37");
        assert_eq!(prop(&ha, "energy"), "30");
        assert_eq!(prop(&ha, "temp"), "None");
        thinq.emit_data(&hex_decode(PAIR_SPINNING));
        assert_eq!(prop(&ha, "status"), "Spinning");
        assert_eq!(prop(&ha, "remaining_time"), "11");
        assert_eq!(prop(&ha, "energy"), "200");
    }

    #[test]
    fn energy_is_sixteen_bits_and_off_hides_settings() {
        let ha = feed(PAIR_ENERGY);
        assert_eq!(prop(&ha, "course"), "TurboWash 39");
        assert_eq!(prop(&ha, "energy"), "256");
        assert_eq!(prop(&ha, "tub_clean_count"), "11");

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(END));
        assert_eq!(prop(&ha, "status"), "End");
        assert_eq!(prop(&ha, "remaining_time"), "0");
        assert_eq!(prop(&ha, "spin"), "None");
        assert_eq!(prop(&ha, "tub_clean_count"), "6");
        thinq.emit_data(&hex_decode(PAIR_OFF));
        assert_eq!(prop(&ha, "power"), "OFF");
        assert_eq!(prop(&ha, "status"), "Off");
        assert_eq!(prop(&ha, "course"), "None");
        assert_eq!(prop(&ha, "initial_time"), "None");
        assert_eq!(prop(&ha, "remaining_time"), "None");
        assert_eq!(prop(&ha, "energy"), "76");
        assert_eq!(prop(&ha, "tub_clean_count"), "6");
    }

    #[test]
    fn wash_count_is_not_taken_while_off() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(READY_COTTON));
        thinq.emit_data(&hex_decode(AFTER_CUT));
        assert_eq!(prop(&ha, "power"), "OFF");
        assert_eq!(prop(&ha, "tub_clean_count"), "5");
    }

    #[test]
    fn locks_options_delay_and_commands() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(CHILD_ON));
        assert_eq!(prop(&ha, "course"), "Quick 14");
        assert_eq!(prop(&ha, "child_lock"), "OFF");
        thinq.emit_data(&hex_decode(CHILD_OFF));
        assert_eq!(prop(&ha, "child_lock"), "ON");
        assert_eq!(prop(&ha, "energy"), "9");

        thinq.emit_data(&hex_decode(REMOTE_ON));
        assert_eq!(prop(&ha, "remote_start"), "ON");
        assert_eq!(prop(&ha, "turbowash"), "ON");
        thinq.emit_data(&hex_decode(READY_COTTON));
        assert_eq!(prop(&ha, "remote_start"), "OFF");
        assert_eq!(prop(&ha, "rinse_plus"), "OFF");
        thinq.emit_data(&hex_decode(RINSE_PLUS));
        assert_eq!(prop(&ha, "rinse_plus"), "ON");
        assert_eq!(prop(&ha, "initial_time"), "218");
        thinq.emit_data(&hex_decode(INTENSIVE));
        assert_eq!(prop(&ha, "intensive"), "ON");
        thinq.emit_data(&hex_decode(PREWASH));
        assert_eq!(prop(&ha, "prewash"), "ON");
        thinq.emit_data(&hex_decode(TURBO));
        assert_eq!(prop(&ha, "turbowash"), "ON");
        assert_eq!(prop(&ha, "spin"), "1000 rpm");

        thinq.emit_data(&hex_decode(DELAY_5H));
        assert_eq!(prop(&ha, "delay_end"), "300");
        thinq.emit_data(&hex_decode(DELAYED));
        assert_eq!(prop(&ha, "status"), "Delayed");
        assert_eq!(prop(&ha, "delay_end"), "179");
        assert_eq!(prop(&ha, "remaining_time"), "56");

        thinq.emit_data(&hex_decode(PAIR_DOOR));
        assert_eq!(prop(&ha, "door"), "OFF");
        assert_eq!(prop(&ha, "course"), "Mixed Fabric");

        let before = ha.device(ID).unwrap().properties.len();
        thinq.emit_data(&hex_decode(START_REFUSED));
        assert_eq!(ha.device(ID).unwrap().properties.len(), before);

        thinq.reset_recorder();
        dev.set_property("power", "ON");
        dev.set_property("power", "OFF");
        dev.set_property("pause", "");
        dev.set_property("start", "");
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(
            sent,
            vec![
                "AA08F02A010098BB",
                "AA09F0240101009CBB",
                "AA09F02404010099BB",
                "AA09F02405010098BB",
            ]
        );
    }

    #[test]
    fn envelopes_are_acked_by_sequence() {
        let (_ha, thinq, _) = make();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(READY_COTTON));
        thinq.emit_data(&hex_decode(PAIR_EASY_CARE));
        let acks: Vec<String> = thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            acks,
            vec![
                hex_encode(&wrap_aabb(&[0xf0, 0x00, 0x0a, 0x04, 0xc7, 0xf9])),
                hex_encode(&wrap_aabb(&[0xf0, 0x00, 0x0a, 0x04, 0xc8, 0x18])),
            ]
        );
        assert!(crate::t2_factory("G2V7_Y___W.B_2QEUK").is_some());
    }
}
