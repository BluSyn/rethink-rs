//! F3M2CYK__ — LG WM4500HBA front-load washer (AABB), read-only.
//!
//! Class `0x20` frames are told apart by `buf[1]` (`buf[3]` is a rolling sequence counter). This
//! washer reports through `0xCD`, a ~400-byte full status dump sent every 5 minutes while running,
//! and `0xBD`, the same dump on events, whose event byte at `buf[3]` (0x03 = cycle end) shifts the
//! rest by one. Their times are [hour][minute] pairs. `0x72` heartbeats drive power (0xC9 started
//! or resumed, 0xC8 stopped, a transient 0x00 just before the cycle end), `0xD8` carries the washes
//! since the last Tub Clean and `0x7F` a relative sensed load size. The `0xEC`/`0xEB` path of
//! F3L2CYU__ is kept, as the 25-byte record layout matches; `0x31` (serial) and anything else are
//! logged, undecoded. Offsets were checked against the LG cloud and photos of the control panel.
//! Port of upstream PR #208.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;

// The old record, then the current one at buf[29..]; 0xEB carries just one.
const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_FRAME_LEN: usize = 54;
const RECORD_B_OFFSET: usize = 29;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_STATUS_FRAME_LEN: usize = 28;
const SINGLE_RECORD_OFFSET: usize = 3;
const RECORD_MARKER: u8 = 0x18;

// Dump offsets are for 0xCD; 0xBD adds BD_SHIFT.
const CD_FRAME_TYPE: u8 = 0xcd;
const BD_FRAME_TYPE: u8 = 0xbd;
const BD_SHIFT: usize = 1;
const BD_EVENT_OFFSET: usize = 3;
const BD_EVENT_CYCLE_END: u8 = 0x03;
const DUMP_PHASE_OFFSET: usize = 8;
const DUMP_REMAINING_OFFSET: usize = 9;
const DUMP_TOTAL_OFFSET: usize = 11;
// Delay Wash time remaining, counting down while the phase is Delay Wash.
const DUMP_RESERVE_OFFSET: usize = 13;
// Constant for a cycle, but not the 0xEC/0xEB course numbering. 0x00 in the first 0xBD of a cycle,
// before the washer commits to one.
const DUMP_COURSE_OFFSET: usize = 15;
// Soil, temp and spin use the 0xEC/0xEB indices. 0x00 means "not applicable": soil clears when
// Rinsing starts and temp when Spinning starts.
const DUMP_SOIL_OFFSET: usize = 17;
const DUMP_TEMP_OFFSET: usize = 18;
const DUMP_EXTRA_RINSE_COUNT_OFFSET: usize = 19;
const DUMP_SPIN_OFFSET: usize = 21;
// The rec[15] option bits (FLAG_*), plus this model's own Control Lock bit.
const DUMP_FLAGS_OFFSET: usize = 24;
const DUMP_FLAG_CONTROL_LOCK: u8 = 0x01;
// The rec[16] bits (OPT2_COLD_WASH), plus Fresh Care.
const DUMP_OPT2_OFFSET: usize = 25;
const DUMP_OPT2_FRESH_CARE: u8 = 0x01;
// The count as of cycle start: the end-of-cycle 0xBD's copy is stale, the updated count comes in
// the 0xD8 burst at cycle end.
const DUMP_TUB_CLEAN_COUNT_OFFSET: usize = 29;
// ezDispense level settings (the panel's 1-3 bars): detergent in 0xC0, softener in 0x30, and the
// softener Refill warning.
const DUMP_DETERGENT_OFFSET: usize = 373;
const DUMP_DETERGENT_MASK: u8 = 0xc0;
const DUMP_SOFTENER_MASK: u8 = 0x30;
const DUMP_SOFTENER_REFILL: u8 = 0x04;

// 20 7F 01 00 <size>, a minute into a cycle once the load is sensed. Higher is heavier.
const LOAD_FRAME_TYPE: u8 = 0x7f;
const LOAD_SIZE_OFFSET: usize = 4;
// The power-on burst starts with a 0x00 placeholder, so a zero is only taken from the first 0xD8
// after the cycle-end 72 00 (a Tub Clean just finished).
const COUNTER_FRAME_TYPE: u8 = 0xd8;
const COUNTER_OFFSET: usize = 2;
const HEARTBEAT_FRAME_TYPE: u8 = 0x72;
const HEARTBEAT_STATE_OFFSET: usize = 3;
const HEARTBEAT_RUNNING: u8 = 0xc9;
const HEARTBEAT_STOPPED: u8 = 0xc8;
const HEARTBEAT_CYCLE_ENDING: u8 = 0x00;

// Offsets below are relative to the record's 0x18 marker (rec[0]).
const PHASE_OFFSET: usize = 1;
// The estimated cycle time while selecting, else the countdown.
const TIME_HOUR_OFFSET: usize = 2;
const TIME_MIN_OFFSET: usize = 3;
const COURSE_OFFSET: usize = 6;
const SOIL_OFFSET: usize = 8;
const SPIN_OFFSET: usize = 9;
const TEMP_OFFSET: usize = 10;
// High nibble: the extra-rinse count.
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
// A plain door-closed sensor, independent of the rec[16] lock bit.
const DOOR_OFFSET: usize = 17;
const DOOR_CLOSED: u8 = 0x02;

const PHASE_OFF: u8 = 0x00;
const PHASE_SELECTING: u8 = 0x05;
const PHASE_SENSING: u8 = 0x14;
const PHASE_COMPLETE: u8 = 0x3c;

// Phases not listed here publish as Running.
const STATUS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x05, "Selecting"),
    (0x06, "Paused"),
    (0x0a, "Delay Wash"),
    (0x14, "Sensing"),
    (0x17, "Washing"),
    (0x1e, "Rinsing"),
    (0x28, "Spinning"),
    (0x3c, "Complete"),
];

const DETERGENT_LEVELS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x40, "Less"),
    (0x80, "Normal"),
    (0xc0, "More"),
];

const SOFTENER_LEVELS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x10, "Less"),
    (0x20, "Normal"),
    (0x30, "More"),
];

// The 0xEC/0xEB rec[6] dial position.
const COURSES: &[(u8, &str)] = &[
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
];

// The 0xCD/0xBD course code. Unconfirmed codes are published as raw hex, to be identified from HA.
const DUMP_COURSES: &[(u8, &str)] = &[
    (0x02, "Sanitary"),
    (0x03, "Allergiene"),
    (0x04, "Bedding"),
    (0x05, "Perm. Press"),
    (0x08, "Bright Whites"),
    (0x0a, "Delicates"),
    (0x0c, "Speed Wash"),
    (0x06, "Normal"),
    (0x07, "Heavy Duty"),
    (0x0d, "Tub Clean"),
    (0x0e, "Towels"),
    (0x10, "Rinse+Spin"),
    (0x13, "Sportswear"),
];

const SOIL: &[(u8, &str)] = &[
    (1, "Light"),
    (2, "Light-Normal"),
    (3, "Normal"),
    (4, "Normal-Heavy"),
    (5, "Heavy"),
];

const SPIN: &[(u8, &str)] = &[
    (1, "No Spin"),
    (2, "Low"),
    (3, "Medium"),
    (4, "High"),
    (5, "Extra High"),
];

// 3 and 5 are unused on this model.
const TEMP: &[(u8, &str)] = &[
    (1, "Tap Cold"),
    (2, "Cold"),
    (4, "Warm"),
    (6, "Hot"),
    (7, "Extra Hot"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn named_or_hex(table: &'static [(u8, &'static str)], code: u8) -> String {
    named(table, code).map_or_else(|| format!("0x{code:02x}"), str::to_string)
}

