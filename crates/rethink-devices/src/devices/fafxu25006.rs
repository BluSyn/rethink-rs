//! FAFXU25006 — LG WM5800HVA front-load washer (long AABB class 0x20).
//!
//! Port of upstream master `300ab46`. 72-byte records, little-endian minutes,
//! inner type at buf[10]; the 0xEB/0xEC records arrive in the shared envelope.
//! Not aliasable to F/Y or RV13 washers.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const ENVELOPE_TYPE: u8 = 0x0a;
const INNER_TYPE_OFFSET: usize = 10;
const INNER_LEN_OFFSET: usize = 11;
const RECORD_LEN: usize = 72;
const RECORD_MARKER: u8 = 0x0a;
const DOOR_EVENT_TYPE: u8 = 0x03;
const DOOR_EVENT_INNER_LEN: u16 = 14;
const DOOR_EVENT_BUF_LEN: usize = 69;
const DOOR_EVENT_OFFSET: usize = 22;
const DOOR_OPEN: u8 = 1;
const DOOR_SHUT: u8 = 2;
const STATUS_QUERY: &str = "F0ED112101000000180411120000";
const SOIL_OFFSET: usize = 1;
const TEMP_OFFSET: usize = 2;
const RINSE_OFFSET: usize = 3;
const SPIN_OFFSET: usize = 4;
const COURSE_OFFSET: usize = 5;
const RESERVE_TIME_OFFSET: usize = 12;
const REMAIN_TIME_OFFSET: usize = 14;
const INITIAL_TIME_OFFSET: usize = 16;
const SPENT_POWER_OFFSET: usize = 18;
const STATE_OFFSET: usize = 21;
const LOAD_LEVEL_OFFSET: usize = 24;
const TUB_CLEAN_COUNT_OFFSET: usize = 28;
const BUZZER_OFFSET: usize = 29;
const EZ_DETERGENT_OFFSET: usize = 30;
const EZ_SOFTENER_OFFSET: usize = 31;
const OPT1_OFFSET: usize = 34;
const OPT1_TURBO_WASH: u8 = 0x20;
const OPT1_PRE_WASH: u8 = 0x40;
const OPT1_AUTO_SOAK: u8 = 0x80;
const OPT2_OFFSET: usize = 35;
const OPT2_STEAM: u8 = 0x10;
const OPT5_OFFSET: usize = 36;
const OPT5_FRESH_CARE: u8 = 0x40;
const OPT3_OFFSET: usize = 37;
const OPT3_CHILD_LOCK: u8 = 0x20;
const OPT3_DRUM_LIGHT: u8 = 0x40;
const DOOR_LOCK_OFFSET: usize = 38;
const OPT4_OFFSET: usize = 39;
const OPT4_DELAY: u8 = 0x80;
const OPT6_OFFSET: usize = 40;
const OPT6_REMOTE_MAINTAIN: u8 = 0x04;
const END_MELODY_OFFSET: usize = 69;
const STATE_POWEROFF: u8 = 0;
const LOAD_LEVEL_UNSET: u8 = 100;

fn le16(rec: &[u8], off: usize) -> i64 {
    i64::from(u16::from_le_bytes([rec[off], rec[off + 1]]))
}

fn named(v: Option<&'static str>) -> rethink_core::ha::PropertyValue {
    v.unwrap_or("None").into()
}

fn state_name(c: u8) -> Option<&'static str> {
    Some(match c {
        0 => "Off",
        1 | 27 => "Standby",
        2 | 28 => "Paused",
        3 => "Detecting",
        5 => "Filling",
        6 => "Detergent amount",
        7 => "Delayed",
        8 => "Soak",
        9 => "Pre-wash",
        11 | 29 => "Washing",
        12 => "Rinsing",
        13 => "Rinse hold",
        14 => "Spinning",
        15 => "Drying",
        16 => "End",
        21 => "Refreshing",
        23 => "Error",
        34 => "Audible diagnosis",
        35 => "Auto DT open pause",
        36 => "Confirm start for control",
        37 => "Clothing recognition",
        38 => "Detergent input",
        39 => "Softener input",
        40 => "Pollution detecting",
        41 => "Tub cleaning",
        42 => "End remote maintain on",
        43 => "Steam",
        47 => "Laundry care",
        48 => "EZDispense cleaning",
        52 => "Dry cooling",
        _ => return None,
    })
}

fn course_name(c: u8) -> Option<&'static str> {
    Some(match c {
        5 => "Allergiene", 6 => "ColdWash", 18 => "Color Care", 22 => "Delicates",
        27 => "Bedding", 35 => "Heavy Duty", 38 => "Jeans", 39 => "Kids Wear",
        46 => "Normal", 47 => "Overnight Wash", 48 => "Perm. Press", 55 => "Rinse & Spin",
        60 => "Sanitary", 68 => "Small Load", 74 => "Quick Wash", 78 => "Spin Only",
        84 => "Towels", 85 => "Tub Clean", 90 => "BrightWhites", 94 => "Hand/Wool",
        99 => "Activewear", 113 => "Sweat Stains", 114 => "AI Wash", 119 => "Pet Care",
        130 => "Swimwear", 131 => "Dresses", 132 => "Large Load", 133 => "XL Load",
        136 => "Microplastics Care",
        _ => return None,
    })
}

