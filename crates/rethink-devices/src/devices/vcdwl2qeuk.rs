//! VCDWL2QEUK front-load washer (LG F4X7511TWS, AABB).
//!
//! Status comes as enveloped `0xEB`/`0xEC` 64-byte records, read from the second
//! byte on. The 0x88 config and 0x41 door frames are told apart by the envelope's
//! length byte (`buf[3]`), which happens to differ between them.

use crate::device_trait::DeviceHandler;
use crate::washer_common::{spin_options, spin_value, temperature_value, TEMPERATURE_LABELS};
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::Arc;

const CONFIG_FRAME_TYPE: u8 = 0x88;
const STATUS_RECORD_LEN: usize = 64;
const DOOR_FRAME_TYPE: u8 = 0x41;
const DOOR_OFFSET: usize = 18;
const DOOR_CLOSED: u8 = 0x02;
const DOOR_OPEN: u8 = 0x01;
const SKYL_OFFSET: usize = 26;
const DISP_DETERGENT_EN: usize = 29;
const DISP_SOFTENER_EN: usize = 30;
const DISP_DETERGENT_ML: usize = 31;
const DISP_SOFTENER_ML: usize = 32;
const DISP_ON: u8 = 0x02;
const OPT_BYTE_A: usize = 33;
const OPT_PREWASH: u8 = 0x40;
const OPT_TURBOWASH: u8 = 0x20;
const OPT_BYTE_B: usize = 34;
const OPT_STEAM: u8 = 0x10;
const FLAGS_BYTE: usize = 36;
const FLAG_CHILD_LOCK: u8 = 0x20;
const FLAG_REMOTE_START: u8 = 0x10;
const TUB_CLEAN_OFFSET: usize = 27;

fn soil_by_level() -> HashMap<u8, &'static str> {
    HashMap::from([(1, "Light"), (3, "Medium"), (5, "Heavy")])
}
/// Shared rinse labels. Index 0 is "Not selected", not a blank.
const RINSES: &[&str] = &[
    "Not selected",
    "Normal",
    "Rinse+",
    "Rinse++",
    "Normal + Hold",
    "Rinse+ + Hold",
];
fn status_course() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x72, "AI Wash"), (0x2e, "Cotton"), (0x54, "Towels"), (0x4b, "Quick 14"),
        (0x4e, "Spin only"), (0x55, "Drum Clean"), (0x13, "Eco 40-60"), (0x7a, "TurboWash 39"),
        (0x2b, "Mixed"), (0x16, "Delicate"), (0x1d, "Easy Care"), (0x5e, "Hand/Wool"),
        (0x4f, "Activewear"), (0x04, "Allergy Care"), (0x1b, "Duvet"), (0x11, "Cold Wash"),
        (0x81, "Bedding"), (0xa9, "Cuffs & Collars"), (0x6a, "Rainy Days"), (0x42, "Silent Wash"),
        (0x07, "Baby Steam Care"), (0x73, "Down Jacket"), (0x88, "Microplastic Care"),
        (0x37, "Rinse + Spin"),
    ])
}
/// Status labels from record[20], in upstream's option order; some labels cover several codes.
const STATUS: &[(&str, &[u8])] = &[
    ("Off", &[0]),
    ("Standby", &[1, 27]),
    ("Paused", &[2, 28, 35]),
    ("Detecting", &[3, 37, 40]),
    ("Filling", &[5]),
    ("Detergent amount", &[6]),
    ("Delayed", &[7]),
    ("Soaking", &[8]),
    ("Pre-wash", &[9]),
    ("Washing", &[11, 29, 38]),
    ("Rinsing", &[12, 39]),
    ("Rinse hold", &[13]),
    ("Spinning", &[14]),
    ("Drying", &[15]),
    ("End", &[16, 42]),
    ("Refreshing", &[21, 47, 57]),
    ("Error", &[23]),
    ("Drum Clean", &[41]),
    ("Steam", &[43]),
    ("Cleaning", &[48]),
    ("Waiting", &[49]),
];

