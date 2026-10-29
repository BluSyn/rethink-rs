//! T1789EFH_F — LG WT7300CW top-load washer.
//!
//! Class `0x20`, 27-byte status blocks. The block's first byte is a prefix;
//! the record itself starts at the `0x19` marker. `0xEC` carries the previous
//! record then the current one. `0xE2` looks like a status record but replays
//! a stale snapshot, so it is not decoded. `auto_ack` is on: unacked, this
//! washer repeats frames and never streams `0xEC`.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const STATUS_LENGTH: usize = 27;
const STATUS_REQUEST: &[u8] = &[0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x00];
const CMD_PAUSE: &[u8] = &[0xf0, 0x24, 0x04, 0x01, 0x00];
const CMD_POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];
const PHASE_OFF: u8 = 0x00;
const PHASE_PAUSED: u8 = 0x02;
const VERB_RESUME: u8 = 0x01;
const VERB_START: u8 = 0x03;
const FLAG_CHILD_LOCK: u8 = 0x01;
const FLAG_DOOR_LOCK: u8 = 0x04;
const FLAG_TURBO_WASH: u8 = 0x40;
const OPT2_EXTRA_RINSE: u8 = 0x01;
const OPT2_WATER_PLUS: u8 = 0x02;
const OPT2_STAIN_CARE: u8 = 0x08;
const OPT2_SOAK: u8 = 0x20;
const REMOTE_START_ON: u8 = 0x01;

