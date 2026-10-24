//! D0211 dishwasher (DB365TXS / DBC435TSL) and the D2211 alias (XD3A25MB).
//!
//! Read-only. 26-byte class `0x32` records. Options and the course are published
//! only while the machine state is Running, including a delay-start countdown.
//! `auto_ack` is on: status `EB`/`EC` frames stay unacked, other frames are acked.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x32;
const RECORD_LEN: usize = 26;
const RECORD_BODY: u8 = 0x18;

const FLAG_CHILD_LOCK: u8 = 0x01;
const FLAG_DOOR: u8 = 0x02;
const FLAG_RINSE: u8 = 0x04;
const FLAG_SALT: u8 = 0x08;
const OPT_DELAY: u8 = 0x01;
const OPT_ENERGY: u8 = 0x02;
const OPT_EXTRA_DRY: u8 = 0x04;
const OPT_HIGH_TEMP: u8 = 0x08;
const OPT_DUAL_ZONE: u8 = 0x10;
const OPT_HALF_LOAD: u8 = 0x40;
const OPT_STEAM: u8 = 0x80;
const STATE_RUNNING: u8 = 0x02;

const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Ready"),
    (2, "Running"),
    (3, "Pause"),
    (4, "Standby"),
    (5, "End"),
];
const PROCESSES: &[(u8, &str)] = &[
    (0x00, "Idle"),
    (0x01, "Delayed Start"),
    (0x02, "Washing"),
    (0x03, "Rinsing"),
    (0x04, "Drying"),
    (0x05, "End"),
    (0x06, "Night Dry"),
    (0x63, "Cancel"),
];
const COURSES: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x05, "Eco"),
    (0x01, "Auto"),
    (0x02, "Intensive"),
    (0x03, "Delicate"),
    (0x04, "Turbo"),
    (0x06, "Rinse"),
    (0x07, "Refresh"),
    (0x08, "Express"),
    (0x09, "Machine clean"),
    (0x0b, "Download cycle"),
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
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self { core: core.clone() });
        let mut base = default_config(&meta, Some(json!({"name": "LG Dishwasher"})));
        base.components = components();
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
        if let Some(rec) = process_common_status(buf, CLASS_BYTE, RECORD_LEN) {
            self.process_status(rec);
            return;
        }
        if buf.len() == 3 && buf.first() == Some(&CLASS_BYTE) && buf.get(1) == Some(&0xd8) {
            self.core
                .publish_property("tub_clean_counter", i64::from(buf[2]).into());
        }
    }

    fn process_status(&self, record: &[u8]) {
        if record.len() < RECORD_LEN || record[1] != RECORD_BODY {
            return;
        }
        let data = &record[2..];
        let (ih, im, rh, rm) = (data[3], data[4], data[7], data[8]);
        if im > 59 || rm > 59 || ih > 99 || rh > 99 {
            return;
        }
        let cycle = data[0] == STATE_RUNNING;
        self.core
            .publish_property("initial_time", (i64::from(ih) * 60 + i64::from(im)).into());
        self.core.publish_property(
            "remaining_time",
            (i64::from(rh) * 60 + i64::from(rm)).into(),
        );
        self.core
            .publish_property("status", named(STATES, data[0]).into());
        self.core
            .publish_property("process", named(PROCESSES, data[1]).into());
        let course = if cycle {
            named(COURSES, data[5])
        } else {
            "Off"
        };
        self.core.publish_property("course", course.into());

        let opt = |bit: u8| cycle && data[12] & bit != 0;
        self.core.publish_on_off("delay_start", opt(OPT_DELAY));
        self.core.publish_on_off("energy_saver", opt(OPT_ENERGY));
        self.core.publish_on_off("extra_dry", opt(OPT_EXTRA_DRY));
        self.core.publish_on_off("high_temp", opt(OPT_HIGH_TEMP));
        self.core.publish_on_off("dual_zone", opt(OPT_DUAL_ZONE));
        self.core.publish_on_off("half_load", opt(OPT_HALF_LOAD));
        self.core.publish_on_off("steam", opt(OPT_STEAM));

        let flags = data[11];
        self.core
            .publish_on_off("child_lock", flags & FLAG_CHILD_LOCK == 0);
        self.core.publish_on_off("door", flags & FLAG_DOOR != 0);
        self.core
            .publish_on_off("rinse_refill", flags & FLAG_RINSE != 0);
        self.core
            .publish_on_off("salt_refill", flags & FLAG_SALT != 0);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode("F0ED1121010000001800"));
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

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| *n)
        .unwrap_or("None")
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, n)| *n).collect()
}

