//! RV13B6BSD_D_US_WIFI — LG DLEX3900B electric dryer (AABB, read-only).
//!
//! Class `0x30`, 29-byte status blocks: a leading byte, then the record from its `0x1b` marker.
//! `0xEB` carries one block, `0xEC` the previous block then the current one. The `0x31` serial,
//! `0xE2` idle snapshot and `0x72` heartbeat frames are not decoded.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x30;
const STATUS_LENGTH: usize = 29;
const RECORD_MARKER: u8 = 0x1b;

const PHASE_OFFSET: usize = 1;
const TIME_HOUR_OFFSET: usize = 2;
const TIME_MIN_OFFSET: usize = 3;
// Unlike the washer, a separate estimate that stays fixed while the countdown runs.
const INITIAL_TIME_HOUR_OFFSET: usize = 4;
const INITIAL_TIME_MIN_OFFSET: usize = 5;
const COURSE_OFFSET: usize = 6;
const DRY_LEVEL_OFFSET: usize = 8;
const TEMP_OFFSET: usize = 9;
const FLAGS_OFFSET: usize = 15;
const FLAG_CHILD_LOCK: u8 = 0x01;
const FLAG_REDUCE_STATIC: u8 = 0x02;
const FLAG_DAMP_DRY_SIGNAL: u8 = 0x08;
const OPT2_OFFSET: usize = 16;
const OPT2_ENERGY_SAVER: u8 = 0x02;
const OPT2_TURBO_STEAM: u8 = 0x04;
const OPT2_WRINKLE_CARE: u8 = 0x10;
const PHASE_OFF: u8 = 0x00;