fn status(phase: u8) -> &'static str {
    named(STATUS, phase).unwrap_or("Running")
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    // Set by the cycle-end 72 00: the next 0xD8 carries the updated washes-since-Tub-Clean count.
    count_follows_cycle_end: Mutex<bool>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            count_follows_cycle_end: Mutex::new(false),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                if let Err(reason) = dev.process_aabb(inner) {
                    tracing::debug!("F3M2CYK__ {reason}: {}", rethink_util::hex::encode(inner));
                }
            }
        }));
        this
    }

    /// `Err` names why the frame was not decoded, for the log.
    fn process_aabb(&self, buf: &[u8]) -> Result<(), &'static str> {
        if buf.len() < 2 || buf[0] != CLASS_BYTE {
            return Err("unrecognized frame");
        }
        match buf[1] {
            STATUS_FRAME_TYPE => self.process_status(buf, RECORD_B_OFFSET, STATUS_FRAME_LEN),
            SINGLE_STATUS_FRAME_TYPE => {
                self.process_status(buf, SINGLE_RECORD_OFFSET, SINGLE_STATUS_FRAME_LEN)
            }
            CD_FRAME_TYPE => self.process_dump(buf, 0),
            BD_FRAME_TYPE => self.process_dump(buf, BD_SHIFT),
            HEARTBEAT_FRAME_TYPE if buf.len() > HEARTBEAT_STATE_OFFSET => {
                self.process_heartbeat(buf[HEARTBEAT_STATE_OFFSET]);
                Ok(())
            }
            COUNTER_FRAME_TYPE if buf.len() > COUNTER_OFFSET => {
                self.process_counter(buf[COUNTER_OFFSET]);
                Ok(())
            }
            LOAD_FRAME_TYPE if buf.len() > LOAD_SIZE_OFFSET => {
                self.core
                    .publish_property("load_size", i64::from(buf[LOAD_SIZE_OFFSET]).into());
                Ok(())
            }
            // 0x31 (serial), 0xE2 (end-of-cycle summary) and anything else not decoded yet.
            _ => Err("unrecognized frame"),
        }
    }

    fn process_dump(&self, buf: &[u8], shift: usize) -> Result<(), &'static str> {
        if buf.len() <= DUMP_TUB_CLEAN_COUNT_OFFSET + shift {
            return Err("status dump too short");
        }
        let at = |offset: usize| buf[offset + shift];
        let hm = |offset: usize| i64::from(at(offset)) * 60 + i64::from(at(offset + 1));

        let phase = at(DUMP_PHASE_OFFSET);
        // The last 0xBD of a cycle still carries the final phase (e.g. Spinning, 1 min left), and
        // can arrive after the 0x72 stop heartbeat.
        let cycle_end = shift == BD_SHIFT && buf[BD_EVENT_OFFSET] == BD_EVENT_CYCLE_END;
        let total = hm(DUMP_TOTAL_OFFSET);

        if phase == PHASE_OFF {
            self.core.publish_on_off("power", false);
            self.core.publish_property("status", status(phase).into());
            self.core.publish_property("remaining_time", 0i64.into());
            self.core.publish_property("initial_time", 0i64.into());
        } else if cycle_end || phase == PHASE_COMPLETE {
            self.core.publish_on_off("power", false);
            self.core
                .publish_property("status", status(PHASE_COMPLETE).into());
            self.core.publish_property("remaining_time", 0i64.into());
            // The post-cycle 0xCD zeroes the total; the last known value is kept.
            if total > 0 {
                self.core.publish_property("initial_time", total.into());
            }
        } else {
            self.core.publish_on_off("power", true);
            self.core.publish_property("status", status(phase).into());
            self.core
                .publish_property("remaining_time", hm(DUMP_REMAINING_OFFSET).into());
            self.core.publish_property("initial_time", total.into());
        }

        let course = at(DUMP_COURSE_OFFSET);
        if course != 0 {
            self.core
                .publish_property("course", named_or_hex(DUMP_COURSES, course).into());
            // A zero setting before the cycle is under way means the course doesn't use it. Later
            // it only means that stage is over, so the last value is kept. TurboWash and Steam
            // clear the same way once their stage is over.
            let presetting = phase == PHASE_SELECTING || phase == PHASE_SENSING;
            let setting = |prop: &str, offset: usize, table: &'static [(u8, &'static str)]| {
                let value = at(offset);
                if value != 0 {
                    self.core
                        .publish_property(prop, named_or_hex(table, value).into());
                } else if presetting {
                    self.core.publish_property(prop, "-".into());
                }
            };
            setting("soil", DUMP_SOIL_OFFSET, SOIL);
            setting("temp", DUMP_TEMP_OFFSET, TEMP);
            setting("spin", DUMP_SPIN_OFFSET, SPIN);

            let option = |prop: &str, on: bool| {
                if on {
                    self.core.publish_on_off(prop, true);
                } else if presetting {
                    self.core.publish_on_off(prop, false);
                }
            };
            let flags = at(DUMP_FLAGS_OFFSET);
            option("turbo_wash", flags & FLAG_TURBO_WASH != 0);
            option("steam", flags & FLAG_STEAM != 0);
            option("extra_rinse", flags & FLAG_EXTRA_RINSE != 0);
            option("pre_wash", flags & FLAG_PRE_WASH != 0);
            let opt2 = at(DUMP_OPT2_OFFSET);
            option("fresh_care", opt2 & DUMP_OPT2_FRESH_CARE != 0);
            option("cold_wash", opt2 & OPT2_COLD_WASH != 0);
            let extra_rinses = at(DUMP_EXTRA_RINSE_COUNT_OFFSET);
            if extra_rinses != 0 || presetting {
                self.core
                    .publish_property("extra_rinse_count", i64::from(extra_rinses).into());
            }
            // Delay and Control Lock are live states, not settings: they follow their bits.
            self.core
                .publish_on_off("delay_wash", flags & FLAG_DELAY_ACTIVE != 0);
            self.core
                .publish_on_off("control_lock", flags & DUMP_FLAG_CONTROL_LOCK != 0);
            self.core
                .publish_property("reserve_time", hm(DUMP_RESERVE_OFFSET).into());

            if buf.len() > DUMP_DETERGENT_OFFSET + shift {
                let dispense = at(DUMP_DETERGENT_OFFSET);
                self.core.publish_property(
                    "detergent_level",
                    named_or_hex(DETERGENT_LEVELS, dispense & DUMP_DETERGENT_MASK).into(),
                );
                self.core.publish_property(
                    "softener_level",
                    named(SOFTENER_LEVELS, dispense & DUMP_SOFTENER_MASK)
                        .unwrap_or("None")
                        .into(),
                );
                self.core
                    .publish_on_off("softener_refill", dispense & DUMP_SOFTENER_REFILL != 0);
            }
        }
        if !cycle_end {
            self.core.publish_property(
                "tub_clean_count",
                i64::from(at(DUMP_TUB_CLEAN_COUNT_OFFSET)).into(),
            );
        }
        Ok(())
    }

    fn process_counter(&self, count: u8) {
        let follows_cycle_end = std::mem::take(&mut *self.count_follows_cycle_end.lock());
        if count != 0 || follows_cycle_end {
            self.core
                .publish_property("tub_clean_count", i64::from(count).into());
        }
    }

    fn process_heartbeat(&self, state: u8) {
        match state {
            HEARTBEAT_RUNNING => self.core.publish_on_off("power", true),
            HEARTBEAT_STOPPED => self.core.publish_on_off("power", false),
            // Doesn't change power, but marks the next 0xD8 as the updated count.
            HEARTBEAT_CYCLE_ENDING => *self.count_follows_cycle_end.lock() = true,
            _ => {}
        }
    }

    fn process_status(
        &self,
        buf: &[u8],
        record_offset: usize,
        expected_len: usize,
    ) -> Result<(), &'static str> {
        if buf.len() != expected_len {
            return Err("status frame length mismatch");
        }
        let rec = &buf[record_offset..];
        if rec[0] != RECORD_MARKER {
            return Err("status frame missing 0x18 marker");
        }
        let phase = rec[PHASE_OFFSET];
        let is_off = phase == PHASE_OFF;
        // Zeroed while off: a stray leftover minute was seen on a real power-off.
        let hm = |hour: usize, minute: usize| {
            if is_off {
                0
            } else {
                i64::from(rec[hour]) * 60 + i64::from(rec[minute])
            }
        };
        let label = |table: &'static [(u8, &'static str)], code: u8| {
            named(table, code).unwrap_or("None").into()
        };

        self.core.publish_on_off("power", !is_off);
        self.core.publish_property("status", status(phase).into());
        self.core
            .publish_property("course", label(COURSES, rec[COURSE_OFFSET]));
        self.core.publish_property(
            "remaining_time",
            hm(TIME_HOUR_OFFSET, TIME_MIN_OFFSET).into(),
        );
        self.core.publish_property(
            "reserve_time",
            hm(RESERVE_HOUR_OFFSET, RESERVE_MIN_OFFSET).into(),
        );
        self.core
            .publish_property("soil", label(SOIL, rec[SOIL_OFFSET]));
        self.core
            .publish_property("spin", label(SPIN, rec[SPIN_OFFSET]));
        self.core
            .publish_property("temp", label(TEMP, rec[TEMP_OFFSET]));
        self.core.publish_property(
            "extra_rinse_count",
            i64::from(rec[EXTRA_RINSE_COUNT_OFFSET] >> 4).into(),
        );

        let flags = rec[FLAGS_OFFSET];
        self.core
            .publish_on_off("extra_rinse", flags & FLAG_EXTRA_RINSE != 0);
        self.core
            .publish_on_off("pre_wash", flags & FLAG_PRE_WASH != 0);
        self.core.publish_on_off("steam", flags & FLAG_STEAM != 0);
        self.core
            .publish_on_off("turbo_wash", flags & FLAG_TURBO_WASH != 0);
        self.core
            .publish_on_off("delay_wash", flags & FLAG_DELAY_ACTIVE != 0);

        let opt2 = rec[OPT2_OFFSET];
        self.core
            .publish_on_off("cold_wash", opt2 & OPT2_COLD_WASH != 0);
        self.core
            .publish_on_off("door_lock", opt2 & OPT2_DOOR_LOCKED != 0);
        self.core
            .publish_on_off("door", rec[DOOR_OFFSET] != DOOR_CLOSED);
        Ok(())
    }
}

