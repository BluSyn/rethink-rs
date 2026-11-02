//! F3L2CYU__ front-load washer (AABB).

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_FRAME_LEN: usize = 54;
const RECORD_B_OFFSET: usize = 29;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_STATUS_FRAME_LEN: usize = 28;
const SINGLE_RECORD_OFFSET: usize = 3;

const PHASE_OFFSET: usize = 1;
const TIME_HOUR_OFFSET: usize = 2;
const TIME_MIN_OFFSET: usize = 3;
const COURSE_OFFSET: usize = 6;
const SOIL_OFFSET: usize = 8;
const SPIN_OFFSET: usize = 9;
const TEMP_OFFSET: usize = 10;
const EXTRA_RINSE_COUNT_OFFSET: usize = 11;
const RESERVE_HOUR_OFFSET: usize = 13;
const RESERVE_MIN_OFFSET: usize = 14;
const FLAGS_OFFSET: usize = 15;
const FLAG_TURBO_WASH: u8 = 0x80;
const FLAG_EXTRA_RINSE: u8 = 0x40;
const FLAG_PRE_WASH: u8 = 0x08;
const FLAG_STEAM: u8 = 0x04;
const FLAG_DELAY_ACTIVE: u8 = 0x02;
const OPT2_OFFSET: usize = 16;
const OPT2_DOOR_LOCKED: u8 = 0x80;
const OPT2_COLD_WASH: u8 = 0x10;
const DOOR_OFFSET: usize = 17;
const DOOR_CLOSED: u8 = 0x02;
const PHASE_OFF: u8 = 0x00;

const STATUS_OPTIONS: &[&str] = &[
    "Off",
    "Standby",
    "Paused",
    "Delayed",
    "Detecting",
    "Error",
    "Filling",
    "Detergent amount",
    "Washing",
    "Pre-wash",
    "Rinsing",
    "Rinse hold",
    "Spinning",
    "Drying",
    "End",
    "Refreshing",
    "Smart Diagnosis",
];