const STATUS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Initial"),
    (0x03, "Pause"),
    (0x32, "Drying"),
    (0x33, "Cooling"),
    (0x04, "End"),
];
const COURSE: &[(u8, &str)] = &[
    (0x01, "Heavy Duty"),
    (0x02, "Towels"),
    (0x03, "Normal"),
    (0x04, "Perm Press"),
    (0x05, "Delicates"),
    (0x07, "Bedding"),
    (0x08, "Antibacterial"),
    (0x09, "Small Load"),
    (0x0b, "Sportswear"),
    (0x10, "Speed Dry"),
    (0x11, "Air Dry"),
    (0x12, "Time Dry"),
    (0x15, "Steam Fresh"),
    (0x16, "Steam Sanitary"),
    (0x1a, "Super Dry"),
];
// 0 (no dry level / no temperature) is left unmapped, so those courses publish "None".
const DRY_LEVEL: &[(u8, &str)] = &[
    (1, "Damp"),
    (2, "Less"),
    (3, "Normal"),
    (4, "More"),
    (5, "Very"),
];
const TEMP: &[(u8, &str)] = &[
    (1, "Ultra Low"),
    (2, "Low"),
    (3, "Medium"),
    (4, "Mid High"),
    (5, "High"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn components() -> HashMap<String, Value> {
    [
        ("power", json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:tumble-dryer","device_class":"running"})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine"})),
        ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time estimate","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
        ("dry_level", json!({"platform":"sensor","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","name":"Dry level","icon":"mdi:water-percent"})),
        ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer"})),
        ("reduce_static", json!({"platform":"binary_sensor","unique_id":"$deviceid-reduce_static","state_topic":"$this/reduce_static","name":"Reduce static","icon":"mdi:flash-off-outline"})),
        ("damp_dry_signal", json!({"platform":"binary_sensor","unique_id":"$deviceid-damp_dry_signal","state_topic":"$this/damp_dry_signal","name":"Damp Dry Signal","icon":"mdi:water-alert-outline"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:lock","entity_category":"diagnostic"})),
        ("energy_saver", json!({"platform":"binary_sensor","unique_id":"$deviceid-energy_saver","state_topic":"$this/energy_saver","name":"Energy Saver","icon":"mdi:leaf"})),
        ("turbo_steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbo_steam","state_topic":"$this/turbo_steam","name":"Turbo Steam","icon":"mdi:kettle-steam"})),
        ("wrinkle_care", json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Wrinkle Care","icon":"mdi:tshirt-crew-outline"})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

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
        let mut config = default_config(&meta, Some(json!({"name": "LG Dryer"})));
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
            self.process_status(&block[1..]);
        }
    }

    fn process_status(&self, rec: &[u8]) {
        if rec[0] != RECORD_MARKER {
            return;
        }
        let phase = rec[PHASE_OFFSET];
        let is_off = phase == PHASE_OFF;
        let minutes = |hour: usize, min: usize| {
            if is_off {
                0
            } else {
                i64::from(rec[hour]) * 60 + i64::from(rec[min])
            }
        };

        self.core.publish_on_off("power", !is_off);
        self.core
            .publish_property("status", named(STATUS, phase).unwrap_or("Running").into());
        self.core.publish_property(
            "course",
            named(COURSE, rec[COURSE_OFFSET]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "remaining_time",
            PropertyValue::Int(minutes(TIME_HOUR_OFFSET, TIME_MIN_OFFSET)),
        );
        self.core.publish_property(
            "initial_time",
            PropertyValue::Int(minutes(INITIAL_TIME_HOUR_OFFSET, INITIAL_TIME_MIN_OFFSET)),
        );
        self.core.publish_property(
            "dry_level",
            named(DRY_LEVEL, rec[DRY_LEVEL_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "temp",
            named(TEMP, rec[TEMP_OFFSET]).unwrap_or("None").into(),
        );

        let flags = rec[FLAGS_OFFSET];
        self.core
            .publish_on_off("child_lock", flags & FLAG_CHILD_LOCK != 0);
        self.core
            .publish_on_off("reduce_static", flags & FLAG_REDUCE_STATIC != 0);
        self.core
            .publish_on_off("damp_dry_signal", flags & FLAG_DAMP_DRY_SIGNAL != 0);

        let opt2 = rec[OPT2_OFFSET];
        self.core
            .publish_on_off("energy_saver", opt2 & OPT2_ENERGY_SAVER != 0);
        self.core
            .publish_on_off("turbo_steam", opt2 & OPT2_TURBO_STEAM != 0);
        self.core
            .publish_on_off("wrinkle_care", opt2 & OPT2_WRINKLE_CARE != 0);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{emit_inner, make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};

    const EB_IDLE: &str = "aa2330eb001b0000010001000000000004000000402800065f04000000640000006bbb";
    const OFF_TO_INITIAL: &str =
        "aa4030ec001b0000010001000000000004000000402800065f0400000064000000001b0100010001000000000004000000402800000000000000640000001fbb";
    const COURSE_NORMAL: &str =
        "aa4030ec001b010001000100000000000400000000280000000000000064000000001b010029002903000304000400000000280000000000000064000000e9bb";
    const COURSE_BEDDING: &str =
        "aa4030ec001b010039003903000304000400000000aa0000000000000064000000001b010037003707000303000400000000a8000000000000006400000002bb";
    const COURSE_SMALL_LOAD: &str =
        "aa4030ec001b010037003707000303000400000000a80000000000000064000000001b01001e001e09000305000400000000a8000000000000006400000073bb";
    const SMALL_LOAD_STARTS_DRYING: &str =
        "aa4030ec001b01001e001e09000305000400000000a80000000000000064000000001b32001e001e09000305000400000000a800000001000000640000007fbb";
    const PAUSE_THEN_ANTIBACTERIAL_SELECTED: &str =
        "aa4030ec001b03001e001e09000305000400000040a80000063200000064000000001b01010a010a08000505000400000040a80000060300000064000000c3bb";
    const HEAVY_DUTY_THEN_NORMAL_ENERGY_SAVER_ON: &str =
        "aa4030ec001b010036003601000305000400000040a80000060300000064000000001b010039003903000304000400000040aa0000060300000064000000b6bb";
    const STARTS_DRYING: &str =
        "aa4030ec001b010039003903000304000400000040aa0000060300000064000000001b320039003903000304000400000000aa000000010000006400000080bb";
    const REMAINING_TICKS_DOWN: &str =
        "aa4030ec001b320039003903000304000400000000aa0000000100000064000000001b320038003903000304000400000000aa000004010000006400000094bb";
    const DRYING_TO_COOLING: &str =
        "aa4030ec001b320038003903000304000400000000aa0000040100000064000000001b330001000203000304000400000000aa0000063200000064000000dfbb";
    const COOLING_TO_OFF_BLIP: &str =
        "aa4030ec001b330001000303000304000400000000aa0000083200000064000000001b000001000303000304000400000000aa00000a330000006400000073bb";
    const OFF_BLIP_TO_END: &str =
        "aa4030ec001b000001000303000304000400000000aa00000a3300000064000000001b040001000303000304000400000040aa00000a000000006400000052bb";
    const END_TO_FINAL_OFF: &str =
        "aa4030ec001b040001000303000304000400000040aa00000a0000000064000000001b000001000100000000000400000040a800000a04000000640000005fbb";
    const BEDDING_THEN_HEAVY_DUTY_TURBO_STEAM_ON: &str =
        "aa4030ec001b010037003707000303000400000000a80000000000000064000000001b010036003601000305000400000000ac000000000000006400000007bb";
    const HEAVY_DUTY_DRYING_TURBO_STEAM_ON: &str =
        "aa4030ec001b010036003601000305000400000000ac0000000000000064000000001b320036003601000305000400000000ac0000000100000064000000d7bb";
    const DAMP_DRY_SIGNAL_ON: &str =
        "aa4030ec001b010036003601000305000400000040280000430300000064000000001b01003600360100030500040000004828000043030000006400000009bb";
    const DAMP_DRY_SIGNAL_OFF_NORMAL_SELECTED: &str =
        "aa4030ec001b030036003601000305000400000048280000113200000064000000001b01002900290300030400040000004028000011030000006400000045bb";
    const CHILD_LOCK_ON: &str =
        "aa4030ec001b03010a010a080005050004000000402800000e3200000064000000001b03010a010a080005050004000000412800000e3200000064000000e6bb";
    const APP_START_COMMAND: &str = "aa1bf0260300000400000000410003000000000300d700000055bb";
    const REMOTE_STARTS_DRYING: &str =
        "aa4030ec001b010029002903000304000400000040290000240000000064000000001b3200010001030003040004d700000029000000010000006400000060bb";
    const APP_POWEROFF_COMMAND: &str = "aa09f0240101009cbb";
    const REMOTE_POWEROFF: &str =
        "aa4030ec001b3201070108030003040004d70000002900006c0100000064000000001b0001070107000000000004000000402800009932000000640000005cbb";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("RV13B6BSD_D_US_WIFI", Device::new)
    }

    fn emit(thinq: &MockThinq2Device, hex: &str) {
        thinq.emit_data(&hex_decode(hex));
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    #[test]
    fn eb_single_record_decodes_with_the_ec_offsets() {
        let (ha, thinq, _) = make();
        emit(&thinq, EB_IDLE);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "course", "None");
    }

    #[test]
    fn off_to_initial_as_dial_browsing_begins() {
        let (ha, thinq, _) = make();
        emit(&thinq, OFF_TO_INITIAL);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Initial");
    }

    #[test]
    fn course_byte_identifies_each_program_and_start_begins_drying() {
        let (ha, thinq, _) = make();
        emit(&thinq, COURSE_NORMAL);
        expect(&ha, "course", "Normal");
        expect(&ha, "dry_level", "Normal");
        expect(&ha, "temp", "Mid High");

        emit(&thinq, COURSE_BEDDING);
        expect(&ha, "course", "Bedding");
        expect(&ha, "temp", "Medium");

        emit(&thinq, COURSE_SMALL_LOAD);
        expect(&ha, "course", "Small Load");
        expect(&ha, "temp", "High");

        emit(&thinq, SMALL_LOAD_STARTS_DRYING);
        expect(&ha, "status", "Drying");
        expect(&ha, "course", "Small Load");
    }

    #[test]
    fn pause_then_antibacterial_shows_very_dry_and_high_temp() {
        let (ha, thinq, _) = make();
        emit(&thinq, PAUSE_THEN_ANTIBACTERIAL_SELECTED);
        expect(&ha, "status", "Initial");
        expect(&ha, "course", "Antibacterial");
        expect(&ha, "dry_level", "Very");
        expect(&ha, "temp", "High");
    }

    #[test]
    fn energy_saver_and_turbo_steam_toggle_independently_of_course() {
        let (ha, thinq, _) = make();
        emit(&thinq, HEAVY_DUTY_THEN_NORMAL_ENERGY_SAVER_ON);
        expect(&ha, "course", "Normal");
        expect(&ha, "energy_saver", "ON");

        emit(&thinq, BEDDING_THEN_HEAVY_DUTY_TURBO_STEAM_ON);
        expect(&ha, "course", "Heavy Duty");
        expect(&ha, "turbo_steam", "ON");

        emit(&thinq, HEAVY_DUTY_DRYING_TURBO_STEAM_ON);
        expect(&ha, "status", "Drying");
        expect(&ha, "turbo_steam", "ON");
    }

    #[test]
    fn damp_dry_signal_toggles_on_and_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, DAMP_DRY_SIGNAL_ON);
        expect(&ha, "damp_dry_signal", "ON");

        emit(&thinq, DAMP_DRY_SIGNAL_OFF_NORMAL_SELECTED);
        expect(&ha, "course", "Normal");
        expect(&ha, "damp_dry_signal", "OFF");
    }

    #[test]
    fn child_lock_engages() {
        let (ha, thinq, _) = make();
        emit(&thinq, CHILD_LOCK_ON);
        expect(&ha, "child_lock", "ON");
    }

    #[test]
    fn full_run_keeps_a_separate_initial_estimate_through_end_and_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, STARTS_DRYING);
        expect(&ha, "status", "Drying");
        expect(&ha, "remaining_time", "57");
        expect(&ha, "initial_time", "57");

        emit(&thinq, REMAINING_TICKS_DOWN);
        expect(&ha, "remaining_time", "56");
        expect(&ha, "initial_time", "57");

        emit(&thinq, DRYING_TO_COOLING);
        expect(&ha, "status", "Cooling");
        expect(&ha, "remaining_time", "1");

        emit(&thinq, COOLING_TO_OFF_BLIP);
        expect(&ha, "status", "Off");

        emit(&thinq, OFF_BLIP_TO_END);
        expect(&ha, "status", "End");

        emit(&thinq, END_TO_FINAL_OFF);
        expect(&ha, "status", "Off");
        expect(&ha, "power", "OFF");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "initial_time", "0");
        expect(&ha, "course", "None");
    }

    #[test]
    fn remote_start_round_trip_flips_initial_to_drying() {
        let (ha, thinq, _) = make();
        emit(&thinq, APP_START_COMMAND);
        emit(&thinq, REMOTE_STARTS_DRYING);
        expect(&ha, "status", "Drying");
    }

    #[test]
    fn remote_power_off_round_trip_drops_straight_to_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, APP_POWEROFF_COMMAND);
        emit(&thinq, REMOTE_POWEROFF);
        expect(&ha, "status", "Off");
        expect(&ha, "power", "OFF");
        expect(&ha, "course", "None");
    }

    #[test]
    fn unknown_frame_types_and_non_envelope_frames_are_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, COURSE_NORMAL);
        let before = prop(&ha, "status");
        emit(&thinq, "aa09307200c9004bbb");
        emit(&thinq, "001122");
        assert_eq!(prop(&ha, "status"), before);
    }

    #[test]
    fn synthetic_record_decodes_every_option_bit() {
        let (ha, thinq, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert!(comps.contains_key("turbo_steam"));
        assert!(comps.contains_key("wrinkle_care"));

        let mut rec = [0u8; 28];
        rec[0] = RECORD_MARKER;
        rec[PHASE_OFFSET] = 0x32;
        rec[TIME_MIN_OFFSET] = 40;
        rec[INITIAL_TIME_HOUR_OFFSET] = 1;
        rec[COURSE_OFFSET] = 0x01;
        rec[DRY_LEVEL_OFFSET] = 3;
        rec[TEMP_OFFSET] = 5;
        rec[FLAGS_OFFSET] = FLAG_CHILD_LOCK | FLAG_REDUCE_STATIC | FLAG_DAMP_DRY_SIGNAL;
        rec[OPT2_OFFSET] = OPT2_ENERGY_SAVER | OPT2_TURBO_STEAM | OPT2_WRINKLE_CARE;
        emit_inner(&thinq, &[&[CLASS_BYTE, 0xeb, 0x00][..], &rec].concat());
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Drying");
        expect(&ha, "course", "Heavy Duty");
        expect(&ha, "remaining_time", "40");
        expect(&ha, "initial_time", "60");
        expect(&ha, "dry_level", "Normal");
        expect(&ha, "temp", "High");
        for name in [
            "child_lock",
            "reduce_static",
            "damp_dry_signal",
            "energy_saver",
            "turbo_steam",
            "wrinkle_care",
        ] {
            expect(&ha, name, "ON");
        }
    }
}