fn status_name(code: u8) -> &'static str {
    STATUS
        .iter()
        .find(|(_, codes)| codes.contains(&code))
        .map_or("None", |(name, _)| name)
}

fn status_options() -> Vec<&'static str> {
    STATUS.iter().map(|(name, _)| *name).collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });
        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        let comps = [
            ("power", json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:washing-machine","device_class":"running"})),
            ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":status_options()})),
            ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
            ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","device_class":"enum","options":TEMPERATURE_LABELS})),
            ("spin", json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","device_class":"enum","options":spin_options()})),
            ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
            ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","device_class":"duration","unit_of_measurement":"min","name":"Initial time"})),
            ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","device_class":"duration","unit_of_measurement":"min","name":"Remaining time"})),
            ("door", json!({"platform":"binary_sensor","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","device_class":"door"})),
            ("detergent_dispenser", json!({"platform":"binary_sensor","unique_id":"$deviceid-detergent_dispenser","state_topic":"$this/detergent_dispenser","name":"Detergent dispenser","icon":"mdi:cup-water"})),
            ("softener_dispenser", json!({"platform":"binary_sensor","unique_id":"$deviceid-softener_dispenser","state_topic":"$this/softener_dispenser","name":"Softener dispenser","icon":"mdi:cup-water"})),
            ("detergent_dose", json!({"platform":"sensor","unique_id":"$deviceid-detergent_dose","state_topic":"$this/detergent_dose","name":"Detergent dose","icon":"mdi:cup","device_class":"volume","unit_of_measurement":"mL","state_class":"measurement"})),
            ("softener_dose", json!({"platform":"sensor","unique_id":"$deviceid-softener_dose","state_topic":"$this/softener_dose","name":"Softener dose","icon":"mdi:cup-outline","device_class":"volume","unit_of_measurement":"mL","state_class":"measurement"})),
            ("soil", json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:liquid-spot"})),
            ("rinse", json!({"platform":"sensor","unique_id":"$deviceid-rinse","state_topic":"$this/rinse","name":"Rinse","icon":"mdi:water-sync"})),
            ("prewash", json!({"platform":"binary_sensor","unique_id":"$deviceid-prewash","state_topic":"$this/prewash","name":"Pre-wash","icon":"mdi:water-sync"})),
            ("turbowash", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbowash","state_topic":"$this/turbowash","name":"TurboWash","icon":"mdi:rocket-launch"})),
            ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"})),
            ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:lock","entity_category":"diagnostic"})),
            ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:play-circle-outline","entity_category":"diagnostic"})),
            ("tub_clean_count", json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since drum clean","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"})),
        ];
        let mut components = Map::new();
        for (k, v) in comps { components.insert(k.into(), v); }
        base.components = components.into_iter().collect();
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(inner);
            }
        }));
        this
    }

    fn process_door(&self, buf: &[u8]) {
        if buf.len() <= DOOR_OFFSET { return; }
        let v = buf[DOOR_OFFSET];
        if v == DOOR_OPEN { self.core.publish_property("door", "ON".into()); }
        else if v == DOOR_CLOSED { self.core.publish_property("door", "OFF".into()); }
    }

    fn process_config(&self, _buf: &[u8]) {
        self.core.publish_property("power", "ON".into());
    }

    fn publish_persistent(&self, rec: &[u8]) {
        self.core.publish_on_off("child_lock", rec[FLAGS_BYTE] & FLAG_CHILD_LOCK != 0);
        self.core.publish_on_off("remote_start", rec[FLAGS_BYTE] & FLAG_REMOTE_START != 0);
        self.core.publish_property("tub_clean_count", (rec[TUB_CLEAN_OFFSET] as i64).into());
    }

    fn process_status(&self, rec: &[u8]) {
        self.core.publish_property("status", status_name(rec[20]).into());
        if rec[20] == 0x00 {
            self.core.publish_property("power", "OFF".into());
            self.core.publish_property("remaining_time", 0i64.into());
            self.core.publish_property("initial_time", 0i64.into());
            self.publish_persistent(rec);
            return;
        }
        self.core.publish_property("power", "ON".into());
        self.core.publish_property("soil", soil_by_level().get(&rec[0]).copied().unwrap_or("None").into());
        self.core.publish_property("rinse", RINSES.get(rec[SKYL_OFFSET] as usize).copied().unwrap_or("None").into());
        // This model's indexes sit one below the shared tables: 0 is Cold / No spin.
        self.core.publish_property("temp", temperature_value(rec[1] as usize + 1).unwrap_or("None").into());
        self.core.publish_property("spin", spin_value(rec[3] as usize + 1).unwrap_or("None").into());
        self.core.publish_property("course", status_course().get(&rec[4]).copied().unwrap_or("None").into());
        self.core.publish_property("remaining_time", (rec[13] as i64).into());
        self.core.publish_property("initial_time", (rec[15] as i64).into());
        self.core.publish_property("energy", (rec[16] as i64 * 256 + rec[17] as i64).into());
        self.core.publish_on_off("detergent_dispenser", rec[DISP_DETERGENT_EN] == DISP_ON);
        self.core.publish_on_off("softener_dispenser", rec[DISP_SOFTENER_EN] == DISP_ON);
        self.core.publish_property("detergent_dose", (rec[DISP_DETERGENT_ML] as i64).into());
        self.core.publish_property("softener_dose", (rec[DISP_SOFTENER_ML] as i64).into());
        self.core.publish_on_off("prewash", rec[OPT_BYTE_A] & OPT_PREWASH != 0);
        self.core.publish_on_off("turbowash", rec[OPT_BYTE_A] & OPT_TURBOWASH != 0);
        self.core.publish_on_off("steam", rec[OPT_BYTE_B] & OPT_STEAM != 0);
        self.publish_persistent(rec);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(record) = process_enveloped_status(buf, 0x20, STATUS_RECORD_LEN) {
            self.process_status(&record[1..]);
            return;
        }
        if buf.len() < 6 || buf[0] != 0x20 { return; }
        match buf[3] {
            CONFIG_FRAME_TYPE => self.process_config(buf),
            DOOR_FRAME_TYPE => self.process_door(buf),
            _ => {}
        }
    }

    pub fn set_property(&self, _prop: &str, _mqtt_value: &str) {}
}

