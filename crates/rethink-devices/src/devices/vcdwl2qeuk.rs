//! VCDWL2QEUK front-load washer (AABB multi-frame).

use crate::device_trait::DeviceHandler;
use crate::washer_common::{spin_options, spin_value, temperature_value, TEMPERATURE_LABELS};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_FRAME_TYPE: u8 = 0x92;
const CONFIG_FRAME_TYPE: u8 = 0x88;
const STATUS_RECORD_OFF: usize = 78;
const STATUS_RECORD_LEN: usize = 63;
const STATUS_FRAME_LEN: usize = 142;
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
fn status_name(code: u8) -> &'static str {
    match code {
        0 => "Off",
        1 | 27 => "Standby",
        2 | 28 | 35 => "Paused",
        3 | 37 | 40 => "Detecting",
        5 => "Filling",
        6 => "Detergent amount",
        7 => "Delayed",
        8 => "Soaking",
        9 => "Pre-wash",
        11 | 29 | 38 => "Washing",
        12 | 39 => "Rinsing",
        13 => "Rinse hold",
        14 => "Spinning",
        15 => "Drying",
        16 | 42 => "End",
        21 | 47 | 57 => "Refreshing",
        23 => "Error",
        41 => "Drum Clean",
        43 => "Steam",
        48 => "Cleaning",
        49 => "Waiting",
        _ => "None",
    }
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
            ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine"})),
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
                t.process_aabb(&inner);
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

    fn process_status(&self, buf: &[u8]) {
        if buf.len() != STATUS_FRAME_LEN { return; }
        let rec = &buf[STATUS_RECORD_OFF..STATUS_RECORD_OFF + STATUS_RECORD_LEN];
        if rec[20] == 0x00 {
            self.core.publish_property("power", "OFF".into());
            self.core.publish_property("status", "Off".into());
            self.core.publish_property("remaining_time", 0i64.into());
            self.core.publish_property("initial_time", 0i64.into());
            self.publish_persistent(rec);
            return;
        }
        self.core.publish_property("power", "ON".into());
        self.core.publish_property("status", status_name(rec[20]).into());
        self.core.publish_property("soil", soil_by_level().get(&rec[0]).copied().unwrap_or("unknown").into());
        self.core.publish_property("rinse", RINSES.get(rec[SKYL_OFFSET] as usize).copied().unwrap_or("None").into());
        // This model's indexes sit one below the shared tables: 0 is Cold / No spin.
        self.core.publish_property("temp", temperature_value(rec[1] as usize + 1).unwrap_or("None").into());
        self.core.publish_property("spin", spin_value(rec[3] as usize + 1).unwrap_or("None").into());
        self.core.publish_property("course", status_course().get(&rec[4]).copied().unwrap_or("unknown").into());
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
        if buf.len() < 6 || buf[0] != 0x20 { return; }
        match buf[3] {
            CONFIG_FRAME_TYPE => self.process_config(buf),
            STATUS_FRAME_TYPE => self.process_status(buf),
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
    use rethink_core::{Thinq2Device, MockHaConnection, MockThinq2Device, Metadata};

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata { Metadata::new("VCDWL2QEUK", "VCDWL2QEUK", "1.0") }
    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

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

    #[test]
    fn status_standby_and_running() {
        let (ha, thinq, _) = make();
        // Build 142-byte status frame
        let mut inner = vec![0u8; 142];
        inner[0] = 0x20;
        inner[3] = 0x92;
        // record B at 78: all zero -> standby OFF
        let mut pkt = vec![0xaa, 0]; // fix len later
        pkt.extend(&inner);
        pkt.push(0); pkt.push(0xbb);
        pkt[1] = (pkt.len()) as u8; // not exact but envelope doesn't check
        // Actually process_data_envelope doesn't use length byte. OK.
        thinq.emit_data(&pkt);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));

        // running: soil=3, temp idx=2 (30), spin idx=4 (800), course=0x2e Cotton, status=0x0b washing
        let mut inner = vec![0u8; 142];
        inner[0] = 0x20;
        inner[3] = 0x92;
        let off = 78;
        inner[off] = 3; // soil medium
        inner[off+1] = 2; // temp 30
        inner[off+3] = 4; // spin 800
        inner[off+4] = 0x2e; // cotton
        inner[off+13] = 42; // remaining
        inner[off+15] = 60; // initial
        inner[off+16] = 0x01; inner[off+17] = 0x00; // energy 256
        inner[off+20] = 0x0b; // washing
        inner[off+26] = 1; // rinse normal
        inner[off+29] = 0x02; // detergent on
        inner[off+30] = 0x02; // softener on
        inner[off+31] = 45;
        inner[off+32] = 30;
        inner[off+33] = 0x40 | 0x20; // prewash+turbo
        inner[off+34] = 0x10; // steam
        inner[off+36] = 0x20 | 0x10; // child+remote
        inner[off+27] = 7; // tub clean count
        let mut pkt = vec![0xaa, 0];
        pkt.extend(&inner);
        pkt.push(0); pkt.push(0xbb);
        thinq.emit_data(&pkt);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("30 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("800 rpm"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("42"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("60"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("256"));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "detergent_dispenser").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "softener_dose").as_deref(), Some("30"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("7"));
    }

    fn running_frame(temp: u8, spin: u8, rinse: u8) -> Vec<u8> {
        let mut inner = vec![0u8; 142];
        inner[0] = 0x20;
        inner[3] = 0x92;
        let off = 78;
        inner[off] = 3;
        inner[off + 1] = temp;
        inner[off + 3] = spin;
        inner[off + 4] = 0x2e;
        inner[off + 20] = 0x0b;
        inner[off + 26] = rinse;
        let mut pkt = vec![0xaa, 0];
        pkt.extend(inner);
        pkt.push(0);
        pkt.push(0xbb);
        pkt
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
}
