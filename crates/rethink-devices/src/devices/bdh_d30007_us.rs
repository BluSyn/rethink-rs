//! BDH_D30007_US — LG DLHC5502V heat-pump dryer (long AABB class 0x30).
//!
//! 42-byte records, big-endian minutes; `auto_ack` is on so the dryer does not
//! repeat frames ten times. `0x85`/`0x86` frames carry `deviceContentsStatus`
//! (length-prefixed ASCII ids + activated flag), not status records.
//! Not aliasable to RH10/RV13/SDH dryers.
//!
//! Port of upstream master `c1a81e8` and PR #351.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x30;
const ENVELOPE_TYPE: u8 = 0x0a;
const INNER_TYPE_OFFSET: usize = 10;
const INNER_LEN_OFFSET: usize = 11;
const RECORD_LEN: usize = 42;
const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_INNER_LEN: u16 = 2 * RECORD_LEN as u16;
const STATUS_RECORD_OFFSET: usize = 13 + RECORD_LEN;
const STATUS_BUF_LEN: usize = 13 + 2 * RECORD_LEN + 1;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_INNER_LEN: u16 = RECORD_LEN as u16;
const SINGLE_RECORD_OFFSET: usize = 13;
const SINGLE_BUF_LEN: usize = 13 + RECORD_LEN + 1;
const STATUS_QUERY: &str = "F0ED112101000000180411120000";
const DRY_LEVEL_OFFSET: usize = 1;
const ECO_HYBRID_OFFSET: usize = 2;
const COURSE_OFFSET: usize = 5;
const REMAIN_TIME_OFFSET: usize = 9;
const INITIAL_TIME_OFFSET: usize = 11;
const STATE_OFFSET: usize = 13;
const SPENT_POWER_OFFSET: usize = 17;
const BUZZER_OFFSET: usize = 19;
const DOWNLOAD_COURSE_OFFSET: usize = 23;
const OPT1_OFFSET: usize = 24;
const OPT1_WRINKLE_CARE: u8 = 0x08;
const OPT1_DRUM_LIGHT: u8 = 0x20;
const OPT1_DAMP_DRY_SIGNAL: u8 = 0x40;
const OPT2_OFFSET: usize = 26;
const OPT2_DETECT_LOAD: u8 = 0x04;
const OPT2_REMOTE_START: u8 = 0x40;
const OPT2_CONDENSER_CLEAN: u8 = 0x20;
const OPT3_OFFSET: usize = 27;
const OPT3_CYCLE_OPTIMIZATION: u8 = 0x01;
const OPT3_REMOTE_MAINTAIN: u8 = 0x02;
const STATE_POWEROFF: u8 = 0;

