//! FAFXU25006 — LG WM5800HVA front-load washer (long AABB class 0x20).
//!
//! Port of upstream master `300ab46`. 72-byte records, little-endian minutes,
//! inner type at buf[10]. Not aliasable to F/Y or RV13 washers.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
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
const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_INNER_LEN: u16 = 2 * RECORD_LEN as u16;
const STATUS_RECORD_OFFSET: usize = 13 + RECORD_LEN;
const STATUS_BUF_LEN: usize = 13 + 2 * RECORD_LEN + 1;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_INNER_LEN: u16 = RECORD_LEN as u16;
const SINGLE_RECORD_OFFSET: usize = 13;
const SINGLE_BUF_LEN: usize = 13 + RECORD_LEN + 1;
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
const BUZZER_OPTS: &[&str] = &["Off","1","2","3","4"];

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
            bin("door", "Door", "mdi:door", json!({"device_class":"door"})),
            ("load_level".into(), json!({"platform":"sensor","unique_id":"$deviceid-load_level","state_topic":"$this/load_level","name":"Load level","icon":"mdi:weight","state_class":"measurement","entity_category":"diagnostic"})),
            ("tub_clean_count".into(), json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since drum clean","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"})),
            sensor_enum("buzzer", "Buzzer", "mdi:volume-high", BUZZER_OPTS),
            bin("end_melody", "End melody", "mdi:music-note", json!({"entity_category":"diagnostic"})),
            bin("auto_soak", "Auto soak", "mdi:water-outline", json!({})),
            bin("fresh_care", "FreshCare", "mdi:air-filter", json!({})),
            bin("child_lock", "Child lock", "mdi:lock", json!({"entity_category":"diagnostic"})),
            bin("remote_maintain", "Remote maintain", "mdi:cellphone-wireless", json!({"entity_category":"diagnostic"})),
            sensor_enum("ez_dispense_detergent", "EZDispense detergent", "mdi:cup-water", EZ_OPTS),
            sensor_enum("ez_dispense_softener", "EZDispense softener", "mdi:cup-water", EZ_OPTS),
            bin("drum_light", "Drum light", "mdi:lightbulb", json!({"entity_category":"diagnostic"})),
            sensor_enum("soil", "Soil level", "mdi:liquid-spot", SOIL_OPTS),
            sensor_enum("spin", "Spin", "mdi:rotate-right", SPIN_OPTS),
            sensor_enum("temp", "Temperature", "mdi:thermometer", TEMP_OPTS),
            sensor_enum("rinse", "Rinse", "mdi:water", RINSE_OPTS),
            bin("turbo_wash", "TurboWash", "mdi:rocket-launch", json!({})),
            bin("pre_wash", "Pre-wash", "mdi:water-plus", json!({})),
            bin("steam", "Steam", "mdi:kettle-steam", json!({})),
            bin("door_lock", "Door lock", "mdi:lock", json!({})),
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
        if buf.len() < 13 || buf[0] != CLASS_BYTE || buf[1] != ENVELOPE_TYPE { return; }
        let inner_len = u16::from_be_bytes([buf[INNER_LEN_OFFSET], buf[INNER_LEN_OFFSET + 1]]);
        match buf[INNER_TYPE_OFFSET] {
            STATUS_FRAME_TYPE if inner_len == STATUS_INNER_LEN => {
                self.process_status(buf, STATUS_RECORD_OFFSET, STATUS_BUF_LEN);
            }
            SINGLE_STATUS_FRAME_TYPE if inner_len == SINGLE_INNER_LEN => {
                self.process_status(buf, SINGLE_RECORD_OFFSET, SINGLE_BUF_LEN);
            }
            DOOR_EVENT_TYPE if inner_len == DOOR_EVENT_INNER_LEN && buf.len() == DOOR_EVENT_BUF_LEN => {
                let door = buf[DOOR_EVENT_OFFSET];
                if door == DOOR_OPEN { self.core.publish_on_off("door", true); }
                else if door == DOOR_SHUT { self.core.publish_on_off("door", false); }
            }
            _ => {}
        }
    }

    fn process_status(&self, buf: &[u8], record_offset: usize, expected_len: usize) {
        if buf.len() != expected_len { return; }
        let rec = &buf[record_offset..record_offset + RECORD_LEN];
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
#[allow(dead_code)]
mod tests {
    use super::*;
    use crate::device_trait::DeviceHandler;
    use crate::registry::t2_factory;
    use crate::test_support::{make_t2, outbox_has_inner, prop};
    use rethink_core::{hex_decode, hex_encode, Thinq2Device};

    fn make() -> (Arc<rethink_core::MockHaConnection>, Arc<rethink_core::MockThinq2Device>, Arc<Device>) {
        make_t2("FAFXU25006", Device::new)
    }


    const EB_POWER_OFF: &str = "aaff200a005a000fa8000100eb00480aff00ffff1b0000000000000000010000007f001b001000020000ff2004ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000e45bb";
    const NORMAL_IDLE: &str = "aaff200a00a2001002000100ec00900aff000e000000000000000000000100000001002e0001006400000e2004ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000002700270000002e0101006400000e2004ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff01000049b8bb";
    const SPEED_WASH: &str = "aaff200a00a2001005000100ec00900a03090e0f2e00000000000000001d001d0000002e0101000200000e2004ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a010a0e104a00000000000000000e000e0000004a0101000200000e2004ffff2d3c000000420004300000000000000400004000000000000000f00064000400000000ffff0100003e62bb";
    const HEAVY_DUTY: &str = "aaff200a00a2001006000100ec00900a010a0e104a00000000000000000e000e0000004a0101000200000e2004ffff2d3c000000420004300000000000000400004000000000000000f00064000400000000ffff0100000a05090e102300000000000000006c006c000000230101000200000e2004ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100005323bb";
    const TUB_CLEAN: &str = "aaff200a00a2001009000100ec00900a03090e10540000000000000000380038000000540101000200000e2004ffff2d3c000000420004300000000000000400004000000000000000f00064000400000000ffff0100000aff000e0e550000000000000000570057000000550101000200000e2004ffff2d3c001000420004300000000000000400004000000000000000f00064000400000000ffff010000d18ebb";
    const SPIN_ONLY: &str = "aaff200a00a200100a000100ec00900aff000e0e550000000000000000570057000000550101000200000e2004ffff2d3c001000420004300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0f4e00000000000000000b000b0000004e010100020000ff2004ffff2d3c000000420004300000000000000400004000000000000000f00064000400000000ffff0100001641bb";
    const PET_CARE: &str = "aaff200a00a200100e000100ec00900a03090e0f8800000000000000003a003a000000880101000200000e2004ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a030b0e0e770000000000000000760076000000770101000200000e2004ffff2d3c400000420004300000000000000400004000000000000000f00064000400000000ffff0100005f27bb";
    const ALLERGY_CARE: &str = "aaff200a00a200100f000100ec00900a030b0e0e770000000000000000760076000000770101000200000e2004ffff2d3c400000420004300000000000000400004000000000000000f00064000400000000ffff0100000aff000e0f050000000000000000630063000000050101000200000e2004ffff2d3c001000420004300000000000000400004000000000000000f00064000400000000ffff0100007d45bb";
    const RINSE_PLUS3: &str = "aaff200a00a200104a000100ec00900a010810102e00000000000000003700370002002e010100020000102004ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a010811102e00000000000000003d003d0002002e010100020000112004ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100005c9abb";
    const STARTS_RUNNING: &str = "aaff200a00a2001057000100ec00900a03070e0e1b00000000000000003600360002001b0101000200000e2004ffff2d3c000000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03070e0e1b00000000000000003600360000001b0b01000200000e200400002d3c000000100004300000000000000400004000000000000000f00064000400000000ffff01000038d4bb";
    const DOOR_LOCKS: &str = "aaff200a00a2001058000100ec00900a03070e0e1b00000000000000003600360000001b0b01000200000e200400002d3c000000100004300000000000000400004000000000000000f00064000400000000ffff0100000a03070e0e1b00000000000000003600360000001b0b0b000200000e200400002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff010000486dbb";
    const RUNNING_COUNTDOWN: &str = "aaff200a00a200105a000100ec00900a03070e0e1b00000000000000003500360000001b0b0b000200000e200400002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000a03070e0e1b00000000000000003400360001001b0b0b000200000e200400002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff010000a195bb";
    const RINSING: &str = "aaff200a00a2000e6e000100ec00900a03080e0e1b00000000000000001d00360039001b0b0b000200000e1f0400002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff000e0e1b00000000000000001c0036003b001b0c0b000200000e1f04ff002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100007e18bb";
    const SPINNING: &str = "aaff200a00a2000f5c000100ec00900aff000e0e1b0000000000000000070036006f001b0c0c000200000e1f04ff002d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0e1b00000000000000000600360071001b0e0c00020000ff1f04ffff2d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100007d4ebb";
    const EB_ENDS: &str = "aaff200a005a000df6000100eb00480aff00ffff54000000000000000001000000630054100e00010000ff1f04ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff010000bcd7bb";
    const PAUSES: &str = "aaff200a00a20011ef000100ec00900aff00ff0e1b00000000000000000500360086001b0e0e00020000ff2004ffff2d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0e1b00000000000000000500360086001b020e00020000ff2004ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff0100000934bb";
    const UNLOCKS_ON_PAUSE: &str = "aaff200a00a20011f0000100ec00900aff00ff0e1b00000000000000000500360086001b020e00020000ff2004ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0e1b00000000000000000500360086001b020200020000ff2004ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff010000c9e9bb";
    const RESUMES: &str = "aaff200a00a20011f5000100ec00900aff00ff0e1b00000000000000000500360086001b020200020000ff2004ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0e1b00000000000000000500360086001b0e0200020000ff2004ffff2d3c000000100004300000000000000400004000000000000000f00064000400000000ffff010000ef01bb";
    const ENDS: &str = "aaff200a00a2001223000100ec00900aff00ff0e1b0000000000000000010036008f001b0e0e00020000ff2004ffff2d3c000000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff1b00000000000000000100000090001b100e00020000ff2104ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff010000d0a1bb";
    const UNLOCKS_AT_END: &str = "aaff200a00a2001224000100ec00900aff00ffff1b00000000000000000100000090001b100e00020000ff2104ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff1b00000000000000000100000090001b101000020000ff2104ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100009031bb";
    const POWERS_OFF: &str = "aaff200a00a2001225000100ec00900aff00ffff1b00000000000000000100000090001b101000020000ff2104ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff1b00000000000000000100000090001b001000020000ff2104ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff010000af44bb";
    const DELAY_WASH: &str = "aaff200a00a2001273000100ec00900a03090e0f2e00000000000000001d001d0000002e0101000200000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000003c001d001d0000002e0101000200000e2104ffff2d3c200000420084300000000000000400004000000000000000f00064000400000000ffff0100002141bb";
    const DELAY_STARTS: &str = "aaff200a00a200134f000100ec00900a03090e0f2e0000000000003c003500350000002e0101000500000e2204ffff2d3c200000420084300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000003c003500350000002e0701000500000e220400002d3c200000100084300000000000000400004000000000000000f00064000400000000ffff0100001c7dbb";
    const DELAY_COUNTS_DOWN: &str = "aaff200a00a2001377000100ec00900a03090e0f2e0000000000001f003500350005002e0707000500000e220400002d3c200000100184300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000001e003500350005002e0707000500000e220400002d3c200000100184300000000000000400004000000000000000f00064000400000000ffff010000e3abbb";
    const DELAY_FIRES: &str = "aaff200a00a200139e000100ec00900a03090e0f2e0000000000000100350035000b002e0707000500000e220400002d3c200000100184300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000000000350035000b002e0b07000500000e220400002d3c200000100104300000000000000400004000000000000000f00064000400000000ffff010000c095bb";
    const SPIN_STARTS: &str = "aaff200a00a20013d7000100ec00900aff000e0f2e0000000000000000120035002a002e0c0c000500000e2204ff002d3c200000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ff0f2e0000000000000000110035002b002e0e0c00050000ff2204ffff2d3c200000100104300000000000000400004000000000000000f00064000400000000ffff0100003fbfbb";
    const CYCLE_ENDS: &str = "aaff200a00a20013fb000100ec00900aff00ff0f2e0000000000000000010035004e002e0e0e00050000ff2204ffff2d3c200000100104300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff2e00000000000000000100000053002e100e00050000ff2304ffff2d3c000000400104300000000000000400004000000000000000f00064000400000000ffff010000af56bb";
    const POWERS_OFF_AT_END: &str = "aaff200a00a20013fe000100ec00900aff00ffff2e00000000000000000100000053002e101000050000ff2304ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000aff00ffff2e00000000000000000100000053002e001000050000ff2304ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff01000077d3bb";
    const DRUM_LIGHT_OFF: &str = "aaff200a00a2001278000100ec00900a03090e0f2e0000000000003c001d001d0000002e0101000200000e2104ffff2d3c200000420084300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000003c001d001d0001002e0101000200000e2104ffff2d3c200000020084300000000000000400004000000000000000f00064000400000000ffff010000d1c0bb";
    const LOAD_WEIGHED: &str = "aaff200a00a200123b000100ec00900a03090e0f2e00000000000000002700270000002e0101006400000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0000002e0101000200000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff010000ab21bb";
    const AUTO_SOAK_ON: &str = "aaff200a00a20012b2000100ec00900a03090e0f2e00000000000000001d001d0000002e0101000200000e2104ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000003b003b0000002e0101000200000e2104ffff2d3ca00000020004300000000000000400004000000000000000f00064000400000000ffff010000bc8dbb";
    const FRESH_CARE_ON: &str = "aaff200a00a20012b5000100ec00900a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200040020004300000000000000400004000000000000000f00064000400000000ffff0100007bf6bb";
    const BUZZER_3: &str = "aaff200a00a20012bb000100ec00900a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0001002e0101000200000e2103ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff01000011efbb";
    const BUZZER_OFF: &str = "aaff200a00a20012be000100ec00900a03090e0f2e00000000000000001d001d0001002e0101000200000e2101ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0001002e0101000200000e2100ffff2d3c200000020004300000000000000400004000000000000000f00064000400000000ffff0000007d39bb";
    const CHILD_LOCK_ON: &str = "aaff200a00a20012c5000100ec00900a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000001d001d0001002e0101000200000e2104ffff2d3c200000620004300000000000000400004000000000000000f00064000400000000ffff010000b699bb";
    const REMOTE_MAINTAIN_ON: &str = "aaff200a00a200127c000100ec00900a03090e0f2e0000000000003c001d001d0001002e0101000200000e2104ffff2d3c200000020084300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e0000000000003c001d001d0002002e0101000200000e2104ffff2d3c200000020084340000000000000400004000000000000000f00064000400000000ffff0100000314bb";
    const DETECTING: &str = "aaff200a00a20012e1000100ec00900a03090e0f2e00000000000000002700270000002e0101006400000e2104ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000002700270000002e0301006400000e210400002d3c200000100004300000000000000400004000000000000000f00064000400000000ffff0100002a7cbb";
    const DOOR_OPENS: &str = "aaff200a004900141c00020103000e610f0201610840010001000f02010105002546414658553235303036000000000000000000000102c586a2f40a09000000000000000000bd49bb";
    const DOOR_SHUTS: &str = "aaff200a004900141d00020103000e610f0201610841010002000f02010105002546414658553235303036000000000000000000000102c686a2f40a0900000000000000000071febb";


    #[test]
    fn selected_course_decodes() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(NORMAL_IDLE));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Warm"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "turbo_wash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "pre_wash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("39"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("39"));
        assert_eq!(prop(&ha, "load_level").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "ez_dispense_detergent").as_deref(), Some("None"));
    }

    #[test]
    fn little_endian_minutes_and_selector() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(HEAVY_DUTY));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("108"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Heavy Duty"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Heavy"));
        thinq.emit_data(&hex_decode(SPEED_WASH));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Quick Wash"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Light"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Hot"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("Extra High"));
        assert_eq!(prop(&ha, "turbo_wash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("14"));
    }

    #[test]
    fn sentinels_and_bitfields() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(TUB_CLEAN));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Tub Clean"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "pre_wash").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(SPIN_ONLY));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("None"));
        thinq.emit_data(&hex_decode(PET_CARE));
        assert_eq!(prop(&ha, "pre_wash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(ALLERGY_CARE));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Allergiene"));
        thinq.emit_data(&hex_decode(RINSE_PLUS3));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Plus 3"));
    }

    #[test]
    fn run_countdown_energy_and_phases() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STARTS_RUNNING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(DOOR_LOCKS));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(RUNNING_COUNTDOWN));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("52"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("54"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("1"));
        thinq.emit_data(&hex_decode(RINSING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Rinsing"));
        thinq.emit_data(&hex_decode(SPINNING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Spinning"));
        thinq.emit_data(&hex_decode(EB_ENDS));
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        thinq.emit_data(&hex_decode(PAUSES));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Paused"));
    }

    #[test]
    fn delay_detecting_door_events_and_start_query() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(DELAY_WASH));
        assert_eq!(prop(&ha, "delay_wash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "drum_light").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(DELAY_STARTS));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Delayed"));
        thinq.emit_data(&hex_decode(DELAY_COUNTS_DOWN));
        assert_eq!(prop(&ha, "reserve_time").as_deref(), Some("30"));
        thinq.emit_data(&hex_decode(DELAY_FIRES));
        assert_eq!(prop(&ha, "delay_wash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        thinq.emit_data(&hex_decode(DETECTING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Detecting"));
        assert_eq!(prop(&ha, "ez_dispense_detergent").as_deref(), Some("Off"));
        thinq.emit_data(&hex_decode(LOAD_WEIGHED));
        assert_eq!(prop(&ha, "load_level").as_deref(), Some("2"));
        thinq.emit_data(&hex_decode(DOOR_OPENS));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(DOOR_SHUTS));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(CHILD_LOCK_ON));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(AUTO_SOAK_ON));
        assert_eq!(prop(&ha, "auto_soak").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(FRESH_CARE_ON));
        assert_eq!(prop(&ha, "fresh_care").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(BUZZER_3));
        assert_eq!(prop(&ha, "buzzer").as_deref(), Some("3"));
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert!(outbox_has_inner(&thinq, &hex_decode(STATUS_QUERY)));
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA12F0ED1121010000001804111200005EBB"
        );
    }

    #[test]
    fn registry_is_not_an_alias_of_other_washers() {
        let a = t2_factory("FAFXU25006").expect("FAFXU25006");
        let b = t2_factory("F_C__Y___W.A__QEUK").expect("F_C washer");
        let c = t2_factory("RV13U6AM8W_D_US_WIFI").expect("RV13 washer");
        assert!(!std::ptr::fn_addr_eq(a, b));
        assert!(!std::ptr::fn_addr_eq(a, c));
    }

    #[test]
    fn eb_single_record_zeroes_remaining_while_off() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EB_POWER_OFF));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Bedding"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("127"));
        thinq.emit_data(&hex_decode(POWERS_OFF));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
    }

    #[test]
    fn non_status_frames_publish_nothing() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA084100430063BB"));
        assert!(prop(&ha, "status").is_none());
    }
}
