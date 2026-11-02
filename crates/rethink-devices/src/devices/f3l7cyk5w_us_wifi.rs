//! F3L7CYK5W_US_WIFI front-load washer (AABB).
//!
//! Port of upstream PR #135. Shares 25-byte 0x18 record layout with F3L2CYU__ but
//! has a different course table and additional fields (child lock, rinse+spin,
//! live rinse count, initial time, tub clean count, load level). Deliberately
//! ignores 0xE2 (stale post-cycle replay).

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::sync::Arc;

const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_FRAME_LEN: usize = 54;
const RECORD_B_OFFSET: usize = 29;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_STATUS_FRAME_LEN: usize = 28;
const SINGLE_RECORD_OFFSET: usize = 3;
const RECORD_MARKER: u8 = 0x18;

const PHASE_OFFSET: usize = 1;
const TIME_HOUR_OFFSET: usize = 2;
const TIME_MIN_OFFSET: usize = 3;
const INITIAL_TIME_HOUR_OFFSET: usize = 4;
const INITIAL_TIME_MIN_OFFSET: usize = 5;
const COURSE_OFFSET: usize = 6;
const SOIL_OFFSET: usize = 8;
const SPIN_OFFSET: usize = 9;
const TEMP_OFFSET: usize = 10;
const RINSE_OFFSET: usize = 11;
const RESERVE_HOUR_OFFSET: usize = 13;
const RESERVE_MIN_OFFSET: usize = 14;
const FLAGS_OFFSET: usize = 15;
const FLAG_CHILD_LOCK: u8 = 0x01;
const FLAG_DELAY_ACTIVE: u8 = 0x02;
const FLAG_STEAM: u8 = 0x04;
const FLAG_PRE_WASH: u8 = 0x08;
const FLAG_RINSE_SPIN: u8 = 0x20;
const FLAG_EXTRA_RINSE: u8 = 0x40;
const OPT2_OFFSET: usize = 16;
const OPT2_COLD_WASH: u8 = 0x10;
const OPT2_DOOR_LOCKED: u8 = 0x80;
const TCL_COUNT_OFFSET: usize = 22;
const LOAD_LEVEL_OFFSET: usize = 24;
const PHASE_OFF: u8 = 0x00;
/// End (cloud code 60). Timers park on a stale minute here, same as Off.
const PHASE_END: u8 = 0x3c;

// The status, course, soil, spin and temperature tables are shared with F3L2CYV5W_WIFI.
pub(crate) const STATUS_OPTIONS: &[&str] = &[
    "Off",
    "Standby",
    "Paused",
    "Delayed",
    "Detecting",
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

/// Cloud state enum. Unmapped codes publish `None` rather than a guessed label.
pub(crate) fn status_name(phase: u8) -> &'static str {
    match phase {
        0 => "Off",
        5 | 83 => "Standby",
        6 | 85 => "Paused",
        10 => "Delayed",
        20 => "Detecting",
        21 => "Filling",
        22 => "Detergent amount",
        23 | 84 => "Washing",
        24 => "Pre-wash",
        30 => "Rinsing",
        31 => "Rinse hold",
        40 => "Spinning",
        50 => "Drying",
        60 => "End",
        61 => "Refreshing",
        79 => "Smart Diagnosis",
        _ => "None",
    }
}
pub(crate) const COURSE: &[(u8, &str)] = &[
    (0x01, "Tub Clean"),
    (0x02, "Allergiene"),
    (0x03, "Sanitary"),
    (0x04, "Bedding"),
    (0x05, "Heavy Duty"),
    (0x06, "Normal"),
    (0x07, "Bright Whites"),
    (0x08, "Perm Press"),
    (0x09, "Delicates"),
    (0x0a, "Towels"),
    (0x0b, "Speed Wash"),
    (0x0c, "Downloaded"),
];
pub(crate) const SOIL: &[(u8, &str)] = &[
    (1, "Light"),
    (2, "Light-Normal"),
    (3, "Normal"),
    (4, "Normal-Heavy"),
    (5, "Heavy"),
];
pub(crate) const SPIN: &[(u8, &str)] = &[
    (1, "No Spin"),
    (2, "Low"),
    (3, "Medium"),
    (4, "High"),
    (5, "Extra High"),
];
pub(crate) const TEMP: &[(u8, &str)] = &[
    (1, "Tap Cold"),
    (2, "Cold"),
    (4, "Warm"),
    (6, "Hot"),
    (7, "Extra Hot"),
];