fn be16(rec: &[u8], off: usize) -> i64 {
    i64::from(u16::from_be_bytes([rec[off], rec[off + 1]]))
}
fn named(v: Option<&'static str>) -> rethink_core::ha::PropertyValue {
    v.unwrap_or("None").into()
}
fn state_name(c: u8) -> Option<&'static str> {
    Some(match c {
        0 => "Off",
        1 => "Standby",
        2 | 7 => "Drying",
        3 => "Paused",
        4 | 22 => "End",
        5 => "Error",
        6 => "Smart Diagnosis",
        8 => "Cooling",
        9 => "Wrinkle care",
        10 | 12 => "Delayed",
        11 | 14 => "Detecting",
        15 => "Steam",
        16 => "Clothing recognition",
        17 => "Condenser clean",
        18 => "Bedding care",
        19 => "Refreshing",
        20 => "Allergy care",
        21 => "Condenser care",
        23 => "Dry ready",
        24 => "Laundry care",
        25 => "Dehumidification",
        26 => "Dehumidification end",
        27 => "End waiting",
        28 => "Drum care",
        29 => "Cooldown paused",
        30 => "Cooldown standby",
        _ => return None,
    })
}
fn course_name(c: u8) -> Option<&'static str> {
    Some(match c {
        0 => "Not selected",
        2 => "Towels",
        4 => "Bedding",
        5 => "Perm. Press",
        7 => "Normal",
        8 => "Activewear",
        9 => "Small Load",
        10 => "Delicates",
        13 => "Air Dry",
        15 => "Bedding Refresh",
        16 => "Power Dry",
        17 => "Heavy Duty",
        18 => "Condenser Care",
        19 => "Drum Care",
        20 => "Down Jacket Refresh",
        21 => "Timed Dry",
        22 => "Outerwear Refresh",
        23 => "Quick Dry",
        27 => "Pet Care",
        44 => "AI Dry",
        45 => "Silent Dry",
        46 => "Shrinkage Relief",
        50 => "Rainy Days",
        51 => "Easy Ironing",
        255 => "Downloaded cycle",
        _ => return None,
    })
}
fn download_name(c: u8) -> Option<&'static str> {
    match c {
        0 => Some("Not selected"),
        114 => Some("Wrinkle Prevention"),
        140 => Some("Rack Dry"),
        _ => None,
    }
}
fn dry_level_name(c: u8) -> Option<&'static str> {
    match c {
        0 => Some("Not selected"),
        1 => Some("Damp"),
        2 => Some("Less"),
        3 => Some("Normal"),
        4 => Some("More"),
        5 => Some("Very"),
        _ => None,
    }
}
fn eco_name(c: u8) -> Option<&'static str> {
    match c {
        0 => Some("Not selected"),
        1 => Some("Eco"),
        2 => Some("Normal"),
        3 => Some("Turbo"),
        _ => None,
    }
}
fn error_name(c: u8) -> Option<&'static str> {
    Some(match c {
        0 => "OK",
        1 => "Temperature sensor error (tE1)",
        2 => "Temperature sensor error (tE2)",
        3 => "Temperature sensor error (tE3)",
        4 => "Temperature sensor error (tE4)",
        5 => "Temperature sensor error (tE5)",
        6 => "Temperature sensor error (tE6)",
        7 => "Condenser error (CE1)",
        8 => "Condenser error (CE2)",
        9 => "Heater error (HE1)",
        10 => "Unknown error (E1)",
        11 => "Unknown error (E3)",
        12 => "Unknown error (E4)",
        13 => "Unknown error (E5)",
        14 => "Drain motor error (OE)",
        15 => "Empty water container",
        16 => "Door open (dE1)",
        17 => "Filter clogged",
        18 => "Filter missing",
        19 => "EEPROM error",
        20 => "Unknown error (F1)",
        21 => "Motor error (LE2)",
        22 => "Unknown error (AE)",
        31 => "Motor error (LE1)",
        38 => "Door sensor error (dE4)",
        40 => "Motor error (LE3)",
        43 => "Door lock error (dE2)",
        44 => "Unknown error (PS)",
        46 => "Unknown error (HS)",
        47 => "Clothes not drying",
        48 => "Unknown error (nP)",
        49 => "Unknown error (IR)",
        50 => "Unknown error (BF1)",
        51 => "Gas error",
        52 => "Unknown error (dE5)",
        53 => "Dehumidification error",
        54 => "Unknown error (LO)",
        55 => "Unknown error (CC)",
        _ => return None,
    })
}
fn buzzer_name(c: u8) -> Option<&'static str> {
    match c { 0 => Some("Off"), 1 => Some("1"), 2 => Some("2"), 3 => Some("3"), 4 => Some("4"), _ => None }
}