fn components() -> std::collections::HashMap<String, Value> {
    let mut c = Map::new();
    let add = |c: &mut Map<String, Value>, k: &str, v: Value| {
        c.insert(k.into(), v);
    };
    add(
        &mut c,
        "status",
        json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:dishwasher","device_class":"enum","options":labels(STATES)}),
    );
    add(
        &mut c,
        "course",
        json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:playlist-play","device_class":"enum","options":labels(COURSES)}),
    );
    add(
        &mut c,
        "process",
        json!({"platform":"sensor","unique_id":"$deviceid-process","state_topic":"$this/process","name":"Process","icon":"mdi:progress-clock","device_class":"enum","options":labels(PROCESSES)}),
    );
    add(
        &mut c,
        "remaining_time",
        json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"}),
    );
    add(
        &mut c,
        "initial_time",
        json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer","device_class":"duration","unit_of_measurement":"min"}),
    );
    add(
        &mut c,
        "salt_refill",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-salt_refill","state_topic":"$this/salt_refill","name":"Salt refill","icon":"mdi:water"}),
    );
    add(
        &mut c,
        "rinse_refill",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-rinse_refill","state_topic":"$this/rinse_refill","name":"Rinse refill","icon":"mdi:cup-water"}),
    );
    add(
        &mut c,
        "door",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","device_class":"door"}),
    );
    add(
        &mut c,
        "child_lock",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","device_class":"lock"}),
    );
    add(
        &mut c,
        "dual_zone",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-dual_zone","state_topic":"$this/dual_zone","name":"Dual zone","icon":"mdi:view-split-horizontal"}),
    );
    add(
        &mut c,
        "energy_saver",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-energy_saver","state_topic":"$this/energy_saver","name":"Energy saver","icon":"mdi:leaf"}),
    );
    add(
        &mut c,
        "steam",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"}),
    );
    add(
        &mut c,
        "delay_start",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-delay_start","state_topic":"$this/delay_start","name":"Delay start","icon":"mdi:clock-plus-outline"}),
    );
    add(
        &mut c,
        "extra_dry",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_dry","state_topic":"$this/extra_dry","name":"Extra dry","icon":"mdi:weather-sunny"}),
    );
    add(
        &mut c,
        "high_temp",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-high_temp","state_topic":"$this/high_temp","name":"High temp","icon":"mdi:thermometer-high"}),
    );
    add(
        &mut c,
        "half_load",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-half_load","state_topic":"$this/half_load","name":"Half load","icon":"mdi:package-variant-closed-minus"}),
    );
    add(
        &mut c,
        "tub_clean_counter",
        json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_counter","state_topic":"$this/tub_clean_counter","name":"Tub clean counter","icon":"mdi:counter"}),
    );
    c.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::device_base::wrap_aabb;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const ID: &str = "test-id";
    const SENSING_STEAM: &str = "aa2032eb0018010000040b0200040b0000728002040100000000000000004cbb";
    const IDLE_ECO: &str = "aa3a32ec001801000003350500033500007200020401000000000000000008180100000335050003350000720002040100000000000000004dbb";
    const RUNNING_DRY_STEAM: &str = "aa3a32ec0018020300040b0200011f0000708002040100000000000000000018020400040b0200011f000070800204010000000000000000d8bb";
    const COMPLETING: &str = "aa3a32ec0018020400040b020000010000728002040100000000000000000018050500040b020000010000720002040100000000000000008dbb";
    const END: &str = "aa3a32ec0018050500040b020000010000720002040100000000000000000018040000040b0000000100007200020401000000000000000001bb";
    const INTENSIVE: &str = "aa3a32ec001801000003050200030500007200020401000000000000000000180202000305020003050000700002040100000000000000001ebb";
    const DUAL_ZONE: &str = "aa3a32ec0018010000022a0100022a0000721002040100000000000000000818020200022a0100022a00007010020401000000000000000054bb";
    const SALT: &str = "aa3a32ec001801000002390100023900007a020204010000000000000000081802020002390100023900007802020401000000000000000064bb";
    const EXTRA_DRY: &str = "aa3a32ec001801000003210500032100007204020401000000000000000000180202000321050003210000700402040100000000000000009cbb";
    const HIGH_HALF: &str = "aa3a32ec0018010000032c0100032c0000724802040100000000000000000018020200032c0100032c00007048020401000000000000000020bb";
    const DELAY: &str = "aa3a32ec0018010000033505000335000072110204010000000000000000001802010003350500033501007011020401000000000000000066bb";
    const IDLE_HIGH: &str = "aa3a32ec0018010000033201000332000072080204010000000000000000001801000003320100033200007218020401000000000000000049bb";
    const CHILD_LOCK: &str = "aa3a32ec0018010000033201000332000072180204010000000000000000001801000003320100033200007318020401000000000000000078bb";
    const IDLE_HALF: &str = "aa3a32ec0018010000031201000312000072540204010000000000000000001801000003120100031200007244020401000000000000000041bb";
    const RINSE: &str = "aa3a32ec001802040003350500003800007210020401000000000000000000180204000335050000380000761002040100000000000000006bbb";
    const TRANSITION: &str = "aa0732d81199bb";
    const HANDSHAKE: &str = "aa373231020153414134313236333932350000d1db00008000000000000253414134313236313032300000ffa7fffc000000000000a5bb";
    const TRUNC_EB: &str = "aa1432eb0018010000040b0200040b00007200bb";
    const EC_ONE: &str = "aa2032ec0018020300040b0200011f00007080020401000000000000000000bb";
    const LONG_EB: &str = "aa2232eb0018010000040b0200040b000072800204010000000000000000dead00bb";

    const OPTION_PROPS: &[&str] = &[
        "delay_start",
        "energy_saver",
        "extra_dry",
        "high_temp",
        "dual_zone",
        "half_load",
        "steam",
    ];

    fn meta() -> Metadata {
        Metadata::new("D0211", "D0211", "1")
    }

    fn feed(frames: &[&str]) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn keys(ha: &MockHaConnection) -> Vec<String> {
        let mut keys: Vec<String> = ha.device(ID).unwrap().properties.keys().cloned().collect();
        keys.sort();
        keys
    }

    fn assert_options_off(ha: &MockHaConnection) {
        for name in OPTION_PROPS {
            assert_eq!(prop(ha, name).as_deref(), Some("OFF"), "{name}");
        }
    }

    /// Packet index 30 is the current `EC` record. `offset` is within that record.
    fn with_bit(frame: &str, offset: usize, bit: u8) -> Vec<u8> {
        let mut pkt = hex_decode(frame);
        pkt[30 + offset] |= bit;
        pkt
    }

    #[test]
    fn status_and_the_counter_fill_every_declared_component() {
        let (ha, thinq, _) = feed(&[INTENSIVE, TRANSITION]);
        let info = ha.device(ID).unwrap();
        let mut components: Vec<String> = info
            .config
            .as_ref()
            .unwrap()
            .components
            .keys()
            .cloned()
            .collect();
        components.sort();
        assert_eq!(keys(&ha), components);
        assert_eq!(
            info.config.as_ref().unwrap().components["course"]["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "Off",
                "Eco",
                "Auto",
                "Intensive",
                "Delicate",
                "Turbo",
                "Rinse",
                "Refresh",
                "Express",
                "Machine clean",
                "Download cycle",
            ]
        );
        let mut enums: Vec<_> = info
            .config
            .as_ref()
            .unwrap()
            .components
            .iter()
            .filter(|(_, c)| c.get("device_class").and_then(|v| v.as_str()) == Some("enum"))
            .map(|(name, _)| name.clone())
            .collect();
        enums.sort();
        assert_eq!(
            enums,
            vec![
                "course".to_string(),
                "process".to_string(),
                "status".to_string()
            ]
        );
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1121010000001800B5BB"
        );
    }

    #[test]
    fn intensive_with_no_options_publishes_the_course_and_every_option_off() {
        let (ha, _, _) = feed(&[INTENSIVE]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Intensive"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("185"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("185"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_options_off(&ha);
    }

    #[test]
    fn captured_option_bits_publish_only_their_own_lamps() {
        let (ha, _, _) = feed(&[RUNNING_DRY_STEAM]);
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("251"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("91"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Intensive"));

        let (ha, _, _) = feed(&[DUAL_ZONE]);
        assert_eq!(prop(&ha, "dual_zone").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("162"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("162"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("OFF"));

        let (ha, _, _) = feed(&[SALT]);
        assert_eq!(prop(&ha, "salt_refill").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("ON"));

        let (ha, _, _) = feed(&[EXTRA_DRY]);
        assert_eq!(prop(&ha, "extra_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "high_temp").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "half_load").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Eco"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("213"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("213"));

        let (ha, _, _) = feed(&[HIGH_HALF]);
        assert_eq!(prop(&ha, "high_temp").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "half_load").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "dual_zone").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "extra_dry").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("224"));

        let (ha, _, _) = feed(&[RINSE]);
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "salt_refill").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Drying"));
    }

    #[test]
    fn awake_panel_hides_course_and_options_until_the_cycle_runs() {
        let (ha, thinq, _) = feed(&[IDLE_ECO]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("233"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("233"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_options_off(&ha);
        assert!(thinq.sent().is_empty(), "EB/EC status is not acked");

        let (ha, _, _) = feed(&[SENSING_STEAM]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("251"));

        let (ha, _, _) = feed(&[IDLE_HIGH]);
        assert_eq!(prop(&ha, "high_temp").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("230"));
        let (ha, _, _) = feed(&[IDLE_HIGH, HIGH_HALF]);
        assert_eq!(prop(&ha, "high_temp").as_deref(), Some("ON"));

        let (ha, _, _) = feed(&[IDLE_HALF]);
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("198"));
        assert_eq!(prop(&ha, "extra_dry").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "half_load").as_deref(), Some("OFF"));

        let (ha, _, _) = feed(&[CHILD_LOCK]);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "dual_zone").as_deref(), Some("OFF"));
    }

    #[test]
    fn delay_countdown_and_the_current_record_win() {
        let (ha, _, _) = feed(&[DELAY]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Delayed Start"));
        assert_eq!(prop(&ha, "delay_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "dual_zone").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Eco"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("233"));

        let (ha, _, _) = feed(&[COMPLETING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("1"));

        let (ha, _, _) = feed(&[END]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("OFF"));
    }

    #[test]
    fn gates_reject_stale_bits_and_the_wrong_layout() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        thinq.emit_data(&with_bit(INTENSIVE, 14, 0x20));
        assert_options_off(&ha);

        thinq.reset_recorder();
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        thinq.emit_data(&with_bit(EXTRA_DRY, 14, 0x08 | 0x40));
        assert_eq!(prop(&ha, "extra_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "high_temp").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "half_load").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "dual_zone").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));

        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        thinq.emit_data(&with_bit(COMPLETING, 14, 0x80));
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_options_off(&ha);

        let (ha, _, _) = feed(&[HANDSHAKE, TRUNC_EB, EC_ONE, LONG_EB]);
        assert!(keys(&ha).is_empty());

        let mut bad_minute = hex_decode(INTENSIVE);
        bad_minute[36] = 0x3c;
        let (ha, thinq, _) = feed(&[]);
        thinq.emit_data(&bad_minute);
        assert!(keys(&ha).is_empty());
    }

    #[test]
    fn counter_frame_publishes_only_the_counter_and_is_acked() {
        let (ha, thinq, dev) = feed(&[TRANSITION]);
        assert_eq!(prop(&ha, "tub_clean_counter").as_deref(), Some("17"));
        assert_eq!(keys(&ha), vec!["tub_clean_counter".to_string()]);
        let acks: Vec<String> = thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(acks, vec![hex_encode(&wrap_aabb(&hex_decode("F000D804")))]);
        dev.set_property("steam", "ON");
        assert_eq!(thinq.outbox().len(), 1, "read-only: no command packet");
    }

    #[test]
    fn model_json_course_codes_decode_and_unknown_codes_stay_none() {
        let mut turbo = hex_decode(INTENSIVE);
        turbo[37] = 0x04;
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        thinq.emit_data(&turbo);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Turbo"));

        turbo[37] = 0x12;
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        thinq.emit_data(&turbo);
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
    }

    #[test]
    fn registry_resolves_d0211_and_the_d2211_alias() {
        assert!(crate::registry::t2_factory("D0211").is_some());
        assert!(crate::registry::t2_factory("D2211").is_some());
    }
}
