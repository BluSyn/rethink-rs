//! D30 dishwasher (LG LDT54788D). 26-byte class `0x32` records.
//!
//! Same framing as D0211, with this model's course names and flag bits.
//! Course and option lamps publish in Ready, Running, and Pause. Night dry
//! and the rinse-aid lamp are not gated. `auto_ack` stays off.

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
const FLAG_RINSE: u8 = 0x08;
const FLAG_NIGHT_DRY: u8 = 0x80;
const OPT_DELAY: u8 = 0x01;
const OPT_ENERGY: u8 = 0x02;
const OPT_EXTRA_DRY: u8 = 0x04;
const OPT_HIGH_TEMP: u8 = 0x08;
const OPT_DUAL_ZONE: u8 = 0x10;
const OPT_HALF_LOAD: u8 = 0x40;

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
    (0x01, "Auto"),
    (0x02, "Heavy"),
    (0x03, "Delicate"),
    (0x04, "Turbo"),
    (0x05, "Normal"),
    (0x06, "Rinse"),
    (0x08, "Express"),
    (0x09, "Machine Clean"),
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
        let (ih, im, rh, rm, dm) = (data[3], data[4], data[7], data[8], data[10]);
        if im > 59 || rm > 59 || dm > 59 || ih > 99 || rh > 99 {
            return;
        }
        let active = matches!(data[0], 0x01 | 0x02 | 0x03);
        self.core
            .publish_property("initial_time", (i64::from(ih) * 60 + i64::from(im)).into());
        self.core.publish_property(
            "remaining_time",
            (i64::from(rh) * 60 + i64::from(rm)).into(),
        );
        self.core.publish_property(
            "delay_start_time",
            (i64::from(data[9]) * 60 + i64::from(dm)).into(),
        );
        self.core
            .publish_property("status", named(STATES, data[0]).into());
        self.core
            .publish_property("process", named(PROCESSES, data[1]).into());
        let course = if active {
            named(COURSES, data[5])
        } else {
            "Off"
        };
        self.core.publish_property("course", course.into());

        let opt = |bit: u8| active && data[12] & bit != 0;
        self.core.publish_on_off("energy_saver", opt(OPT_ENERGY));
        self.core.publish_on_off("half_load", opt(OPT_HALF_LOAD));
        self.core.publish_on_off("extra_dry", opt(OPT_EXTRA_DRY));
        self.core.publish_on_off("high_temp", opt(OPT_HIGH_TEMP));
        self.core.publish_on_off("dual_zone", opt(OPT_DUAL_ZONE));
        self.core.publish_on_off("delay_start", opt(OPT_DELAY));

        let flags = data[11];
        self.core
            .publish_on_off("night_dry", flags & FLAG_NIGHT_DRY != 0);
        self.core
            .publish_on_off("rinse_refill", flags & FLAG_RINSE != 0);
        self.core.publish_on_off("door", flags & FLAG_DOOR != 0);
        self.core
            .publish_on_off("child_lock", flags & FLAG_CHILD_LOCK == 0);
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
        "energy_saver",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-energy_saver","state_topic":"$this/energy_saver","name":"Energy saver","icon":"mdi:leaf"}),
    );
    add(
        &mut c,
        "half_load",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-half_load","state_topic":"$this/half_load","name":"Half load","icon":"mdi:package-variant-closed-minus"}),
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
        "tub_clean_counter",
        json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_counter","state_topic":"$this/tub_clean_counter","name":"Tub clean counter","icon":"mdi:counter","state_class":"measurement"}),
    );
    add(
        &mut c,
        "delay_start",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-delay_start","state_topic":"$this/delay_start","name":"Delay start","icon":"mdi:clock-plus-outline"}),
    );
    add(
        &mut c,
        "delay_start_time",
        json!({"platform":"sensor","unique_id":"$deviceid-delay_start_time","state_topic":"$this/delay_start_time","name":"Delay start time","icon":"mdi:clock-fast","device_class":"duration","unit_of_measurement":"min"}),
    );
    add(
        &mut c,
        "night_dry",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-night_dry","state_topic":"$this/night_dry","name":"Night dry","icon":"mdi:weather-night"}),
    );
    add(
        &mut c,
        "dual_zone",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-dual_zone","state_topic":"$this/dual_zone","name":"Dual zone","icon":"mdi:view-split-horizontal"}),
    );
    c.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const ID: &str = "test-id";
    const EB_STARTING: &str = "aa2032eb0018010000030e0200030e0000f218020000000000000000000461bb";
    const EB_WASHING: &str = "aa2032eb0018020200030e0200030e0000f018020000000000000000000460bb";
    const EC_ENERGY: &str = "aa3a32ec0018010000023205000232000072020200000000000000000004001802020002320500023200007002020000000000000000000456bb";
    const EC_OFF: &str = "aa3a32ec0018040000030e0000030e0000720002000000000000000000040018000000030e0000030e0000720002000000000000000000043fbb";
    const EC_DONE: &str = "aa3a32ec0018010000030e0200030e0000f21802000000000000000000040018040000030e0000030e00007200020000000000000000000450bb";
    const EC_END_IDLE: &str = "aa3a32ec0018020600030e020000010000f80002000000000000000000040018050000030e00000001000078000200000000000000000004b4bb";
    const EC_END_END: &str = "aa3a32ec0018020400030e020000010000f81802000000000000000000040018050500030e020000010000f80002000000000000000000042bbb";
    const EC_RINSING: &str = "aa3a32ec0018020200030e020001200000f01802000000000000000000040018020300030e020001200000f0180200000000000000000004eabb";
    const EC_DRYING: &str = "aa3a32ec0018020300030e020000210000f81802000000000000000000040018020400030e020000210000f818020000000000000000000484bb";
    const EC_NIGHT: &str = "aa3a32ec0018050500030e020000010000f80002000000000000000000040018020600030e020000010000f80002000000000000000000043dbb";
    const EC_HALF: &str = "aa3a32ec00180100000136030001360000f20002000000000000000000040818010000012b0300012b0000f24002000000000000000000046dbb";
    const EC_HALF_DRY: &str = "aa3a32ec0818010000012b0300012b0000f240020000000000000000000400180100000203030002030000f24402000000000000000000044dbb";
    const EC_DELICATE: &str = "aa3a32ec00180100000203030002030000f244020000000000000000000401180202000203030002030000f04402000000000000000000049dbb";
    const EC_DELICATE_NIGHT: &str = "aa3a32ec00180505000203030000010000f000020000000000000000000400180206000203030000010000f000020000000000000000000417bb";
    const EC_HIGH: &str = "aa3a32ec0018010000023205000232000072080200000000000000000004081801000002320500023200007208020000000000000000000443bb";
    const EC_PAUSE: &str = "aa3a32ec0018020200023205000232000070080200000000000000000004001803020002320500023200007208020000000000000000000446bb";
    const EC_RESUME: &str = "aa3a32ec0018030200023205000232000072080200000000000000000004001802020002320500023200007008020000000000000000000446bb";
    const EC_LONG: &str = "aac632ec00180505000203030000010000f0000200000000000000000004aebb000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003180505000203030000010000f000020000000000000000000445030001040000000043002902c500000000001000050436c0102002ff0000000000142e2223384631000001d1000003018600000000000000000000004d14242c180207c10276bb";
    const EC_DELAY_SEL: &str = "aa3a32ec0018010000003b0400003b0000f20002000000000000000000040018010000003b0400003b0000f20102000000000000000000044cbb";
    const EC_DELAYED: &str = "aa3a32ec0018020100003b0400003b0100f00102000000000000000000040118020100003b0400003b003bf001020000000000000000000402bb";
    const EC_DELAYED_LATER: &str = "aa3a32ec0018020100003b0400003b003bf00102000000000000000000040018020100003b0400003b003af0010200000000000000000004dabb";
    const EC_EXPRESS: &str = "aa3a32ec00180100000022080000220000f200020000000000000000000400180202000022080000220000f0000200000000000000000004e8bb";
    const EC_LOCK: &str = "aa3a32ec00180100000022080000220000f200020000000000000000000400180100000022080000220000f3000200000000000000000004e8bb";
    const EC_CLEAN: &str = "aa3a32ec00180100000116090001160000720002000000000000000000040018020200011609000116000070000200000000000000000004c6bb";
    const EC_RINSE_LOCK: &str = "aa3a32ec0018010000000c0600000c0000720002000000000000000000040018010000000c0600000c00007300020000000000000000000434bb";
    const EC_RINSE: &str = "aa3a32ec0018020200000c0600000c0000700002000000000000000000040018020300000c0600000c00007000020000000000000000000436bb";
    const D8_40: &str = "aa0732d828b6bb";
    const D8_0: &str = "aa0732d800eebb";
    const D8_46: &str = "aa0732d82ebcbb";
    const SERIAL: &str = "aa373231020153414134313236333730330000b6f000008000000000000253414133383636393731310000d743fffc00000000000024bb";

    fn meta() -> Metadata {
        Metadata::new("D30", "LDT54788D", "1.0")
    }

    fn feed(frames: &[&str]) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        (ha, thinq)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    #[test]
    fn captured_frames_publish_course_phase_and_option_lamps() {
        let (ha, thinq) = feed(&[EB_STARTING]);
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1121010000001800B5BB"
        );
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Heavy"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("194"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("194"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EB_WASHING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Heavy"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "high_temp").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "dual_zone").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EC_ENERGY]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "high_temp").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EC_DELICATE_NIGHT]);
        assert_eq!(prop(&ha, "process").as_deref(), Some("Night Dry"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Delicate"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "half_load").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "extra_dry").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EC_HALF]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Delicate"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("103"));
        assert_eq!(prop(&ha, "half_load").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "extra_dry").as_deref(), Some("OFF"));
        let (ha, _) = feed(&[EC_HALF_DRY]);
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("123"));
        assert_eq!(prop(&ha, "half_load").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "extra_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        let (ha, _) = feed(&[EC_DELICATE]);
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "half_load").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "extra_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("OFF"));
    }

    #[test]
    fn end_standby_and_night_dry_use_the_current_record() {
        let (ha, _) = feed(&[EC_OFF]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));

        let (ha, _) = feed(&[EC_DONE]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("194"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("194"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EC_END_IDLE]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EC_END_END]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("ON"));

        let (ha, _) = feed(&[EC_RINSING]);
        assert_eq!(prop(&ha, "process").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("92"));
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EC_DRYING]);
        assert_eq!(prop(&ha, "process").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("33"));
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("ON"));

        let (ha, _) = feed(&[EC_NIGHT]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Night Dry"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Heavy"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("ON"));
    }

    #[test]
    fn pause_delay_lock_and_hidden_courses_match_the_captures() {
        let (ha, _) = feed(&[EC_HIGH]);
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("170"));
        assert_eq!(prop(&ha, "high_temp").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "dual_zone").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "half_load").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "extra_dry").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EC_PAUSE]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Pause"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "high_temp").as_deref(), Some("ON"));
        let (ha, _) = feed(&[EC_RESUME]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EC_DELAY_SEL]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Turbo"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("59"));
        assert_eq!(prop(&ha, "delay_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "delay_start_time").as_deref(), Some("0"));
        let (ha, _) = feed(&[EC_DELAYED]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Delayed Start"));
        assert_eq!(prop(&ha, "delay_start_time").as_deref(), Some("59"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("59"));
        let (ha, _) = feed(&[EC_DELAYED_LATER]);
        assert_eq!(prop(&ha, "delay_start_time").as_deref(), Some("58"));
        assert_eq!(prop(&ha, "delay_start").as_deref(), Some("ON"));

        let (ha, _) = feed(&[EC_EXPRESS]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Express"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("34"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        let (ha, _) = feed(&[EC_LOCK]);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));

        let (ha, _) = feed(&[EC_CLEAN]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Machine Clean"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("82"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("OFF"));

        let (ha, _) = feed(&[EC_RINSE_LOCK]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Rinse"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("12"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        let (ha, _) = feed(&[EC_RINSE]);
        assert_eq!(prop(&ha, "process").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("OFF"));
    }

    #[test]
    fn counter_serial_and_the_long_frame() {
        let (ha, thinq) = feed(&[D8_40, D8_46, D8_0]);
        assert_eq!(prop(&ha, "tub_clean_counter").as_deref(), Some("0"));
        assert!(
            thinq.sent().is_empty(),
            "auto_ack is off, so 0xD8 is not acked"
        );
        let (ha, _) = feed(&[D8_40]);
        assert_eq!(prop(&ha, "tub_clean_counter").as_deref(), Some("40"));
        let (ha, _) = feed(&[D8_46]);
        assert_eq!(prop(&ha, "tub_clean_counter").as_deref(), Some("46"));

        let (ha, _) = feed(&[EC_LONG, SERIAL]);
        assert!(ha.device(ID).unwrap().properties.is_empty());

        let info = feed(&[]).0.device(ID).unwrap();
        assert_eq!(
            info.config.as_ref().unwrap().components["tub_clean_counter"]["state_class"],
            "measurement"
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("D30").is_some());
    }
}