const STATE_OPTS: &[&str] = &[
    "Off", "Standby", "Drying", "Paused", "End", "Error", "Smart Diagnosis", "Cooling",
    "Wrinkle care", "Delayed", "Detecting", "Steam", "Clothing recognition", "Condenser clean",
    "Bedding care", "Refreshing", "Allergy care", "Condenser care", "Dry ready", "Laundry care",
    "Dehumidification", "Dehumidification end", "End waiting", "Drum care", "Cooldown paused",
    "Cooldown standby",
];
const COURSE_OPTS: &[&str] = &[
    "Not selected",
    "Towels",
    "Bedding",
    "Perm. Press",
    "Normal",
    "Activewear",
    "Small Load",
    "Delicates",
    "Air Dry",
    "Bedding Refresh",
    "Power Dry",
    "Heavy Duty",
    "Condenser Care",
    "Drum Care",
    "Down Jacket Refresh",
    "Timed Dry",
    "Outerwear Refresh",
    "Quick Dry",
    "Pet Care",
    "AI Dry",
    "Silent Dry",
    "Shrinkage Relief",
    "Rainy Days",
    "Easy Ironing",
    "Downloaded cycle",
];
const DOWNLOAD_OPTS: &[&str] = &["Not selected", "Wrinkle Prevention", "Rack Dry"];
const DRY_LEVEL_OPTS: &[&str] = &["Not selected", "Damp", "Less", "Normal", "More", "Very"];
const ECO_OPTS: &[&str] = &["Not selected", "Eco", "Normal", "Turbo"];
const ERROR_OPTS: &[&str] = &[
    "OK",
    "Temperature sensor error (tE1)",
    "Temperature sensor error (tE2)",
    "Temperature sensor error (tE3)",
    "Temperature sensor error (tE4)",
    "Temperature sensor error (tE5)",
    "Temperature sensor error (tE6)",
    "Condenser error (CE1)",
    "Condenser error (CE2)",
    "Heater error (HE1)",
    "Unknown error (E1)",
    "Unknown error (E3)",
    "Unknown error (E4)",
    "Unknown error (E5)",
    "Drain motor error (OE)",
    "Empty water container",
    "Door open (dE1)",
    "Filter clogged",
    "Filter missing",
    "EEPROM error",
    "Unknown error (F1)",
    "Motor error (LE2)",
    "Unknown error (AE)",
    "Motor error (LE1)",
    "Door sensor error (dE4)",
    "Motor error (LE3)",
    "Door lock error (dE2)",
    "Unknown error (PS)",
    "Unknown error (HS)",
    "Clothes not drying",
    "Unknown error (nP)",
    "Unknown error (IR)",
    "Unknown error (BF1)",
    "Gas error",
    "Unknown error (dE5)",
    "Dehumidification error",
    "Unknown error (LO)",
    "Unknown error (CC)",
];
// Options order as upstream publishes it: JavaScript lists integer-like keys first.
const BUZZER_OPTS: &[&str] = &["1","2","3","4","Off"];