fn components() -> HashMap<String, Value> {
    [
        (
            "power",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "name": "Power",
                "icon": "mdi:washing-machine",
                "device_class": "running",
            }),
        ),
        (
            // Free text, not an enum: unmapped phases publish Running.
            "status",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-status",
                "state_topic": "$this/status",
                "name": "Status",
                "icon": "mdi:state-machine",
            }),
        ),
        (
            "course",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-course",
                "state_topic": "$this/course",
                "name": "Course",
                "icon": "mdi:pin-outline",
            }),
        ),
        (
            "remaining_time",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-remaining_time",
                "state_topic": "$this/remaining_time",
                "name": "Remaining time",
                "icon": "mdi:timer-outline",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        ),
        (
            "initial_time",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-initial_time",
                "state_topic": "$this/initial_time",
                "name": "Initial time",
                "icon": "mdi:timer-sand",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        ),
        (
            "tub_clean_count",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-tub_clean_count",
                "state_topic": "$this/tub_clean_count",
                "name": "Washes since Tub Clean",
                "icon": "mdi:counter",
                "state_class": "measurement",
            }),
        ),
        (
            "soil",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-soil",
                "state_topic": "$this/soil",
                "name": "Soil level",
                "icon": "mdi:liquid-spot",
            }),
        ),
        (
            "spin",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-spin",
                "state_topic": "$this/spin",
                "name": "Spin",
                "icon": "mdi:autorenew",
            }),
        ),
        (
            "temp",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-temp",
                "state_topic": "$this/temp",
                "name": "Temperature",
                "icon": "mdi:thermometer",
            }),
        ),
        (
            "turbo_wash",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-turbo_wash",
                "state_topic": "$this/turbo_wash",
                "name": "TurboWash",
                "icon": "mdi:rocket-launch",
            }),
        ),
        (
            "steam",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-steam",
                "state_topic": "$this/steam",
                "name": "Steam",
                "icon": "mdi:kettle-steam",
            }),
        ),
        (
            "extra_rinse",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-extra_rinse",
                "state_topic": "$this/extra_rinse",
                "name": "Extra rinse",
                "icon": "mdi:water-sync",
            }),
        ),
        (
            "pre_wash",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-pre_wash",
                "state_topic": "$this/pre_wash",
                "name": "Pre-wash",
                "icon": "mdi:water-sync",
            }),
        ),
        (
            "extra_rinse_count",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-extra_rinse_count",
                "state_topic": "$this/extra_rinse_count",
                "name": "Extra rinse count",
                "icon": "mdi:water-sync",
                "state_class": "measurement",
            }),
        ),
        (
            "fresh_care",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-fresh_care",
                "state_topic": "$this/fresh_care",
                "name": "Fresh Care",
                "icon": "mdi:tshirt-crew-outline",
            }),
        ),
        (
            "cold_wash",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-cold_wash",
                "state_topic": "$this/cold_wash",
                "name": "Cold wash",
                "icon": "mdi:snowflake",
            }),
        ),
        (
            "control_lock",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-control_lock",
                "state_topic": "$this/control_lock",
                "name": "Control Lock",
                "icon": "mdi:lock",
            }),
        ),
        (
            "delay_wash",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-delay_wash",
                "state_topic": "$this/delay_wash",
                "name": "Delay Wash",
                "icon": "mdi:clock-plus-outline",
            }),
        ),
        (
            "reserve_time",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-reserve_time",
                "state_topic": "$this/reserve_time",
                "name": "Delay Wash time remaining",
                "icon": "mdi:clock-outline",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        ),
        (
            "load_size",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-load_size",
                "state_topic": "$this/load_size",
                "name": "Sensed load size",
                "icon": "mdi:weight",
                "state_class": "measurement",
            }),
        ),
        (
            "softener_level",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-softener_level",
                "state_topic": "$this/softener_level",
                "name": "Softener level setting",
                "icon": "mdi:cup-water",
            }),
        ),
        (
            "softener_refill",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-softener_refill",
                "state_topic": "$this/softener_refill",
                "name": "Softener refill",
                "icon": "mdi:water-alert",
            }),
        ),
        (
            "detergent_level",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-detergent_level",
                "state_topic": "$this/detergent_level",
                "name": "Detergent level setting",
                "icon": "mdi:cup-water",
            }),
        ),
        // Door and door lock come only from 0xEC/0xEB, which this washer hasn't been seen sending,
        // so they are left out of discovery rather than sit at Unknown.
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
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
    use crate::test_support::{make_t2, DEVICE_ID};
    use rethink_core::{
        hex_decode, unwrap_aabb, MockHaConnection, MockThinq2Device, PropertyValue,
    };

    const MODEL_ID: &str = "F3M2CYK__";

    // Real frames from the WM4500HBA: a ~2-day capture with a Warm/High/TurboWash load (53 min
    // total) and a second load (18 min, no Washing phase), a Tub Clean (1:29) that showed the times
    // are [hour][minute] pairs, and loads checked against panel photos. It never sends 0xEC/0xEB.

    // 0xCD (idle keepalive) — first load, Normal/Warm/High spin/Normal soil/TurboWash, total 53 min
    const CD_WASHING: &str =
        "AA0020CD00019001020B170031003500000600030400010400058080226A000600000000000005041E2122262A1818181818FFFFFCF0F50000000000000000000000000000000000000000000000000000000000008383838383B60000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000500800600000000000000000000017C0243FFEF020100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000C22D0040003000000000000000000000000000000000000000000000000000001ABB";
    const CD_RINSING: &str =
        "AA0020CD00019001020B1E001D003500000600000400010400058080226A00060000000000005504585152504D1919191919EDEBFFFFFF0000000000000000000000000000000000000000000000000000000000008383838383B7C0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000005021E0600000000050000000000017C02D5FFEF020700000000EBF8010332020242000000093A0700C9123202029E000000064C0D00BC143104020500000010801E00D5260000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000C22D0040003000000000000000000000000000000000000000000000000000008EBB";
    const CD_SPINNING: &str =
        "AA0020CD00019001020B28000D003500000600000000000400058080226A0006000000000000870452524E4D4B1818181818ECFEFFF8FF0A8F0A8F0A8F0A8F0A8F10311031103110311031005D005D005D005D005D8383838383B85500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000500F206000500F2050000000000017C02C4E7E2010700000000E0F8010631040275000000116C1000CA1B32020276000000064011008014310402A600000021602100CC16330101C40000001B370900CF1D340201D60000001E732F003743340201AC0000005C4A910031150000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000258025800000000C22D0040003000000000000000000000000000000000000000000000000000000CBB";
    const CD_COMPLETE: &str =
        "AA0020CD00019001020B3C0001000000000000000000000000000000006A0007000000000000B204515353534D19191A1A1AFFFFFFFFFF0A8F0A8F0A8F0A8F0A8F10311031103110311031005D005D005D005D005D8383838383BA4000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000500F206000500F2050000000000017C02C4E7E2010700000000E0F8010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004B0045601000000C22D004000300000000000000000000000000000000000000000000000000000C9BB";

    // Second load (18 min, course not recorded): total=remaining=18, phase goes straight to Rinsing
    // — no Washing phase (matches the physical panel for that course).
    const CD_SECOND_LOAD_RINSING: &str =
        "AA0020CD00019001020B1E000E001200001000000200010400070080226A000700000000000002041B1E2122241718171718FCEBE6E6E60000000000000749073F00000000000002140219000000000000001100118383838383B98A000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000179023FFFE60200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000300000000000000000000000000000000000000000000000000000BDBB";

    // 0xBD (event, sent on phase/state changes)
    const BD_SELECTING: &str =
        "AA0020BD0001019001020B050001000000000000000000000000000000006A000600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017702580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000C20000000030000000000000000000000000000000000000000000000000000034BB";
    const BD_SENSING: &str =
        "AA0020BD0002019001020B14002E002E00000600030400010400008080226A00060000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000050080060000000000000000036C017C024A0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000C200000000300000000000000000000000000000000000000000000000000000A4BB";
    const BD_WASHING: &str =
        "AA0020BD0002019001020B170021003500000600030400010400058080226A00060000000000004C0058000000001900000000ED000000000000000000000000000000000000000000000000000000000000000000008300000000A99500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000500800600000000000000000000017C02D8FFEF020700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000C22D004000300000000000000000000000000000000000000000000000000000B4BB";
    const BD_RINSING: &str =
        "AA0020BD0002019001020B1E0012003500000600000400010400058080226A00060000000000007A0446494E54571818181818E1EEE7E3E500000000000009EA0A3400000000000010AE10090000000000000072006E8383838383A3C500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000500F206000500F2050000000000017C02ECE7E2010700000000EBF8010032020242000000093A0700C9123202029E000000064C0D00BC143104020500000010801E00D5263301007E00000027271A005B4A3202025E000000072D0E004F10320202640000000B270B0040083202024A000000061E0700380600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000258025800000000C22D004000300000000000000000000000000000000000000000000000000000EEBB";

    // The 476-byte-body variant seen while spinning (larger than the usual ~406) — must decode the
    // same way.
    const BD_SPINNING_LARGE: &str =
        "AA0020BD0003019001020B280001003500000600000000000400058080226A0006000000000000B203515353530019191A1A00FFFFFFFF000A8F0A8F0A8F0A8F000010311031103110310000005D005D005D005D00008383838300BA7500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000500F206000500F2050000000000017C02C4E7E2010700000000E0F8010631040275000000116C1000CA1B32020276000000064011008014310402A600000021602100CC16330101C40000001B370900CF1D340201D60000001E732F003743340201AC0000005C4A9100311500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004B0045601000000C22D0040003000000000000000000000000000000000000000000000000000004503010100040506073900000000FF015B0000000002181800021917000605FD6F6F0000FDFD000000FD000007AE073C000010000610210034740000401864CC494C9A001A00B1BB";

    // Second load (18 min), at the Selecting step just before start: total already committed to 18.
    const BD_SECOND_LOAD_SELECTING: &str =
        "AA0020BD0001019001020B050012001200001000000200010400070000206A000700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017702580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000020000000030000000000000000000000000000000000000000000000000000056BB";

    // A real 0xEB single-record frame for this same washer model family (F3L2CYU__, offsets
    // identical) — this washer never actually sends these, but the dispatcher must still route them
    // correctly.
    const EB_RECONNECT_IDLE: &str =
        "aa2020eb001805000000000000000000000000000000000000003318000068bb";

    // Frames too short to hold phase/remaining/total — must be ignored without throwing.
    const CD_TOO_SHORT: &str = "AA0620CD000102BB";
    const BD_TOO_SHORT: &str = "AA0620BD000102BB";

    // Third capture: Tub Clean (course 0x0d), 1:29 total, washes-since-Tub-Clean = 8
    const TC_BD_SELECTING: &str =
        "AA0020BD0001019001020B05011D011D00000D00000000020300000400006A0008000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000177025800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000300000000000000000000000000000000000000000000000000000A1BB";

    // 1:23 remaining of 1:29 — a uint16 read would give 279/285.
    const TC_CD_WASHING: &str =
        "AA0020CD00019001020B170117011D00000D00000000020300010480026A000800000000000037041B1D1D1F2017181B1E20FFF4F3F3F30000000000000000000000000000000000000000000000000000000000008383848484AE0A000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001004F0600000000000000000000017E024AFFF4000003010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000020000000030000000000000000000000000000000000000000000000000000010BB";

    // Crossed the hour boundary: 00 3a = 58 min remaining.
    const TC_CD_WASHING_UNDER_HOUR: &str =
        "AA0020CD00019001020B17003A011D00000D00000000020300010480026A0008000000000001EF042D2E2E2E303F3F404041F3F3F3F3F30000000000000000000000000000000000000000000000000000000000008787878787B06A000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001004F0600000000000000000000017E025DFFF400001C0100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000300000000000000000000000000000000000000000000000000000E9BB";

    // End-of-cycle 0xBD (event byte 0x03), still reporting Spinning with 0:01 left.
    const TC_BD_CYCLE_END: &str =
        "AA0020BD0003019001020B280001011D00000D00000000000300010080026A00080000000000025B04494A4C4D491C1C1C1D1DFFFFFFFFFF06D206D206D206D206D201A901A901A901A901A9000500050005000500058484848484B8A0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001005A0600000000010000000000017E026EFFE602011E010000E9F8010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003E803E80000000002000000003000000000000000000000000000000000000000000000000000004503010100040506073900000000FF014D0000000001171808022418000001FD64640000FDFD000000FD000007B0003C000010000D201100006A00024018D0BB484B1A001A0040BB";

    // 0x72 heartbeats around the Tub Clean: C9 at start, transient 00 then C8 at the end.
    const HB_START: &str = "AA09207200C9005BBB";
    const HB_TRANSIENT_ZERO: &str = "AA09207200000010BB";
    const HB_STOP: &str = "AA09207200C80058BB";

    // 0xD8 washes-since-Tub-Clean bursts: power-on placeholder 00, then the real count; after a
    // load the updated count; after the Tub Clean, 00.
    const COUNT_POWER_ON_PLACEHOLDER: &str = "AA0720D800FCBB";
    const COUNT_8: &str = "AA0720D808E4BB";
    const COUNT_7_AFTER_LOAD: &str = "AA0720D807E5BB";
    const COUNT_RESET_AFTER_TUB_CLEAN: &str = "AA0720D800FCBB";

    // Fourth capture: Towels (course 0x0e), 2026-09-23
    // Selecting, 1:01 estimate, washes-since-Tub-Clean = 0.
    const TOWELS_BD_SELECTING: &str =
        "AA0020BD0001019001020B050101010100000E00030400030500000000006A00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001770258000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000082000000003000000000000000000000000000000000000000000000000000006EBB";

    // Cycle end, in this order: 72 00, then the updated count (01), then the end-of-cycle 0xBD
    // still carrying the pre-cycle count (00) — the reverse of the Tub Clean's order.
    const TOWELS_HB_CYCLE_ENDING: &str = "AA09207200000010BB";
    const TOWELS_COUNT_1: &str = "AA0720D801FFBB";
    const TOWELS_BD_CYCLE_END: &str =
        "AA0020BD0003019001020B280001003A00000E00000000000500040080026A0000000000000001B10355555552001A1A1B1B00FFFFFFFF00084308430843084300000DF40DF40DF40DF40000009E009E009E009E00008383838300B8AA00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000401A20600000000040000000000017C029CF6DF03000B010000DFF80100310402BB0000000A3C15005D143202025C000000061D0500470D3202024E0000000B1A09003E133402014C0000001F731E001233310403B00000000C140400220B32020292000000041105001E09330101CE00000003280700800F00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000514045601000000822D002D003000000000000000000000000000000000000000000000000000004503010100040506073900000000FF015A0000000007181804031D17000704FD6F6E0000FDFD000000FD000007B1013C000010000E301100347400004018E4CC494D839A1A002DBB";

    // Next power-on, 40 minutes later: placeholder 00 burst.
    const TOWELS_NEXT_POWER_ON_PLACEHOLDER: &str = "AA0720D800FCBB";

    // Fifth capture: Heavy Duty (course 0x07), 2026-09-23 21:46Z, panel photo: Heavy Duty, Cold,
    // Medium spin, Light soil, 1:44
    const HEAVY_DUTY_BD_SELECTING: &str =
        "AA0020BD0001019001020B05012C012C00000700010200010300008000006A000200000000000000001E000000001A00000000FF000000000000000000000000000000000000000000000000000000000000000000008300000000BB0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017702580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000C200000000300000000000000000000000000000000000000000000000000000ECBB";

    // Sixth capture: Sanitary (course 0x02), 2026-09-23 23:11Z, panel photo: Sanitary, Extra Hot,
    // High spin, Normal soil, Steam and TurboWash off
    // Washing, 1:48 left of 1:50.
    const SANITARY_CD_WASHING: &str =
        "AA0020CD00019001020B170130013200000200030700030400010080026A000300000000000002043D3D3C3C3D1C1C1B1B1BFFF8F8F8F80000000000000000000000000000000000000000000000000000000000008484848384B6DA00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100550600000000000000000000017A026DFFF8040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000822D001200300000000000000000000000000000000000000000000000000000F3BB";

    // Seventh capture: Rinse+Spin (course 0x10), 2026-09-23 23:25Z, panel photo: Rinse+Spin, 0:18,
    // Cold, High spin, no soil, TurboWash off
    const RINSE_SPIN_BD_SELECTING: &str =
        "AA0020BD0001019001020B050012001200001000000200010400070000206A000300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000550000000000000000000000017A026EF1F2010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000020000000030000000000000000000000000000000000000000000000000000004BB";

    // Eighth capture: Allergiene (course 0x03), 2026-09-23 23:48Z, panel photo: Allergiene, Steam
    // and Delay Wash lit (display 1:00), TurboWash and Extra Rinse off, High spin, detergent 2 bars
    // Selected with a 1:00 delay: options 0x06 (Steam + Delay), delay time 01 00.
    const ALLERGIENE_BD_DELAY_SET: &str =
        "AA0020BD0007019001020B050209020901000300000000020400000600006A00040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001770258000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000082000000003000000000000000000000000000000000000000000000000000001FBB";

    // Four minutes later: phase 0x0a (Delay Wash), delay counting down (00 39 = 0:57).
    const ALLERGIENE_CD_DELAY_WAITING: &str =
        "AA0020CD00019001020B0A0127012700390300000000020400010680026A000400000000000000043E3C3A39381F1F1F1F1FFFFFFFFFFF0000000000000000000000000000000000000000000000000000000000008484848484B6B00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010054060000000000000000032E017B027D00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008200000000300000000000000000000000000000000000000000000000000000EEBB";

    // The delay ran out: still phase 0x0a for a moment, delay time 00 00, options 0x04 (Delay bit
    // cleared).
    const ALLERGIENE_BD_DELAY_OVER: &str =
        "AA0020BD0002019001020B0A0127012700000300000000020400010480026A000400000000000003002A000000001D00000000FF000000000000000000000000000000000000000000000000000000000000000000008400000000BB3A0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010054060000000000000000032E017B027D000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000082000000003000000000000000000000000000000000000000000000000000002EBB";

    // Ninth capture: Bright Whites (course 0x08), 2026-09-24 05:02Z, panel photo: Bright Whites,
    // Hot, Extra High, Heavy soil, TurboWash and Extra Rinse lit, detergent 1 bar
    const BRIGHT_WHITES_BD_SELECTING: &str =
        "AA0020BD0001019001020B05003B003B0000080005060102050000C000206A000500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017702580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000420000000030000000000000000000000000000000000000000000000000000005BB";

    // Tenth capture: Delicates (course 0x0a), 2026-09-24 05:09Z, panel photo: Delicates, Tap Cold,
    // Low spin, Light-Normal soil, Pre-wash lit, TurboWash off
    const DELICATES_BD_SELECTING: &str =
        "AA0020BD0001019001020B050035003500000A00020100020200070800006A00050000000000001C000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100510600000000000000000000017C0251FFF3040001010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000420000000030000000000000000000000000000000000000000000000000000086BB";

    // Eleventh capture: Perm. Press (course 0x05), 2026-09-24 05:17Z, panel photo: Perm. Press, Tap
    // Cold, Medium, Normal-Heavy soil, TurboWash lit, detergent 3 bars, softener 2 bars + Refill,
    // Signal on
    const PERM_PRESS_BD_SENSING: &str =
        "AA0020BD0002019001020B140033003300000500040100010300008080026A000500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000510000000000000000000000017C02650000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000E60000000030000000000000000000000000000000000000000000000000000039BB";

    // 2026-09-24 05:46Z: the Downloaded dial position (reports course 0x06, same as Normal), Cold,
    // Low, Normal soil, Fresh Care lamp lit
    const DOWNLOADED_BD_SELECTING: &str =
        "AA0020BD0001019001020B05010B010B00000600030200020200000001206A0605000000000000000228282800001B1B1B0000E9E9E900000000000000000000000000000000000000000000000000000000000000008483830000B870000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000177025800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008200000000300000000000000000000000000000000000000000000000000000F0BB";

    // 2026-09-24 05:55Z: Sportswear (course 0x13), Warm, Medium, Normal soil, Extra Rinse x2, Fresh
    // Care lamp still lit, detergent 1 bar
    const SPORTSWEAR_BD_SELECTING: &str =
        "AA0020BD0001019001020B05011D011D00001300030402040300074001006A000500000000000006000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017E0267E9E7010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000420000000030000000000000000000000000000000000000000000000000000026BB";

    // 2026-09-24 20:19Z: Towels (course 0x0e), Cold, Extra High, Normal soil, Cold Wash lamp lit
    const TOWELS_COLD_WASH_BD_SELECTING: &str =
        "AA0020BD0001019001020B050106010600000E00030200030500000010006A0008000000000000000123230000001A1A000000F9F90000000000000000000000000000000000000000000000000000000000000000008383000000B89A00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017702580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000820000000030000000000000000000000000000000000000000000000000000075BB";

    // 2026-09-24 17:29Z: Speed Wash (course 0x0c), 0:07, No Spin, Cold, Light soil
    const SPEED_WASH_BD_SELECTING: &str =
        "AA0020BD0001019001020B050007000700000C00010200010100070000006A000600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017702580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000420000000030000000000000000000000000000000000000000000000000000053BB";

    // 2026-09-24 17:52Z: Bedding (course 0x04), 1:00, Warm, Medium, Normal soil; Control Lock
    // engaged after start and released ~5 minutes later
    const BEDDING_BD_SELECTING: &str =
        "AA0020BD0001019001020B050100010000000400030400020300070000006A000700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017702580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000C2000000003000000000000000000000000000000000000000000000000000002FBB";

    // Report taken while Control Lock was on: options 0x01.
    const BEDDING_CD_LOCKED: &str =
        "AA0020CD00019001020B170037010000000400030400020300070180026A00070000000000003F0425273034371817181919EFE8E9E8E80000000009AD09A909C3000000000B3E0B720B600000000000990068005A8383838383A8CA00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017B0282EEE7010003010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000C22D003100300000000000000000000000000000000000000000000000000000FBBB";

    // Next report, after it was released: options 0x00.
    const BEDDING_CD_UNLOCKED: &str =
        "AA0020CD00019001020B170032010000000400030400020300070080026A0007000000000000AD043A3D4042441A1A1B1B1BE8E8E9E9E809C309C309AF09AA09AC0B600B600B910BC60BEC005A005A004F0048004683848484849F6000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017B029CEEE7010008010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000C22D003100300000000000000000000000000000000000000000000000000000B0BB";

    // 2026-09-24 23:59Z: Downloaded dial (course 0x06), Warm, Low, TurboWash, Fresh Care off;
    // detergent 2 bars, softener at max (3 bars), no Refill warning (panel photo)
    const DOWNLOADED_SOFTENER_MAX_BD_SELECTING: &str =
        "AA0020BD0001019001020B050036003600000600030400010200008000206A060900000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017702580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000B20000000030000000000000000000000000000000000000000000000000000020BB";

    // 2026-09-25 00:16Z: Perm. Press with the softener set to one bar, detergent 3 bars.
    const SOFTENER_ONE_BAR_BD_SELECTING: &str =
        "AA0020BD0001019001020B05002D002D00000500030400010300008000006A000900000000000000001E000000001A00000000FD000000000000000000000000000000000000000000000000000000000000000000008300000000BA1500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017702580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000D200000000300000000000000000000000000000000000000000000000000000B1BB";

    // 2026-09-25 00:31Z: Rinse+Spin paused one second after Start — phase 0x06 in the next 0xCD.
    const RINSE_SPIN_CD_PAUSED: &str =
        "AA0020CD00019001020B060012001200001000000200010400070000206A000900000000000000041F1F1F1F201A1A1A1A1AFDFDFDFDFD0000000000000000000000000000000000000000000000000000000000008383838383B9DA00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000017D0246FD0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000003000000000000000000000000000000000000000000000000000006EBB";

    // 0x7F sensed-load frames: 0x40 on the first (full) Normal load, 0x13 with a single washcloth.
    const LOAD_FULL: &str = "AA09207F010040C6BB";
    const LOAD_WASHCLOTH: &str = "AA09207F01001333BB";

    // Real 0x31 serial/identity frame, sent once per reconnect — not decoded.
    const SERIAL: &str =
        "AA372031020153414134303438343330320000F54F00008000000000000253414134303438343230320000B371000040000007C00054BB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2(MODEL_ID, Device::new)
    }

    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    fn feed(frame: &str) -> Arc<MockHaConnection> {
        let (ha, thinq, _dev) = make();
        emit(&thinq, frame);
        ha
    }

    fn get(ha: &MockHaConnection, name: &str) -> Option<PropertyValue> {
        ha.device(DEVICE_ID)?.properties.get(name).cloned()
    }

    fn text(value: &str) -> Option<PropertyValue> {
        Some(value.into())
    }

    fn int(value: i64) -> Option<PropertyValue> {
        Some(PropertyValue::Int(value))
    }

    fn soil_temp_spin(ha: &MockHaConnection) -> [Option<PropertyValue>; 3] {
        [get(ha, "soil"), get(ha, "temp"), get(ha, "spin")]
    }

    #[test]
    fn config_exposes_remaining_time_and_a_separate_initial_time() {
        let (ha, _, _) = make();
        let config = ha
            .device(DEVICE_ID)
            .and_then(|d| d.config)
            .expect("config published on construction");
        let components = &config.components;
        for c in [
            "remaining_time",
            "initial_time",
            "power",
            "status",
            "course",
            "tub_clean_count",
            "soil",
            "spin",
            "temp",
            "turbo_wash",
            "steam",
            "delay_wash",
            "reserve_time",
            "detergent_level",
            "extra_rinse",
            "pre_wash",
            "extra_rinse_count",
            "fresh_care",
            "cold_wash",
            "control_lock",
            "softener_level",
            "softener_refill",
            "load_size",
        ] {
            assert!(components.contains_key(c), "component {c} present");
        }
        // 0xEC/0xEB-only fields this model never sends must not be advertised.
        for c in ["door", "door_lock"] {
            assert!(!components.contains_key(c), "component {c} absent");
        }
        assert_eq!(components["initial_time"]["device_class"], "duration");
        assert_eq!(components["initial_time"]["unit_of_measurement"], "min");
        assert!(crate::t2_factory(MODEL_ID).is_some());
    }

    #[test]
    fn cd_frames_decode_phase_power_and_times_across_a_wash() {
        let (ha, thinq, _) = make();
        for (frame, status, power, remaining, initial) in [
            (CD_WASHING, "Washing", "ON", 49, 53),
            (CD_RINSING, "Rinsing", "ON", 29, 53),
            (CD_SPINNING, "Spinning", "ON", 13, 53),
            // The post-cycle frame zeroes the total; the last known total is kept.
            (CD_COMPLETE, "Complete", "OFF", 0, 53),
        ] {
            emit(&thinq, frame);
            assert_eq!(get(&ha, "status"), text(status));
            assert_eq!(get(&ha, "power"), text(power));
            assert_eq!(get(&ha, "remaining_time"), int(remaining));
            assert_eq!(get(&ha, "initial_time"), int(initial));
        }
        // Incremented from 6 at the end of the load.
        assert_eq!(get(&ha, "tub_clean_count"), int(7));
    }

    #[test]
    fn cd_second_load_goes_straight_to_rinsing() {
        let ha = feed(CD_SECOND_LOAD_RINSING);
        assert_eq!(get(&ha, "status"), text("Rinsing"));
        assert_eq!(get(&ha, "remaining_time"), int(14));
        assert_eq!(get(&ha, "initial_time"), int(18));
    }

    #[test]
    fn bd_frames_decode_phase_power_and_times_across_a_wash() {
        let (ha, thinq, _) = make();
        for (frame, status, power, remaining, initial) in [
            (BD_SELECTING, "Selecting", "ON", 1, 0),
            (BD_SENSING, "Sensing", "ON", 46, 46),
            (BD_WASHING, "Washing", "ON", 33, 53),
            (BD_RINSING, "Rinsing", "ON", 18, 53),
            // The 476-byte end-of-cycle 0xBD (event byte 0x03) still says Spinning, 0:01 left.
            (BD_SPINNING_LARGE, "Complete", "OFF", 0, 53),
        ] {
            emit(&thinq, frame);
            assert_eq!(get(&ha, "status"), text(status));
            assert_eq!(get(&ha, "power"), text(power));
            assert_eq!(get(&ha, "remaining_time"), int(remaining));
            assert_eq!(get(&ha, "initial_time"), int(initial));
        }
        assert_eq!(get(&ha, "course"), text("Normal"));
        assert_eq!(get(&ha, "tub_clean_count"), int(6));
    }

    #[test]
    fn bd_second_load_selecting_has_its_total_committed() {
        let ha = feed(BD_SECOND_LOAD_SELECTING);
        assert_eq!(get(&ha, "status"), text("Selecting"));
        assert_eq!(get(&ha, "remaining_time"), int(18));
        assert_eq!(get(&ha, "initial_time"), int(18));
        assert_eq!(get(&ha, "course"), text("Rinse+Spin"));
    }

    #[test]
    fn tub_clean_times_are_hour_minute_pairs() {
        let (ha, thinq, _) = make();

        emit(&thinq, TC_BD_SELECTING);
        assert_eq!(get(&ha, "status"), text("Selecting"));
        assert_eq!(get(&ha, "course"), text("Tub Clean"));
        assert_eq!(get(&ha, "initial_time"), int(89));
        assert_eq!(get(&ha, "tub_clean_count"), int(8));

        emit(&thinq, TC_CD_WASHING);
        assert_eq!(get(&ha, "status"), text("Washing"));
        assert_eq!(get(&ha, "remaining_time"), int(83));
        assert_eq!(get(&ha, "initial_time"), int(89));

        emit(&thinq, TC_CD_WASHING_UNDER_HOUR);
        assert_eq!(get(&ha, "remaining_time"), int(58));

        emit(&thinq, TC_BD_CYCLE_END);
        assert_eq!(get(&ha, "status"), text("Complete"));
        assert_eq!(get(&ha, "power"), text("OFF"));
        assert_eq!(get(&ha, "remaining_time"), int(0));
        assert_eq!(get(&ha, "initial_time"), int(89));
    }

    #[test]
    fn heartbeat_drives_power_and_ignores_the_transient_zero() {
        let (ha, thinq, _) = make();
        emit(&thinq, HB_START);
        assert_eq!(get(&ha, "power"), text("ON"));
        emit(&thinq, HB_TRANSIENT_ZERO);
        assert_eq!(get(&ha, "power"), text("ON"));
        emit(&thinq, HB_STOP);
        assert_eq!(get(&ha, "power"), text("OFF"));
    }

    #[test]
    fn eb_ec_dispatch_still_works() {
        let ha = feed(EB_RECONNECT_IDLE);
        assert_eq!(get(&ha, "status"), text("Selecting"));
        assert_eq!(get(&ha, "power"), text("ON"));
    }

    #[test]
    fn d8_power_on_zero_is_ignored_and_a_tub_clean_resets_the_count() {
        let (ha, thinq, _) = make();

        // Power-on: a placeholder 00 burst, then the real count.
        emit(&thinq, COUNT_POWER_ON_PLACEHOLDER);
        assert_eq!(get(&ha, "tub_clean_count"), None);
        emit(&thinq, COUNT_8);
        assert_eq!(get(&ha, "tub_clean_count"), int(8));
        emit(&thinq, COUNT_POWER_ON_PLACEHOLDER);
        assert_eq!(get(&ha, "tub_clean_count"), int(8));

        // The Tub Clean ends: 72 00, then the end-of-cycle 0xBD (stale pre-cycle 8, not
        // published), then the 0xD8 reset.
        emit(&thinq, HB_TRANSIENT_ZERO);
        emit(&thinq, TC_BD_CYCLE_END);
        assert_eq!(get(&ha, "tub_clean_count"), int(8));
        emit(&thinq, COUNT_RESET_AFTER_TUB_CLEAN);
        assert_eq!(get(&ha, "tub_clean_count"), int(0));
        // The rest of the burst, and later power-on placeholders, don't disturb it.
        emit(&thinq, COUNT_RESET_AFTER_TUB_CLEAN);
        assert_eq!(get(&ha, "tub_clean_count"), int(0));
    }

    #[test]
    fn d8_after_an_ordinary_load_carries_the_incremented_count() {
        let (ha, thinq, _) = make();
        // Load 1, count 6 at start.
        emit(&thinq, BD_WASHING);
        assert_eq!(get(&ha, "tub_clean_count"), int(6));
        // End-of-cycle 0xBD: stale count, not published.
        emit(&thinq, BD_SPINNING_LARGE);
        emit(&thinq, COUNT_7_AFTER_LOAD);
        assert_eq!(get(&ha, "tub_clean_count"), int(7));
    }

    #[test]
    fn towels_count_survives_the_d8_arriving_before_the_cycle_end_bd() {
        let (ha, thinq, _) = make();

        emit(&thinq, TOWELS_BD_SELECTING);
        assert_eq!(get(&ha, "course"), text("Towels"));
        assert_eq!(get(&ha, "initial_time"), int(61));
        assert_eq!(get(&ha, "tub_clean_count"), int(0));

        emit(&thinq, TOWELS_HB_CYCLE_ENDING);
        emit(&thinq, TOWELS_COUNT_1);
        assert_eq!(get(&ha, "tub_clean_count"), int(1));
        emit(&thinq, TOWELS_BD_CYCLE_END);
        assert_eq!(get(&ha, "status"), text("Complete"));
        // Not overwritten with the stale 0.
        assert_eq!(get(&ha, "tub_clean_count"), int(1));
        emit(&thinq, TOWELS_NEXT_POWER_ON_PLACEHOLDER);
        assert_eq!(get(&ha, "tub_clean_count"), int(1));
    }

    #[test]
    fn heavy_duty_course_and_estimate_decode_from_the_selecting_bd() {
        let ha = feed(HEAVY_DUTY_BD_SELECTING);
        assert_eq!(get(&ha, "course"), text("Heavy Duty"));
        assert_eq!(get(&ha, "status"), text("Selecting"));
        assert_eq!(get(&ha, "initial_time"), int(104));
        assert_eq!(get(&ha, "remaining_time"), int(104));
        assert_eq!(get(&ha, "tub_clean_count"), int(2));
        assert_eq!(get(&ha, "soil"), text("Light"));
        assert_eq!(get(&ha, "temp"), text("Cold"));
        assert_eq!(get(&ha, "spin"), text("Medium"));
    }

    #[test]
    fn sanitary_course_and_extra_hot_match_the_panel() {
        let ha = feed(SANITARY_CD_WASHING);
        assert_eq!(get(&ha, "course"), text("Sanitary"));
        assert_eq!(get(&ha, "status"), text("Washing"));
        assert_eq!(get(&ha, "remaining_time"), int(108));
        assert_eq!(get(&ha, "initial_time"), int(110));
        assert_eq!(get(&ha, "temp"), text("Extra Hot"));
        assert_eq!(get(&ha, "spin"), text("High"));
        assert_eq!(get(&ha, "soil"), text("Normal"));
        assert_eq!(get(&ha, "tub_clean_count"), int(3));
    }

    #[test]
    fn rinse_spin_course_time_temp_and_spin_match_the_panel() {
        let ha = feed(RINSE_SPIN_BD_SELECTING);
        assert_eq!(get(&ha, "course"), text("Rinse+Spin"));
        assert_eq!(get(&ha, "status"), text("Selecting"));
        assert_eq!(get(&ha, "initial_time"), int(18));
        assert_eq!(get(&ha, "temp"), text("Cold"));
        assert_eq!(get(&ha, "spin"), text("High"));
        // Rinse+Spin has no soil setting.
        assert_eq!(get(&ha, "soil"), text("-"));
    }

    #[test]
    fn steam_stays_on_after_the_tub_clean_clears_the_bit() {
        let (ha, thinq, _) = make();
        emit(&thinq, TC_BD_SELECTING);
        assert_eq!(get(&ha, "steam"), text("ON"));
        emit(&thinq, TC_CD_WASHING_UNDER_HOUR);
        assert_eq!(get(&ha, "steam"), text("ON"));
    }

    #[test]
    fn allergiene_delay_wash_options_countdown_and_phase() {
        let (ha, thinq, _) = make();

        emit(&thinq, ALLERGIENE_BD_DELAY_SET);
        assert_eq!(get(&ha, "course"), text("Allergiene"));
        assert_eq!(get(&ha, "steam"), text("ON"));
        assert_eq!(get(&ha, "delay_wash"), text("ON"));
        assert_eq!(get(&ha, "turbo_wash"), text("OFF"));
        assert_eq!(get(&ha, "reserve_time"), int(60));
        assert_eq!(get(&ha, "detergent_level"), text("Normal"));

        emit(&thinq, ALLERGIENE_CD_DELAY_WAITING);
        assert_eq!(get(&ha, "status"), text("Delay Wash"));
        assert_eq!(get(&ha, "reserve_time"), int(57));
        assert_eq!(get(&ha, "delay_wash"), text("ON"));

        // Delay is a live state: it clears when the delay runs out, while Steam is held.
        emit(&thinq, ALLERGIENE_BD_DELAY_OVER);
        assert_eq!(get(&ha, "reserve_time"), int(0));
        assert_eq!(get(&ha, "delay_wash"), text("OFF"));
        assert_eq!(get(&ha, "steam"), text("ON"));
    }

    #[test]
    fn turbo_wash_steam_and_detergent_level_match_the_panel_photos() {
        // Frame, TurboWash, Steam, detergent level (bars in the photo).
        for (frame, turbo, steam, detergent) in [
            // Normal, 09-21: TurboWash lit, 3 bars.
            (CD_WASHING, "ON", "OFF", "More"),
            // Heavy Duty: TurboWash lit, 3 bars.
            (HEAVY_DUTY_BD_SELECTING, "ON", "OFF", "More"),
            // Sanitary: both off, 2 bars.
            (SANITARY_CD_WASHING, "OFF", "OFF", "Normal"),
            // Rinse+Spin: both off, no bars.
            (RINSE_SPIN_BD_SELECTING, "OFF", "OFF", "Off"),
            // Tub Clean: Steam lit, doesn't dispense.
            (TC_CD_WASHING, "OFF", "ON", "Off"),
        ] {
            let (ha, thinq, _) = make();
            // Selecting, all options off: sets the baseline.
            emit(&thinq, RINSE_SPIN_BD_SELECTING);
            emit(&thinq, frame);
            assert_eq!(get(&ha, "turbo_wash"), text(turbo));
            assert_eq!(get(&ha, "steam"), text(steam));
            assert_eq!(get(&ha, "delay_wash"), text("OFF"));
            assert_eq!(get(&ha, "reserve_time"), int(0));
            assert_eq!(get(&ha, "detergent_level"), text(detergent));
        }
    }

    #[test]
    fn bright_whites_settings_extra_rinse_and_one_detergent_bar() {
        let ha = feed(BRIGHT_WHITES_BD_SELECTING);
        assert_eq!(get(&ha, "course"), text("Bright Whites"));
        assert_eq!(get(&ha, "initial_time"), int(59));
        assert_eq!(get(&ha, "temp"), text("Hot"));
        assert_eq!(get(&ha, "spin"), text("Extra High"));
        assert_eq!(get(&ha, "soil"), text("Heavy"));
        assert_eq!(get(&ha, "turbo_wash"), text("ON"));
        assert_eq!(get(&ha, "extra_rinse"), text("ON"));
        assert_eq!(get(&ha, "steam"), text("OFF"));
        assert_eq!(get(&ha, "detergent_level"), text("Less"));
    }

    #[test]
    fn delicates_settings_and_pre_wash() {
        let ha = feed(DELICATES_BD_SELECTING);
        assert_eq!(get(&ha, "course"), text("Delicates"));
        assert_eq!(get(&ha, "initial_time"), int(53));
        assert_eq!(get(&ha, "temp"), text("Tap Cold"));
        assert_eq!(get(&ha, "spin"), text("Low"));
        assert_eq!(get(&ha, "soil"), text("Light-Normal"));
        assert_eq!(get(&ha, "pre_wash"), text("ON"));
        assert_eq!(get(&ha, "turbo_wash"), text("OFF"));
        assert_eq!(get(&ha, "extra_rinse"), text("OFF"));
    }

    #[test]
    fn perm_press_settings_and_detergent_more() {
        let ha = feed(PERM_PRESS_BD_SENSING);
        assert_eq!(get(&ha, "course"), text("Perm. Press"));
        assert_eq!(get(&ha, "temp"), text("Tap Cold"));
        assert_eq!(get(&ha, "spin"), text("Medium"));
        assert_eq!(get(&ha, "soil"), text("Normal-Heavy"));
        assert_eq!(get(&ha, "turbo_wash"), text("ON"));
        // The detergent byte also carries the softener bits here; they don't disturb the level.
        assert_eq!(get(&ha, "detergent_level"), text("More"));
    }

    #[test]
    fn sportswear_extra_rinse_twice_and_fresh_care() {
        let ha = feed(SPORTSWEAR_BD_SELECTING);
        assert_eq!(get(&ha, "course"), text("Sportswear"));
        assert_eq!(get(&ha, "temp"), text("Warm"));
        assert_eq!(get(&ha, "spin"), text("Medium"));
        assert_eq!(get(&ha, "soil"), text("Normal"));
        assert_eq!(get(&ha, "extra_rinse"), text("ON"));
        assert_eq!(get(&ha, "extra_rinse_count"), int(2));
        assert_eq!(get(&ha, "fresh_care"), text("ON"));
        assert_eq!(get(&ha, "detergent_level"), text("Less"));
    }

    #[test]
    fn fresh_care_and_extra_rinse_count_on_the_other_photographed_loads() {
        for (frame, fresh_care, extra_rinses) in [
            // Fresh Care lamp lit.
            (DOWNLOADED_BD_SELECTING, "ON", 0),
            // One extra rinse.
            (BRIGHT_WHITES_BD_SELECTING, "OFF", 1),
            (DELICATES_BD_SELECTING, "OFF", 0),
        ] {
            let ha = feed(frame);
            assert_eq!(get(&ha, "fresh_care"), text(fresh_care));
            assert_eq!(get(&ha, "extra_rinse_count"), int(extra_rinses));
        }
    }

    #[test]
    fn towels_with_cold_wash() {
        let ha = feed(TOWELS_COLD_WASH_BD_SELECTING);
        assert_eq!(get(&ha, "course"), text("Towels"));
        assert_eq!(get(&ha, "temp"), text("Cold"));
        assert_eq!(get(&ha, "spin"), text("Extra High"));
        assert_eq!(get(&ha, "soil"), text("Normal"));
        assert_eq!(get(&ha, "cold_wash"), text("ON"));
        assert_eq!(get(&ha, "fresh_care"), text("OFF"));
    }

    #[test]
    fn cold_wash_reads_off_on_cold_loads_without_it() {
        // Cold temp, Cold Wash lamp off in the panel photo.
        let ha = feed(SPEED_WASH_BD_SELECTING);
        assert_eq!(get(&ha, "temp"), text("Cold"));
        assert_eq!(get(&ha, "cold_wash"), text("OFF"));
    }

    #[test]
    fn speed_wash_no_spin_cold_light_soil() {
        let ha = feed(SPEED_WASH_BD_SELECTING);
        assert_eq!(get(&ha, "course"), text("Speed Wash"));
        assert_eq!(get(&ha, "initial_time"), int(7));
        assert_eq!(get(&ha, "spin"), text("No Spin"));
        assert_eq!(get(&ha, "temp"), text("Cold"));
        assert_eq!(get(&ha, "soil"), text("Light"));
    }

    #[test]
    fn bedding_course_and_control_lock_follows_its_bit() {
        let (ha, thinq, _) = make();
        emit(&thinq, BEDDING_BD_SELECTING);
        assert_eq!(get(&ha, "course"), text("Bedding"));
        assert_eq!(get(&ha, "initial_time"), int(60));
        assert_eq!(get(&ha, "control_lock"), text("OFF"));

        emit(&thinq, BEDDING_CD_LOCKED);
        assert_eq!(get(&ha, "control_lock"), text("ON"));
        assert_eq!(get(&ha, "status"), text("Washing"));

        emit(&thinq, BEDDING_CD_UNLOCKED);
        assert_eq!(get(&ha, "control_lock"), text("OFF"));
    }

    #[test]
    fn a_paused_cycle_reports_paused_and_stays_powered() {
        let ha = feed(RINSE_SPIN_CD_PAUSED);
        assert_eq!(get(&ha, "status"), text("Paused"));
        assert_eq!(get(&ha, "power"), text("ON"));
        assert_eq!(get(&ha, "course"), text("Rinse+Spin"));
        assert_eq!(get(&ha, "remaining_time"), int(18));
    }

    #[test]
    fn softener_level_and_refill_warning_match_the_panel_photos() {
        for (frame, detergent, softener, refill) in [
            // 2 + 3 bars, tank full.
            (
                DOWNLOADED_SOFTENER_MAX_BD_SELECTING,
                "Normal",
                "More",
                "OFF",
            ),
            // 3 + 2 bars, red Refill lit.
            (PERM_PRESS_BD_SENSING, "More", "Normal", "ON"),
            // Softener bars dark.
            (DELICATES_BD_SELECTING, "Less", "Off", "OFF"),
            // Softener set to one bar.
            (SOFTENER_ONE_BAR_BD_SELECTING, "More", "Less", "OFF"),
        ] {
            let ha = feed(frame);
            assert_eq!(get(&ha, "detergent_level"), text(detergent));
            assert_eq!(get(&ha, "softener_level"), text(softener));
            assert_eq!(get(&ha, "softener_refill"), text(refill));
        }
    }

    #[test]
    fn soil_temp_spin_hold_once_their_stage_has_cleared_them() {
        let (ha, thinq, _) = make();
        let load_1 = [text("Normal"), text("Warm"), text("High")];
        // No course committed yet: all zero, nothing published.
        emit(&thinq, BD_SELECTING);
        assert_eq!(get(&ha, "soil"), None);
        emit(&thinq, BD_SENSING);
        assert_eq!(soil_temp_spin(&ha), load_1);
        // The soil byte is cleared once rinsing starts.
        emit(&thinq, CD_RINSING);
        assert_eq!(soil_temp_spin(&ha), load_1);
        // The temp byte is cleared once spinning starts.
        emit(&thinq, CD_SPINNING);
        assert_eq!(soil_temp_spin(&ha), load_1);
    }

    #[test]
    fn settings_a_course_does_not_use_publish_as_a_dash_at_selection() {
        let (ha, thinq, _) = make();
        emit(&thinq, BD_SENSING);
        // Tub Clean: no soil or temp.
        emit(&thinq, TC_BD_SELECTING);
        assert_eq!(soil_temp_spin(&ha), [text("-"), text("-"), text("Medium")]);
        // A Rinse+Spin-like 18-min load: no soil.
        emit(&thinq, BD_SECOND_LOAD_SELECTING);
        assert_eq!(soil_temp_spin(&ha), [text("-"), text("Cold"), text("High")]);
        emit(&thinq, TOWELS_BD_SELECTING);
        assert_eq!(
            soil_temp_spin(&ha),
            [text("Normal"), text("Warm"), text("Extra High")]
        );
    }

    #[test]
    fn dumps_too_short_for_phase_and_times_are_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, CD_TOO_SHORT);
        emit(&thinq, BD_TOO_SHORT);
        assert_eq!(get(&ha, "status"), None);
    }

    #[test]
    fn frames_with_another_class_byte_are_ignored() {
        let (ha, thinq, _) = make();
        let mut wrong_class = hex_decode(CD_WASHING);
        wrong_class[2] = 0x30;
        thinq.emit_data(&wrong_class);
        assert_eq!(get(&ha, "status"), None);
    }

    #[test]
    fn load_frames_publish_the_sensed_load_size() {
        let (ha, thinq, _) = make();
        emit(&thinq, LOAD_FULL);
        assert_eq!(get(&ha, "load_size"), int(0x40));
        emit(&thinq, LOAD_WASHCLOTH);
        assert_eq!(get(&ha, "load_size"), int(0x13));
    }

    // Upstream captures the log; here the reason process_aabb hands to it is checked instead.
    #[test]
    fn undecoded_frame_types_are_logged_but_d8_and_7f_are_not() {
        let (_, _, dev) = make();
        let decode = |frame: &str| dev.process_aabb(unwrap_aabb(&hex_decode(frame)).unwrap());
        assert_eq!(decode(COUNT_POWER_ON_PLACEHOLDER), Ok(()));
        assert_eq!(decode(LOAD_FULL), Ok(()));
        assert_eq!(decode(SERIAL), Err("unrecognized frame"));
    }
}