fn soil_name(c: u8) -> Option<&'static str> {
    match c { 1 => Some("Light"), 2 => Some("Light/Normal"), 3 => Some("Normal"), 4 => Some("Normal/Heavy"), 5 => Some("Heavy"), _ => None }
}
fn spin_name(c: u8) -> Option<&'static str> {
    match c { 12 => Some("Drain only"), 13 => Some("Low"), 14 => Some("Medium"), 15 => Some("High"), 16 => Some("Extra High"), _ => None }
}
fn temp_name(c: u8) -> Option<&'static str> {
    match c { 7 => Some("Tap Cold"), 8 => Some("Cold"), 9 => Some("Warm"), 10 => Some("Hot"), 11 => Some("Extra Hot"), _ => None }
}
fn rinse_name(c: u8) -> Option<&'static str> {
    match c { 14 => Some("Normal"), 15 => Some("Plus"), 16 => Some("Plus 2"), 17 => Some("Plus 3"), _ => None }
}
fn ez_name(c: u8) -> Option<&'static str> {
    match c { 0 => Some("Off"), 1 => Some("Less"), 2 => Some("Normal"), 3 => Some("More"), _ => None }
}
fn buzzer_name(c: u8) -> Option<&'static str> {
    match c { 0 => Some("Off"), 1 => Some("1"), 2 => Some("2"), 3 => Some("3"), 4 => Some("4"), _ => None }
}

const STATE_OPTS: &[&str] = &["Off","Standby","Paused","Detecting","Filling","Detergent amount","Delayed","Soak","Pre-wash","Washing","Rinsing","Rinse hold","Spinning","Drying","End","Refreshing","Error","Audible diagnosis","Auto DT open pause","Confirm start for control","Clothing recognition","Detergent input","Softener input","Pollution detecting","Tub cleaning","End remote maintain on","Steam","Laundry care","EZDispense cleaning","Dry cooling"];
const COURSE_OPTS: &[&str] = &["Allergiene","ColdWash","Color Care","Delicates","Bedding","Heavy Duty","Jeans","Kids Wear","Normal","Overnight Wash","Perm. Press","Rinse & Spin","Sanitary","Small Load","Quick Wash","Spin Only","Towels","Tub Clean","BrightWhites","Hand/Wool","Activewear","Sweat Stains","AI Wash","Pet Care","Swimwear","Dresses","Large Load","XL Load","Microplastics Care"];
const SOIL_OPTS: &[&str] = &["Light","Light/Normal","Normal","Normal/Heavy","Heavy"];
const SPIN_OPTS: &[&str] = &["Drain only","Low","Medium","High","Extra High"];
const TEMP_OPTS: &[&str] = &["Tap Cold","Cold","Warm","Hot","Extra Hot"];
const RINSE_OPTS: &[&str] = &["Normal","Plus","Plus 2","Plus 3"];
const EZ_OPTS: &[&str] = &["Off","Less","Normal","More"];
// Options order as upstream publishes it: JavaScript lists integer-like keys first.
const BUZZER_OPTS: &[&str] = &["1","2","3","4","Off"];

fn sensor_enum(id: &str, name: &str, icon: &str, opts: &[&str]) -> (String, serde_json::Value) {
    (id.into(), json!({"platform":"sensor","unique_id":format!("$deviceid-{id}"),"state_topic":format!("$this/{id}"),"name":name,"icon":icon,"device_class":"enum","options":opts}))
}
fn bin(id: &str, name: &str, icon: &str, extra: serde_json::Value) -> (String, serde_json::Value) {
    let mut v = json!({"platform":"binary_sensor","unique_id":format!("$deviceid-{id}"),"state_topic":format!("$this/{id}"),"name":name,"icon":icon});
    if let Some(obj) = extra.as_object() {
        for (k, val) in obj { v[k] = val.clone(); }
    }
    (id.into(), v)
}

