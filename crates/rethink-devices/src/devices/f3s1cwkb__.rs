//! F3S1CWKB__ — washer half of the split-module LG WKEX200HBA WashTower (AABB).
//!
//! The split module registers its halves as two ThinQ2 devices: this washer (deviceType 221) and
//! the `RV13S1AMJW_US` dryer (deviceType 222). A different dialect from both the combined
//! `WTL_FXU_BDV_NA_01` module and the F3L* washers. Unit `0x33`: `0xEC` (previous + current) and
//! `0xEB` (current only) carry 16-bit length-prefixed 43-byte records, `0x19` is the power frame
//! and `0x31` the module boot frame. The run phase only arrives in the `0xBD`/`0xCD` status dumps;
//! the record's own phase byte stays 0 all cycle. `0xE2` post-cycle replays are not decoded.
//!
//! Power is inferred from a selected course or the session flag (byte 37 bit 0x40, which clears a
//! few minutes after the last panel interaction). The retained record the module replays after a
//! remote power-off, after a boot frame, or as the first `0xEB` of a fresh connect reads as off.
//! A course-cleared record after a real run latches Complete. Door position and Cold Wash have no
//! known source byte and are not published. Power is a switch (`F0 E5` command); `auto_ack` is off.
//!
//! Port of upstream PR #268.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const WASHER_UNIT: u8 = 0x33;
const STATUS_FRAME_TYPE: u8 = 0xec;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const POWER_FRAME_TYPE: u8 = 0x19;
const POWER_FRAME_LEN: usize = 3;
const BOOT_FRAME_TYPE: u8 = 0x31;
const BD_DUMP_FRAME_TYPE: u8 = 0xbd;
const CD_DUMP_FRAME_TYPE: u8 = 0xcd;

// Dump header: 01 02 0B <phase> 00 <remaining> 00 <total>, the marker at offset 5 (0xCD) or 6
// (0xBD), so the first bytes are scanned.
const PHASE_MARKER: [u8; 3] = [0x01, 0x02, 0x0b];
const PHASE_SCAN_START: usize = 4;
const PHASE_SCAN_END: usize = 14;

const SOIL_OFFSET: usize = 1;
const TEMP_OFFSET: usize = 2;
const SPIN_OFFSET: usize = 4;
const COURSE_OFFSET: usize = 5;
const RESERVE_OFFSET: usize = 11;
const REMAINING_OFFSET: usize = 14;
// Post-sensing estimate, frozen once the cycle starts: only a fallback for the live countdown.
const ESTIMATE_OFFSET: usize = 16;
const RINSE_COUNT_OFFSET: usize = 27;
const PRE_WASH_OFFSET: usize = 34;
const PRE_WASH_FLAG: u8 = 0x40;
const STEAM_OFFSET: usize = 35;
const STEAM_FLAG: u8 = 0x10;
const SESSION_OFFSET: usize = 37;
const SESSION_FLAG: u8 = 0x40;
// Door lock / Remote Start armed.
const LOCK_FLAG: u8 = 0x10;
const RINSE_OFFSET: usize = 39;
const RINSE_FLAG: u8 = 0x40;

const PHASE_OFF: u8 = 0x00;
const RUN_PHASES: [u8; 4] = [0x14, 0x17, 0x1e, 0x28];

const POWER_ON: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x02, 0x01];
const POWER_OFF: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x02, 0x00];

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

const COURSES: &[(u8, &str)] = &[
    (0x2e, "Normal"),
    (0x0d, "Bedding"),
    (0x16, "Delicates"),
    (0x23, "Heavy Duty"),
    (0x4a, "Speed Wash"),
    (0xff, "Downloaded"),
    (0x3c, "Sanitary"),
    (0x55, "Tub Clean"),
];

// This model's own codes, unrelated to the F3L* tables.
const TEMPS: &[(u8, &str)] = &[
    (13, "Extra Cold"),
    (14, "Cold"),
    (16, "Warm"),
    (18, "Hot"),
    (19, "Extra Hot"),
];

const SPINS: &[(u8, &str)] = &[
    (12, "No Spin"),
    (13, "Low"),
    (14, "Medium"),
    (15, "High"),
    (16, "Extra High"),
];