impl DeviceHandler for Device {
    fn id(&self) -> &str { &self.core.id }
    fn start(&self) {}
    fn drop_device(&self) { self.core.drop_device(); }
    fn set_property(&self, prop: &str, value: &str) { Device::set_property(self, prop, value); }
    fn publish_config(&self) {
        self.core.republish_config();
    }
}

pub fn create(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata { Metadata::new("VCDWL2QEUK", "VCDWL2QEUK", "0.0.0") }
    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }
    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }
    /// Copy of an AA..BB frame with one inner byte patched (inner[i] = full[2 + i]).
    fn patch_inner(frame: &str, inner_index: usize, value: u8) -> Vec<u8> {
        let mut f = hex_decode(frame);
        f[2 + inner_index] = value;
        f
    }

    // Real frames captured live from the appliance (upstream master's tests). For
    // WASHING/END, the drum-clean frame with record B replaced by a wash record.
    const CFG_COLD: &str = "aa00200a00880000d300020103004d010e0102010020001e0b0000008603c6000e000e00000c020201010200014000000c00000202022d2d1e001f2b47001c630063010700030008000800080008000800020000000000000000000201050025564344574c325145554b000000000000000000000102c712ea140b070000000000000000006400bb";
    const DRUM: &str = "aaff200a0092000196000100ec00800003050e005500000000000000004800480040005529010000000001200202022d1e0000001000041800000000000004000000000000000000000000000000000003050e005500000000000000004800480000005529010009000001200202022d1e000000100104180000000000000400000000000000000000000000000000e163bb";
    const WASHING: &str = "aa00200a0092000196000100ec00800003050e005500000000000000004800480040005529010000000001200202022d1e0000001000041800000000000004000000000000000000000000000000000003010e014b00000000000000000e000e0000004b0b0100090000011f0202022d1e200000100104180000000000000400000000000000000000000000000000e100bb";
    const END: &str = "aa00200a0092000196000100ec00800003050e005500000000000000004800480040005529010000000001200202022d1e0000001000041800000000000004000000000000000000000000000000000003010e014b000000000000000000000e0000004b100100090000011f0202022d1e200000100104180000000000000400000000000000000000000000000000e100bb";
    const OFF: &str = "aa00200a0092000cbc000100ec00800003020e097200000000000000003b003b0000007201000000000001010200002d1e000000020004180000000000000400000000000000000000000000000000000000000c7200000000000000003b003b0000007200010009000001010200002d1e0000000000041800000000000004000000000000000000000000000000005800bb";
    const DOOR_CLOSED_F: &str = "aaff200a0041000c76000201030006100e0102100201050025564344574c325145554b000000000000000000000102ce12ea140b07000000000000000000c10ebb";
    const DOOR_OPEN_F: &str = "aaff200a0041000c77000201030006100e0102100101050025564344574c325145554b000000000000000000000102ce12ea140b07000000000000000000063dbb";
    const SPIN1000: &str = "aa00200a0092000d3f000100ec00800003020e097200000000000000003b003b0000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003020e067200000000000000003800380000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000004600bb";
    const SPIN1200: &str = "aa00200a0092000d45000100ec00800003020e067200000000000000003800380000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003020e087200000000000000003800380000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000009000bb";
    const DET_ON: &str = "aa00200a0092000d4a000100ec00800003020e087200000000000000003800380000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003020e087200000000000000003800380000007201000000000001010202002d1e0000000200041800000000000004000000000000000000000000000000000b00bb";
    const SOFT_ON: &str = "aa00200a0092000d57000100ec00800003020e087200000000000000003800380000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003020e087200000000000000003800380000007201000000000001010200022d1e0000000200041800000000000004000000000000000000000000000000005c00bb";
    const DET_DOSE120: &str = "aa00200a0092000d72000100ec00800003020e08720000000000000000380038000000720100000000000101020000091e0000000200041800000000000004000000000000000000000000000000000003020e08720000000000000000380038000000720100000000000101020000781e0000000200041800000000000004000000000000000000000000000000005000bb";
    const SOFT_DOSE120: &str = "aa00200a0092000d63000100ec00800003020e087200000000000000003800380000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003020e087200000000000000003800380000007201000000000001010200002d780000000200041800000000000004000000000000000000000000000000007800bb";
    const SOIL_LET: &str = "aa00200a0092000e00000100ec00800003010e047300000000000000009400940000007301000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000001010e047300000000000000008600860000007301000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000003400bb";
    const SOIL_MED: &str = "aa00200a0092000e02000100ec00800001010e047300000000000000008600860000007301000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003010e047300000000000000009400940000007301000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000008800bb";
    const SOIL_TUNG: &str = "aa00200a0092000e04000100ec00800003010e047300000000000000009400940000007301000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000005010e04730000000000000000b200b20000007301000000000001010200002d1e000000020004180000000000000400000000000000000000000000000000f700bb";
    const STEAM_ON: &str = "aa00200a0092000e0c000100ec00800003050e097200000000000000005b005b0000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003050e097200000000000000006e006e0000007201000000000001010200002d1e0010000200041800000000000004000000000000000000000000000000005600bb";
    const TURBOWASH_ON: &str = "aa00200a0092000e12000100ec00800003030e092e0000000000000000ef00ef0000002e01000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003030e062e0000000000000000c400c40000002e01000000000001010200002d1e2000000200041800000000000004000000000000000000000000000000007d00bb";
    const FORVASK_ON: &str = "aa00200a0092000e1b000100ec00800003030e062e0000000000000000dd00dd0000002e01000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003030e062e0000000000000000ee00ee0000002e01000000000001010200002d1e400000020004180000000000000400000000000000000000000000000000b600bb";
    const ECO: &str = "aa00200a0092000db7000100ec00800005030e092e00000000000000010101010000002e01000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003030e09130000000000000000dc00dc0000001301000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000008e00bb";
    const DELIKAT: &str = "aa00200a0092000ddd000100ec00800003030e062b00000000000000005100510000002b01000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003010e041600000000000000002d002d0000001601000009000001010200002d1e0000000200041800000000000004000000000000000000000000000000004200bb";
    const SKYL_NONE: &str = "aa00200a0092000e4f000100ec00800003030e092e0000000000000000ef00ef0000002e01000000000001010200002d1e00000002000418000000000000040000000000000000000000000000000000030300092e0000000000000000d100d10000002e01000000000000010200002d1e0000000200041800000000000004000000000000000000000000000000008c00bb";
    const SKYL_PLUS: &str = "aa00200a0092000e55000100ec008000030300092e0000000000000000d100d10000002e01000000000000010200002d1e0000000200041800000000000004000000000000000000000000000000000003030f092e0000000000000000fe00fe0000002e01000000000002010200002d1e000000020004180000000000000400000000000000000000000000000000fc00bb";
    const SKYL_HOLD: &str = "aa00200a0092000e5f000100ec008000030310092e00000000000000010d010d0000002e01000000000003010200002d1e00000002000418000000000000040000000000000000000000000000000000030312092e0000000000000000ef00ef0000002e01000000000004010200002d1e0000000200041800000000000004000000000000000000000000000000000900bb";
    const MIKROPLAST: &str = "aa00200a0092000e6e000100ec00800003030e092e0000000000000000ef00ef0000002e01000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003030e068800000000000000007000700000008801000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000004200bb";
    const CHILD_LOCK_ON: &str = "aa00200a0092000ec5000100ec00800003030e097200000000000000003b003b0000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003030e097200000000000000003b003b0000007201000000000001010200002d1e0000002200041800000000000004000000000000000000000000000000008200bb";
    const REMOTE_START_ON: &str = "aa00200a0092000ecc000100ec00800003030e097200000000000000003b003b0000007201000000000001010200002d1e0000000200041800000000000004000000000000000000000000000000000003030e097200000000000000003b003b0000007201000000000001010200002d1e0000001200041800000000000004000000000000000000000000000000008700bb";
    const RUN_CHILD_REMOTE: &str = "aa00200a009200100b000100ec00800003030e097200000000000000002b003b010800720b260006000001010202022d1e0000001001061800000000000004000000000000000000000000000000000003030e097200000000000000002b003b010900720b260006000001010202022d1e0000003001061800000000000004000000000000000000000000000000009b00bb";
    const CYCLE_END: &str = "aa00200a0092001101000100ec0080000000000c72000000000000000000003b01590072100e0009000001020202022d1e000000000006180000000000000400000000000000000000000000000000000000000c72000000000000000000003b0159007200100009000001020202022d1e0000000000061800000000000004000000000000000000000000000000007e00bb";
    const RINSE: &str = "aa00200a009200102d000100ec00800003030e0972000000000000000021003b011a00720b260006000001010202022d1e0000001001061800000000000004000000000000000000000000000000000000000e0972000000000000000020003b011c00720c0b0006000001010202022d1e0000001001041800000000000004000000000000000000000000000000009200bb";
    const SPINNING: &str = "aa00200a00920010a3000100ec00800000000e097200000000000000000d003b013400720c270006000001010202022d1e00000010010618000000000000040000000000000000000000000000000000000000097200000000000000000c003b013500720e0c0006000001010202022d1e000000100104180000000000000400000000000000000000000000000000b800bb";
    const SPIN_ONLY: &str = "aa00200a0092000c1a000100ec00800003020e097200000000000000003b003b0000007201000000000001010200002d1e00000002000418000000000000040000000000000000000000000000000000000000014e00000000000000000800080000004e01000009000000010200002d1e0000000200041800000000000004000000000000000000000000000000006e00bb";

    #[test]
    fn config_and_config_frame_power() {
        let (ha, thinq, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert!(comps.contains_key("tub_clean_count"));
        assert!(comps.contains_key("detergent_dose"));
        assert_eq!(comps["temp"]["device_class"], "enum");
        assert_eq!(comps["spin"]["device_class"], "enum");

        // config frame: buf[3]=0x88
        let mut inner = vec![0u8; 20];
        inner[0] = 0x20;
        inner[3] = 0x88;
        let mut pkt = vec![0xaa, (inner.len()+4) as u8];
        pkt.extend(inner);
        pkt.push(0); pkt.push(0xbb);
        thinq.emit_data(&pkt);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
    }

    fn running_frame(temp: u8, spin: u8, rinse: u8) -> Vec<u8> {
        // record B starts at inner[78]
        let mut f = hex_decode(WASHING);
        f[2 + 78 + 1] = temp;
        f[2 + 78 + 3] = spin;
        f[2 + 78 + 26] = rinse;
        f
    }

    #[test]
    fn shared_tables_are_shifted_and_unmapped_codes_are_none() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&running_frame(0, 0, 0));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Cold"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("No spin"));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Not selected"));

        thinq.emit_data(&running_frame(4, 2, 2));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("50 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("600 rpm"));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Rinse+"));

        thinq.emit_data(&running_frame(0xff, 0xff, 4));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Normal + Hold"));
    }

    #[test]
    fn door_frame() {
        let (ha, thinq, _) = make();
        let mut inner = vec![0u8; 30];
        inner[0] = 0x20;
        inner[3] = 0x41;
        inner[18] = 0x01; // open
        let mut pkt = vec![0xaa, 0];
        pkt.extend(&inner); pkt.push(0); pkt.push(0xbb);
        thinq.emit_data(&pkt);
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        inner[18] = 0x02;
        let mut pkt = vec![0xaa, 0];
        pkt.extend(&inner); pkt.push(0); pkt.push(0xbb);
        thinq.emit_data(&pkt);
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    #[test]
    fn config_frame_marks_power_on() {
        let (ha, thinq, _) = make();
        emit(&thinq, CFG_COLD);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
    }

    #[test]
    fn status_decodes_course_temp_spin_from_record_b() {
        let (ha, thinq, _) = make();
        emit(&thinq, WASHING);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Quick 14"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("20 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("400 rpm"));
    }

    #[test]
    fn washing_decodes_status_remaining_initial_power() {
        let (ha, thinq, _) = make();
        emit(&thinq, WASHING);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("14"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("14"));
    }

    #[test]
    fn drum_clean_step_and_times() {
        let (ha, thinq, _) = make();
        emit(&thinq, DRUM);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drum Clean"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Drum Clean"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("72"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("72"));
    }

    #[test]
    fn end_decodes_status_end_remaining_zero() {
        let (ha, thinq, _) = make();
        emit(&thinq, END);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("14"));
    }

    #[test]
    fn energy_is_big_endian_wh() {
        let (ha, thinq, _) = make();
        // rec[17] is inner[95]
        thinq.emit_data(&patch_inner(WASHING, 95, 41));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("41"));
        let mut f = patch_inner(WASHING, 94, 1);
        f[2 + 95] = 16;
        thinq.emit_data(&f);
        assert_eq!(prop(&ha, "energy").as_deref(), Some("272"));
    }

    #[test]
    fn unconfirmed_enum_indices_publish_none() {
        let (ha, thinq, _) = make();
        // record B starts at inner[78]: rec[1]=temp, rec[3]=spin, rec[4]=course
        thinq.emit_data(&patch_inner(WASHING, 79, 0xff));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("None"));
        thinq.emit_data(&patch_inner(WASHING, 81, 0xff));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("None"));
        thinq.emit_data(&patch_inner(WASHING, 82, 0xff));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
    }

    #[test]
    fn soil_levels_decode_and_non_medium_washes_run() {
        let (ha, thinq, _) = make();
        emit(&thinq, SOIL_LET);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Light"));
        emit(&thinq, SOIL_MED);
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Medium"));
        emit(&thinq, SOIL_TUNG);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Heavy"));
    }

    #[test]
    fn status_frame_of_unexpected_length_is_ignored() {
        let (ha, thinq, _) = make();
        let before = prop(&ha, "status");
        emit(&thinq, "aa00200a0092000196000100ec0080000300bb");
        assert_eq!(prop(&ha, "status"), before);
    }

    #[test]
    fn standby_frame_powers_off_and_clears_the_countdown() {
        let (ha, thinq, _) = make();
        emit(&thinq, WASHING);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("14"));
        emit(&thinq, OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("0"));
    }

    #[test]
    fn zero_soil_lead_with_nonzero_status_is_running() {
        let (ha, thinq, _) = make();
        emit(&thinq, SPIN_ONLY);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"), "spin-only must not be read as OFF");
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
    }

    #[test]
    fn door_snapshot_decodes_open_and_closed() {
        let (ha, thinq, _) = make();
        emit(&thinq, DOOR_CLOSED_F);
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        emit(&thinq, DOOR_OPEN_F);
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        emit(&thinq, DOOR_CLOSED_F);
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    #[test]
    fn uninitialised_door_byte_holds_the_last_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, DOOR_OPEN_F);
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        thinq.emit_data(&patch_inner(DOOR_OPEN_F, 18, 0x05));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"), "last state held, not overwritten");
    }

    #[test]
    fn spin_index_6_is_1000_and_8_is_1200() {
        let (ha, thinq, _) = make();
        emit(&thinq, SPIN1000);
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1000 rpm"));
        emit(&thinq, SPIN1200);
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1200 rpm"));
    }

    #[test]
    fn config_exposes_the_ezdispense_entities() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        for c in ["detergent_dispenser", "softener_dispenser", "detergent_dose", "softener_dose"] {
            assert!(comps.contains_key(c), "component {c} present");
        }
        assert_eq!(comps["detergent_dispenser"]["platform"], "binary_sensor");
        assert_eq!(comps["softener_dispenser"]["platform"], "binary_sensor");
        assert_eq!(comps["detergent_dose"]["platform"], "sensor");
        assert_eq!(comps["detergent_dose"]["unit_of_measurement"], "mL");
        assert_eq!(comps["softener_dose"]["unit_of_measurement"], "mL");
    }

    #[test]
    fn ezdispense_enable_flags_decode_independently() {
        let (ha, thinq, _) = make();
        emit(&thinq, DET_ON);
        assert_eq!(prop(&ha, "detergent_dispenser").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "softener_dispenser").as_deref(), Some("OFF"));
        emit(&thinq, SOFT_ON);
        assert_eq!(prop(&ha, "detergent_dispenser").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "softener_dispenser").as_deref(), Some("ON"));
    }

    #[test]
    fn ezdispense_doses_are_literal_ml() {
        let (ha, thinq, _) = make();
        emit(&thinq, DET_DOSE120);
        assert_eq!(prop(&ha, "detergent_dose").as_deref(), Some("120"));
        assert_eq!(prop(&ha, "softener_dose").as_deref(), Some("30"));
        emit(&thinq, SOFT_DOSE120);
        assert_eq!(prop(&ha, "softener_dose").as_deref(), Some("120"));
        assert_eq!(prop(&ha, "detergent_dose").as_deref(), Some("45"));
    }

    #[test]
    fn newly_mapped_course_codes() {
        let (ha, thinq, _) = make();
        emit(&thinq, ECO);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Eco 40-60"));
        emit(&thinq, DELIKAT);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Delicate"));
        emit(&thinq, MIKROPLAST);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Microplastic Care"));
    }

    #[test]
    fn rinse_level_decodes_from_rec_26() {
        let (ha, thinq, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        assert_eq!(devinfo.config.as_ref().unwrap().components["rinse"]["platform"], "sensor");
        emit(&thinq, SKYL_NONE);
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Not selected"));
        emit(&thinq, SKYL_PLUS);
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Rinse+"));
        emit(&thinq, SKYL_HOLD);
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Normal + Hold"));
    }

    #[test]
    fn config_exposes_the_soil_and_option_entities() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert_eq!(comps["soil"]["platform"], "sensor");
        for c in ["prewash", "turbowash", "steam"] {
            assert!(comps.contains_key(c), "component {c} present");
            assert_eq!(comps[c]["platform"], "binary_sensor");
        }
    }

    #[test]
    fn option_bitflags_decode() {
        let (ha, thinq, _) = make();
        emit(&thinq, SOIL_MED);
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        emit(&thinq, FORVASK_ON);
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
        emit(&thinq, TURBOWASH_ON);
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));
        emit(&thinq, STEAM_ON);
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
    }

    #[test]
    fn child_lock_and_remote_start_flags_decode() {
        let (ha, thinq, _) = make();
        emit(&thinq, CHILD_LOCK_ON);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        emit(&thinq, REMOTE_START_ON);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        emit(&thinq, RUN_CHILD_REMOTE);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"), "still a running frame");
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
    }

    #[test]
    fn flags_publish_in_standby_too() {
        let (ha, thinq, _) = make();
        emit(&thinq, RUN_CHILD_REMOTE);
        emit(&thinq, OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn tub_clean_count_decodes_in_run_and_standby() {
        let (ha, thinq, _) = make();
        emit(&thinq, RUN_CHILD_REMOTE);
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("1"));
        emit(&thinq, CYCLE_END);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("2"));
    }

    #[test]
    fn zero_soil_back_half_phases_decode_as_running() {
        let (ha, thinq, _) = make();
        emit(&thinq, RINSE);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("32"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("AI Wash"));
        emit(&thinq, SPINNING);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Spinning"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("12"));
        emit(&thinq, SPIN_ONLY);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Spin only"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("400 rpm"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("8"));
    }

    #[test]
    fn sub_step_status_codes_map_to_their_phase() {
        let (ha, thinq, _) = make();
        for (code, phase) in [
            (0x03, "Detecting"),
            (0x25, "Detecting"),
            (0x26, "Washing"),
            (0x27, "Rinsing"),
        ] {
            // inner[98] = record B rec[20] (status)
            thinq.emit_data(&patch_inner(WASHING, 98, code));
            assert_eq!(prop(&ha, "status").as_deref(), Some(phase), "status 0x{code:x}");
        }
    }

    #[test]
    fn unknown_frame_types_and_non_envelope_frames_are_ignored() {
        let (ha, thinq, _) = make();
        let before = prop(&ha, "power");
        emit(&thinq, "aa00200a005a0001020304050600bb");
        emit(&thinq, "aa0720e90a91bb");
        emit(&thinq, "001122");
        assert_eq!(prop(&ha, "power"), before);
    }

    #[test]
    fn status_is_an_enum_and_unmapped_soil_is_none() {
        let (ha, thinq, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let status = &devinfo.config.as_ref().unwrap().components["status"];
        assert_eq!(status["device_class"], "enum");
        assert_eq!(
            status["options"],
            json!([
                "Off",
                "Standby",
                "Paused",
                "Detecting",
                "Filling",
                "Detergent amount",
                "Delayed",
                "Soaking",
                "Pre-wash",
                "Washing",
                "Rinsing",
                "Rinse hold",
                "Spinning",
                "Drying",
                "End",
                "Refreshing",
                "Error",
                "Drum Clean",
                "Steam",
                "Cleaning",
                "Waiting"
            ])
        );
        // inner[78] = rec[0] (soil); unmapped levels and status codes read None
        thinq.emit_data(&patch_inner(WASHING, 78, 0x02));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("None"));
        thinq.emit_data(&patch_inner(WASHING, 98, 0x22));
        assert_eq!(prop(&ha, "status").as_deref(), Some("None"));
    }

    #[test]
    fn eb_single_record_is_decoded() {
        let ec = hex_decode(WASHING);
        // 20 0A <len16> <seq16> .. .. .. EB <inner len16> <record> <checksum byte>
        let mut eb = ec[2..15].to_vec();
        eb[10] = 0xeb;
        eb[12] = STATUS_RECORD_LEN as u8;
        eb.extend_from_slice(&ec[2 + 13 + STATUS_RECORD_LEN..ec.len() - 2]);
        let (ha, thinq, _) = make();
        thinq.emit_data(&rethink_core::device_base::wrap_aabb(&eb));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Quick 14"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("14"));
    }
}