const STATUS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Standby"),
    (0x02, "Paused"),
    (0x03, "Detecting"),
    (0x04, "Soaking"),
    (0x05, "Washing"),
    (0x06, "Rinsing"),
    (0x07, "Spinning"),
    (0x08, "End"),
    (0x09, "Delayed"),
    (0x0a, "Updating firmware"),
    (0x0b, "Smart Diagnosis"),
];
const COURSE: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x01, "Normal"),
    (0x02, "Heavy Duty"),
    (0x04, "Delicates"),
    (0x05, "Waterproof"),
    (0x07, "Speed Wash"),
    (0x08, "Bedding"),
    (0x0c, "Tub Clean"),
    (0x11, "Rinse+Spin"),
    (0x12, "Spin Only"),
    (0x19, "Deep Wash"),
    (0x29, "Pre Wash+Normal"),
];
const SMART_COURSE: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x64, "Small Load"),
    (0x65, "Single Item"),
    (0x67, "Wrinkle Stop/Gentle Wave"),
    (0x68, "Rapid Clean"),
    (0x69, "Sweat Stains"),
    (0x6a, "Swimwear"),
    (0x6b, "Curtains"),
    (0x6d, "Dress Shirts"),
    (0x6e, "Collar Stains"),
    (0x6f, "Pillow Covers"),
    (0x70, "Lingerie"),
    (0x71, "Baby Wear"),
    (0x73, "Children's Wear"),
    (0x74, "Juice & Food Stains"),
    (0x75, "Baby Bibs"),
    (0x76, "Pet Hair and Odor"),
    (0x77, "EconoWash"),
    (0x78, "Collars and Cuffs"),
    (0x79, "Delicate Dresses"),
    (0x7a, "Half Load Wash"),
    (0x7b, "Full Load Wash"),
    (0x83, "Whites"),
];
const SOIL: &[(u8, &str)] = &[
    (0, "Not selected"),
    (1, "Light"),
    (2, "Light-Normal"),
    (3, "Normal"),
    (4, "Normal-Heavy"),
    (5, "Heavy"),
];
const SPIN: &[(u8, &str)] = &[
    (0, "No Spin"),
    (1, "Low"),
    (2, "Medium"),
    (3, "High"),
    (4, "Extra High"),
];
const SPIN_OPTIONS: &[&str] = &["Not selected", "No Spin", "Low", "Medium", "High", "Extra High"];
const TEMP: &[(u8, &str)] = &[
    (0, "Not selected"),
    (1, "Tap Cold"),
    (2, "Cold"),
    (3, "Semi Warm"),
    (4, "Warm"),
    (5, "Hot"),
    (6, "Extra Hot"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
        .unwrap_or("None")
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn components() -> HashMap<String, Value> {
    [
        ("power".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:washing-machine"})),
        ("status".into(), json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":labels(STATUS)})),
        ("remaining_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time estimate","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
        ("start".into(), json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","name":"Start","icon":"mdi:play"})),
        ("pause".into(), json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","name":"Pause","icon":"mdi:pause"})),
        ("smart_course".into(), json!({"platform":"sensor","unique_id":"$deviceid-smart_course","state_topic":"$this/smart_course","name":"Smart course","icon":"mdi:cloud-download-outline","device_class":"enum","options":labels(SMART_COURSE)})),
        ("downloaded_course".into(), json!({"platform":"sensor","unique_id":"$deviceid-downloaded_course","state_topic":"$this/downloaded_course","name":"Downloaded cycle","icon":"mdi:tray-arrow-down","entity_category":"diagnostic","device_class":"enum","options":labels(SMART_COURSE)})),
        ("power_off".into(), json!({"platform":"button","unique_id":"$deviceid-power_off","command_topic":"$this/power_off/set","name":"Power off","icon":"mdi:power"})),
        ("course".into(), json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline","device_class":"enum","options":labels(COURSE)})),
        ("delay_wash".into(), json!({"platform":"sensor","unique_id":"$deviceid-delay_wash","state_topic":"$this/delay_wash","name":"Delay wash","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
        ("door_lock".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","device_class":"lock"})),
        ("child_lock".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:lock-alert","entity_category":"diagnostic"})),
        ("remote_start".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
        ("turbo_wash".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-turbo_wash","state_topic":"$this/turbo_wash","name":"Turbo Wash","icon":"mdi:speedometer"})),
        ("extra_rinse".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_rinse","state_topic":"$this/extra_rinse","name":"Extra Rinse","icon":"mdi:water-sync"})),
        ("water_plus".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-water_plus","state_topic":"$this/water_plus","name":"Water Plus","icon":"mdi:water-plus"})),
        ("stain_care".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-stain_care","state_topic":"$this/stain_care","name":"Stain Care","icon":"mdi:liquid-spot"})),
        ("soak".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-soak","state_topic":"$this/soak","name":"Soak","icon":"mdi:water-outline"})),
        ("soil".into(), json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:liquid-spot","device_class":"enum","options":labels(SOIL)})),
        ("spin".into(), json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin speed","icon":"mdi:rotate-3d-variant","device_class":"enum","options":SPIN_OPTIONS})),
        ("temp".into(), json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer","device_class":"enum","options":labels(TEMP)})),
    ]
    .into_iter()
    .collect()
}

/// Start/resume packet from the current record. Door-lock is cleared; opt2 is copied whole.
fn start_command(rec: &[u8]) -> Vec<u8> {
    let verb = if rec[1] == PHASE_PAUSED { VERB_RESUME } else { VERB_START };
    vec![
        0xf0,
        0x26,
        rec[6],
        rec[8],
        rec[9],
        rec[10],
        rec[11],
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        rec[15] & !FLAG_DOOR_LOCK,
        rec[16],
        verb,
    ]
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    /// Marker-relative record (`rec[0] == 0x19`). Commands copy fields out of it.
    last_record: Mutex<Option<Vec<u8>>>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            last_record: Mutex::new(None),
        });
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
        if let Some(block) = process_common_status(buf, CLASS_BYTE, STATUS_LENGTH) {
            self.process_record(block);
        }
    }

    fn process_record(&self, block: &[u8]) {
        // Skip the prefix byte so offsets are relative to the 0x19 marker.
        if block.len() < 26 {
            return;
        }
        let rec = &block[1..];
        *self.last_record.lock() = Some(rec.to_vec());
        let phase = rec[1];
        let is_off = phase == PHASE_OFF;
        self.core.publish_on_off("power", !is_off);
        self.core.publish_property("status", named(STATUS, phase).into());
        let remain = if is_off { 0 } else { rec[2] as i64 * 60 + rec[3] as i64 };
        let initial = if is_off { 0 } else { rec[4] as i64 * 60 + rec[5] as i64 };
        self.core.publish_property("remaining_time", PropertyValue::Int(remain));
        self.core.publish_property("initial_time", PropertyValue::Int(initial));
        self.core.publish_property("course", named(COURSE, rec[6]).into());
        self.core
            .publish_property("smart_course", named(SMART_COURSE, rec[21]).into());
        self.core
            .publish_property("downloaded_course", named(SMART_COURSE, rec[24]).into());
        let delay = rec[13] as i64 * 60 + rec[14] as i64;
        self.core.publish_property("delay_wash", PropertyValue::Int(delay));
        let flags = rec[15];
        self.core.publish_on_off("child_lock", flags & FLAG_CHILD_LOCK != 0);
        // device_class lock is inverted: bit set means locked, which HA shows as OFF.
        self.core.publish_on_off("door_lock", flags & FLAG_DOOR_LOCK == 0);
        // Off keeps the last cycle's Turbo Wash bit. Report the options as off.
        let cycle_flags = if is_off { 0 } else { flags };
        let opt2 = if is_off { 0 } else { rec[16] };
        self.core.publish_on_off("turbo_wash", cycle_flags & FLAG_TURBO_WASH != 0);
        self.core.publish_on_off("extra_rinse", opt2 & OPT2_EXTRA_RINSE != 0);
        self.core.publish_on_off("water_plus", opt2 & OPT2_WATER_PLUS != 0);
        self.core.publish_on_off("stain_care", opt2 & OPT2_STAIN_CARE != 0);
        self.core.publish_on_off("soak", opt2 & OPT2_SOAK != 0);
        self.core
            .publish_on_off("remote_start", rec[17] & REMOTE_START_ON != 0);
        self.core.publish_property("soil", named(SOIL, rec[8]).into());
        let spin = if is_off { "Not selected" } else { named(SPIN, rec[9]) };
        self.core.publish_property("spin", spin.into());
        self.core.publish_property("temp", named(TEMP, rec[10]).into());
    }
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
    fn set_property(&self, prop: &str, _value: &str) {
        let rec = self.last_record.lock().clone();
        let Some(rec) = rec else {
            return;
        };
        if rec.len() < 17 {
            return;
        }
        match prop {
            "pause" => self.core.send(CMD_PAUSE),
            "power_off" => self.core.send(CMD_POWER_OFF),
            "start" => self.core.send(&start_command(&rec)),
            _ => {}
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
    const EC_IDLE: &str = "AA3C20EC0019000000011A00000000000000000000100000040800000064000019000000011A000000000000000000001000000408000000640037BB";
    const EC_HEAVY_DUTY: &str = "AA3C20EC0019050018011A0200050304000000000410000000050000006400001906001D011E0200000304000000000410000000050000006400FABB";
    const EC_RUNNING: &str = "AA3C20EC001905002B00340800030104000000000410000000030000006400001902002B0034080003010400000000041000000005000000640054BB";
    const EC_PAUSED: &str = "AA3C20EC001902002B00340800030104000000000410000000050000006400001902002B00340800030104000000000010000000050000006400A9BB";
    const EC_RESUMED: &str = "AA3C20EC001902002B00340800030104000000000010000000050000006400001905002B00340800030104000000000010000000020000006400ADBB";
    const EC_SPIN_TO_COMPLETE: &str = "AA3C20EC00190700010019010000030000000000440000000006000000640000190800000019010000030000000000440000000107000000640099BB";
    const EB_IDLE: &str = "AA2120EB00190000010023000000000000000000400000000208000000670091BB";
    const E2_SETTINGS: &str = "AA2120E2031903003B003A010003030400000000400000000201000000640046BB";
    const E2_STALE_WASHING: &str = "AA2120E2031905012A012A0800030104040000000415010002020000007800BBBB";
    const IDENTITY: &str = "AA372031020153414133393935353130330000D05F0000800000000000025341413339393534393033000044FC000040000000000008BB";
    const SHORT_ACK: &str = "AA0720D80EE2BB";
    const EC_REMAINING_OVER_AN_HOUR: &str = "AA3C20EC0019050018011A02000503040000000004100000000500000064000019060105011E0200000304000000000410000000050000006400FABB";
    const NORMAL_TURBO_ON: &str = "AA3C20EC001901000100000000000000000000000080000000000000006700001901003B003A010003030400000000400000000000000000670060BB";
    const DEEP_WASH_DIALLED: &str = "AA3C20EC001901012801270200050304000000000010000000000000006700001901003A003919000302040000000000100000000000000067005DBB";
    const WATER_PLUS: &str = "AA3C20EC001901003B003A0100030304000000004000000000000000008300001901003A00391900030204000000000012000000000000008300C6BB";
    const WHITES_EXTRA_RINSE: &str = "AA3C20EC0019010100003B0800010204000000000000000000006B00006B00001901020602050100050205020000004001000000008300008300F9BB";
    const SOAK: &str = "AA3C20EC001901003B003A0100030304000000004000000000000000008300001901011D003A0100030304000000004020000000000000008301E5BB";
    const STAIN_CARE: &str = "AA3C20EC001901011D003A01000303040000000040200000000000000083010019010120003A01000303030200000000080000000000000083000ABB";
    const CONTROL_LOCK: &str = "AA3C20EC00190100390038040001010100000000001000000000000000830000190100390038040001010100000000011000000000000000830068BB";
    const DELAY_16H: &str = "AA3C20EC00190100390038040001010100000F0000100000000000000083000019010039003804000101010000100000100000000000000083000EBB";
    const DELAY_19H: &str = "AA3C20EC00190100390038040001010100001200001000000000000000830000190100390038040001010100001300001000000000000000830034BB";
    const REMOTE_START: &str = "AA3C20EC00190100390038040001010100000000001001000000000000830000190100390038040001010100000000041001000000000000830017BB";
    const SPIN_ONLY: &str = "AA3C20EC0019010021002011000003010000000000100000000000000083000019010010000F120000030000000000000000000000000000830093BB";
    const PREWASH_NORMAL: &str = "AA3C20EC0019010039003804000101010000000000100000000000000083000019010119011829000303040000000040000000000000000083000FBB";
    const TUB_CLEAN: &str = "AA3C20EC001901011901182900030304000000004000000000000000008300001901010801080C00010305000000000000000000000000008300ACBB";
    const LIVE_COMPLETE: &str = "AA3C20EC00190700010129080000010004000000041101000006000000780000190800010129080000010004000000041100000207000000780098BB";
    const LIVE_COMPLETE_UNLOCKED: &str = "AA3C20EC00190800010129080000010004000000041100000207000000780000190800010129080000010004000000001100000207000000780099BB";
    const NORMAL_ARMED: &str = "AA3C20EC001901003B003A0100030304000000004000010000000000006400001901003B003A010003030400000000440001000000000000640021BB";
    const DEEPWASH_ARMED: &str = "AA3C20EC001901011200391900030204020000000011000000000000006400001901011200391900030204020000000011010000000000006400A4BB";
    const DL_CURTAINS: &str = "AA3C20EC0019010132013102000502050000000040200000000078000078010019010100003B0800010204000000000000000000006B00006B005EBB";
    const DL_SMALL_LOAD: &str = "AA3C20EC001901001D001D0700010302000000004000000000006500006500001901003B003A0100010204000000004000000000006400006400A9BB";
    const DL_SWEAT_STAINS: &str = "AA3C20EC001901003B003A01000102040000000040000000000064000064000019010130012F02000303040000000040200000000069000069011EBB";
    const DL_SWIMWEAR: &str = "AA3C20EC0019010130012F0200030304000000004020000000006900006901001901010F010E0400010102020000000001000000006A00006A0091BB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("T1789EFH_F", "LG WT7300CW", "1.0");
        let thinq = MockThinq2Device::new(ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn feed(hex: &str) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(hex));
        ha
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    #[test]
    fn power_is_not_a_running_sensor() {
        let (ha, _, _) = make();
        let comps = ha.device(ID).unwrap().config.unwrap().components;
        assert!(comps["power"].get("device_class").is_none());
        assert_eq!(comps["power"]["name"], json!("Power"));
        let opts = comps["status"]["options"].as_array().unwrap();
        assert!(opts.iter().any(|v| v == "End"));
        assert!(opts.iter().any(|v| v == "Washing"));
        assert!(!opts.iter().any(|v| v == "Wash (main)"));
    }

    #[test]
    fn start_requests_a_status_snapshot() {
        let (_, thinq, dev) = make();
        dev.start();
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1121010000001800B5BB"
        );
    }

    #[test]
    fn end_of_cycle_reports_complete_not_the_spin_it_left() {
        let ha = feed(EC_SPIN_TO_COMPLETE);
        expect(&ha, "status", "End");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "power", "ON");
    }

    #[test]
    fn pause_and_resume_follow_the_current_record() {
        expect(&feed(EC_RUNNING), "status", "Paused");
        expect(&feed(EC_PAUSED), "status", "Paused");
        expect(&feed(EC_RESUMED), "status", "Washing");
    }

    #[test]
    fn heavy_duty_reports_the_phase_the_machine_is_in_now() {
        let ha = feed(EC_HEAVY_DUTY);
        expect(&ha, "status", "Rinsing");
        expect(&ha, "remaining_time", "29");
    }

    #[test]
    fn remaining_time_reads_the_hour_byte() {
        expect(&feed(EC_REMAINING_OVER_AN_HOUR), "remaining_time", "65");
    }

    #[test]
    fn initial_time_reads_the_hour_byte() {
        let ha = feed(EC_HEAVY_DUTY);
        expect(&ha, "initial_time", "90");
        assert!(prop(&ha, "remaining_time").unwrap().parse::<i64>().unwrap() < 90);
    }

    #[test]
    fn powered_off_times_are_zero() {
        let ha = feed(EB_IDLE);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "initial_time", "0");
    }

    #[test]
    fn heavy_duty_current_record_settings() {
        let ha = feed(EC_HEAVY_DUTY);
        expect(&ha, "spin", "High");
        expect(&ha, "temp", "Warm");
        expect(&ha, "soil", "Not selected");
    }

    #[test]
    fn bedding_decodes_a_different_triple() {
        let ha = feed(EC_RUNNING);
        expect(&ha, "soil", "Normal");
        expect(&ha, "spin", "Low");
        expect(&ha, "temp", "Warm");
    }

    #[test]
    fn idle_settings_are_unset() {
        let ha = feed(EC_IDLE);
        expect(&ha, "course", "Not selected");
        expect(&ha, "soil", "Not selected");
        expect(&ha, "spin", "Not selected");
        expect(&ha, "temp", "Not selected");
    }

    #[test]
    fn complete_disarms_remote_start_and_unlocks() {
        let done = feed(LIVE_COMPLETE);
        expect(&done, "status", "End");
        expect(&done, "remote_start", "OFF");
        expect(&done, "door_lock", "OFF");
        expect(&feed(LIVE_COMPLETE_UNLOCKED), "door_lock", "ON");
    }

    #[test]
    fn non_status_frames_publish_nothing() {
        for hex in [E2_SETTINGS, E2_STALE_WASHING, IDENTITY, SHORT_ACK] {
            assert!(prop(&feed(hex), "power").is_none(), "{hex}");
        }
    }

    #[test]
    fn unknown_phase_publishes_the_unknown_fallback() {
        let ha = feed("AA2120EB0019FF00010023000000000000000000400000000208000000670091BB");
        expect(&ha, "status", "None");
    }

    #[test]
    fn course_codes_decode_to_panel_names() {
        expect(&feed(NORMAL_TURBO_ON), "course", "Normal");
        expect(&feed(EC_HEAVY_DUTY), "course", "Heavy Duty");
        expect(&feed(DEEP_WASH_DIALLED), "course", "Deep Wash");
        expect(&feed(SPIN_ONLY), "course", "Spin Only");
        expect(&feed(PREWASH_NORMAL), "course", "Pre Wash+Normal");
        expect(&feed(TUB_CLEAN), "course", "Tub Clean");
    }

    #[test]
    fn downloaded_position_reports_the_base_cycle() {
        let ha = feed(WHITES_EXTRA_RINSE);
        expect(&ha, "course", "Normal");
        expect(&ha, "temp", "Hot");
        expect(&ha, "soil", "Heavy");
    }

    #[test]
    fn powered_off_reports_no_cycle_options() {
        let ha = feed(EB_IDLE);
        expect(&ha, "status", "Off");
        expect(&ha, "turbo_wash", "OFF");
        expect(&ha, "course", "Not selected");
    }

    #[test]
    fn turbo_wash_bit() {
        expect(&feed(NORMAL_TURBO_ON), "turbo_wash", "ON");
        expect(&feed(DEEP_WASH_DIALLED), "turbo_wash", "OFF");
    }

    #[test]
    fn water_plus_is_distinct_from_deep_wash() {
        let dialled = feed(DEEP_WASH_DIALLED);
        let pressed = feed(WATER_PLUS);
        assert_eq!(prop(&dialled, "course"), prop(&pressed, "course"));
        expect(&dialled, "water_plus", "OFF");
        expect(&pressed, "water_plus", "ON");
    }

    #[test]
    fn extra_rinse_stain_care_and_soak() {
        expect(&feed(WHITES_EXTRA_RINSE), "extra_rinse", "ON");
        expect(&feed(NORMAL_TURBO_ON), "extra_rinse", "OFF");
        expect(&feed(STAIN_CARE), "stain_care", "ON");
        expect(&feed(SOAK), "soak", "ON");
        expect(&feed(STAIN_CARE), "soak", "OFF");
        expect(&feed(SOAK), "stain_care", "OFF");
    }

    #[test]
    fn control_lock() {
        expect(&feed(CONTROL_LOCK), "child_lock", "ON");
        expect(&feed(NORMAL_TURBO_ON), "child_lock", "OFF");
    }

    #[test]
    fn door_locks_during_ordinary_cycles_with_remote_start_off() {
        for hex in [EC_HEAVY_DUTY, EC_SPIN_TO_COMPLETE] {
            let ha = feed(hex);
            expect(&ha, "door_lock", "OFF");
            expect(&ha, "remote_start", "OFF");
        }
    }

    #[test]
    fn remote_start_and_door_lock_are_separate_bits() {
        let ha = feed(REMOTE_START);
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "OFF");
        expect(&feed(NORMAL_TURBO_ON), "remote_start", "OFF");
        expect(&feed(NORMAL_TURBO_ON), "door_lock", "ON");
    }

    #[test]
    fn delay_wash_is_binary_hours() {
        expect(&feed(DELAY_16H), "delay_wash", "960");
        expect(&feed(DELAY_19H), "delay_wash", "1140");
        expect(&feed(NORMAL_TURBO_ON), "delay_wash", "0");
    }

    #[test]
    fn two_hour_remaining_estimate() {
        expect(&feed(WHITES_EXTRA_RINSE), "remaining_time", "126");
    }

    #[test]
    fn no_published_value_uses_the_reserved_none_payload() {
        for hex in [EB_IDLE, EC_IDLE, NORMAL_TURBO_ON, WHITES_EXTRA_RINSE, SPIN_ONLY] {
            let ha = feed(hex);
            let props = ha.device(ID).unwrap().properties;
            for (name, value) in props {
                assert_ne!(value.as_string(), "None", "{name} on {hex}");
            }
        }
    }

    #[test]
    fn start_reproduces_the_cloud_command() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(NORMAL_ARMED));
        dev.set_property("start", "PRESS");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA15F02601030304000000000000000040000376BB"
        );
    }

    #[test]
    fn start_copies_rinse_and_opt2_verbatim() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(DEEPWASH_ARMED));
        dev.set_property("start", "PRESS");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA15F02619030204020000000000000000110358BB"
        );
    }

    #[test]
    fn door_lock_bit_is_not_echoed() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(NORMAL_ARMED));
        dev.set_property("start", "PRESS");
        let pkt = &thinq.outbox()[0];
        let inner = &pkt[2..pkt.len() - 2];
        assert_eq!(inner[14], 0x40);
    }

    #[test]
    fn start_is_not_gated_on_remote_start() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(EB_IDLE));
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox().len(), 1);
    }

    #[test]
    fn start_refuses_before_any_status() {
        let (_, thinq, dev) = make();
        dev.set_property("start", "PRESS");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn pause_sends_the_cloud_command() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(NORMAL_ARMED));
        dev.set_property("pause", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F02404010099BB");
    }

    #[test]
    fn start_while_paused_uses_the_resume_verb() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(EC_PAUSED));
        dev.set_property("start", "PRESS");
        let pkt = &thinq.outbox()[0];
        let inner = &pkt[2..pkt.len() - 2];
        assert_eq!(inner[0], 0xf0);
        assert_eq!(inner[1], 0x26);
        assert_eq!(inner[16], 0x01);
    }

    #[test]
    fn unknown_property_sends_nothing() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(NORMAL_ARMED));
        dev.set_property("nonsense", "PRESS");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn smart_courses_decode_to_app_names_and_base_cycles() {
        expect(&feed(DL_CURTAINS), "smart_course", "Curtains");
        expect(&feed(DL_SMALL_LOAD), "smart_course", "Small Load");
        expect(&feed(DL_SWEAT_STAINS), "smart_course", "Sweat Stains");
        expect(&feed(DL_SWIMWEAR), "smart_course", "Swimwear");
        expect(&feed(DL_CURTAINS), "course", "Bedding");
        expect(&feed(DL_SMALL_LOAD), "course", "Normal");
        expect(&feed(DL_SWEAT_STAINS), "course", "Heavy Duty");
        expect(&feed(DL_SWIMWEAR), "course", "Delicates");
        expect(&feed(DL_SWIMWEAR), "downloaded_course", "Swimwear");
        expect(&feed(EC_HEAVY_DUTY), "smart_course", "Not selected");
        assert_ne!(
            prop(&feed(WHITES_EXTRA_RINSE), "smart_course"),
            prop(&feed(NORMAL_TURBO_ON), "smart_course")
        );
        assert_eq!(
            prop(&feed(WHITES_EXTRA_RINSE), "course"),
            prop(&feed(NORMAL_TURBO_ON), "course")
        );
    }

    #[test]
    fn power_off_sends_the_cloud_command() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(NORMAL_ARMED));
        dev.set_property("power_off", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F0240101009CBB");
    }

    #[test]
    fn power_off_is_not_gated_on_remote_start() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(EB_IDLE));
        dev.set_property("power_off", "PRESS");
        assert_eq!(thinq.outbox().len(), 1);
    }

    #[test]
    fn no_spin_is_only_a_real_selection() {
        expect(&feed(EB_IDLE), "spin", "Not selected");
        expect(&feed(SPIN_ONLY), "spin", "High");
    }

    #[test]
    fn acks_heartbeat_and_settings_but_not_status_records() {
        let (_, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA09207200000010BB"));
        thinq.emit_data(&hex_decode(SHORT_ACK));
        thinq.emit_data(&hex_decode(E2_SETTINGS));
        let acks: Vec<String> = thinq
            .sent()
            .iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            acks,
            vec![
                "AA08F00072044DBB".to_string(),
                "AA08F000D8042BBB".to_string(),
                "AA08F000E204DDBB".to_string(),
            ]
        );
        assert!(thinq.outbox().is_empty());
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(EC_HEAVY_DUTY));
        assert!(thinq.sent().iter().all(|m| m.cmd != "ack"));
    }
}