const SOILS: &[(u8, &str)] = &[
    (1, "Light"),
    (2, "Light-Normal"),
    (3, "Normal"),
    (4, "Normal-Heavy"),
    (5, "Heavy"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

/// The 16-bit big-endian length-prefixed records of an `0xEB`/`0xEC` frame.
fn records(buf: &[u8]) -> Vec<&[u8]> {
    let mut records = Vec::new();
    let mut offset = 2;
    while offset + 2 <= buf.len() {
        let len = usize::from(u16::from_be_bytes([buf[offset], buf[offset + 1]]));
        offset += 2;
        let Some(record) = buf.get(offset..offset + len) else {
            break;
        };
        records.push(record);
        offset += len;
    }
    records
}

#[derive(Default)]
struct State {
    /// Set by a remote power-off, after which the module reboots and replays its pre-off record.
    remote_off: bool,
    /// Set by the boot frame: the next record is a retained pre-boot snapshot.
    boot_pending: bool,
    seen_record: bool,
    /// A real run was seen: a course selected with the lock bit set, or a running phase.
    cycle_ran: bool,
    completed: bool,
    course_selected: bool,
    /// Per-course rinse counter baselines, learned from records with extra rinse off.
    rinse_base: HashMap<u8, u8>,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    state: Mutex<State>,
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
            state: Mutex::new(State::default()),
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
        if buf.len() < 2 || buf[0] != WASHER_UNIT {
            return;
        }
        match buf[1] {
            POWER_FRAME_TYPE if buf.len() == POWER_FRAME_LEN => self.process_power(buf[2] != 0x00),
            BOOT_FRAME_TYPE => self.state.lock().boot_pending = true,
            STATUS_FRAME_TYPE | SINGLE_STATUS_FRAME_TYPE => self.process_status(buf),
            BD_DUMP_FRAME_TYPE | CD_DUMP_FRAME_TYPE => self.process_phase_frame(buf),
            _ => {}
        }
    }

    fn process_power(&self, on: bool) {
        if on {
            let mut state = self.state.lock();
            state.remote_off = false;
            state.completed = false;
            state.cycle_ran = false;
        }
        self.core.publish_on_off("power", on);
    }

    fn process_phase_frame(&self, buf: &[u8]) {
        let end = buf.len().min(PHASE_SCAN_END);
        let Some(marker) = (PHASE_SCAN_START..end.saturating_sub(3))
            .find(|&i| buf[i..].starts_with(&PHASE_MARKER))
        else {
            return;
        };
        let phase = buf[marker + 3];
        let label = match named(STATUS, phase) {
            Some(label) if phase != PHASE_OFF => label,
            _ => return,
        };
        let publish = {
            let mut state = self.state.lock();
            if state.course_selected && RUN_PHASES.contains(&phase) {
                state.cycle_ran = true;
            }
            // The trailing post-completion dump still carries the last phase.
            state.course_selected || state.cycle_ran
        };
        if publish {
            self.core.publish_property("status", label.into());
        }
    }

    fn process_status(&self, buf: &[u8]) {
        // 0xEC carries [previous, current]; 0xEB only the current record.
        let Some(&active) = records(buf).last() else {
            return;
        };
        if active.len() <= SESSION_OFFSET {
            return;
        }
        let course = active[COURSE_OFFSET];
        let session = active[SESSION_OFFSET];
        let rinse = active
            .get(RINSE_OFFSET)
            .is_some_and(|options| options & RINSE_FLAG != 0);
        let rinse_count = active[RINSE_COUNT_OFFSET];

        let mut state = self.state.lock();
        state.course_selected = course != 0;
        if course != 0 && session & LOCK_FLAG != 0 {
            state.cycle_ran = true;
        }
        let mut power_on = session & SESSION_FLAG != 0 || course != 0;
        if state.remote_off || state.boot_pending {
            power_on = false;
        }
        state.boot_pending = false;
        if !state.seen_record {
            state.seen_record = true;
            // A fresh connect's first snapshot with no course may be the retained replay.
            if buf[1] == SINGLE_STATUS_FRAME_TYPE && course == 0 {
                power_on = false;
            }
        }

        if !power_on {
            state.cycle_ran = false;
            state.completed = false;
            drop(state);
            self.publish_off();
            return;
        }

        if course != 0 {
            state.completed = false;
        }
        let completed = course == 0 && (state.cycle_ran || state.completed);
        if completed && state.cycle_ran {
            state.cycle_ran = false;
            state.completed = true;
        }
        if !rinse {
            state.rinse_base.insert(course, rinse_count);
        }
        let extra_rinse_count = match state.rinse_base.get(&course) {
            Some(&base) if rinse => (i64::from(rinse_count) - i64::from(base)).max(0),
            _ => 0,
        };
        drop(state);

        self.core.publish_on_off("power", true);
        let status = if completed {
            "Complete"
        } else if active[0] != PHASE_OFF {
            named(STATUS, active[0]).unwrap_or("Running")
        } else {
            "Selecting"
        };
        self.core.publish_property("status", status.into());
        let course_label = match course {
            0 => "None".to_string(),
            code => named(COURSES, code).map_or_else(|| format!("0x{code:x}"), str::to_string),
        };
        self.core.publish_property("course", course_label.into());
        // With no course selected the remaining byte parks on an idle marker (1), not a cycle.
        let remaining = if completed || course == 0 {
            0
        } else {
            [REMAINING_OFFSET, ESTIMATE_OFFSET, REMAINING_OFFSET + 1]
                .into_iter()
                .map(|offset| active[offset])
                .find(|&minutes| minutes != 0)
                .unwrap_or(0)
        };
        self.core
            .publish_property("remaining_time", PropertyValue::Int(remaining.into()));
        self.publish_named("temp", TEMPS, active[TEMP_OFFSET]);
        self.publish_named("spin", SPINS, active[SPIN_OFFSET]);
        self.publish_named("soil", SOILS, active[SOIL_OFFSET]);
        let reserve = u16::from_be_bytes([active[RESERVE_OFFSET], active[RESERVE_OFFSET + 1]]);
        self.core
            .publish_property("reserve_time", PropertyValue::Int(reserve.into()));
        self.core.publish_on_off("delay_wash", reserve > 0);
        self.core
            .publish_flag("steam", active[STEAM_OFFSET], STEAM_FLAG);
        self.core
            .publish_flag("pre_wash", active[PRE_WASH_OFFSET], PRE_WASH_FLAG);
        self.core.publish_flag("door_lock", session, LOCK_FLAG);
        self.core.publish_on_off("extra_rinse", rinse);
        self.core
            .publish_property("extra_rinse_count", PropertyValue::Int(extra_rinse_count));
    }

    fn publish_off(&self) {
        self.core.publish_on_off("power", false);
        self.core.publish_property("status", "Off".into());
        self.core.publish_property("course", "None".into());
        self.core
            .publish_property("remaining_time", PropertyValue::Int(0));
        self.core.publish_property("temp", "None".into());
        self.core.publish_property("spin", "None".into());
        self.core.publish_property("soil", "None".into());
        self.core
            .publish_property("reserve_time", PropertyValue::Int(0));
        self.core
            .publish_property("extra_rinse_count", PropertyValue::Int(0));
        for prop in [
            "extra_rinse",
            "pre_wash",
            "steam",
            "delay_wash",
            "door_lock",
        ] {
            self.core.publish_on_off(prop, false);
        }
    }

    fn publish_named(&self, prop: &str, table: &'static [(u8, &'static str)], code: u8) {
        self.core
            .publish_property(prop, named(table, code).unwrap_or("None").into());
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        if prop == "power" {
            let on = mqtt_value == "ON";
            self.state.lock().remote_off = !on;
            self.core.send(if on { POWER_ON } else { POWER_OFF });
        }
    }
}