fn named(table: &[(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, name)| *name)
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
            ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
            ("reserve_time", json!({"platform":"sensor","unique_id":"$deviceid-reserve_time","state_topic":"$this/reserve_time","name":"Delay Wash time remaining","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min"})),
            ("soil", json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:liquid-spot"})),
            ("spin", json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew"})),
            ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer"})),
            ("rinse_count", json!({"platform":"sensor","unique_id":"$deviceid-rinse_count","state_topic":"$this/rinse_count","name":"Rinses remaining","icon":"mdi:water-sync","state_class":"measurement"})),
            ("extra_rinse", json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_rinse","state_topic":"$this/extra_rinse","name":"Extra rinse","icon":"mdi:water-plus"})),
            ("extra_rinse_count", json!({"platform":"sensor","unique_id":"$deviceid-extra_rinse_count","state_topic":"$this/extra_rinse_count","name":"Extra rinse count","icon":"mdi:water-plus","state_class":"measurement"})),
            ("pre_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-pre_wash","state_topic":"$this/pre_wash","name":"Pre-wash","icon":"mdi:water-sync"})),
            ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"})),
            ("cold_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-cold_wash","state_topic":"$this/cold_wash","name":"Cold wash","icon":"mdi:snowflake"})),
            ("rinse_spin", json!({"platform":"binary_sensor","unique_id":"$deviceid-rinse_spin","state_topic":"$this/rinse_spin","name":"Rinse + Spin","icon":"mdi:water-sync"})),
            ("delay_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-delay_wash","state_topic":"$this/delay_wash","name":"Delay Wash","icon":"mdi:clock-plus-outline"})),
            ("door_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","icon":"mdi:lock","entity_category":"diagnostic"})),
            ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Control lock","icon":"mdi:lock-outline"})),
            ("load_level", json!({"platform":"sensor","unique_id":"$deviceid-load_level","state_topic":"$this/load_level","name":"Load level","icon":"mdi:weight","state_class":"measurement","entity_category":"diagnostic"})),
            ("tub_clean_count", json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since Tub Clean","icon":"mdi:counter","entity_category":"diagnostic"})),
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
        if rec.len() < LOAD_LEVEL_OFFSET + 1 || rec[0] != RECORD_MARKER {
            return;
        }
        let phase = rec[PHASE_OFFSET];
        let is_off = phase == PHASE_OFF;
        let idle = is_off || phase == PHASE_END;

        self.core.publish_on_off("power", !is_off);
        self.core
            .publish_property("status", status_name(phase).into());
        self.core.publish_property(
            "course",
            named(COURSE, rec[COURSE_OFFSET]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "remaining_time",
            if idle {
                0i64
            } else {
                rec[TIME_HOUR_OFFSET] as i64 * 60 + rec[TIME_MIN_OFFSET] as i64
            }
            .into(),
        );
        self.core.publish_property(
            "initial_time",
            if idle {
                0i64
            } else {
                rec[INITIAL_TIME_HOUR_OFFSET] as i64 * 60 + rec[INITIAL_TIME_MIN_OFFSET] as i64
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
            named(SOIL, rec[SOIL_OFFSET]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "spin",
            named(SPIN, rec[SPIN_OFFSET]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "temp",
            named(TEMP, rec[TEMP_OFFSET]).unwrap_or("None").into(),
        );
        self.core
            .publish_property("rinse_count", ((rec[RINSE_OFFSET] & 0x0f) as i64).into());
        self.core.publish_property(
            "extra_rinse_count",
            ((rec[RINSE_OFFSET] >> 4) as i64).into(),
        );

        let flags = rec[FLAGS_OFFSET];
        self.core.publish_on_off("child_lock", flags & FLAG_CHILD_LOCK != 0);
        self.core.publish_on_off("delay_wash", flags & FLAG_DELAY_ACTIVE != 0);
        self.core.publish_on_off("steam", flags & FLAG_STEAM != 0);
        self.core.publish_on_off("pre_wash", flags & FLAG_PRE_WASH != 0);
        self.core.publish_on_off("rinse_spin", flags & FLAG_RINSE_SPIN != 0);
        self.core.publish_on_off("extra_rinse", flags & FLAG_EXTRA_RINSE != 0);

        let opt2 = rec[OPT2_OFFSET];
        self.core.publish_on_off("cold_wash", opt2 & OPT2_COLD_WASH != 0);
        self.core.publish_on_off("door_lock", opt2 & OPT2_DOOR_LOCKED != 0);
        self.core
            .publish_property("load_level", (rec[LOAD_LEVEL_OFFSET] as i64).into());
        self.core
            .publish_property("tub_clean_count", (rec[TCL_COUNT_OFFSET] as i64).into());
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < 2 || buf[0] != 0x20 {
            return;
        }
        // Deliberately ignore 0xE2 (stale post-cycle replay).
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
    fn start(&self) {
        crate::devices::washer_ctrl::request_status(&self.core);
    }
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Metadata, Thinq2Device};

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata {
        Metadata::new("F3L7CYK5W_US_WIFI", "F3L7CYK5W_US_WIFI", "1.0")
    }
    fn make() -> (
        std::sync::Arc<MockHaConnection>,
        std::sync::Arc<MockThinq2Device>,
        std::sync::Arc<Device>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    /// A fresh device fed `frames` in order, like upstream's `feed()`.
    fn feed(frames: &[&str]) -> std::sync::Arc<MockHaConnection> {
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
    const DIAL_SPEED_WASH: &str =
        "aa3a20ec001805010101010a0003050403000000000000000000332e0000001805000f000f0b0001050601000000000000000000332e000713bb";
    const DIAL_DOWNLOADED: &str =
        "aa3a20ec001805000f000f0b0001050601000000000000000000332e0007001805002d002d0c0003050402000000000000000000332e0f00f9bb";
    const DIAL_TUB_CLEAN: &str =
        "aa3a20ec001805002d002d0c0003050402000000000000000000332e0f00001805011d011d010000030002000000040000000000332e0000e0bb";
    const DIAL_NORMAL: &str =
        "aa3a20ec00180502150215050005050402000000000000000000332e000000180501030103060003040402000000000000000000332e00001fbb";
    const COLD_WASH_ON: &str =
        "aa3a20ec00180501210121060003030702000000000000000000332e000000180501170117060003030202000000001000000000332e0000c0bb";
    const EXTRA_RINSE_2: &str =
        "aa3a20ec001805010e010e060003030412000000400000000000332e000000180501190119060003030422000000400000000000332e000047bb";
    const CHILD_LOCK_ON: &str =
        "aa3a20ec00180501030103060003040402000000000000000000332e000000180501030103060003040402000000010000000000332e000076bb";
    const STEAM_ON: &str =
        "aa3a20ec00180501030103060003040402000000000000000000332e0000001805021c021c060000040002000000040000000000332e000006bb";
    const PRE_WASH_ON: &str =
        "aa3a20ec00180501030103060003040402000000000000000000332e000000180501120112060003040402000000080000000000332e00001dbb";
    const DELAY_2H: &str =
        "aa3a20ec00180501030103060003040402000000000000000000332e000000180501030103060003040402000200020000000000332e000073bb";
    const RINSE_SPIN_ON: &str =
        "aa3a20ec00180501000100040003030402000000000000000100332e0007001805000c000c040000030001000000200000000100332e00071bbb";
    const RINSING: &str =
        "aa3a20ec001805000c000c040000030001000000200000000100332e000700181e000c000c040000030001000000208000000105332e00074fbb";
    const ADD_GARMENTS: &str =
        "aa3a20ec00181e000c000c040000030001000000208002000005332e0007001815000c000c04000003000100000020001200001e332e00070fbb";
    const PAUSED: &str =
        "aa3a20ec001815000c000c04000003000100000020001200001e332e0007001806000c000c040000030001000000200010000015332e0007b5bb";
    const RUN_COMPLETE: &str =
        "aa3a20ec0018280001000c040000030000000000208002000e1e332e000700183c00010000fe0000000000000000000000000e28332e000031bb";
    const EB_OFF: &str = "aa2020eb00180000010000fe0000000000000000000000000005332b00001abb";
    const SELECTING: &str =
        "aa3a20ec00180000010000fe0000000000000000000000000005332b000000180501030103060003040402000000008000000000332b0000d5bb";
    const SENSING: &str =
        "aa3a20ec00180501030103060003040402000000008000000000332b000000181401030103060003040402000000008000000005332b000065bb";
    const DOOR_CLOSES: &str =
        "aa3a20ec00181401030103060003040402000000008000000005332b000000181401030103060003040402000000008002000005332b000013bb";
    const WASHING_START: &str =
        "aa3a20ec00181401030103060003040402000000008002000005332b000000181701140114060003040402000000008002000014332b0004d5bb";
    const WASHING_MINUTE_LATER: &str =
        "aa3a20ec00181701140114060003040402000000008002000014332b000400181701130114060003040402000000008002000114332b0004edbb";
    const WASHING_TO_RINSING: &str =
        "aa3a20ec001817003a0114060003040402000000008002002414332b000400181e00390114060000040402000000008002002617332b000407bb";
    const SPINNING: &str =
        "aa3a20ec00181e00150114060000040401000000008002003f17332b00040018280014011406000004000000000000800200411e332b00041abb";
    const CYCLE_COMPLETE: &str =
        "aa3a20ec0018280001011406000004000000000000800200671e332b000400183c00010000fe0000000000000000000000006c28332c0000aabb";
    const OFF: &str =
        "aa3a20ec00183c00010000fe0000000000000000000000006c28332c000000180000010000fe0000000000000000000000006c3c332c000001bb";
    const SECOND_RUN_WASHING: &str =
        "aa3a20ec00181401030103060003040402000000008002000005332d0000001817003a003a060003040402000000008002000014332d000398bb";
    const E2_STALE_REPLAY: &str =
        "aa2020e203181401030103060003040402000000008000006c05332b000030bb";

    #[test]
    fn config_components() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert!(comps.contains_key("power"));
        assert_eq!(comps["status"]["device_class"], json!("enum"));
        let opts = comps["status"]["options"].as_array().unwrap();
        assert!(opts.iter().any(|v| v == "Standby"));
        assert!(opts.iter().any(|v| v == "End"));
        assert!(!opts.iter().any(|v| v == "Initial" || v == "Complete" || v == "Running"));
        assert!(comps.contains_key("course"));
        assert!(comps.contains_key("child_lock"));
        assert!(comps.contains_key("rinse_spin"));
        assert!(comps.contains_key("tub_clean_count"));
        assert!(!comps.contains_key("turbo_wash")); // model has no TurboWash
        // The tub clean counter resets at each Tub Clean, so it must not be total_increasing.
        assert!(comps["tub_clean_count"].get("state_class").is_none());
        assert!(comps["child_lock"].get("entity_category").is_none());
    }

    #[test]
    fn unknown_phase_publishes_no_guessed_label() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(
            "aa2020eb0018ff00010000fe0000000000000000000000000005332b00001abb",
        ));
        assert_eq!(prop(&ha, "status").as_deref(), Some("None"));
        assert_ne!(prop(&ha, "status").as_deref(), Some("Running"));
    }

    #[test]
    fn eb_single_record_frames_decode_with_the_ec_offsets() {
        let p = feed(&[EB_OFF]);
        expect(&p, "power", "OFF");
        expect(&p, "status", "Off");
        expect(&p, "course", "None");
        expect(&p, "remaining_time", "0");
        expect(&p, "tub_clean_count", "43");
    }

    #[test]
    fn course_table_is_this_models_own() {
        expect(&feed(&[DIAL_NORMAL]), "course", "Normal");
        expect(&feed(&[DIAL_SPEED_WASH]), "course", "Speed Wash");
        expect(&feed(&[DIAL_DOWNLOADED]), "course", "Downloaded");
        expect(&feed(&[DIAL_TUB_CLEAN]), "course", "Tub Clean");
    }

    #[test]
    fn selecting_remaining_and_initial_time_agree() {
        let p = feed(&[SELECTING]);
        expect(&p, "power", "ON");
        expect(&p, "status", "Standby");
        expect(&p, "course", "Normal");
        expect(&p, "soil", "Normal");
        expect(&p, "spin", "High");
        expect(&p, "temp", "Warm");
        expect(&p, "remaining_time", "63");
        expect(&p, "initial_time", "63");
    }

    #[test]
    fn sensing_is_a_distinct_phase() {
        let p = feed(&[SELECTING, SENSING]);
        expect(&p, "status", "Detecting");
        expect(&p, "remaining_time", "63");
    }

    #[test]
    fn rinse_byte_packs_rinse_and_extra_rinse_counts() {
        let plain = feed(&[DIAL_NORMAL]);
        expect(&plain, "rinse_count", "2");
        expect(&plain, "extra_rinse_count", "0");
        expect(&plain, "extra_rinse", "OFF");

        let extra = feed(&[EXTRA_RINSE_2]);
        expect(&extra, "extra_rinse", "ON");
        expect(&extra, "extra_rinse_count", "2");
        expect(&extra, "rinse_count", "2");
    }

    #[test]
    fn option_bits_each_isolate_cleanly() {
        let lock = feed(&[CHILD_LOCK_ON]);
        expect(&lock, "child_lock", "ON");
        expect(&lock, "steam", "OFF");
        expect(&lock, "pre_wash", "OFF");
        expect(&lock, "delay_wash", "OFF");

        expect(&feed(&[STEAM_ON]), "steam", "ON");
        expect(&feed(&[PRE_WASH_ON]), "pre_wash", "ON");

        let rs = feed(&[RINSE_SPIN_ON]);
        expect(&rs, "rinse_spin", "ON");
        expect(&rs, "child_lock", "OFF");
        expect(&rs, "extra_rinse", "OFF");
    }

    #[test]
    fn cold_wash_forces_the_temperature_index() {
        let p = feed(&[COLD_WASH_ON]);
        expect(&p, "cold_wash", "ON");
        expect(&p, "temp", "Cold");
        expect(&p, "child_lock", "OFF");
    }

    #[test]
    fn delay_wash_exposes_its_reserve_clock() {
        let p = feed(&[DELAY_2H]);
        expect(&p, "delay_wash", "ON");
        expect(&p, "reserve_time", "120");
    }

    #[test]
    fn door_position_is_never_published() {
        let open = feed(&[SENSING]);
        assert_eq!(prop(&open, "door"), None);
        expect(&open, "door_lock", "ON");

        let shut = feed(&[SENSING, DOOR_CLOSES]);
        assert_eq!(prop(&shut, "door"), None);
        expect(&shut, "door_lock", "ON");
    }

    #[test]
    fn initial_time_pins_while_remaining_counts_down() {
        let start = feed(&[SENSING, WASHING_START]);
        expect(&start, "status", "Washing");
        expect(&start, "remaining_time", "80");
        expect(&start, "initial_time", "80");

        let later = feed(&[SENSING, WASHING_START, WASHING_MINUTE_LATER]);
        expect(&later, "remaining_time", "79");
        expect(&later, "initial_time", "80");

        let second = feed(&[SECOND_RUN_WASHING]);
        expect(&second, "remaining_time", "58");
        expect(&second, "initial_time", "58");
    }

    #[test]
    fn add_garments_unlocks_the_door_and_pause_follows() {
        let running = feed(&[RINSE_SPIN_ON, RINSING]);
        expect(&running, "status", "Rinsing");
        expect(&running, "door_lock", "ON");
        expect(&running, "load_level", "7");

        let adding = feed(&[RINSE_SPIN_ON, RINSING, ADD_GARMENTS]);
        expect(&adding, "status", "Filling");
        expect(&adding, "door_lock", "OFF");

        let paused = feed(&[RINSE_SPIN_ON, RINSING, ADD_GARMENTS, PAUSED]);
        expect(&paused, "status", "Paused");
        expect(&paused, "door_lock", "OFF");
    }

    #[test]
    fn settings_drop_out_as_they_stop_applying() {
        let rinsing = feed(&[WASHING_START, WASHING_TO_RINSING]);
        expect(&rinsing, "status", "Rinsing");
        expect(&rinsing, "soil", "None");
        expect(&rinsing, "temp", "Warm");
        expect(&rinsing, "initial_time", "80");

        let spinning = feed(&[WASHING_START, WASHING_TO_RINSING, SPINNING]);
        expect(&spinning, "status", "Spinning");
        expect(&spinning, "temp", "None");
        expect(&spinning, "door_lock", "ON");
    }

    #[test]
    fn completing_a_cycle_unlocks_and_counts_the_wash() {
        let before = feed(&[WASHING_START, SPINNING]);
        expect(&before, "tub_clean_count", "43");

        let done = feed(&[WASHING_START, SPINNING, CYCLE_COMPLETE]);
        expect(&done, "status", "End");
        expect(&done, "power", "ON");
        expect(&done, "tub_clean_count", "44");
        expect(&done, "door_lock", "OFF");
        expect(&done, "remaining_time", "0");
        expect(&done, "initial_time", "0");
    }

    #[test]
    fn rinse_spin_run_lands_on_complete() {
        let p = feed(&[RINSING, RUN_COMPLETE]);
        expect(&p, "status", "End");
        expect(&p, "course", "None");
        expect(&p, "tub_clean_count", "46");
    }

    #[test]
    fn powering_off_clears_the_selection() {
        let p = feed(&[WASHING_START, SPINNING, CYCLE_COMPLETE, OFF]);
        expect(&p, "status", "Off");
        expect(&p, "power", "OFF");
        expect(&p, "course", "None");
        expect(&p, "remaining_time", "0");
    }

    #[test]
    fn e2_post_cycle_replay_is_ignored() {
        let p = feed(&[WASHING_START, SPINNING, CYCLE_COMPLETE, E2_STALE_REPLAY]);
        expect(&p, "status", "End");
        expect(&p, "tub_clean_count", "44");
        expect(&p, "remaining_time", "0");
    }

    #[test]
    fn start_requests_a_status_snapshot() {
        let (_, thinq, dev) = make();
        DeviceHandler::start(dev.as_ref());
        let sent: Vec<String> = thinq.outbox().iter().map(|b| hex_encode(b)).collect();
        assert_eq!(sent, ["AA0EF0ED1121010000001800B5BB"]);
    }

    #[test]
    fn non_status_frames_publish_nothing() {
        for junk in ["aa0720d800fcbb", "aa09207200c9005bbb", "aa0a20ec001805010cbb"] {
            let p = feed(&[junk]);
            assert!(p.device(DEVICE_ID).unwrap().properties.is_empty(), "{junk}");
        }
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("F3L7CYK5W_US_WIFI").is_some());
    }
}