pub struct Device { core: Arc<AabbDeviceCore> }
impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        // Unacked, this dryer repeats most frames ten times, 1.5 s apart.
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let mut cfg = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        cfg.components = [
            ("power".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:tumble-dryer","device_class":"running"})),
            ("status".into(), json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":STATE_OPTS})),
            ("error".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-error","state_topic":"$this/error","name":"Error","icon":"mdi:check-circle","device_class":"problem","entity_category":"diagnostic"})),
            ("error_message".into(), json!({"platform":"sensor","unique_id":"$deviceid-error-message","state_topic":"$this/error_message","name":"Error message","icon":"mdi:alert-circle-outline","device_class":"enum","entity_category":"diagnostic","options":ERROR_OPTS})),
            ("download_course".into(), json!({"platform":"sensor","unique_id":"$deviceid-download_course","state_topic":"$this/download_course","name":"Downloaded cycle","icon":"mdi:cloud-download-outline","device_class":"enum","options":DOWNLOAD_OPTS,"entity_category":"diagnostic"})),
            ("course".into(), json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline","device_class":"enum","options":COURSE_OPTS})),
            ("remaining_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
            ("initial_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time estimate","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
            ("energy".into(), json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","unit_of_measurement":"Wh","state_class":"total_increasing"})),
            ("dry_level".into(), json!({"platform":"sensor","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","name":"Dry level","icon":"mdi:water-percent","device_class":"enum","options":DRY_LEVEL_OPTS})),
            ("eco_hybrid".into(), json!({"platform":"sensor","unique_id":"$deviceid-eco_hybrid","state_topic":"$this/eco_hybrid","name":"Eco Hybrid","icon":"mdi:leaf","device_class":"enum","options":ECO_OPTS})),
            ("buzzer".into(), json!({"platform":"sensor","unique_id":"$deviceid-buzzer","state_topic":"$this/buzzer","name":"Buzzer","icon":"mdi:volume-high","device_class":"enum","options":BUZZER_OPTS,"entity_category":"diagnostic"})),
            ("wrinkle_care".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Wrinkle Care","icon":"mdi:tshirt-crew-outline"})),
            ("damp_dry_signal".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-damp_dry_signal","state_topic":"$this/damp_dry_signal","name":"Damp Dry Signal","icon":"mdi:water-alert-outline"})),
            ("detect_load".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-detect_load","state_topic":"$this/detect_load","name":"Detect load","icon":"mdi:scale"})),
            ("cycle_optimization".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-cycle_optimization","state_topic":"$this/cycle_optimization","name":"Cycle Optimization","icon":"mdi:auto-fix","entity_category":"diagnostic"})),
            ("remote_maintain".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_maintain","state_topic":"$this/remote_maintain","name":"Remote maintain","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
            ("condenser_clean".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-condenser_clean","state_topic":"$this/condenser_clean","name":"Condenser clean","icon":"mdi:air-filter"})),
            ("remote_start".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
            ("drum_light".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-drum_light","state_topic":"$this/drum_light","name":"Drum light","icon":"mdi:lightbulb","entity_category":"diagnostic"})),
        ].into_iter().collect();
        core.set_config(cfg);
        let this = Arc::new(Self { core: core.clone() });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) { t.process_aabb(inner); }
        }));
        this
    }
    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < 13 || buf[0] != CLASS_BYTE || buf[1] != ENVELOPE_TYPE { return; }
        let inner_len = u16::from_be_bytes([buf[INNER_LEN_OFFSET], buf[INNER_LEN_OFFSET+1]]);
        match buf[INNER_TYPE_OFFSET] {
            STATUS_FRAME_TYPE if inner_len == STATUS_INNER_LEN => self.process_status(buf, STATUS_RECORD_OFFSET, STATUS_BUF_LEN),
            SINGLE_STATUS_FRAME_TYPE if inner_len == SINGLE_INNER_LEN => self.process_status(buf, SINGLE_RECORD_OFFSET, SINGLE_BUF_LEN),
            _ => {}
        }
    }
    fn process_status(&self, buf: &[u8], record_offset: usize, expected_len: usize) {
        if buf.len() != expected_len { return; }
        let rec = &buf[record_offset..record_offset+RECORD_LEN];
        let state = rec[STATE_OFFSET];
        let is_off = state == STATE_POWEROFF;
        self.core.publish_on_off("power", !is_off);
        self.core.publish_property("status", named(state_name(state)));
        let error = rec[15];
        self.core.publish_property("error_message", named(error_name(error)));
        self.core.publish_on_off("error", error != 0);
        self.core.publish_property("course", named(course_name(rec[COURSE_OFFSET])));
        self.core.publish_property("download_course", named(download_name(rec[DOWNLOAD_COURSE_OFFSET])));
        self.core.publish_property("dry_level", named(dry_level_name(rec[DRY_LEVEL_OFFSET])));
        self.core.publish_property("eco_hybrid", named(eco_name(rec[ECO_HYBRID_OFFSET])));
        self.core.publish_property("buzzer", named(buzzer_name(rec[BUZZER_OFFSET])));
        self.core.publish_property("remaining_time", if is_off { 0 } else { be16(rec, REMAIN_TIME_OFFSET) }.into());
        self.core.publish_property("initial_time", if is_off { 0 } else { be16(rec, INITIAL_TIME_OFFSET) }.into());
        self.core.publish_property("energy", be16(rec, SPENT_POWER_OFFSET).into());
        let o1 = rec[OPT1_OFFSET];
        self.core.publish_on_off("wrinkle_care", o1 & OPT1_WRINKLE_CARE != 0);
        self.core.publish_on_off("drum_light", o1 & OPT1_DRUM_LIGHT != 0);
        self.core.publish_on_off("damp_dry_signal", o1 & OPT1_DAMP_DRY_SIGNAL != 0);
        let o2 = rec[OPT2_OFFSET];
        self.core.publish_on_off("detect_load", o2 & OPT2_DETECT_LOAD != 0);
        self.core.publish_on_off("remote_start", o2 & OPT2_REMOTE_START != 0);
        self.core.publish_on_off("condenser_clean", o2 & OPT2_CONDENSER_CLEAN != 0);
        let o3 = rec[OPT3_OFFSET];
        self.core.publish_on_off("remote_maintain", o3 & OPT3_REMOTE_MAINTAIN != 0);
        self.core.publish_on_off("cycle_optimization", o3 & OPT3_CYCLE_OPTIMIZATION != 0);
    }
}
impl DeviceHandler for Device {
    fn id(&self) -> &str { &self.core.id }
    fn start(&self) { self.core.send(&hex_decode(STATUS_QUERY)); }
    fn drop_device(&self) { self.core.drop_device(); }
    fn set_property(&self, _p: &str, _v: &str) {}
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
        make_t2("BDH_D30007_US", Device::new)
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
    const TUB_CLEAN: &str =
        "aaff300a0066004f45000100ec005400030200020700000000640000010000000000040700000000000481070000000000000000000000000000000300021300000000a000000100000200000413000000000000810700000000000000000000000000317ebb";
    const TIME_DRY: &str =
        "aaff300a0066004f48000100ec005400000300021300000000a000000100000200000413000000000000810700000000000000000000000000000002000215000000001e001e0100000200000415000000000000810700000000000000000000000000b665bb";
    const AI_COURSE: &str =
        "aaff300a0066004f5d000100ec005400000200020400000000c3001e010000020000040400000000000081070000000000000000000000000000000200022c0000000064001e010000000000041c00000000000481070000000000000000000000000039cabb";
    const TOWELS: &str =
        "aaff300a0066004f51000100ec00540000030002100000000096001e01000001000004100000000000008107000000000000000000000000000000020002020000000064001e0100000000000402000000000004810700000000000000000000000000e470bb";
    const DRYING: &str =
        "aaff300a0066004e58000100ec005400000200020400000000c200c3070300020002040400000000004081070000000000000000000000000000000200020400000000c100c30703000200030404000000000040810700000000000000000000000000959ebb";
    const DRY_LEVEL_VERY: &str =
        "aaff300a0066004f13000100ec00540004020002070000000064000001000000000004070000002000048107000000000000000000000000000005020002070000000064000001000000000004070000002000048107000000000000000000000000004992bb";
    const ECO_HYBRID_ECO: &str =
        "aaff300a0066004f05000100ec005400030200020700000000640000010000000000040700000020000481070000000000000000000000000000030100020700000000640000010000000000040700000020000481070000000000000000000000000002b3bb";
    const BUZZER_OFF: &str =
        "aaff300a0066004f26000100ec005400030200020700000000640000010000000000040700000000000481070000000000000000000000000000030200020700000000640000010000000000000700000000000481070000000000000000000000000000b1bb";
    const WRINKLE_CARE_ON: &str =
        "aaff300a0066004f3b000100ec00540003020002070000000064000001000000000004070000000000048107000000000000000000000000000003020002070000000064000001000000000004070000000800048107000000000000000000000000002692bb";
    const DAMP_DRY_SIGNAL_ON: &str =
        "aaff300a0066004f20000100ec005400030200020700000000640000010000000000040700000000000481070000000000000000000000000000030200020700000000640000010000000000040700000040000481070000000000000000000000000086bfbb";
    const REMOTE_START_ON: &str =
        "aaff300a0066004f6b000100ec00540003020002070000000064001e01000000000004070000000000048107000000000000000000000000000003020002070000000064001e0100000000000407000000000044810700000000000000000000000000a3d4bb";
    const PAUSES: &str =
        "aaff300a0066004e86000100ec005400000200020400000000bf00c3070300020006040400000000004081070000000000000000000000000000000200020400000000bf00c303070002000a0404000000200000810700000000000000000000000000e6fdbb";
    const POWERS_OFF: &str =
        "aaff300a0066004e8f000100ec005400000200020400000000bf00c303070002000a04040000002000008107000000000000000000000000000000000002000000000000000000030000000a0400000000000000810700000000000000000000000000f3b5bb";
    const CONDENSER_CLEAN: &str =
        "aaff300a0066005f34000100ec0054000002000204000000001500c30703000406f70404000000000040830700000000000000000000000000000002000204000000001400c31107000506fd0404000000000060830700000000000000000000000000ed51bb";
    const COOLING: &str =
        "aaff300a0066005f7f000100ec0054000002000204000000001000c31107000507040404000000000060830700000000000000000000000000000002000204000000001000c30811000507060404000000000040830700000000000000000000000000c820bb";
    const END_REMOTE_MAINTAIN: &str =
        "aaff300a0066006060000100ec0054000002000204000000000100c30811000507180404000000000040830700000000000000000000000000000002000204000000000100c316080007071a0404000000200040830700000000000000000000000000c4ebbb";
    const REMOTE_MAINTAIN_ON: &str =
        "aaff300a0066006191000100ec0054000302000207000000006400640e0100000000040700000000004481070000000000000000000000000000030200020700000000680069070e0002000a040700000000004083070000000000000000000000000000a8bb";
    const REMOTE_MAINTAIN_OFF: &str =
        "aaff300a00660060fe000100ec0054000002000204000000000100c316080007071a04040000000000408307000000000000000000000000000000000002000000000000000000160000071c0400000000000000810700000000000000000000000000b025bb";
    const REMOTE_START_SWITCHED_OFF: &str =
        "aaff300a0066007b9f000100ec0054000302000207000000006400000100000000000407000000000044810700000000000000000000000000000302000207000000006400000100000000000407000000000004810700000000000000000000000000bd50bb";
    const HAND_STARTED_WITH_REMOTE_OFF: &str =
        "aaff300a0066006eac000100ec0054000000000200000000000000000100000000000400000000200000810700000000000000000000000000000302000207000000006400640e010000000004070000000000448107000000000000000000000000002823bb";
    const CONDENSER_CLEAN_CLOUD: &str =
        "aaff300a006600738c000100ec005400030200020700000000150055070e000402500407000000000040830700000000000000000000000000000302000207000000001400551107000502590407000000000060830700000000000000000000000000b6f9bb";
    const CYCLE_OPTIMIZATION_OFF: &str =
        "aaff300a0066007c2e000100ec0054000301000207000000006400640100000000000407000000200004810700000000000000000000000000000302000207000000006400640100000000000407000000200004800700000000000000000000000000aaf8bb";
    const CYCLE_OPTIMIZATION_ON: &str =
        "aaff300a0066007c61000100ec0054000302000207000000006400640100000000000207000000200004800700000000000000000000000000000302000207000000006400640100000000000207000000200004810700000000000000000000000000ee33bb";
    const APP_QUICK_DRY: &str =
        "aaff300a0066007d2d000100ec005400000000020d000000001e001e010000050000020d000000000000810700000000000000000000000000000002000217000000003c001e0100000200000217000000000000810700000000000000000000000000662cbb";
    const APP_SHRINKAGE_RELIEF: &str =
        "aaff300a0066007d1e000100ec0054000003000216000000003c001e010000010000021600000000000081070000000000000000000000000000020200022e000000006e001e0100000200000220000000000000810700000000000000000000000000f7e8bb";
    const DOWNLOADED_CYCLE: &str =
        "aaff300a0066007d52000100ec00540000030002320000000028001e01000002000002220000000000008107000000000000000000000000000003020002ff000000006400280100000000000207000072080004810700000000000000000000000000ede3bb";

    #[test]
    fn course_table_covers_the_dial() {
        expect(&feed(&[TUB_CLEAN]), "course", "Drum Care");
        expect(&feed(&[TIME_DRY]), "course", "Timed Dry");
        expect(&feed(&[TOWELS]), "course", "Towels");
        expect(&feed(&[DRYING]), "course", "Bedding");
        expect(&feed(&[DRY_LEVEL_VERY]), "course", "Normal");
    }

    #[test]
    fn app_only_cycles_use_the_panel_names() {
        expect(&feed(&[APP_QUICK_DRY]), "course", "Quick Dry");
        expect(&feed(&[TIME_DRY]), "course", "Timed Dry");
        expect(&feed(&[APP_SHRINKAGE_RELIEF]), "course", "Shrinkage Relief");
    }

    #[test]
    fn downloaded_cycle_is_a_sentinel_plus_its_identifier() {
        let p = feed(&[DOWNLOADED_CYCLE]);
        expect(&p, "course", "Downloaded cycle");
        expect(&p, "download_course", "Wrinkle Prevention");

        expect(&feed(&[TOWELS]), "download_course", "Not selected");
    }

    #[test]
    fn pet_care_is_code_27_and_frames_are_acked() {
        const PET_CARE: &str = "aaff300a0066005964000100ec005400000000000000000000000000010000000000010000000020000080070000000302000000000000000000000300001b00000000be0000010000010000011b0000002000008007000000030200000000000000005e96bb";
        let (ha, thinq, _dev) = make();
        thinq.emit_data(&hex_decode(PET_CARE));
        expect(&ha, "course", "Pet Care");
        expect(&ha, "status", "Standby");
        expect(&ha, "remaining_time", "190");
        let sent = thinq.sent();
        assert_eq!(sent.len(), 1, "auto_ack enabled");
        assert_eq!(sent[0].cmd, "ack");
    }

    #[test]
    fn ai_dial_position_reads_the_course_byte() {
        expect(&feed(&[AI_COURSE]), "course", "AI Dry");
    }

    #[test]
    fn minute_counters_are_big_endian() {
        expect(&feed(&[TUB_CLEAN]), "remaining_time", "160");

        let drying = feed(&[DRYING]);
        expect(&drying, "remaining_time", "193");
        expect(&drying, "initial_time", "195");
        expect(&drying, "energy", "3");
    }

    #[test]
    fn dry_level_and_eco_hybrid_read_their_own_bytes() {
        let very = feed(&[DRY_LEVEL_VERY]);
        expect(&very, "dry_level", "Very");
        expect(&very, "eco_hybrid", "Normal");

        let eco = feed(&[ECO_HYBRID_ECO]);
        expect(&eco, "eco_hybrid", "Eco");
        expect(&eco, "dry_level", "Normal");

        expect(&feed(&[TUB_CLEAN]), "eco_hybrid", "Turbo");
        expect(&feed(&[TUB_CLEAN]), "dry_level", "Not selected");
    }

    #[test]
    fn buzzer_volume() {
        expect(&feed(&[BUZZER_OFF]), "buzzer", "Off");
        expect(&feed(&[TUB_CLEAN]), "buzzer", "4");
    }

    #[test]
    fn rec24_bits_do_not_collide() {
        let wrinkle = feed(&[WRINKLE_CARE_ON]);
        expect(&wrinkle, "wrinkle_care", "ON");
        expect(&wrinkle, "damp_dry_signal", "OFF");
        expect(&wrinkle, "drum_light", "OFF");

        let signal = feed(&[DAMP_DRY_SIGNAL_ON]);
        expect(&signal, "damp_dry_signal", "ON");
        expect(&signal, "wrinkle_care", "OFF");
        expect(&signal, "drum_light", "OFF");
    }

    #[test]
    fn cycle_optimization_shares_the_remote_maintain_byte() {
        let off = feed(&[CYCLE_OPTIMIZATION_OFF]);
        expect(&off, "cycle_optimization", "OFF");
        expect(&off, "remote_maintain", "OFF");

        let on = feed(&[CYCLE_OPTIMIZATION_ON]);
        expect(&on, "cycle_optimization", "ON");
        expect(&on, "remote_maintain", "OFF");

        expect(&feed(&[DRYING]), "cycle_optimization", "ON");
        expect(&feed(&[DRYING]), "remote_maintain", "OFF");
    }

    #[test]
    fn remote_start_is_the_setting_and_rearms_at_cycle_start() {
        let off = feed(&[REMOTE_START_SWITCHED_OFF]);
        expect(&off, "remote_start", "OFF");
        expect(&off, "status", "Standby");
        expect(&off, "detect_load", "ON");

        let started = feed(&[REMOTE_START_SWITCHED_OFF, HAND_STARTED_WITH_REMOTE_OFF]);
        expect(&started, "status", "Detecting");
        expect(&started, "remote_start", "ON");
        expect(&started, "course", "Normal");
    }

    #[test]
    fn remote_start_and_detect_load_share_rec26() {
        let remote = feed(&[REMOTE_START_ON]);
        expect(&remote, "remote_start", "ON");
        expect(&remote, "detect_load", "ON");

        let towels = feed(&[TOWELS]);
        expect(&towels, "detect_load", "ON");
        expect(&towels, "remote_start", "OFF");

        expect(&feed(&[TUB_CLEAN]), "detect_load", "OFF");
    }

    #[test]
    fn condenser_clean_flag_tracks_its_state() {
        let clean = feed(&[CONDENSER_CLEAN]);
        expect(&clean, "status", "Condenser clean");
        expect(&clean, "condenser_clean", "ON");
        expect(&clean, "remote_start", "ON");

        let cooling = feed(&[CONDENSER_CLEAN, COOLING]);
        expect(&cooling, "status", "Cooling");
        expect(&cooling, "condenser_clean", "OFF");
        expect(&cooling, "remote_start", "ON");

        expect(&feed(&[TUB_CLEAN]), "condenser_clean", "OFF");

        let witnessed = feed(&[CONDENSER_CLEAN_CLOUD]);
        expect(&witnessed, "status", "Condenser clean");
        expect(&witnessed, "condenser_clean", "ON");
    }

    #[test]
    fn remote_started_cycle_ends_in_remote_maintain() {
        let p = feed(&[COOLING, END_REMOTE_MAINTAIN]);
        expect(&p, "status", "End");
        expect(&p, "power", "ON");
        expect(&p, "remaining_time", "1");
        expect(&p, "energy", "1818");
        expect(&p, "drum_light", "ON");
    }

    #[test]
    fn remote_maintain_is_rec27_bit_0x02() {
        let on = feed(&[REMOTE_MAINTAIN_ON]);
        expect(&on, "status", "Drying");
        expect(&on, "remote_maintain", "ON");

        let off = feed(&[REMOTE_MAINTAIN_OFF]);
        expect(&off, "status", "Off");
        expect(&off, "remote_maintain", "OFF");

        expect(&feed(&[TUB_CLEAN]), "remote_maintain", "OFF");
    }

    #[test]
    fn pausing_lights_the_drum() {
        let p = feed(&[PAUSES]);
        expect(&p, "status", "Paused");
        expect(&p, "drum_light", "ON");
        expect(&p, "remaining_time", "191");
        expect(&p, "power", "ON");

        let resumed = feed(&[PAUSES, DRYING]);
        expect(&resumed, "status", "Drying");
        expect(&resumed, "drum_light", "OFF");
    }

    #[test]
    fn powering_off_keeps_the_energy_used() {
        let p = feed(&[PAUSES, POWERS_OFF]);
        expect(&p, "power", "OFF");
        expect(&p, "status", "Off");
        expect(&p, "course", "Not selected");
        expect(&p, "remaining_time", "0");
        expect(&p, "initial_time", "0");
        expect(&p, "energy", "10");
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
            "aa0730d82b91bb",
            "aa09307200c80048bb",
            "aaff300a0066004f45000100ec0054000302000207",
            // a washer frame: right shape, wrong class byte
            "aaff200a00a2001002000100ec00900aff000e000000000000000000000100000001002e0001006400000e2004ffff2d3c000000400004300000000000000400004000000000000000f00064000400000000ffff0100000a03090e0f2e00000000000000002700270000002e0101006400000e2004ffff2d3c200000420004300000000000000400004000000000000000f00064000400000000ffff01000049b8bb",
        ] {
            let p = feed(&[junk]);
            assert!(p.device(DEVICE_ID).unwrap().properties.is_empty(), "{junk}");
        }
    }

    fn with_error(frame: &str, code: u8) -> Vec<u8> {
        let mut buf = hex_decode(frame);
        let inner_type = buf[2 + 10];
        let record = if inner_type == 0xec { 13 + 42 } else { 13 };
        buf[2 + record + 15] = code;
        buf
    }

    #[test]
    fn error_byte_uses_the_newer_dryer_table() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&with_error(TUB_CLEAN, 4));
        expect(&ha, "error", "ON");
        expect(&ha, "error_message", "Temperature sensor error (tE4)");
        thinq.emit_data(&with_error(TUB_CLEAN, 43));
        expect(&ha, "error_message", "Door lock error (dE2)");
        thinq.emit_data(&with_error(TUB_CLEAN, 0));
        expect(&ha, "error", "OFF");
        expect(&ha, "error_message", "OK");
        let mut unset = with_error(TUB_CLEAN, 0);
        let inner_type = unset[2 + 10];
        let record = if inner_type == 0xec { 13 + 42 } else { 13 };
        unset[2 + record + 2] = 0;
        thinq.emit_data(&unset);
        expect(&ha, "eco_hybrid", "Not selected");
    }

    #[test]
    fn options_list_every_label_once() {
        let tables: [(&[&str], fn(u8) -> Option<&'static str>); 7] = [
            (STATE_OPTS, state_name),
            (COURSE_OPTS, course_name),
            (DOWNLOAD_OPTS, download_name),
            (DRY_LEVEL_OPTS, dry_level_name),
            (ECO_OPTS, eco_name),
            (ERROR_OPTS, error_name),
            (BUZZER_OPTS, buzzer_name),
        ];
        for (opts, name) in tables {
            let mut labels: Vec<&str> = (0..=255).filter_map(name).collect();
            labels.sort_unstable();
            labels.dedup();
            let mut sorted = opts.to_vec();
            sorted.sort_unstable();
            assert_eq!(labels, sorted);
        }
    }

    #[test]
    fn registry_is_not_an_alias_of_other_dryers() {
        let a = t2_factory("BDH_D30007_US").expect("BDH_D30007_US");
        let b = t2_factory("RH10V9_CH").expect("RH10");
        let c = t2_factory("RV13B6BSD_D_US_WIFI").expect("RV13 dryer");
        assert!(!std::ptr::fn_addr_eq(a, b));
        assert!(!std::ptr::fn_addr_eq(a, c));
    }
}