fn components() -> HashMap<String, Value> {
    [
        (
            "power".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "command_topic": "$this/power/set",
                "name": "Power",
                "icon": "mdi:washing-machine",
            }),
        ),
        (
            "status".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-status",
                "state_topic": "$this/status",
                "name": "Status",
                "icon": "mdi:state-machine",
            }),
        ),
        (
            "course".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-course",
                "state_topic": "$this/course",
                "name": "Course",
                "icon": "mdi:pin-outline",
            }),
        ),
        (
            "remaining_time".into(),
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
            "reserve_time".into(),
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
            "soil".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-soil",
                "state_topic": "$this/soil",
                "name": "Soil level",
                "icon": "mdi:liquid-spot",
            }),
        ),
        (
            "spin".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-spin",
                "state_topic": "$this/spin",
                "name": "Spin",
                "icon": "mdi:autorenew",
            }),
        ),
        (
            "temp".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-temp",
                "state_topic": "$this/temp",
                "name": "Temperature",
                "icon": "mdi:thermometer",
            }),
        ),
        (
            "extra_rinse".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-extra_rinse",
                "state_topic": "$this/extra_rinse",
                "name": "Extra rinse",
                "icon": "mdi:water-sync",
            }),
        ),
        (
            "extra_rinse_count".into(),
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
            "pre_wash".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-pre_wash",
                "state_topic": "$this/pre_wash",
                "name": "Pre-wash",
                "icon": "mdi:water-sync",
            }),
        ),
        (
            "steam".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-steam",
                "state_topic": "$this/steam",
                "name": "Steam",
                "icon": "mdi:kettle-steam",
            }),
        ),
        (
            "delay_wash".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-delay_wash",
                "state_topic": "$this/delay_wash",
                "name": "Delay Wash",
                "icon": "mdi:clock-plus-outline",
            }),
        ),
        (
            "door_lock".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-door_lock",
                "state_topic": "$this/door_lock",
                "name": "Door lock",
                "icon": "mdi:lock",
                "entity_category": "diagnostic",
            }),
        ),
    ]
    .into_iter()
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    const MODEL_ID: &str = "F3S1CWKB__";

    // Real captures from the washer unit of a split-module WKEX200HBA WashTower.

    // 0xEB snapshot replayed on reconnect: the retained idle record, with no course selected but
    // the session flag set and the remaining byte parked on 1.
    const EB_RETAINED_IDLE: &str = "AA3333EB002B0000000E000000000000000000000100000052000000109000000000240400000000000000400000000000DABB";

    // 0xEC during a Speed Wash: course 0x4A, door-lock bit set, 1 minute remaining.
    const EC_SPEED_WASH_RUNNING: &str = "AA6033EC002B0000000E0E4A000000000000000002000F002F00000E0C9007000000250400000000000000100000000000002B0000000E0E4A000000000000000001000F003200000E0C9007000000250400000000000000100000000000F4BB";

    // 0xEC 82 s later: the current record's course is cleared while the panel is still powered.
    const EC_SPEED_WASH_COMPLETE: &str = "AA6033EC002B0000000E0E4A000000000000000001000F003200000E0C9007000000250400000000000000100000000000002B0000000E0000000000000000000001000000360000100E90000000002604000000000000005000000000002BBB";

    // 0xCD status dump carrying phase Washing (0x17 after the 01 02 0B marker).
    const CD_WASHING: &str = "AA0033CD00019001020B17011101140000060003040002040005008022900024000000000000030417191A1E1F161724211CFFF7EDF3EF0000000000000000000000000000000000000000000000000000000000008484858584AE900000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000050074060000000000000000000001750248FFEF04000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008EBB";

    const SEND_POWER_ON: &str = "AA0DF0E5000201FF010201C7BB";
    const SEND_POWER_OFF: &str = "AA0DF0E5000201FF010200C4BB";

    fn meta() -> Metadata {
        Metadata::new(MODEL_ID, "WKEX200HBA", "2.10.123")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        let props = ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(
            props.get(name).map(|p| p.as_string()).as_deref(),
            Some(value),
            "{name}"
        );
    }

    #[test]
    fn config_exposes_expected_ha_components() {
        let (ha, _, _) = make();
        let components = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        for c in [
            "power",
            "status",
            "course",
            "remaining_time",
            "reserve_time",
            "soil",
            "spin",
            "temp",
            "extra_rinse",
            "extra_rinse_count",
            "pre_wash",
            "steam",
            "delay_wash",
            "door_lock",
        ] {
            assert!(components.contains_key(c), "component {c} present");
        }
        assert!(crate::t2_factory(MODEL_ID).is_some());
    }

    #[test]
    fn retained_idle_eb_on_a_fresh_connect_reads_off_with_0_remaining() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EB_RETAINED_IDLE));
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "course", "None");
    }

    #[test]
    fn ec_decodes_the_running_speed_wash_from_the_current_record() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_SPEED_WASH_RUNNING));
        expect(&ha, "power", "ON");
        expect(&ha, "course", "Speed Wash");
        expect(&ha, "remaining_time", "1");
        expect(&ha, "door_lock", "ON");
        expect(&ha, "spin", "Medium");
    }

    #[test]
    fn course_cleared_record_after_a_run_latches_complete() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_SPEED_WASH_RUNNING));
        thinq.emit_data(&hex_decode(EC_SPEED_WASH_COMPLETE));
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Complete");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "course", "None");
    }

    #[test]
    fn cd_dump_publishes_the_run_phase_once_a_course_is_selected() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_SPEED_WASH_RUNNING));
        thinq.emit_data(&hex_decode(CD_WASHING));
        expect(&ha, "status", "Washing");
    }

    #[test]
    fn set_property_power_sends_the_f0_e5_command() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), SEND_POWER_ON);

        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), SEND_POWER_OFF);
    }

    #[test]
    fn retained_replay_after_a_remote_power_off_stays_off() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(EC_SPEED_WASH_RUNNING));
        dev.set_property("power", "OFF");
        // The module reboots after a remote off and replays its retained idle record (session
        // flag set, no course). The remote-off latch must keep it reading OFF.
        thinq.emit_data(&hex_decode(EB_RETAINED_IDLE));
        expect(&ha, "power", "OFF");
    }
}