pub struct Device { core: Arc<AabbDeviceCore> }

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let mut cfg = default_config(&meta, Some(json!({"name": "LG Washer"})));
        let comps = vec![
            bin("power", "Power", "mdi:washing-machine", json!({"device_class":"running"})),
            sensor_enum("status", "Status", "mdi:state-machine", STATE_OPTS),
            sensor_enum("course", "Course", "mdi:pin-outline", COURSE_OPTS),
            ("remaining_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
            ("initial_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
            ("energy".into(), json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","unit_of_measurement":"Wh","state_class":"total_increasing"})),
            ("reserve_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-reserve_time","state_topic":"$this/reserve_time","name":"Delay Wash time remaining","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min"})),
            bin("delay_wash", "Delay Wash", "mdi:clock-plus-outline", json!({})),
            ("door".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","device_class":"door"})),
            ("load_level".into(), json!({"platform":"sensor","unique_id":"$deviceid-load_level","state_topic":"$this/load_level","name":"Load level","icon":"mdi:weight","state_class":"measurement","entity_category":"diagnostic"})),
            ("tub_clean_count".into(), json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since drum clean","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"})),
            ("buzzer".into(), json!({"platform":"sensor","unique_id":"$deviceid-buzzer","state_topic":"$this/buzzer","name":"Buzzer","icon":"mdi:volume-high","device_class":"enum","options":BUZZER_OPTS,"entity_category":"diagnostic"})),
            bin("end_melody", "End melody", "mdi:music-note", json!({"entity_category":"diagnostic"})),
            bin("auto_soak", "Auto soak", "mdi:water-outline", json!({})),
            bin("fresh_care", "FreshCare", "mdi:air-filter", json!({})),
            bin("child_lock", "Child lock", "mdi:lock", json!({"entity_category":"diagnostic"})),
            bin("remote_maintain", "Remote maintain", "mdi:cellphone-wireless", json!({"entity_category":"diagnostic"})),
            sensor_enum("ez_dispense_detergent", "EZDispense detergent", "mdi:cup-water", EZ_OPTS),
            sensor_enum("ez_dispense_softener", "EZDispense softener", "mdi:cup-water", EZ_OPTS),
            bin("drum_light", "Drum light", "mdi:lightbulb", json!({"entity_category":"diagnostic"})),
            sensor_enum("soil", "Soil level", "mdi:liquid-spot", SOIL_OPTS),
            sensor_enum("spin", "Spin", "mdi:autorenew", SPIN_OPTS),
            sensor_enum("temp", "Temperature", "mdi:thermometer", TEMP_OPTS),
            sensor_enum("rinse", "Rinse", "mdi:water-sync", RINSE_OPTS),
            bin("turbo_wash", "TurboWash", "mdi:rocket-launch", json!({})),
            bin("pre_wash", "Pre-wash", "mdi:water-sync", json!({})),
            bin("steam", "Steam", "mdi:kettle-steam", json!({})),
            bin("door_lock", "Door lock", "mdi:lock", json!({"entity_category":"diagnostic"})),
        ];
        cfg.components = comps.into_iter().collect();
        core.set_config(cfg);
        let this = Arc::new(Self { core: core.clone() });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_enveloped_status(buf, CLASS_BYTE, RECORD_LEN) {
            self.process_status(rec);
            return;
        }
        if buf.len() < 13 || buf[0] != CLASS_BYTE || buf[1] != ENVELOPE_TYPE {
            return;
        }
        // 0x00/0x02/0x04/0x4d/0xe2 are not status records and are not decoded.
        let inner_len = u16::from_be_bytes([buf[INNER_LEN_OFFSET], buf[INNER_LEN_OFFSET + 1]]);
        if buf[INNER_TYPE_OFFSET] != DOOR_EVENT_TYPE
            || inner_len != DOOR_EVENT_INNER_LEN
            || buf.len() != DOOR_EVENT_BUF_LEN
        {
            return;
        }
        let door = buf[DOOR_EVENT_OFFSET];
        if door == DOOR_OPEN || door == DOOR_SHUT {
            self.core.publish_on_off("door", door == DOOR_OPEN);
        }
    }

    fn process_status(&self, rec: &[u8]) {
        if rec[0] != RECORD_MARKER { return; }
        let state = rec[STATE_OFFSET];
        let is_off = state == STATE_POWEROFF;
        self.core.publish_on_off("power", !is_off);
        self.core.publish_property("status", named(state_name(state)));
        self.core.publish_property("course", named(course_name(rec[COURSE_OFFSET])));
        self.core.publish_property("soil", named(soil_name(rec[SOIL_OFFSET])));
        self.core.publish_property("spin", named(spin_name(rec[SPIN_OFFSET])));
        self.core.publish_property("temp", named(temp_name(rec[TEMP_OFFSET])));
        self.core.publish_property("rinse", named(rinse_name(rec[RINSE_OFFSET])));
        self.core.publish_property("remaining_time", if is_off { 0i64 } else { le16(rec, REMAIN_TIME_OFFSET) }.into());
        self.core.publish_property("initial_time", if is_off { 0i64 } else { le16(rec, INITIAL_TIME_OFFSET) }.into());
        self.core.publish_property("energy", le16(rec, SPENT_POWER_OFFSET).into());
        self.core.publish_property("reserve_time", le16(rec, RESERVE_TIME_OFFSET).into());
        self.core.publish_on_off("delay_wash", rec[OPT4_OFFSET] & OPT4_DELAY != 0);
        self.core.publish_on_off("drum_light", rec[OPT3_OFFSET] & OPT3_DRUM_LIGHT != 0);
        let load = rec[LOAD_LEVEL_OFFSET];
        if load == LOAD_LEVEL_UNSET {
            self.core.publish_property("load_level", "None".into());
        } else {
            self.core.publish_property("load_level", i64::from(load).into());
        }
        self.core.publish_property("tub_clean_count", i64::from(rec[TUB_CLEAN_COUNT_OFFSET]).into());
        self.core.publish_property("buzzer", named(buzzer_name(rec[BUZZER_OFFSET])));
        self.core.publish_property("ez_dispense_detergent", named(ez_name(rec[EZ_DETERGENT_OFFSET])));
        self.core.publish_property("ez_dispense_softener", named(ez_name(rec[EZ_SOFTENER_OFFSET])));
        self.core.publish_on_off("end_melody", rec[END_MELODY_OFFSET] != 0);
        let opt1 = rec[OPT1_OFFSET];
        self.core.publish_on_off("turbo_wash", opt1 & OPT1_TURBO_WASH != 0);
        self.core.publish_on_off("pre_wash", opt1 & OPT1_PRE_WASH != 0);
        self.core.publish_on_off("auto_soak", opt1 & OPT1_AUTO_SOAK != 0);
        self.core.publish_on_off("fresh_care", rec[OPT5_OFFSET] & OPT5_FRESH_CARE != 0);
        self.core.publish_on_off("child_lock", rec[OPT3_OFFSET] & OPT3_CHILD_LOCK != 0);
        self.core.publish_on_off("remote_maintain", rec[OPT6_OFFSET] & OPT6_REMOTE_MAINTAIN != 0);
        self.core.publish_on_off("steam", rec[OPT2_OFFSET] & OPT2_STEAM != 0);
        self.core.publish_on_off("door_lock", rec[DOOR_LOCK_OFFSET] != 0);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str { &self.core.id }
    fn start(&self) { self.core.send(&hex_decode(STATUS_QUERY)); }
    fn drop_device(&self) { self.core.drop_device(); }
    fn set_property(&self, _prop: &str, _value: &str) {}
    fn publish_config(&self) { self.core.republish_config(); }
}

pub fn create(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::t2_factory;
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("FAFXU25006", Device::new)
    }

    /// A fresh device fed `frames` in order, like upstream's `feed()`.
    fn feed(frames: &[&str]) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        ha
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    // Real frames from upstream's tests, each cross-checked there against the LG cloud.
    const EB_POWER_OFF: &str =
        "aaff200a005a000fa8000100eb00480aff00ffff1b0000000000000000010000007f001b001000020000ff2004ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000e45bb";
    const NORMAL_IDLE: &str =
        "aaff200a00a2001002000100ec00900aff000e000000000000000000000100000001002e0001006400000e2004ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000002700270000002e0101006400000e2004ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff01000049b8bb";
    const SPEED_WASH: &str =
        "aaff200a00a2001005000100ec00900a03090e0f2e00000000000000001d001d0000002e0101000200000e2004ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a010a0e104a00000000000000000e000e0000004a0101000200000e2004ffff2d3c000000420004300000000000000400004000000000000000f00064000400000000ffff0100003e62bb";
    const HEAVY_DUTY: &str =
        "aaff200a00a2001006000100ec00900a010a0e104a00000000000000000e000e0000004a0101000200000e2004ffff2d3c000000420004300000000000000400004000000000000000f00064000400000000ffff0100000a05090e102300000000000000006c006c000000230101000200000e2004ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100005323bb";
    const TUB_CLEAN: &str =
        "aaff200a00a2001009000100ec00900a03090e10540000000000000000380038000000540101000200000e2004ffff2d3c000000420004300000000000000400004000000000000000f00064000400000000ffff0100000aff000e0e550000000000000000570057000000550101000200000e2004ffff2d3c001000420004300000000000000400004000000000000000f00064000400000000ffff010000d18ebb";
    const SPIN_ONLY: &str =
        "aaff200a00a200100a000100ec00900aff000e0e550000000000000000570057000000550101000200000e2004ffff2d3c001000420004300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0f4e00000000000000000b000b0000004e010100020000ff2004ffff2d3c000000420004300000000000000400004000000000000000f00064000400000000ffff0100001641bb";
    const PET_CARE: &str =
        "aaff200a00a200100e000100ec00900a03090e0f8800000000000000003a003a000000880101000200000e2004ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a030b0e0e770000000000000000760076000000770101000200000e2004ffff2d3c400000420004300000000000000400004000000000000000f00064000400000000ffff0100005f27bb";
    const ALLERGY_CARE: &str =
        "aaff200a00a200100f000100ec00900a030b0e0e770000000000000000760076000000770101000200000e2004ffff2d3c400000420004300000000000000400004000000000000000f00064000400000000ffff0100000aff000e0f050000000000000000630063000000050101000200000e2004ffff2d3c001000420004300000000000000400004000000000000000f00064000400000000ffff0100007d45bb";
    const RINSE_PLUS3: &str =
        "aaff200a00a200104a000100ec00900a010810102e00000000000000003700370002002e010100020000102004ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a010811102e00000000000000003d003d0002002e010100020000112004ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100005c9abb";
    const STARTS_RUNNING: &str =
        "aaff200a00a2001057000100ec00900a03070e0e1b00000000000000003600360002001b0101000200000e2004ffff2d3c000000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03070e0e1b00000000000000003600360000001b0b01000200000e200400002d3c000000100004300000000000000400004000000000000000f00064000400000000ffff01000038d4bb";
    const DOOR_LOCKS: &str =
        "aaff200a00a2001058000100ec00900a03070e0e1b00000000000000003600360000001b0b01000200000e200400002d3c000000100004300000000000000400004000000000000000f00064000400000000ffff0100000a03070e0e1b00000000000000003600360000001b0b0b000200000e200400002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff010000486dbb";
    const RUNNING_COUNTDOWN: &str =
        "aaff200a00a200105a000100ec00900a03070e0e1b00000000000000003500360000001b0b0b000200000e200400002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000a03070e0e1b00000000000000003400360001001b0b0b000200000e200400002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff010000a195bb";
    const RINSING: &str =
        "aaff200a00a2000e6e000100ec00900a03080e0e1b00000000000000001d00360039001b0b0b000200000e1f0400002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff000e0e1b00000000000000001c0036003b001b0c0b000200000e1f04ff002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100007e18bb";
    const SPINNING: &str =
        "aaff200a00a2000f5c000100ec00900aff000e0e1b0000000000000000070036006f001b0c0c000200000e1f04ff002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0e1b00000000000000000600360071001b0e0c00020000ff1f04ffff2d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100007d4ebb";
    const EB_ENDS: &str =
        "aaff200a005a000df6000100eb00480aff00ffff54000000000000000001000000630054100e00010000ff1f04ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff010000bcd7bb";
    const PAUSES: &str =
        "aaff200a00a20011ef000100ec00900aff00ff0e1b00000000000000000500360086001b0e0e00020000ff2004ffff2d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0e1b00000000000000000500360086001b020e00020000ff2004ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff0100000934bb";
    const UNLOCKS_ON_PAUSE: &str =
        "aaff200a00a20011f0000100ec00900aff00ff0e1b00000000000000000500360086001b020e00020000ff2004ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0e1b00000000000000000500360086001b020200020000ff2004ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff010000c9e9bb";
    const RESUMES: &str =
        "aaff200a00a20011f5000100ec00900aff00ff0e1b00000000000000000500360086001b020200020000ff2004ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0e1b00000000000000000500360086001b0e0200020000ff2004ffff2d3c000000100004300000000000000400004000000000000000f00064000400000000ffff010000ef01bb";
    const ENDS: &str =
        "aaff200a00a2001223000100ec00900aff00ff0e1b0000000000000000010036008f001b0e0e00020000ff2004ffff2d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff1b00000000000000000100000090001b100e00020000ff2104ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff010000d0a1bb";
    const UNLOCKS_AT_END: &str =
        "aaff200a00a2001224000100ec00900aff00ffff1b00000000000000000100000090001b100e00020000ff2104ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff1b00000000000000000100000090001b101000020000ff2104ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100009031bb";
    const POWERS_OFF: &str =
        "aaff200a00a2001225000100ec00900aff00ffff1b00000000000000000100000090001b101000020000ff2104ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff1b00000000000000000100000090001b001000020000ff2104ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff010000af44bb";
    const DELAY_WASH: &str =
        "aaff200a00a2001273000100ec00900a03090e0f2e00000000000000001d001d0000002e0101000200000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000003c001d001d0000002e0101000200000e2104ffff2d3c200000420084300000000000000400004000000000000000f00064000400000000ffff0100002141bb";
    const DELAY_STARTS: &str =
        "aaff200a00a200134f000100ec00900a03090e0f2e0000000000003c003500350000002e0101000500000e2204ffff2d3c200000420084300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000003c003500350000002e0701000500000e220400002d3c200000100084300000000000000400004000000000000000f00064000400000000ffff0100001c7dbb";
    const DELAY_COUNTS_DOWN: &str =
        "aaff200a00a2001377000100ec00900a03090e0f2e0000000000001f003500350005002e0707000500000e220400002d3c200000100184300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000001e003500350005002e0707000500000e220400002d3c200000100184300000000000000400004000000000000000f00064000400000000ffff010000e3abbb";
    const DELAY_FIRES: &str =
        "aaff200a00a200139e000100ec00900a03090e0f2e0000000000000100350035000b002e0707000500000e220400002d3c200000100184300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000000000350035000b002e0b07000500000e220400002d3c200000100104300000000000000400004000000000000000f00064000400000000ffff010000c095bb";
    const SPIN_STARTS: &str =
        "aaff200a00a20013d7000100ec00900aff000e0f2e0000000000000000120035002a002e0c0c000500000e2204ff002d3c200000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0f2e0000000000000000110035002b002e0e0c00050000ff2204ffff2d3c200000100104300000000000000400004000000000000000f00064000400000000ffff0100003fbfbb";
    const CYCLE_ENDS: &str =
        "aaff200a00a20013fb000100ec00900aff00ff0f2e0000000000000000010035004e002e0e0e00050000ff2204ffff2d3c200000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff2e00000000000000000100000053002e100e00050000ff2304ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff010000af56bb";
    const POWERS_OFF_AT_END: &str =
        "aaff200a00a20013fe000100ec00900aff00ffff2e00000000000000000100000053002e101000050000ff2304ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff2e00000000000000000100000053002e001000050000ff2304ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff01000077d3bb";
    const DRUM_LIGHT_OFF: &str =
        "aaff200a00a2001278000100ec00900a03090e0f2e0000000000003c001d001d0000002e0101000200000e2104ffff2d3c200000420084300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000003c001d001d0001002e0101000200000e2104ffff2d3c200000020084300000000000000400004000000000000000f00064000400000000ffff010000d1c0bb";
    const LOAD_WEIGHED: &str =
        "aaff200a00a200123b000100ec00900a03090e0f2e00000000000000002700270000002e0101006400000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0000002e0101000200000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff010000ab21bb";
    const AUTO_SOAK_ON: &str =
        "aaff200a00a20012b2000100ec00900a03090e0f2e00000000000000001d001d0000002e0101000200000e2104ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000003b003b0000002e0101000200000e2104ffff2d3ca00000020004300000000000000400004000000000000000f00064000400000000ffff010000bc8dbb";
    const FRESH_CARE_ON: &str =
        "aaff200a00a20012b5000100ec00900a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200040020004300000000000000400004000000000000000f00064000400000000ffff0100007bf6bb";
    const BUZZER_3: &str =
        "aaff200a00a20012bb000100ec00900a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0001002e0101000200000e2103ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff01000011efbb";
    const BUZZER_OFF: &str =
        "aaff200a00a20012be000100ec00900a03090e0f2e00000000000000001d001d0001002e0101000200000e2101ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0001002e0101000200000e2100ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0000007d39bb";
    const CHILD_LOCK_ON: &str =
        "aaff200a00a20012c5000100ec00900a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200000620004300000000000000400004000000000000000f00064000400000000ffff010000b699bb";
    const REMOTE_MAINTAIN_ON: &str =
        "aaff200a00a200127c000100ec00900a03090e0f2e0000000000003c001d001d0001002e0101000200000e2104ffff2d3c200000020084300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000003c001d001d0002002e0101000200000e2104ffff2d3c200000020084340000000000000400004000000000000000f00064000400000000ffff0100000314bb";
    const DETECTING: &str =
        "aaff200a00a20012e1000100ec00900a03090e0f2e00000000000000002700270000002e0101006400000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000002700270000002e0301006400000e210400002d3c200000100004300000000000000400004000000000000000f00064000400000000ffff0100002a7cbb";
    const DOOR_OPENS: &str =
        "aaff200a004900141c00020103000e610f0201610840010001000f02010105002546414658553235303036000000000000000000000102c586a2f40a09000000000000000000bd49bb";
    const DOOR_SHUTS: &str =
        "aaff200a004900141d00020103000e610f0201610841010002000f02010105002546414658553235303036000000000000000000000102c686a2f40a0900000000000000000071febb";

    #[test]
    fn selected_course_decodes_with_its_options() {
        let p = feed(&[NORMAL_IDLE]);
        expect(&p, "power", "ON");
        expect(&p, "status", "Standby");
        expect(&p, "course", "Normal");
        expect(&p, "soil", "Normal");
        expect(&p, "temp", "Warm");
        expect(&p, "spin", "High");
        expect(&p, "rinse", "Normal");
        expect(&p, "turbo_wash", "ON");
        expect(&p, "pre_wash", "OFF");
        expect(&p, "steam", "OFF");
        expect(&p, "door_lock", "OFF");
    }

    #[test]
    fn minute_counters_are_little_endian() {
        let p = feed(&[NORMAL_IDLE]);
        expect(&p, "remaining_time", "39");
        expect(&p, "initial_time", "39");

        let heavy = feed(&[HEAVY_DUTY]);
        expect(&heavy, "remaining_time", "108");
        expect(&heavy, "initial_time", "108");
    }

    #[test]
    fn course_table_covers_the_selector() {
        expect(&feed(&[SPEED_WASH]), "course", "Quick Wash");
        expect(&feed(&[HEAVY_DUTY]), "course", "Heavy Duty");
        expect(&feed(&[TUB_CLEAN]), "course", "Tub Clean");
        expect(&feed(&[SPIN_ONLY]), "course", "Spin Only");
        expect(&feed(&[PET_CARE]), "course", "Pet Care");
        expect(&feed(&[ALLERGY_CARE]), "course", "Allergiene");
    }

    #[test]
    fn soil_temp_spin_and_rinse_read_their_own_bytes() {
        let speed = feed(&[SPEED_WASH]);
        expect(&speed, "soil", "Light");
        expect(&speed, "temp", "Hot");
        expect(&speed, "spin", "Extra High");

        let heavy = feed(&[HEAVY_DUTY]);
        expect(&heavy, "soil", "Heavy");
        expect(&heavy, "temp", "Warm");

        let pet = feed(&[PET_CARE]);
        expect(&pet, "temp", "Extra Hot");
        expect(&pet, "spin", "Medium");

        expect(&feed(&[RINSE_PLUS3]), "rinse", "Plus 3");
    }

    #[test]
    fn not_applicable_sentinels_report_unknown() {
        let tub = feed(&[TUB_CLEAN]);
        expect(&tub, "soil", "None");
        expect(&tub, "temp", "None");
        expect(&feed(&[SPIN_ONLY]), "rinse", "None");
    }

    #[test]
    fn steam_and_pre_wash_do_not_collide() {
        let tub = feed(&[TUB_CLEAN]);
        expect(&tub, "steam", "ON");
        expect(&tub, "pre_wash", "OFF");
        expect(&tub, "turbo_wash", "OFF");

        let pet = feed(&[PET_CARE]);
        expect(&pet, "pre_wash", "ON");
        expect(&pet, "steam", "OFF");
        expect(&pet, "turbo_wash", "OFF");

        let allergy = feed(&[ALLERGY_CARE]);
        expect(&allergy, "steam", "ON");
        expect(&allergy, "pre_wash", "OFF");
    }

    #[test]
    fn turbo_wash_shares_the_pre_wash_byte() {
        expect(&feed(&[NORMAL_IDLE]), "turbo_wash", "ON");
        expect(&feed(&[HEAVY_DUTY]), "turbo_wash", "ON");
        expect(&feed(&[SPEED_WASH]), "turbo_wash", "OFF");
    }

    #[test]
    fn starting_moves_the_state_then_locks_the_door() {
        let started = feed(&[NORMAL_IDLE, STARTS_RUNNING]);
        expect(&started, "status", "Washing");
        expect(&started, "door_lock", "OFF");

        let locked = feed(&[NORMAL_IDLE, STARTS_RUNNING, DOOR_LOCKS]);
        expect(&locked, "door_lock", "ON");
        expect(&locked, "status", "Washing");
    }

    #[test]
    fn countdown_falls_while_cycle_length_stays() {
        let p = feed(&[RUNNING_COUNTDOWN]);
        expect(&p, "remaining_time", "52");
        expect(&p, "initial_time", "54");
        expect(&p, "energy", "1");
    }

    #[test]
    fn later_phases_decode_from_the_state_byte() {
        expect(&feed(&[RINSING]), "status", "Rinsing");
        expect(&feed(&[SPINNING]), "status", "Spinning");
    }

    #[test]
    fn eb_single_record_frames_decode_with_the_ec_offsets() {
        let off = feed(&[EB_POWER_OFF]);
        expect(&off, "power", "OFF");
        expect(&off, "status", "Off");
        expect(&off, "course", "Bedding");
        expect(&off, "remaining_time", "0");
        expect(&off, "energy", "127");

        expect(&feed(&[EB_ENDS]), "status", "End");
    }

    #[test]
    fn pause_and_resume_move_state_and_door_lock() {
        let paused = feed(&[PAUSES]);
        expect(&paused, "status", "Paused");
        expect(&paused, "remaining_time", "5");
        expect(&paused, "door_lock", "ON");

        let unlocked = feed(&[PAUSES, UNLOCKS_ON_PAUSE]);
        expect(&unlocked, "door_lock", "OFF");
        expect(&unlocked, "status", "Paused");

        let resumed = feed(&[PAUSES, UNLOCKS_ON_PAUSE, RESUMES]);
        expect(&resumed, "status", "Spinning");
        expect(&resumed, "remaining_time", "5");
    }

    #[test]
    fn cycle_end_unlocks_then_powers_off() {
        let ended = feed(&[ENDS]);
        expect(&ended, "status", "End");
        expect(&ended, "energy", "144");
        expect(&ended, "initial_time", "0");
        expect(&ended, "door_lock", "ON");

        expect(&feed(&[ENDS, UNLOCKS_AT_END]), "door_lock", "OFF");

        let off = feed(&[ENDS, UNLOCKS_AT_END, POWERS_OFF]);
        expect(&off, "power", "OFF");
        expect(&off, "status", "Off");
        expect(&off, "remaining_time", "0");
        expect(&off, "energy", "144");
    }

    #[test]
    fn delay_wash_publishes_countdown_and_flag() {
        let p = feed(&[DELAY_WASH]);
        expect(&p, "delay_wash", "ON");
        expect(&p, "reserve_time", "60");
        expect(&p, "remaining_time", "29");
        expect(&p, "initial_time", "29");
        expect(&p, "status", "Standby");

        let plain = feed(&[NORMAL_IDLE]);
        expect(&plain, "delay_wash", "OFF");
        expect(&plain, "reserve_time", "0");
    }

    #[test]
    fn delay_counts_down_once_reserved() {
        expect(&feed(&[DELAY_WASH]), "status", "Standby");

        let started = feed(&[DELAY_WASH, DELAY_STARTS]);
        expect(&started, "status", "Delayed");
        expect(&started, "delay_wash", "ON");
        expect(&started, "reserve_time", "60");
        expect(&started, "drum_light", "OFF");
        expect(&started, "remaining_time", "53");
        expect(&started, "initial_time", "53");

        let later = feed(&[DELAY_STARTS, DELAY_COUNTS_DOWN]);
        expect(&later, "status", "Delayed");
        expect(&later, "reserve_time", "30");
        expect(&later, "remaining_time", "53");
        expect(&later, "door_lock", "ON");
    }

    #[test]
    fn delay_firing_clears_flag_and_starts_wash() {
        let p = feed(&[DELAY_COUNTS_DOWN, DELAY_FIRES]);
        expect(&p, "status", "Washing");
        expect(&p, "delay_wash", "OFF");
        expect(&p, "reserve_time", "0");
        expect(&p, "remaining_time", "53");
        expect(&p, "initial_time", "53");
        expect(&p, "door_lock", "ON");
    }

    #[test]
    fn dispensers_dash_in_their_own_phase() {
        let p = feed(&[SPIN_STARTS]);
        expect(&p, "status", "Spinning");
        expect(&p, "remaining_time", "17");
        expect(&p, "ez_dispense_softener", "None");
        expect(&p, "ez_dispense_detergent", "None");
    }

    #[test]
    fn finished_cycle_reports_end_then_powers_off() {
        let ended = feed(&[SPIN_STARTS, CYCLE_ENDS]);
        expect(&ended, "status", "End");
        expect(&ended, "power", "ON");
        expect(&ended, "initial_time", "0");
        expect(&ended, "remaining_time", "1");
        expect(&ended, "energy", "83");
        expect(&ended, "drum_light", "ON");

        let off = feed(&[CYCLE_ENDS, POWERS_OFF_AT_END]);
        expect(&off, "status", "Off");
        expect(&off, "power", "OFF");
        expect(&off, "door_lock", "OFF");
        expect(&off, "energy", "83");
    }

    #[test]
    fn tub_clean_counter_steps_as_a_cycle_ends() {
        expect(&feed(&[SPIN_STARTS]), "tub_clean_count", "34");
        expect(&feed(&[SPIN_STARTS, CYCLE_ENDS]), "tub_clean_count", "35");
    }

    #[test]
    fn door_is_announced_in_its_own_frame() {
        expect(&feed(&[DOOR_OPENS]), "door", "ON");
        expect(&feed(&[DOOR_SHUTS]), "door", "OFF");

        let with_status = feed(&[NORMAL_IDLE, DOOR_OPENS]);
        let status_only = feed(&[NORMAL_IDLE]);
        expect(&with_status, "door", "ON");
        assert_eq!(prop(&with_status, "status"), prop(&status_only, "status"));
        assert_eq!(prop(&with_status, "course"), prop(&status_only, "course"));

        expect(&feed(&[DOOR_OPENS, NORMAL_IDLE]), "door", "ON");
    }

    #[test]
    fn drum_light_is_rec37_bit_0x40() {
        expect(&feed(&[DELAY_WASH]), "drum_light", "ON");

        let off = feed(&[DELAY_WASH, DRUM_LIGHT_OFF]);
        expect(&off, "drum_light", "OFF");
        expect(&off, "energy", "1");
        expect(&off, "delay_wash", "ON");
    }

    #[test]
    fn load_level_is_unknown_until_weighed() {
        expect(&feed(&[LOAD_WEIGHED]), "load_level", "2");
        expect(&feed(&[NORMAL_IDLE]), "load_level", "None");
    }

    #[test]
    fn second_settings_pass_isolates_one_option_per_byte() {
        let soak = feed(&[AUTO_SOAK_ON]);
        expect(&soak, "auto_soak", "ON");
        expect(&soak, "turbo_wash", "ON");
        expect(&soak, "pre_wash", "OFF");

        let fresh = feed(&[FRESH_CARE_ON]);
        expect(&fresh, "fresh_care", "ON");
        expect(&fresh, "auto_soak", "OFF");

        let lock = feed(&[CHILD_LOCK_ON]);
        expect(&lock, "child_lock", "ON");
        expect(&lock, "drum_light", "ON");

        let maintain = feed(&[REMOTE_MAINTAIN_ON]);
        expect(&maintain, "remote_maintain", "ON");
        expect(&maintain, "delay_wash", "ON");
        expect(&maintain, "reserve_time", "60");
    }

    #[test]
    fn buzzer_scale_and_end_melody() {
        expect(&feed(&[BUZZER_3]), "buzzer", "3");
        expect(&feed(&[AUTO_SOAK_ON]), "buzzer", "4");

        let off = feed(&[BUZZER_OFF]);
        expect(&off, "buzzer", "Off");
        expect(&off, "end_melody", "OFF");
        expect(&feed(&[BUZZER_3]), "end_melody", "ON");
    }

    #[test]
    fn panel_start_sensing_the_load_reports_detecting() {
        let p = feed(&[DETECTING]);
        expect(&p, "status", "Detecting");
        expect(&p, "course", "Normal");
    }

    #[test]
    fn ez_dispense_leaves_its_sentinel_when_a_cycle_begins() {
        let idle = feed(&[NORMAL_IDLE]);
        expect(&idle, "ez_dispense_detergent", "None");
        expect(&idle, "ez_dispense_softener", "None");

        let detecting = feed(&[DETECTING]);
        expect(&detecting, "ez_dispense_detergent", "Off");
        expect(&detecting, "ez_dispense_softener", "Off");
    }

    #[test]
    fn start_requests_a_status_snapshot() {
        let (_, thinq, dev) = make();
        DeviceHandler::start(dev.as_ref());
        let sent: Vec<String> = thinq.outbox().iter().map(|b| hex_encode(b)).collect();
        assert_eq!(sent, ["AA12F0ED1121010000001804111200005EBB"]);
    }

    #[test]
    fn non_status_frames_publish_nothing() {
        for junk in [
            "aa07201902b9bb",
            "aa0720c302c3bb",
            "aaff200a0049000fff00020103000e610f0201",
            "aaff200a00a2001002000100ec00900aff000e00000000bb",
        ] {
            let p = feed(&[junk]);
            assert!(p.device(DEVICE_ID).unwrap().properties.is_empty(), "{junk}");
        }
    }

    #[test]
    fn envelope_may_omit_the_trailing_checksum_byte() {
        // AA FF frames carry one checksum byte past the records; the envelope parser allows 0 or 1.
        let frame = hex_decode(EB_POWER_OFF);
        let mut short = frame[..frame.len() - 3].to_vec();
        short.extend_from_slice(&frame[frame.len() - 2..]);
        expect(&feed(&[&hex_encode(&short)]), "status", "Off");

        let mut long = frame[..frame.len() - 2].to_vec();
        long.extend_from_slice(&[0x00, 0x00, 0xbb]);
        let p = feed(&[&hex_encode(&long)]);
        assert!(p.device(DEVICE_ID).unwrap().properties.is_empty());
    }

    #[test]
    fn diagnostics_and_option_order_follow_upstream() {
        let (ha, _, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(comps["buzzer"]["options"], json!(["1", "2", "3", "4", "Off"]));
        assert_eq!(comps["buzzer"]["entity_category"], "diagnostic");
        assert_eq!(comps["door_lock"]["entity_category"], "diagnostic");
        assert!(comps["door"].get("icon").is_none());
    }

    #[test]
    fn registry_is_not_an_alias_of_other_washers() {
        let a = t2_factory("FAFXU25006").expect("FAFXU25006");
        let b = t2_factory("F_C__Y___W.A__QEUK").expect("F_C washer");
        let c = t2_factory("RV13U6AM8W_D_US_WIFI").expect("RV13 washer");
        assert!(!std::ptr::fn_addr_eq(a, b));
        assert!(!std::ptr::fn_addr_eq(a, c));
    }
}