fn status_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x05, "Standby"),
        (83, "Standby"),
        (0x06, "Paused"),
        (85, "Paused"),
        (0x07, "Error"),
        (0x0a, "Delayed"),
        (0x14, "Detecting"),
        (21, "Filling"),
        (22, "Detergent amount"),
        (0x17, "Washing"),
        (84, "Washing"),
        (24, "Pre-wash"),
        (0x1e, "Rinsing"),
        (31, "Rinse hold"),
        (0x28, "Spinning"),
        (50, "Drying"),
        (0x3c, "End"),
        (61, "Refreshing"),
        (79, "Smart Diagnosis"),
    ])
}
fn course_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x01, "Tub Clean"),
        (0x02, "Bright Whites"),
        (0x03, "Allergiene"),
        (0x04, "Sanitary"),
        (0x05, "Bedding"),
        (0x06, "Heavy Duty"),
        (0x07, "Normal"),
        (0x08, "Sportswear"),
        (0x09, "Perm Press"),
        (0x0a, "Delicates"),
        (0x0b, "Towels"),
        (0x0c, "Speed Wash"),
        (0x0d, "Rinse+Spin"),
        (0x0e, "Small Load"),
    ])
}
fn soil_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "Light"),
        (2, "Light-Normal"),
        (3, "Normal"),
        (4, "Normal-Heavy"),
        (5, "Heavy"),
    ])
}
fn spin_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "No Spin"),
        (2, "Low"),
        (3, "Medium"),
        (4, "High"),
        (5, "Extra High"),
    ])
}
fn temp_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "Tap Cold"),
        (2, "Cold"),
        (4, "Warm"),
        (6, "Hot"),
        (7, "Extra Hot"),
    ])
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
            ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":STATUS_OPTIONS})),
            ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
            ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
            ("reserve_time", json!({"platform":"sensor","unique_id":"$deviceid-reserve_time","state_topic":"$this/reserve_time","name":"Delay Wash time remaining","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min"})),
            ("soil", json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:liquid-spot"})),
            ("spin", json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew"})),
            ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer"})),
            ("extra_rinse", json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_rinse","state_topic":"$this/extra_rinse","name":"Extra rinse","icon":"mdi:water-sync"})),
            ("extra_rinse_count", json!({"platform":"sensor","unique_id":"$deviceid-extra_rinse_count","state_topic":"$this/extra_rinse_count","name":"Extra rinse count","icon":"mdi:water-sync","state_class":"measurement"})),
            ("pre_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-pre_wash","state_topic":"$this/pre_wash","name":"Pre-wash","icon":"mdi:water-sync"})),
            ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"})),
            ("cold_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-cold_wash","state_topic":"$this/cold_wash","name":"Cold wash","icon":"mdi:snowflake"})),
            ("turbo_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbo_wash","state_topic":"$this/turbo_wash","name":"TurboWash","icon":"mdi:rocket-launch"})),
            ("delay_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-delay_wash","state_topic":"$this/delay_wash","name":"Delay Wash","icon":"mdi:clock-plus-outline"})),
            ("door", json!({"platform":"binary_sensor","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","device_class":"door"})),
            ("door_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","icon":"mdi:lock","entity_category":"diagnostic"})),
        ];
        let mut components = Map::new();
        for (k, v) in comps {
            components.insert(k.into(), v);
        }
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

    fn process_status(&self, buf: &[u8], record_offset: usize, expected_len: usize) {
        if buf.len() != expected_len {
            return;
        }
        let rec = &buf[record_offset..];
        if rec.is_empty() || rec[0] != 0x18 {
            return;
        }
        let phase = rec[PHASE_OFFSET];
        let is_off = phase == PHASE_OFF;

        self.core.publish_on_off("power", !is_off);
        self.core.publish_property(
            "status",
            status_map().get(&phase).copied().unwrap_or("None").into(),
        );
        self.core.publish_property(
            "course",
            course_map()
                .get(&rec[COURSE_OFFSET])
                .copied()
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "remaining_time",
            if is_off {
                0i64
            } else {
                rec[TIME_HOUR_OFFSET] as i64 * 60 + rec[TIME_MIN_OFFSET] as i64
            }
            .into(),
        );
        self.core.publish_property(
            "reserve_time",
            if is_off {
                0i64
            } else {
                rec[RESERVE_HOUR_OFFSET] as i64 * 60 + rec[RESERVE_MIN_OFFSET] as i64
            }
            .into(),
        );
        self.core.publish_property(
            "soil",
            soil_map()
                .get(&rec[SOIL_OFFSET])
                .copied()
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "spin",
            spin_map()
                .get(&rec[SPIN_OFFSET])
                .copied()
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "temp",
            temp_map()
                .get(&rec[TEMP_OFFSET])
                .copied()
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "extra_rinse_count",
            ((rec[EXTRA_RINSE_COUNT_OFFSET] >> 4) as i64).into(),
        );

        let flags = rec[FLAGS_OFFSET];
        self.core.publish_on_off("extra_rinse", flags & FLAG_EXTRA_RINSE != 0);
        self.core.publish_on_off("pre_wash", flags & FLAG_PRE_WASH != 0);
        self.core.publish_on_off("steam", flags & FLAG_STEAM != 0);
        self.core.publish_on_off("turbo_wash", flags & FLAG_TURBO_WASH != 0);
        self.core.publish_on_off("delay_wash", flags & FLAG_DELAY_ACTIVE != 0);

        let opt2 = rec[OPT2_OFFSET];
        self.core.publish_on_off("cold_wash", opt2 & OPT2_COLD_WASH != 0);
        self.core.publish_on_off("door_lock", opt2 & OPT2_DOOR_LOCKED != 0);
        self.core.publish_on_off("door", rec[DOOR_OFFSET] != DOOR_CLOSED);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < 2 || buf[0] != 0x20 {
            return;
        }
        if buf[1] == STATUS_FRAME_TYPE {
            self.process_status(buf, RECORD_B_OFFSET, STATUS_FRAME_LEN);
        } else if buf[1] == SINGLE_STATUS_FRAME_TYPE {
            self.process_status(buf, SINGLE_RECORD_OFFSET, SINGLE_STATUS_FRAME_LEN);
        }
    }

    pub fn set_property(&self, _prop: &str, _mqtt_value: &str) {}
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {}
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        Device::set_property(self, prop, value);
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
    use rethink_core::{hex_decode, Metadata, MockHaConnection, MockThinq2Device, Thinq2Device};

    const DEVICE_ID: &str = "test-id";

    // Real 0xEB/0xEC frames captured from a WM3900HBA (F3L2CYU2E.ABLEUUS), from upstream's tests.
    const EB_RECONNECT_IDLE: &str =
        "aa2020eb001805000000000000000000000000000000000000003318000068bb";
    const NORMAL_IDLE: &str =
        "aa3a20ec0018050000000000000000000000000000000000000033170000001805002e002e07000304040100000080000000000033170000f8bb";
    const COURSE_TOWELS: &str =
        "aa3a20ec001805002900290a000303020200000000000000000033170007001805010101010b0003050403000000000000000000331700001cbb";
    const COURSE_SPEEDWASH: &str =
        "aa3a20ec001805010101010b000305040300000000000000000033170000001805000f000f0c0001050601000000000000000000331700074fbb";
    const COURSE_RINSE_SPIN: &str =
        "aa3a20ec001805000f000f0c000105060100000000000000000033170007001805000b000b0d0000040000000000000000000000331700077fbb";
    const COURSE_SMALL_LOAD: &str =
        "aa3a20ec001805000b000b0d000004000000000000000000000033170007001805002d002d0e000304040200000000000000000033170f0025bb";
    const COURSE_TUB_CLEAN: &str =
        "aa3a20ec001805002d002d0e000304040200000000000000000033170f00001805011d011d01000003000200000004000000000033170000ddbb";
    const SOIL_NORMAL_HEAVY: &str =
        "aa3a20ec001805002e002e070003040401000000800000000000331700000018050033003307000404040100000080000000010033170000fdbb";
    const SPIN_EXTRA_HIGH: &str =
        "aa3a20ec001805002e002e07000304040100000080000000010033170000001805003a003a07000305040100000080000000010033170000e2bb";
    const TEMP_HOT: &str =
        "aa3a20ec001805002e002e07000304040100000080000000010033170000001805002e002e07000304060100000080000000010033170000f5bb";
    const COLD_WASH_ON: &str =
        "aa3a20ec001805002e002e0700030404010000008000000001003317000000180501060106070003040201000000801000000100331700000bbb";
    const EXTRA_RINSE_ON_1: &str =
        "aa3a20ec001805002e002e0700030404010000008000000001003317000000180500330033070003040411000000c0000000010033170000adbb";
    const PRE_WASH_ON: &str =
        "aa3a20ec001805002e002e0700030404010000008000000001003317000000180501010101070003040401000000880000000100331700001bbb";
    const STEAM_ON: &str =
        "aa3a20ec001805002e002e07000304040100000080000000010033170000001805020b020b070000040001000000840000000100331700000cbb";
    const SELECTING_TO_WASHING: &str =
        "aa3a20ec001805000f000f0c000105060100000000800200020033170007001817000f000f0c00010506010000000080020000053317000702bb";
    const WASHING: &str =
        "aa3a20ec001817000f000f0c000105060100000000800200000533170007001817000e000f0c0001050601000000008002000005331700073ebb";
    const RINSING: &str =
        "aa3a20ec001817000d000f0c00010506010000000080020012053317000700181e000c000f0c000005060100000000800200231733170007e1bb";
    const RINSING_LOCKED_CLOSED: &str =
        "aa3a20ec00181e000c000f0c00000506010000000080020023173317000700181e000b000f0c00000506010000000080020024173317000789bb";
    const SPINNING: &str =
        "aa3a20ec00181e000a000f0c0000050601000000008002002506331700070018280009000f0c000005000000000000800200261e3317000780bb";
    const COMPLETE: &str =
        "aa3a20ec0018280001000f0c000005000000000000800200391e3317000700183c00010000fe000000000000000000000000402833180000d4bb";
    const PAUSE_UNLOCKS: &str =
        "aa3a20ec00181e000b000f0c000005060100000000800200241733170007001806000b000f0c000005060100000000000200251e3317000719bb";
    const DOOR_OPENS_WHILE_PAUSED: &str =
        "aa3a20ec001806000b000f0c000005060100000000000200251e33170007001806000b000f0c000005060100000000000000251e33170007efbb";
    const RESUME_RELOCKS: &str =
        "aa3a20ec00181e000b000f0c000005060100000000000000251e3317000700181e000b000f0c0000050601000000008000002506331700076dbb";
    const DOOR_CLOSES_AGAIN: &str =
        "aa3a20ec00181e000b000f0c00000506010000000080000025063317000700181e000b000f0c000005060100000000800200250633170007efbb";
    const DELAY_1H_ARMED_STILL_SELECTING: &str =
        "aa3a20ec001805000f000f0c000105060100000000000000400033180007001805000f000f0c000105060100010002000000400033180007eabb";
    const TURBO_ON_STILL_SELECTING: &str =
        "aa3a20ec001805000f000f0c000105060100010002000000400033180007001805000f000f0c00010506010001008200000040003318000717bb";
    const DELAY_PENDING_LOCKS_DOOR: &str =
        "aa3a20ec001805000f000f0c00010506010001008200000040003318000700180a000f000f0c00010506010001008280000040053318000719bb";
    const DELAY_DOOR_CLOSES: &str =
        "aa3a20ec00180a000f000f0c00010506010001008280000040053318000700180a000f000f0c000105060100010082800200000533180007cdbb";
    const APP_PAUSE: &str =
        "aa3a20ec00180a000f000f0c000105060100003a82800200000533180007001806000f000f0c000105060100003a82800200000a3318000798bb";
    const APP_RESUME_3H: &str =
        "aa3a20ec001806000f000f0c000105060100003a82800200000a3318000700180a000f000f0c000105060100030082800200000633180007c2bb";
    const RESERVE_TICKS: &str =
        "aa3a20ec00180a000f000f0c00010506010003008280020000063318000700180a000f000f0c000105060100023b82800200000633180007cfbb";
    const APP_POWEROFF: &str =
        "aa3a20ec00180a000f000f0c000105060100023a8280020000063318000700180000010000fe000000000000000000000200000a331800001abb";

    fn meta() -> Metadata { Metadata::new("F3L2CYU__", "F3L2CYU__", "1.0") }
    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    fn build_eb(phase: u8, course: u8, soil: u8, spin: u8, temp: u8, flags: u8, opt2: u8, door: u8, h: u8, m: u8) -> Vec<u8> {
        // single status: inner len 28 = 3 header + 25 rec
        let mut rec = vec![0u8; 25];
        rec[0] = 0x18;
        rec[1] = phase;
        rec[2] = h; rec[3] = m;
        rec[6] = course;
        rec[8] = soil;
        rec[9] = spin;
        rec[10] = temp;
        rec[11] = 0x11; // rinse count high nibble 1
        rec[15] = flags;
        rec[16] = opt2;
        rec[17] = door;
        let mut inner = vec![0x20, 0xeb, 0x00];
        inner.extend(rec);
        assert_eq!(inner.len(), 28);
        let mut pkt = vec![0xaa, (inner.len()+4) as u8];
        pkt.extend(inner);
        pkt.push(0); pkt.push(0xbb);
        pkt
    }

    #[test]
    fn config_and_status() {
        let (ha, thinq, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert!(comps.contains_key("turbo_wash"));
        assert!(comps.contains_key("door"));

        thinq.emit_data(&build_eb(0x00, 0, 0, 0, 0, 0, 0, 0x02, 0, 0));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));

        thinq.emit_data(&build_eb(0x17, 0x07, 3, 4, 4, 0x80 | 0x40, 0x80 | 0x10, 0x02, 0, 45));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("45"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Warm"));
        assert_eq!(prop(&ha, "turbo_wash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "cold_wash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    #[test]
    fn status_is_an_enum_over_every_status_label() {
        let (ha, _, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(comps["status"]["device_class"], "enum");
        assert_eq!(comps["status"]["options"], json!(STATUS_OPTIONS));
        let mut labels: Vec<&str> = status_map().into_values().collect();
        labels.sort_unstable();
        labels.dedup();
        let mut options = STATUS_OPTIONS.to_vec();
        options.sort_unstable();
        assert_eq!(labels, options);
    }

    #[test]
    fn eb_single_record_frames_decode_with_the_ec_offsets() {
        let (ha, thinq, _) = make();
        emit(&thinq, EB_RECONNECT_IDLE);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Standby");
        expect(&ha, "course", "None");
        expect(&ha, "door", "ON");
    }

    #[test]
    fn idle_selecting_on_normal_settles_to_defaults() {
        let (ha, thinq, _) = make();
        emit(&thinq, NORMAL_IDLE);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Standby");
        expect(&ha, "course", "Normal");
        expect(&ha, "remaining_time", "46");
        expect(&ha, "soil", "Normal");
        expect(&ha, "spin", "High");
        expect(&ha, "temp", "Warm");
        expect(&ha, "turbo_wash", "ON");
        expect(&ha, "door", "ON");
        expect(&ha, "door_lock", "OFF");
    }

    #[test]
    fn course_byte_identifies_each_program() {
        let (ha, thinq, _) = make();
        for (frame, course) in [
            (COURSE_TOWELS, "Towels"),
            (COURSE_SPEEDWASH, "Speed Wash"),
            (COURSE_RINSE_SPIN, "Rinse+Spin"),
            (COURSE_SMALL_LOAD, "Small Load"),
            (COURSE_TUB_CLEAN, "Tub Clean"),
        ] {
            emit(&thinq, frame);
            expect(&ha, "course", course);
        }
    }

    #[test]
    fn single_variable_settings_toggles_decode() {
        let (ha, thinq, _) = make();

        emit(&thinq, SOIL_NORMAL_HEAVY);
        expect(&ha, "soil", "Normal-Heavy");

        emit(&thinq, SPIN_EXTRA_HIGH);
        expect(&ha, "spin", "Extra High");

        emit(&thinq, TEMP_HOT);
        expect(&ha, "temp", "Hot");

        emit(&thinq, COLD_WASH_ON);
        expect(&ha, "cold_wash", "ON");
        expect(&ha, "temp", "Cold");

        emit(&thinq, EXTRA_RINSE_ON_1);
        expect(&ha, "extra_rinse", "ON");
        expect(&ha, "extra_rinse_count", "1");

        emit(&thinq, PRE_WASH_ON);
        expect(&ha, "pre_wash", "ON");

        emit(&thinq, STEAM_ON);
        expect(&ha, "steam", "ON");
    }

    #[test]
    fn speed_wash_run_phases_and_countdown() {
        let (ha, thinq, _) = make();

        emit(&thinq, SELECTING_TO_WASHING);
        expect(&ha, "status", "Washing");
        expect(&ha, "remaining_time", "15");

        emit(&thinq, WASHING);
        expect(&ha, "status", "Washing");
        expect(&ha, "remaining_time", "14");

        emit(&thinq, RINSING);
        expect(&ha, "status", "Rinsing");
        expect(&ha, "remaining_time", "12");

        emit(&thinq, SPINNING);
        expect(&ha, "status", "Spinning");
        expect(&ha, "remaining_time", "9");

        emit(&thinq, COMPLETE);
        expect(&ha, "status", "End");
        // The Complete frame parks the course byte on the 0xfe sentinel.
        expect(&ha, "course", "None");
    }

    #[test]
    fn pause_unlocks_door_and_resume_relocks() {
        let (ha, thinq, _) = make();

        emit(&thinq, RINSING_LOCKED_CLOSED);
        expect(&ha, "door_lock", "ON");
        expect(&ha, "door", "OFF");

        emit(&thinq, PAUSE_UNLOCKS);
        expect(&ha, "status", "Paused");
        expect(&ha, "door_lock", "OFF");
        expect(&ha, "door", "OFF");

        emit(&thinq, DOOR_OPENS_WHILE_PAUSED);
        expect(&ha, "door", "ON");
        expect(&ha, "door_lock", "OFF");

        emit(&thinq, RESUME_RELOCKS);
        expect(&ha, "status", "Rinsing");
        expect(&ha, "door_lock", "ON");

        emit(&thinq, DOOR_CLOSES_AGAIN);
        expect(&ha, "door", "OFF");
        expect(&ha, "door_lock", "ON");
    }

    #[test]
    fn delay_wash_countdown_remote_pause_resume_and_power_off() {
        let (ha, thinq, _) = make();

        emit(&thinq, DELAY_1H_ARMED_STILL_SELECTING);
        expect(&ha, "status", "Standby");
        expect(&ha, "delay_wash", "ON");
        expect(&ha, "reserve_time", "60");

        emit(&thinq, TURBO_ON_STILL_SELECTING);
        expect(&ha, "turbo_wash", "ON");

        emit(&thinq, DELAY_PENDING_LOCKS_DOOR);
        expect(&ha, "status", "Delayed");
        expect(&ha, "door_lock", "ON");

        emit(&thinq, DELAY_DOOR_CLOSES);
        expect(&ha, "door", "OFF");

        emit(&thinq, APP_PAUSE);
        expect(&ha, "status", "Paused");

        emit(&thinq, APP_RESUME_3H);
        expect(&ha, "status", "Delayed");
        expect(&ha, "reserve_time", "180");

        emit(&thinq, RESERVE_TICKS);
        expect(&ha, "reserve_time", "179");

        emit(&thinq, APP_POWEROFF);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "reserve_time", "0");
        expect(&ha, "course", "None");
    }

    #[test]
    fn unknown_and_non_envelope_frames_are_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, NORMAL_IDLE);
        let before = prop(&ha, "status");
        emit(&thinq, "aa0720d81795bb");
        emit(&thinq, "aa09207200000010bb");
        emit(&thinq, "001122");
        assert_eq!(prop(&ha, "status"), before);
    }
}
