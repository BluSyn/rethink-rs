//! MI2U7B — LG SideKick pedestal washer (class `0x20`).
//!
//! Reports are long `AA FF 20 0A` frames with a CRC-16: `0xEB` carries one 49-byte record, `0xEC`
//! the previous and the current one. Only the current (last) record is read, for display and for
//! the command cache. `0x03` events carry the drawer position and, observed in Tub Clean only, a
//! cycle completion. Short `20 E6` replies answer `F0 E5` property writes (`20 00 24` answers the
//! `F0 24` Keep Remote Start On write); the record they embed is never published, only fresh
//! reports are. Unmapped course and state codes publish `None`.
//!
//! Frames are validated before they are processed and acked. Commands are explicit only: no polls,
//! startup writes, automatic starts or retries. Writes are serialized and a reply settles one only
//! if its property set matches. Settings need a fresh powered-on report and are dropped once the
//! course or power changes. A write unanswered for 3 s blocks further writes until the appliance
//! reconnects, since replies carry no sequence number. Energy is the unscaled counter, in Wh.
//!
//! Port of upstream PR #269.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{
    default_config, unwrap_aabb, AabbDeviceCore, AabbWriteQueue, AwaitedWrite, WriteModel,
    WRITE_TIMEOUT,
};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::crc16::crc16;
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const RECORD_LEN: usize = 49;
const LONG_FRAME: u8 = 0xff;
const OPCODE_RECORD: u8 = 0xeb;
const OPCODE_RECORDS: u8 = 0xec;
const OPCODE_EVENT: u8 = 0x03;
const COMPLETION_EVENT: [u8; 5] = [0x0c, 0x0b, 0x01, 0x08, 0x0c];
const DRAWER_EVENT: [u8; 5] = [0x0f, 0x0b, 0x01, 0x08, 0x0f];

const SET_PROPERTY: [u8; 6] = [0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff];
const SET_PROPERTY_REPLY: [u8; 6] = [0x20, 0xe6, 0x00, 0x02, 0x01, 0xff];
const KEEP_REMOTE_START: [u8; 4] = [0xf0, 0x24, 0x10, 0x01];
const KEEP_REMOTE_START_REPLY: [u8; 3] = [0x20, 0x00, 0x24];
const WRITE_SET_PROPERTY: u8 = 0xe5;
const WRITE_KEEP_REMOTE_START: u8 = 0x24;

const PROPERTY_POWER: u8 = 0x02;
const PROPERTY_ACTION: u8 = 0x03;
const PROPERTY_COURSE: u8 = 0x0a;
const PROPERTY_BUZZER: u8 = 0x13;
const PROPERTY_RINSE: u8 = 0x20;
const PROPERTY_WARM_WATER: u8 = 0x46;
const PROPERTY_MELODY: u8 = 0x50;
const ACTION_START: u8 = 1;
const ACTION_PAUSE: u8 = 2;
const ACTION_RESUME: u8 = 3;
/// Observed answering power and actions that were applied.
const RESULT_APPLIED: u8 = 0x15;
const MAX_RINSES: u8 = 6;
/// Replaceable settings: an unsent write of one is replaced by a newer one.
const SETTINGS: [&str; 5] = [
    "rinse",
    "buzzer",
    "warm_water",
    "melody",
    "keep_remote_start",
];

const RINSE_OFFSET: usize = 3;
const COURSE_OFFSET: usize = 5;
const REMAINING_TIME_OFFSET: usize = 14;
// Total Time is byte 16; byte 17 belongs to the independent energy counter.
const TOTAL_TIME_OFFSET: usize = 16;
const ENERGY_OFFSET: usize = 17;
const STATE_OFFSET: usize = 21;
const DOWNLOADED_COURSE_OFFSET: usize = 23;
const BUZZER_OFFSET: usize = 29;
const WARM_WATER_OFFSET: usize = 36;
const WARM_WATER_ON: u8 = 0x08;
const KEEP_REMOTE_START_OFFSET: usize = 40;
const KEEP_REMOTE_START_ON: u8 = 0x04;
const MELODY_OFFSET: usize = 45;

const STATE_OFF: u8 = 0;
const STATE_STANDBY: u8 = 1;
const STATE_FINISHED: u8 = 16;
/// Washing, Rinsing and Spinning: a cycle is under way.
const RUNNING_STATES: [u8; 3] = [11, 12, 14];
const COURSE_TUB_CLEAN: u8 = 85;
const COURSE_DOWNLOADED: u8 = 255;

/// Built-in courses confirmed on this model, in code order (upstream's course option order).
const COURSES: [(u8, &str); 5] = [
    (33, "Hand Wash"),
    (46, "Normal"),
    (55, "Rinse + Spin"),
    (85, "Tub Clean"),
    (100, "Intimates"),
];
/// Downloaded courses by record byte 23, with the bundle the LG app writes to select one.
const DOWNLOADED_COURSES: [(u8, &str, [u8; 14]); 2] = [
    (
        0x81,
        "Spin Only",
        [
            0x0b, 0x81, 0x0a, 0xff, 0x0c, 0x37, 0x20, 0, 0x21, 0x13, 0x1f, 0, 0x46, 0,
        ],
    ),
    (
        0xee,
        "Active Wear",
        [
            0x0b, 0xee, 0x0a, 0xff, 0x0c, 0x63, 0x20, 2, 0x21, 0x13, 0x1f, 0, 0x46, 0,
        ],
    ),
];
const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Standby"),
    (27, "Standby"),
    (2, "Paused"),
    (28, "Paused"),
    (3, "Detecting"),
    (5, "Filling"),
    (6, "Detergent amount"),
    (7, "Delayed"),
    (8, "Soak"),
    (9, "Pre-wash"),
    (11, "Washing"),
    (29, "Washing"),
    (12, "Rinsing"),
    (13, "Rinse hold"),
    (14, "Spinning"),
    (15, "Drying"),
    (16, "Finished"),
    (21, "Refreshing"),
    (23, "Error"),
    (34, "Audible diagnosis"),
    (35, "Auto DT open pause"),
    (36, "Confirm start for control"),
    (37, "Clothing recognition"),
    (38, "Detergent input"),
    (39, "Softener input"),
    (40, "Pollution detecting"),
    (41, "Tub cleaning"),
    (42, "End remote maintain on"),
    (43, "Steam"),
    (47, "Laundry care"),
    (48, "EZDispense cleaning"),
    (52, "Dry cooling"),
];
const BUZZER: [&str; 5] = ["Off", "Low", "Medium", "Loud", "Very Loud"];
const MELODIES: [(&str, u8); 8] = [
    ("Normal", 0),
    ("Vivaldi Winter", 1),
    ("Verdi La Traviata Drinking Song", 6),
    ("Bubble", 7),
    ("Beethoven Symphony No. 5", 8),
    ("Arirang", 9),
    ("Pachelbel's Canon", 10),
    ("Beethoven Symphony No. 9", 11),
];

const TIMED_OUT: &str = "Command timed out; reconnect required before further commands";
const REFUSED: &str = "Command refused by appliance";
const NEEDS_FRESH_STATUS: &str = "Fresh powered-on status required";
const CONTEXT_CHANGED: &str = "Course or power changed; submit command again";

fn components() -> HashMap<String, Value> {
    let courses: Vec<&str> = COURSES
        .iter()
        .map(|(_, label)| *label)
        .chain(DOWNLOADED_COURSES.iter().map(|(_, label, _)| *label))
        .chain(["Downloaded"])
        .collect();
    let melodies: Vec<&str> = MELODIES.iter().map(|(label, _)| *label).collect();
    [
        ("power", json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"Power","icon":"mdi:power","device_class":"switch"})),
        ("start", json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","name":"Start","icon":"mdi:play"})),
        ("pause", json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","name":"Pause","icon":"mdi:pause"})),
        ("resume", json!({"platform":"button","unique_id":"$deviceid-resume","command_topic":"$this/resume/set","name":"Resume","icon":"mdi:play-pause"})),
        ("rinse", json!({"platform":"number","unique_id":"$deviceid-rinse","state_topic":"$this/rinse","command_topic":"$this/rinse/set","name":"Rinse count","icon":"mdi:water-sync","min":0,"max":MAX_RINSES,"step":1,"mode":"box"})),
        ("buzzer", json!({"platform":"select","unique_id":"$deviceid-buzzer","state_topic":"$this/buzzer","command_topic":"$this/buzzer/set","name":"Buzzer","icon":"mdi:volume-high","options":BUZZER,"entity_category":"config"})),
        ("warm_water", json!({"platform":"switch","unique_id":"$deviceid-warm_water","state_topic":"$this/warm_water","command_topic":"$this/warm_water/set","name":"Warm Water","icon":"mdi:thermometer"})),
        ("melody", json!({"platform":"select","unique_id":"$deviceid-melody","state_topic":"$this/melody","command_topic":"$this/melody/set","name":"Ending melody","options":melodies,"entity_category":"config"})),
        ("keep_remote_start", json!({"platform":"switch","unique_id":"$deviceid-keep_remote_start","state_topic":"$this/keep_remote_start","command_topic":"$this/keep_remote_start/set","name":"Keep Remote Start On","icon":"mdi:remote","entity_category":"config"})),
        ("problem", json!({"platform":"binary_sensor","unique_id":"$deviceid-problem","state_topic":"$this/problem","name":"Command problem","device_class":"problem","entity_category":"diagnostic"})),
        ("problem_reason", json!({"platform":"sensor","unique_id":"$deviceid-problem_reason","state_topic":"$this/problem_reason","name":"Command problem reason","entity_category":"diagnostic"})),
        ("cycle_finished", json!({"platform":"event","unique_id":"$deviceid-cycle_finished","state_topic":"$this/cycle_finished","name":"Cycle finished","event_types":["finished"],"enabled_by_default":true})),
        ("total_time", json!({"platform":"sensor","unique_id":"$deviceid-total_time","state_topic":"$this/total_time","name":"Total Time","device_class":"duration","unit_of_measurement":"min"})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine"})),
        ("course", json!({"platform":"select","command_topic":"$this/course/set","options":courses,"unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","unit_of_measurement":"Wh","state_class":"total_increasing"})),
        ("drawer", json!({"platform":"binary_sensor","unique_id":"$deviceid-drawer","state_topic":"$this/drawer","name":"Drawer","device_class":"opening"})),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
}

fn be16(buf: &[u8], offset: usize) -> usize {
    usize::from(u16::from_be_bytes([buf[offset], buf[offset + 1]]))
}

/// The payload of a complete long report, `AA FF 20 0A <len16> .. <opcode> <len16> <payload>
/// <crc16> BB`.
fn long_report_payload(frame: &[u8]) -> Option<&[u8]> {
    let n = frame.len();
    let valid = n >= 18
        && frame[..4] == [0xaa, LONG_FRAME, 0x20, 0x0a]
        && frame[n - 1] == 0xbb
        && be16(frame, 4) == n
        && be16(frame, 13) + 18 == n
        && usize::from(crc16(&frame[..n - 3])) == be16(frame, n - 3);
    valid.then(|| &frame[15..n - 3])
}

/// A long report, or a washer-style short write reply with a valid checksum.
fn is_valid_frame(frame: &[u8]) -> bool {
    let n = frame.len();
    if n < 6 || frame[0] != 0xaa || frame[n - 1] != 0xbb {
        return false;
    }
    if frame[1] == LONG_FRAME {
        return long_report_payload(frame).is_some();
    }
    let sum = frame[..n - 2]
        .iter()
        .fold(0u8, |sum, b| sum.wrapping_add(*b));
    usize::from(frame[1]) == n && frame[2] == 0x20 && sum ^ 0x55 == frame[n - 2]
}

enum Report<'a> {
    Status(Status<'a>),
    /// Open, closed, or an unknown value.
    Drawer(Option<bool>),
    Completion,
}

struct Status<'a> {
    record: &'a [u8],
    course: &'static str,
    status: &'static str,
    remaining_time: Option<i64>,
    state_code: u8,
    total_time: i64,
    energy: i64,
}

/// Complete long wire frames only, not unwrapped packet bodies.
fn parse_frame(frame: &[u8]) -> Option<Report<'_>> {
    let payload = long_report_payload(frame)?;
    let opcode = frame[12];
    let record = if opcode == OPCODE_RECORD && payload.len() == RECORD_LEN {
        Some(payload)
    } else if opcode == OPCODE_RECORDS && payload.len() == 2 * RECORD_LEN && payload[0] == 0 {
        Some(&payload[RECORD_LEN..])
    } else {
        None
    };
    if let Some(record) = record.filter(|record| record[0] == 0) {
        return Some(Report::Status(parse_status(record)));
    }
    // The earlier 55-byte event of the Tub Clean cycle is not a completion.
    if opcode == OPCODE_EVENT && payload.len() == 99 && payload[..5] == COMPLETION_EVENT {
        return Some(Report::Completion);
    }
    // Only the reversible drawer event is understood; other events (such as the 73-byte one) and
    // the inner lid have no established meaning.
    if opcode == OPCODE_EVENT && payload.len() == 15 && payload[..5] == DRAWER_EVENT {
        let open = match payload[14] {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        };
        return Some(Report::Drawer(open));
    }
    None
}

fn parse_status(record: &[u8]) -> Status<'_> {
    let course = record[COURSE_OFFSET];
    let state = record[STATE_OFFSET];
    let course_label = if course == COURSE_DOWNLOADED {
        let downloaded = record[DOWNLOADED_COURSE_OFFSET];
        Some(
            DOWNLOADED_COURSES
                .iter()
                .find(|(code, _, _)| *code == downloaded)
                .map_or("Downloaded", |(_, label, _)| *label),
        )
    } else {
        COURSES
            .iter()
            .find(|(code, _)| *code == course)
            .map(|(_, label)| *label)
    };
    let state_label = STATES
        .iter()
        .find(|(code, _)| *code == state)
        .map(|(_, label)| *label);
    let remaining_time = match state {
        // Fresh Off/Standby reports hide stale estimates.
        STATE_OFF | STATE_STANDBY => None,
        STATE_FINISHED if course == COURSE_TUB_CLEAN => Some(0),
        _ => Some(i64::from(u16::from_le_bytes([
            record[REMAINING_TIME_OFFSET],
            record[REMAINING_TIME_OFFSET + 1],
        ]))),
    };
    Status {
        record,
        course: course_label.unwrap_or("None"),
        status: state_label.unwrap_or("None"),
        remaining_time,
        state_code: state,
        total_time: i64::from(record[TOTAL_TIME_OFFSET]),
        energy: be16(record, ENERGY_OFFSET) as i64,
    }
}

fn on_off(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "ON",
        Some(false) => "OFF",
        None => "None",
    }
}

fn all_unique(ids: &[u8]) -> bool {
    ids.iter().enumerate().all(|(i, id)| !ids[..i].contains(id))
}

fn set_property(pairs: &[u8]) -> Vec<u8> {
    let mut inner = SET_PROPERTY.to_vec();
    inner.push((pairs.len() / 2) as u8);
    inner.extend_from_slice(pairs);
    inner
}

/// The property ids of a well-formed `F0 E5` write.
fn property_ids(inner: &[u8]) -> Option<Vec<u8>> {
    if inner.len() < 9 || inner[..6] != SET_PROPERTY {
        return None;
    }
    let count = usize::from(inner[6]);
    if count == 0 || inner.len() != 7 + count * 2 {
        return None;
    }
    let ids: Vec<u8> = (0..count).map(|i| inner[7 + i * 2]).collect();
    all_unique(&ids).then_some(ids)
}

fn write_type(inner: &[u8]) -> Option<u8> {
    if property_ids(inner).is_some() {
        Some(WRITE_SET_PROPERTY)
    } else if inner.len() == 5 && inner[..4] == KEEP_REMOTE_START && inner[4] <= 1 {
        Some(WRITE_KEEP_REMOTE_START)
    } else {
        None
    }
}

fn accepted_result(id: u8, result: u8, command: &[u8]) -> bool {
    // The downloaded-course bundles apply successfully with 01 for Warm Water off.
    result == 0
        || (matches!(id, PROPERTY_POWER | PROPERTY_ACTION) && result == RESULT_APPLIED)
        || (id == PROPERTY_WARM_WATER
            && result == 1
            && DOWNLOADED_COURSES
                .iter()
                .any(|(_, _, pairs)| set_property(pairs) == command))
}

fn course_setting(label: &str) -> Option<Vec<u8>> {
    if let Some((code, _)) = COURSES.iter().find(|(_, course)| *course == label) {
        return Some(vec![PROPERTY_COURSE, *code]);
    }
    DOWNLOADED_COURSES
        .iter()
        .find(|(_, course, _)| *course == label)
        .map(|(_, _, pairs)| pairs.to_vec())
}

fn rinse_count(value: &str) -> Option<u8> {
    if crate::js::is_blank(value) {
        return None;
    }
    let count = crate::js::number(value)?;
    (count.fract() == 0.0 && (0.0..=f64::from(MAX_RINSES)).contains(&count)).then_some(count as u8)
}

enum Command {
    /// `F0 E5` property id/value pairs.
    Properties(Vec<u8>),
    KeepRemoteStart(bool),
}

impl Command {
    fn inner(&self) -> Vec<u8> {
        match self {
            Command::Properties(pairs) => set_property(pairs),
            Command::KeepRemoteStart(on) => [&KEEP_REMOTE_START[..], &[u8::from(*on)]].concat(),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Completion {
    #[default]
    Unarmed,
    Armed,
    Completed,
}

#[derive(Default)]
struct State {
    /// The current record of the last fresh report.
    record: Option<Vec<u8>>,
    /// Bumped when the course or power changes; settings chosen before are not sent.
    generation: u64,
    problem: Option<&'static str>,
    /// Each write sent and not yet answered, with the generation it was sent in.
    sent_generations: Vec<(Vec<u8>, u64)>,
    /// A finish repeated at startup is not a new completion. Nothing is persisted.
    completion: Completion,
}

struct Model {
    core: Arc<AabbDeviceCore>,
    state: Mutex<State>,
}

impl Model {
    fn set_problem(&self, st: &mut State, reason: Option<&'static str>) {
        st.problem = reason;
        self.core.publish_on_off("problem", reason.is_some());
        self.core
            .publish_property("problem_reason", reason.unwrap_or("OK").into());
    }

    fn build_write(&self, command: &Command, gated: bool, generation: u64) -> Option<Vec<u8>> {
        let mut st = self.state.lock();
        if gated {
            let powered = st
                .record
                .as_ref()
                .is_some_and(|rec| rec[STATE_OFFSET] != STATE_OFF);
            if !powered {
                self.set_problem(&mut st, Some(NEEDS_FRESH_STATUS));
                return None;
            }
            if generation != st.generation {
                self.set_problem(&mut st, Some(CONTEXT_CHANGED));
                return None;
            }
        }
        self.set_problem(&mut st, None);
        Some(command.inner())
    }
}

impl WriteModel for Model {
    fn write_type(&self, inner: &[u8]) -> Option<u8> {
        write_type(inner)
    }

    fn write_sent(&self, write: &AwaitedWrite) {
        let mut st = self.state.lock();
        let generation = st.generation;
        st.sent_generations.push((write.inner.clone(), generation));
    }

    fn write_timed_out(&self, _expired: &[AwaitedWrite]) {
        let mut st = self.state.lock();
        st.record = None;
        st.sent_generations.clear();
        self.set_problem(&mut st, Some(TIMED_OUT));
    }
}

pub struct Device {
    model: Arc<Model>,
    writes: AabbWriteQueue,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        Self::with_write_timeout(ha, thinq, meta, WRITE_TIMEOUT)
    }

    fn with_write_timeout(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        write_timeout: Duration,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let model = Arc::new(Model {
            core: core.clone(),
            state: Mutex::default(),
        });
        let writes = AabbWriteQueue::new(core.clone(), model.clone(), write_timeout);
        let this = Arc::new(Self { model, writes });
        let mut config = default_config(&meta, Some(json!({"name": "LG SideKick"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| dev.process_data(data)));
        this
    }

    fn process_data(&self, frame: &[u8]) {
        if !is_valid_frame(frame) {
            return;
        }
        if frame[1] == LONG_FRAME {
            self.process_report(frame);
        }
        if let Some(inner) = unwrap_aabb(frame) {
            self.process_aabb(inner);
        }
        self.model.core.ack_after_processing(frame);
        self.writes.next_write();
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 4 && buf[..3] == KEEP_REMOTE_START_REPLY {
            if let Some(write) = self
                .writes
                .write_answered(WRITE_KEEP_REMOTE_START, |_| true)
            {
                let mut st = self.model.state.lock();
                if let Some(i) = st
                    .sent_generations
                    .iter()
                    .position(|(inner, _)| *inner == write.inner)
                {
                    st.sent_generations.remove(i);
                }
                if write.own {
                    self.model
                        .set_problem(&mut st, (buf[3] != 0).then_some(REFUSED));
                }
            }
            return;
        }
        if buf.len() < 9 || buf[..6] != SET_PROPERTY_REPLY {
            return;
        }
        let count = usize::from(buf[6]);
        let offset = 7 + 2 * count;
        if count == 0 || buf.len() != offset + RECORD_LEN || buf[offset] != 0 {
            return;
        }
        let ids: Vec<u8> = (0..count).map(|i| buf[7 + i * 2]).collect();
        if !all_unique(&ids) {
            return;
        }
        let answered = self.writes.write_answered(WRITE_SET_PROPERTY, |write| {
            property_ids(&write.inner).is_some_and(|expected| {
                expected.len() == count && expected.iter().all(|id| ids.contains(id))
            })
        });
        let Some(write) = answered else {
            return;
        };
        let mut st = self.model.state.lock();
        let generation = st
            .sent_generations
            .iter()
            .position(|(inner, _)| *inner == write.inner)
            .map(|i| st.sent_generations.remove(i).1);
        if generation != Some(st.generation) {
            return;
        }
        let failed = ids
            .iter()
            .enumerate()
            .any(|(i, &id)| !accepted_result(id, buf[8 + i * 2], &write.inner));
        if write.own {
            self.model.set_problem(&mut st, failed.then_some(REFUSED));
        }
        // Reply records may be stale or reflect refused settings: only reports publish state.
        if !failed && (ids.contains(&PROPERTY_POWER) || ids.contains(&PROPERTY_COURSE)) {
            st.generation += 1;
            st.record = None;
        }
    }

    fn process_report(&self, frame: &[u8]) {
        let core = &self.model.core;
        match parse_frame(frame) {
            Some(Report::Completion) => {
                let mut st = self.model.state.lock();
                if st.completion != Completion::Armed {
                    return;
                }
                st.completion = Completion::Completed;
                let event = json!({"event_type": "finished"}).to_string();
                core.ha.publish_event(&core.id, "cycle_finished", &event);
            }
            Some(Report::Drawer(open)) => core.publish_property("drawer", on_off(open).into()),
            Some(Report::Status(status)) => self.process_status(&status),
            None => {}
        }
    }

    fn process_status(&self, status: &Status) {
        let rec = status.record;
        {
            let mut st = self.model.state.lock();
            let context_changed = st.record.as_ref().is_none_or(|prev| {
                rec[COURSE_OFFSET] != prev[COURSE_OFFSET]
                    || rec[DOWNLOADED_COURSE_OFFSET] != prev[DOWNLOADED_COURSE_OFFSET]
            });
            if context_changed || rec[STATE_OFFSET] == STATE_OFF {
                st.generation += 1;
            }
            st.record = Some(rec.to_vec());
            if matches!(status.state_code, STATE_OFF | STATE_STANDBY) {
                st.completion = Completion::Unarmed;
            } else if st.completion == Completion::Unarmed
                && RUNNING_STATES.contains(&status.state_code)
            {
                st.completion = Completion::Armed;
            }
        }
        let core = &self.model.core;
        core.publish_on_off("power", rec[STATE_OFFSET] != STATE_OFF);
        let rinse = rec[RINSE_OFFSET];
        core.publish_property(
            "rinse",
            if rinse <= MAX_RINSES {
                i64::from(rinse).into()
            } else {
                "None".into()
            },
        );
        let buzzer = BUZZER.get(usize::from(rec[BUZZER_OFFSET]));
        core.publish_property("buzzer", buzzer.copied().unwrap_or("None").into());
        core.publish_flag("warm_water", rec[WARM_WATER_OFFSET], WARM_WATER_ON);
        core.publish_flag(
            "keep_remote_start",
            rec[KEEP_REMOTE_START_OFFSET],
            KEEP_REMOTE_START_ON,
        );
        let melody = MELODIES
            .iter()
            .find(|(_, code)| *code == rec[MELODY_OFFSET])
            .map_or("None", |(label, _)| *label);
        core.publish_property("melody", melody.into());
        core.publish_property("status", status.status.into());
        core.publish_property("total_time", status.total_time.into());
        core.publish_property("course", status.course.into());
        core.publish_property(
            "remaining_time",
            status
                .remaining_time
                .map_or_else(|| "None".into(), PropertyValue::from),
        );
        core.publish_property("energy", status.energy.into());
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.model.core.id
    }

    fn start(&self) {
        let mut st = self.model.state.lock();
        let problem = st.problem;
        self.model.set_problem(&mut st, problem);
    }

    fn retire(&self) {
        self.writes.stop();
    }

    fn drop_device(&self) {
        self.retire();
        self.model.core.drop_device();
    }

    fn set_property(&self, prop: &str, value: &str) {
        if self.writes.writes_blocked() {
            return;
        }
        let switch = match value {
            "ON" => Some(true),
            "OFF" => Some(false),
            _ => None,
        };
        let pairs = match prop {
            "power" => switch.map(|on| vec![PROPERTY_POWER, u8::from(on)]),
            "course" => course_setting(value),
            "start" => Some(vec![PROPERTY_ACTION, ACTION_START]),
            "pause" => Some(vec![PROPERTY_ACTION, ACTION_PAUSE]),
            "resume" => Some(vec![PROPERTY_ACTION, ACTION_RESUME]),
            "rinse" => rinse_count(value).map(|count| vec![PROPERTY_RINSE, count]),
            "buzzer" => BUZZER
                .iter()
                .position(|level| *level == value)
                .map(|level| vec![PROPERTY_BUZZER, level as u8]),
            "melody" => MELODIES
                .iter()
                .find(|(label, _)| *label == value)
                .map(|(_, code)| vec![PROPERTY_MELODY, *code]),
            "warm_water" => switch.map(|on| vec![PROPERTY_WARM_WATER, u8::from(on)]),
            _ => None,
        };
        let command = match prop {
            "keep_remote_start" => switch.map(Command::KeepRemoteStart),
            _ => pairs.map(Command::Properties),
        };
        let Some(command) = command else {
            return;
        };
        // Power is never gated: settings and actions need the context they were chosen in.
        let gated = prop != "power";
        let coalescing_key = SETTINGS.into_iter().find(|setting| *setting == prop);
        let generation = self.model.state.lock().generation;
        let model = self.model.clone();
        self.writes.queue_write(
            move || model.build_write(&command, gated, generation),
            coalescing_key,
        );
    }

    fn publish_config(&self) {
        self.model.core.republish_config();
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
    use crate::device_trait::T2Factory;
    use crate::test_support::{make_t2, meta, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};
    use std::collections::BTreeMap;
    use std::thread;
    use std::time::Instant;

    // Captured MI2U7B wire reports, limited to protocol/status bytes with no device identifiers.
    // EC records are previous/current; the EB report is an independent captured control.
    const CAPTURED_SPINNING: &str = "aaff200a0074000ae2000100ec00620000000013ff000000000000000008000800000000010081000000000004000000000000001001003000000000010000000000000013ff0000000000000000080008000000000e01810000000000040000000000000010010030000000000100000059b3bb";
    const CAPTURED_PAUSED: &str = "aaff200a0074000b06000100ec00620000000013ff0000000000000000060008000000000e0281000000000004000000000000001001003000000000010000000000000013ff000000000000000006000800000000020e810000000000040000000000000010010030000000000100000005a2bb";
    const CAPTURED_STANDBY: &str = "aaff200a004300095f000100eb003100000001132e00000000000000002700270000002e0100ee0000000000040000000000000802002030000000000b000000cc60bb";

    /// Stands in for upstream's mocked timers.
    const SHORT_TIMEOUT: Duration = Duration::from_millis(100);

    type Harness = (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>);

    fn make() -> Harness {
        make_t2("MI2U7B", Device::new)
    }

    fn make_with_timeout(timeout: Duration) -> Harness {
        make_t2("MI2U7B", |ha, thinq, meta| {
            Device::with_write_timeout(ha, thinq, meta, timeout)
        })
    }

    fn props(ha: &MockHaConnection) -> BTreeMap<String, String> {
        ha.device(DEVICE_ID)
            .map(|dev| {
                dev.properties
                    .into_iter()
                    .map(|(name, value)| (name, value.as_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    fn events(ha: &MockHaConnection) -> Vec<String> {
        ha.device(DEVICE_ID)
            .unwrap()
            .events
            .into_iter()
            .filter(|(topic, _)| topic == "cycle_finished")
            .map(|(_, payload)| payload)
            .collect()
    }

    fn component(ha: &MockHaConnection, key: &str) -> Value {
        ha.device(DEVICE_ID).unwrap().config.unwrap().components[key].clone()
    }

    fn wait_for(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }

    // Synthetic mutations cover boundaries and fields not varied in the captured reports.
    fn checksum(mut packet: Vec<u8>) -> Vec<u8> {
        let n = packet.len();
        let crc = crc16(&packet[..n - 3]);
        packet[n - 3..n - 1].copy_from_slice(&crc.to_be_bytes());
        packet
    }

    fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0; payload.len() + 18];
        packet[..4].copy_from_slice(&[0xaa, 0xff, 0x20, 0x0a]);
        let len = packet.len() as u16;
        packet[4..6].copy_from_slice(&len.to_be_bytes());
        packet[12] = opcode;
        packet[13..15].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        packet[15..15 + payload.len()].copy_from_slice(payload);
        packet[len as usize - 1] = 0xbb;
        checksum(packet)
    }

    /// Upstream's `status()` defaults are state 1, course 46 and 39 minutes.
    fn status(state: u8, course: u8, minutes: u16) -> Vec<u8> {
        let mut payload = [0; 49];
        payload[5] = course;
        payload[21] = state;
        payload[14..16].copy_from_slice(&minutes.to_le_bytes());
        payload[16..18].copy_from_slice(&39u16.to_le_bytes());
        frame(0xeb, &payload)
    }

    fn drawer(value: u8, counter: u8, length: usize) -> Vec<u8> {
        let mut payload = vec![0; length];
        payload[..5].copy_from_slice(&[0x0f, 0x0b, 0x01, 0x08, 0x0f]);
        if length > 14 {
            payload[14] = value;
        }
        if length > 5 {
            payload[5] = counter;
        }
        frame(0x03, &payload)
    }

    fn completion() -> Vec<u8> {
        let mut payload = [0; 99];
        payload[..5].copy_from_slice(&[0x0c, 0x0b, 0x01, 0x08, 0x0c]);
        frame(3, &payload)
    }

    fn command_status(state: u8, course: u8, rinse: u8) -> Vec<u8> {
        let mut payload = vec![0; 49];
        payload[..6].copy_from_slice(&[0, 0, 0, rinse, 19, course]);
        payload[21] = state;
        payload[29] = 4;
        payload
    }

    /// Upstream's `commandStatus()` defaults.
    fn standby() -> Vec<u8> {
        command_status(1, 46, 2)
    }

    fn reply_to(target: u8, ids: &[u8], rec: &[u8], results: &[u8]) -> Vec<u8> {
        let mut inner = vec![0x20, 0xe6, 0, 2, 1, target, ids.len() as u8];
        for (id, result) in ids.iter().zip(results) {
            inner.extend([*id, *result]);
        }
        inner.extend_from_slice(rec);
        wrap_aabb(&inner)
    }

    fn reply(ids: &[u8], rec: &[u8], results: &[u8]) -> Vec<u8> {
        reply_to(0xff, ids, rec, results)
    }

    /// Upstream's `reply(ids)`: all accepted, with the default record.
    fn ok_reply(ids: &[u8]) -> Vec<u8> {
        reply(ids, &standby(), &vec![0; ids.len()])
    }

    fn transition(previous: &[u8], current: &[u8]) -> Vec<u8> {
        frame(0xec, &[previous, current].concat())
    }

    fn inner(packet: &[u8]) -> Vec<u8> {
        packet[2..packet.len() - 2].to_vec()
    }

    fn last_pair(packet: &[u8]) -> Vec<u8> {
        let inner = inner(packet);
        inner[inner.len() - 2..].to_vec()
    }

    // ── passive exact-model support ──

    #[test]
    fn confirmed_course_labels_standby_and_observed_tub_clean_rinsing_context() {
        let (ha, thinq, _dev) = make();
        for (code, label) in [
            (46, "Normal"),
            (85, "Tub Clean"),
            (100, "Intimates"),
            (33, "Hand Wash"),
            (55, "Rinse + Spin"),
            (255, "Downloaded"),
            (254, "None"),
        ] {
            thinq.emit_data(&status(1, code, 39));
            expect(&ha, "course", label);
            expect(&ha, "status", "Standby");
            expect(&ha, "remaining_time", "None");
        }
        thinq.emit_data(&status(12, 85, 13));
        let expected: BTreeMap<String, String> = [
            ("power", "ON"),
            ("rinse", "0"),
            ("buzzer", "Off"),
            ("warm_water", "OFF"),
            ("keep_remote_start", "OFF"),
            ("melody", "Normal"),
            ("course", "Tub Clean"),
            ("status", "Rinsing"),
            ("remaining_time", "13"),
            ("total_time", "39"),
            ("energy", "0"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        assert_eq!(props(&ha), expected);
        thinq.emit_data(&status(12, 85, 513));
        expect(&ha, "remaining_time", "513");
        // State labels apply across courses.
        thinq.emit_data(&status(12, 46, 7));
        expect(&ha, "status", "Rinsing");
        expect(&ha, "remaining_time", "7");
    }

    #[test]
    fn energy_reads_big_endian_bytes_17_18_including_carry_and_reset_while_off() {
        let (ha, thinq, _dev) = make();
        for (high, low, value) in [(0, 255, "255"), (1, 0, "256"), (1, 2, "258"), (0, 0, "0")] {
            let mut payload = [0; 49];
            payload[16] = 39;
            payload[17] = high;
            payload[18] = low;
            payload[19] = 9;
            thinq.emit_data(&frame(0xeb, &payload));
            expect(&ha, "energy", value);
            expect(&ha, "total_time", "39");
        }
        let energy = component(&ha, "energy");
        assert_eq!(energy["unit_of_measurement"], "Wh");
        assert_eq!(energy["device_class"], "energy");
        assert_eq!(energy["state_class"], "total_increasing");
    }

    #[test]
    fn phase_labels_publish_none_for_unknown_codes_countdown_follows_ours() {
        let (ha, thinq, _dev) = make();
        for (code, label) in [
            (0, "Off"),
            (2, "Paused"),
            (3, "Detecting"),
            (11, "Washing"),
            (14, "Spinning"),
            (16, "Finished"),
            (41, "Tub cleaning"),
        ] {
            thinq.emit_data(&status(code, 85, 9));
            expect(&ha, "status", label);
            let remaining = match code {
                0 => "None",
                16 => "0",
                _ => "9",
            };
            expect(&ha, "remaining_time", remaining);
        }
        thinq.emit_data(&status(16, 46, 9));
        expect(&ha, "remaining_time", "9");
        thinq.emit_data(&status(255, 254, 9));
        expect(&ha, "status", "None");
        expect(&ha, "course", "None");
        thinq.emit_data(&status(1, 46, 39));
        expect(&ha, "remaining_time", "None");
    }

    #[test]
    fn total_time_uses_byte_16_independently_of_big_endian_energy_at_17() {
        let (ha, thinq, _dev) = make();
        let mut payload = [0; 49];
        payload[21] = 11;
        payload[16..20].copy_from_slice(&[0x34, 0x12, 0x56, 0x78]);
        thinq.emit_data(&frame(0xeb, &payload));
        expect(&ha, "total_time", &0x34.to_string());
        expect(&ha, "energy", &0x1256.to_string());
        payload[14..16].copy_from_slice(&513u16.to_le_bytes());
        thinq.emit_data(&frame(0xeb, &payload));
        expect(&ha, "remaining_time", "513");
        expect(&ha, "total_time", &0x34.to_string());
        expect(&ha, "energy", &0x1256.to_string());
    }

    #[test]
    fn completion_requires_exact_signature_and_active_latch_stays_unretained_and_uncached() {
        let (ha, thinq, dev) = make();
        let finished = r#"{"event_type":"finished"}"#;
        thinq.emit_data(&completion());
        dev.start();
        // Home Assistant's discovery request.
        dev.publish_config();
        assert!(events(&ha).is_empty());
        thinq.emit_data(&status(12, 85, 39));
        for length in [10, 17, 55, 73, 98, 100] {
            thinq.emit_data(&frame(3, &vec![0; length]));
        }
        let mut wrong_prefix = completion();
        wrong_prefix[19] ^= 1;
        thinq.emit_data(&checksum(wrong_prefix));
        let mut wrong_opcode = completion();
        wrong_opcode[12] = 0xeb;
        thinq.emit_data(&checksum(wrong_opcode));
        assert!(events(&ha).is_empty());
        thinq.emit_data(&completion());
        // A one-shot, unretained event: never a retained property, never in the publish cache.
        assert_eq!(events(&ha), [finished]);
        assert!(!props(&ha).contains_key("cycle_finished"));
        assert!(!dev
            .model
            .core
            .publish_cache
            .lock()
            .contains_key("cycle_finished"));
        thinq.emit_data(&completion());
        thinq.emit_data(&status(14, 46, 39));
        thinq.emit_data(&completion());
        dev.publish_config();
        assert_eq!(events(&ha).len(), 1);
        for reset_state in [0, 1] {
            thinq.emit_data(&status(reset_state, 46, 39));
            thinq.emit_data(&completion());
            thinq.emit_data(&status(11, 46, 39));
            thinq.emit_data(&completion());
        }
        assert_eq!(events(&ha), [finished; 3]);
        let (restarted_ha, restarted, _restarted_dev) = make();
        restarted.emit_data(&completion());
        assert!(events(&restarted_ha).is_empty());
    }

    #[test]
    fn drawer_events_reverse_independently_and_do_not_infer_inner_lid_state() {
        let (ha, thinq, _dev) = make();
        thinq.emit_data(&status(1, 46, 39));
        for (value, counter, expected) in [
            (1, 127, "ON"),
            (1, 128, "ON"),
            (0, 129, "OFF"),
            (2, 130, "None"),
        ] {
            thinq.emit_data(&drawer(value, counter, 15));
            expect(&ha, "drawer", expected);
            expect(&ha, "status", "Standby");
        }
        thinq.emit_data(&drawer(1, 0, 15));
        thinq.emit_data(&status(12, 85, 39));
        for offset in 0..5 {
            let mut wrong_event = drawer(0, 0, 15);
            wrong_event[15 + offset] ^= 1;
            thinq.emit_data(&checksum(wrong_event));
            expect(&ha, "drawer", "ON");
        }
        for length in [10, 14, 16, 73, 99] {
            thinq.emit_data(&drawer(0, 0, length));
        }
        expect(&ha, "drawer", "ON");
    }

    #[test]
    fn rejects_malformed_envelopes_lengths_crc_wrong_status_layouts_and_all_truncations() {
        let valid = status(12, 85, 39);
        let mut invalid: Vec<Vec<u8>> = (0..valid.len()).map(|n| valid[..n].to_vec()).collect();
        for (offset, value) in [
            (0, 0),
            (1, 0),
            (2, 0x30),
            (3, 0x0b),
            (5, 66),
            (14, 48),
            (12, 0xec),
            (15, 1),
            (valid.len() - 1, 0),
        ] {
            let mut bad = valid.clone();
            bad[offset] = value;
            invalid.push(checksum(bad));
        }
        let mut corrupt = valid.clone();
        corrupt[20] ^= 1;
        invalid.push(corrupt);
        invalid.push([&valid[..], &[0]].concat());
        for length in [48, 50, 98] {
            invalid.push(frame(0xeb, &vec![0; length]));
        }
        for opcode in [0x85, 0x86, 0xec] {
            invalid.push(frame(opcode, &[0; 49]));
        }
        let (ha, thinq, _dev) = make();
        thinq.emit_data(&valid);
        thinq.emit_data(&drawer(1, 0, 15));
        let before = props(&ha);
        for bad in &invalid {
            assert!(parse_frame(bad).is_none(), "{}", hex_encode(bad));
            thinq.emit_data(bad);
        }
        let closed = drawer(0, 0, 15);
        for n in 0..closed.len() {
            assert!(parse_frame(&closed[..n]).is_none());
        }
        let complete = completion();
        for n in 0..complete.len() {
            assert!(parse_frame(&complete[..n]).is_none());
        }
        let mut bad_completion = complete.clone();
        bad_completion[20] ^= 1;
        assert!(parse_frame(&bad_completion).is_none());
        let mut bad_drawer = drawer(0, 0, 15);
        bad_drawer[20] ^= 1;
        assert!(parse_frame(&bad_drawer).is_none());
        assert_eq!(props(&ha), before);
    }

    #[test]
    fn startup_preserves_prior_reports_bridge_only_sends_acks_without_explicit_commands() {
        let factory = crate::registry::lookup_t2("MI2U7B").expect("MI2U7B is registered");
        assert!(std::ptr::fn_addr_eq(factory, super::create as T2Factory));
        assert!(crate::registry::lookup_t2("MI2U7B_other").is_none());
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta("MI2U7B"));
        ha.publish_property(DEVICE_ID, "status", "previous state".into());
        let dev = Device::new(ha.clone(), thinq.clone(), meta("MI2U7B"));
        expect(&ha, "status", "previous state");
        assert!(!dev.model.core.publish_cache.lock().contains_key("status"));
        dev.start();
        dev.publish_config();
        for prop in ["start", "drawer", "anything"] {
            dev.set_property(prop, "ON");
        }
        for packet in [
            status(1, 46, 39),
            drawer(1, 0, 15),
            status(12, 85, 39),
            completion(),
            frame(0x85, &[0; 52]),
            Vec::new(),
        ] {
            thinq.emit_data(&packet);
        }
        assert_eq!(component(&ha, "course")["platform"], "select");
        let sent = thinq.sent();
        assert_eq!(sent.len(), 5);
        assert!(sent.iter().all(|message| message.cmd == "ack"));
        assert!(thinq.outbox().is_empty());
    }

    // ── automatic ACKs ──

    /// Records each ack with the status published by the time it went out.
    struct AckProbe {
        thinq: Arc<MockThinq2Device>,
        ha: Arc<MockHaConnection>,
        acks: Mutex<Vec<(Option<String>, Vec<u8>)>>,
    }

    impl Thinq2Device for AckProbe {
        fn id(&self) -> &str {
            self.thinq.id()
        }
        fn meta(&self) -> &Metadata {
            self.thinq.meta()
        }
        fn send_packet(&self, buf: &[u8]) {
            self.thinq.send_packet(buf);
        }
        fn send_ack(&self, buf: &[u8]) {
            self.acks
                .lock()
                .push((prop(&self.ha, "status"), buf.to_vec()));
        }
        fn send(&self, cmd: &str, msg_type: i32, data: Value) {
            self.thinq.send(cmd, msg_type, data);
        }
        fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>) {
            self.thinq.on_data(handler);
        }
        fn emit_data(&self, buf: &[u8]) {
            self.thinq.emit_data(buf);
        }
    }

    #[test]
    fn valid_long_frames_are_processed_before_sequence_carrying_acks_including_retries() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta("MI2U7B"));
        let probe = Arc::new(AckProbe {
            thinq: thinq.clone(),
            ha: ha.clone(),
            acks: Mutex::new(Vec::new()),
        });
        let dev = Device::new(ha.clone(), probe.clone(), meta("MI2U7B"));
        let mut packet = status(12, 85, 39);
        packet[7..9].copy_from_slice(&0x1234u16.to_be_bytes());
        let packet = checksum(packet);
        thinq.emit_data(&packet);
        thinq.emit_data(&packet);
        let ack = (
            Some("Rinsing".to_string()),
            wrap_aabb(&[0xf0, 0, 0x0a, 4, 0x12, 0x34]),
        );
        assert_eq!(*probe.acks.lock(), [ack.clone(), ack]);
        dev.drop_device();
    }

    #[test]
    fn invalid_frames_are_not_acked_heartbeats_and_appliance_acks_never_cause_ack_loops() {
        let (_ha, thinq, dev) = make();
        let packet = status(1, 46, 39);
        let mut corrupt = packet.clone();
        corrupt[20] ^= 1;
        for n in 0..packet.len() {
            thinq.emit_data(&packet[..n]);
        }
        thinq.emit_data(&corrupt);
        thinq.emit_data(&wrap_aabb(&[0x20, 0xc3]));
        thinq.emit_data(&wrap_aabb(&[0x20, 0, 0x24, 0]));
        assert!(thinq.sent().is_empty());
        assert!(thinq.outbox().is_empty());
        dev.drop_device();
    }

    // ── captured current-record reports ──

    #[test]
    fn captured_ec98_selects_the_last_record_captured_eb49_still_decodes() {
        let (ha, thinq, dev) = make();
        for (hex, course, phase, remaining, total) in [
            (CAPTURED_STANDBY, "Normal", "Standby", "None", "39"),
            (CAPTURED_SPINNING, "Spin Only", "Spinning", "8", "8"),
            (CAPTURED_PAUSED, "Spin Only", "Paused", "6", "8"),
        ] {
            let packet = hex_decode(hex);
            let Some(Report::Status(decoded)) = parse_frame(&packet) else {
                panic!("Expected a captured status record");
            };
            assert_eq!(decoded.record, &packet[packet.len() - 52..packet.len() - 3]);
            thinq.emit_data(&packet);
            expect(&ha, "course", course);
            expect(&ha, "status", phase);
            expect(&ha, "remaining_time", remaining);
            expect(&ha, "total_time", total);
            expect(&ha, "energy", "0");
        }
        dev.drop_device();
    }

    #[test]
    fn ec98_rejects_crc_length_record_markers_wrong_opcode_and_every_truncation() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(CAPTURED_PAUSED));
        let before = props(&ha);
        let spinning = hex_decode(CAPTURED_SPINNING);
        let mut invalid: Vec<Vec<u8>> = (0..spinning.len())
            .map(|n| spinning[..n].to_vec())
            .collect();
        let mut corrupt = spinning.clone();
        corrupt[70] ^= 1;
        invalid.push(corrupt);
        invalid.push([&spinning[..], &[0]].concat());
        for (offset, value) in [
            (5, spinning.len() as u8 - 1),
            (14, 97),
            (12, 0xeb),
            (15, 1),
            (64, 1),
            (spinning.len() - 1, 0),
        ] {
            let mut bad = spinning.clone();
            bad[offset] = value;
            invalid.push(checksum(bad));
        }
        for length in [49, 97, 99] {
            invalid.push(frame(0xec, &vec![0; length]));
        }
        for bad in &invalid {
            assert!(parse_frame(bad).is_none(), "{}", hex_encode(bad));
            thinq.emit_data(bad);
        }
        assert_eq!(props(&ha), before);
        dev.drop_device();
    }

    #[test]
    fn command_cache_uses_ec_current_power_and_course_not_the_previous_record() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&transition(
            &command_status(0, 85, 2),
            &command_status(1, 46, 2),
        ));
        dev.set_property("rinse", "3");
        assert_eq!(
            inner(&thinq.outbox()[0]),
            [0xf0, 0xe5, 0, 2, 1, 0xff, 1, 0x20, 3]
        );
        dev.set_property("rinse", "4");
        thinq.emit_data(&transition(
            &command_status(1, 46, 2),
            &command_status(1, 85, 2),
        ));
        thinq.emit_data(&reply(&[0x20], &command_status(1, 46, 3), &[0]));
        expect(&ha, "course", "Tub Clean");
        assert_eq!(thinq.outbox().len(), 1);
        dev.set_property("rinse", "5");
        assert_eq!(thinq.outbox().len(), 2);
        thinq.emit_data(&reply(&[0x20], &command_status(1, 85, 5), &[0]));
        thinq.emit_data(&transition(
            &command_status(1, 85, 2),
            &command_status(0, 85, 2),
        ));
        dev.set_property("rinse", "6");
        assert_eq!(thinq.outbox().len(), 2);
        expect(&ha, "power", "OFF");
        dev.drop_device();
    }

    // ── confirmed explicit controls ──

    #[test]
    fn reports_settings_without_startup_writes() {
        let (ha, thinq, dev) = make();
        dev.start();
        assert!(thinq.outbox().is_empty());
        thinq.emit_data(&frame(0xeb, &standby()));
        expect(&ha, "power", "ON");
        expect(&ha, "rinse", "2");
        expect(&ha, "buzzer", "Very Loud");
        assert_eq!(component(&ha, "buzzer")["entity_category"], "config");
        thinq.emit_data(&frame(0xeb, &command_status(0, 46, 2)));
        expect(&ha, "power", "OFF");
        dev.drop_device();
    }

    #[test]
    fn rinse_writes_only_property_20_accepted_replies_never_override_fresh_reports() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        dev.set_property("rinse", "6");
        assert_eq!(
            inner(&thinq.outbox()[0]),
            [0xf0, 0xe5, 0, 2, 1, 0xff, 1, 0x20, 6]
        );
        expect(&ha, "rinse", "2");
        let mut accepted = command_status(1, 46, 6);
        accepted[36] = 8;
        thinq.emit_data(&reply(&[0x20], &accepted, &[0]));
        expect(&ha, "problem", "OFF");
        expect(&ha, "rinse", "2");
        expect(&ha, "warm_water", "OFF");
        thinq.emit_data(&frame(0xeb, &accepted));
        expect(&ha, "rinse", "6");
        expect(&ha, "warm_water", "ON");
        // A fresh report is authoritative, even if it differs from the accepted write.
        thinq.emit_data(&frame(0xeb, &command_status(1, 46, 1)));
        expect(&ha, "rinse", "1");
        expect(&ha, "warm_water", "OFF");
        dev.set_property("course", "Hand Wash");
        thinq.emit_data(&reply(&[0x0a], &accepted, &[0]));
        expect(&ha, "course", "Normal");
        let defaults = command_status(1, 33, 2);
        thinq.emit_data(&frame(0xeb, &defaults));
        expect(&ha, "course", "Hand Wash");
        expect(&ha, "rinse", "2");
        expect(&ha, "warm_water", "OFF");
        dev.set_property("rinse", "3");
        assert_eq!(
            inner(&thinq.outbox()[2]),
            [0xf0, 0xe5, 0, 2, 1, 0xff, 1, 0x20, 3]
        );
        dev.drop_device();
    }

    #[test]
    fn all_buzzer_levels_use_target_ff_property_13_and_read_byte_29() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        for (code, label) in ["Off", "Low", "Medium", "Loud", "Very Loud"]
            .into_iter()
            .enumerate()
        {
            dev.set_property("buzzer", label);
            let code = code as u8;
            assert_eq!(
                inner(&thinq.outbox()[usize::from(code)]),
                [0xf0, 0xe5, 0, 2, 1, 0xff, 1, 0x13, code]
            );
            let mut rec = standby();
            rec[29] = code;
            let before = prop(&ha, "buzzer");
            thinq.emit_data(&reply(&[0x13], &rec, &[0]));
            assert_eq!(prop(&ha, "buzzer"), before);
            thinq.emit_data(&frame(0xeb, &rec));
            expect(&ha, "buzzer", label);
        }
        dev.drop_device();
    }

    #[test]
    fn warm_water_uses_property_46_but_reads_only_byte_36_mask_08() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        for (i, value) in ["ON", "OFF"].into_iter().enumerate() {
            let bit = u8::from(value == "ON");
            dev.set_property("warm_water", value);
            assert_eq!(
                inner(&thinq.outbox()[i]),
                [0xf0, 0xe5, 0, 2, 1, 0xff, 1, 0x46, bit]
            );
            let mut rec = standby();
            rec[36] = 0x40 | if bit == 1 { 8 } else { 0 };
            rec[46] = if bit == 1 { 0 } else { 8 };
            let before = prop(&ha, "warm_water");
            thinq.emit_data(&reply(&[0x46], &rec, &[0]));
            assert_eq!(prop(&ha, "warm_water"), before);
            thinq.emit_data(&frame(0xeb, &rec));
            expect(&ha, "warm_water", value);
        }
        dev.drop_device();
    }

    #[test]
    fn melody_property_50_reads_byte_45_for_each_captured_choice() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        for (i, (label, code)) in [
            ("Normal", 0),
            ("Vivaldi Winter", 1),
            ("Verdi La Traviata Drinking Song", 6),
            ("Bubble", 7),
            ("Beethoven Symphony No. 5", 8),
            ("Arirang", 9),
            ("Pachelbel's Canon", 10),
            ("Beethoven Symphony No. 9", 11),
        ]
        .into_iter()
        .enumerate()
        {
            dev.set_property("melody", label);
            assert_eq!(
                inner(&thinq.outbox()[i]),
                [0xf0, 0xe5, 0, 2, 1, 0xff, 1, 0x50, code]
            );
            let mut rec = standby();
            rec[45] = code;
            thinq.emit_data(&reply(&[0x50], &rec, &[0]));
            thinq.emit_data(&frame(0xeb, &rec));
            expect(&ha, "melody", label);
        }
        dev.drop_device();
    }

    #[test]
    fn keep_remote_start_on_uses_short_f024_writes_and_byte_40_mask_04_not_readiness() {
        let (ha, thinq, dev) = make();
        let mut rec = standby();
        rec[40] = 0x30;
        thinq.emit_data(&frame(0xeb, &rec));
        dev.set_property("keep_remote_start", "ON");
        dev.set_property("keep_remote_start", "OFF");
        assert_eq!(thinq.outbox()[0], wrap_aabb(&[0xf0, 0x24, 0x10, 1, 1]));
        expect(&ha, "keep_remote_start", "OFF");
        thinq.emit_data(&wrap_aabb(&[0x20, 0, 0x24, 0]));
        expect(&ha, "problem", "OFF");
        expect(&ha, "keep_remote_start", "OFF");
        assert_eq!(thinq.outbox()[1], wrap_aabb(&[0xf0, 0x24, 0x10, 1, 0]));
        rec[40] = 0x34;
        thinq.emit_data(&frame(0xeb, &rec));
        expect(&ha, "keep_remote_start", "ON");
        thinq.emit_data(&wrap_aabb(&[0x20, 0, 0x24, 1]));
        expect(&ha, "problem", "ON");
        expect(&ha, "keep_remote_start", "ON");
        rec[40] = 0x30;
        thinq.emit_data(&frame(0xeb, &rec));
        expect(&ha, "keep_remote_start", "OFF");
        assert_eq!(prop(&ha, "remote_start"), None);
        dev.drop_device();
    }

    #[test]
    fn serializes_coalesces_unsent_rinse_and_matches_reply_property_sets_and_target() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        dev.set_property("buzzer", "Low");
        dev.set_property("rinse", "3");
        dev.set_property("rinse", "4");
        thinq.emit_data(&ok_reply(&[0x20]));
        thinq.emit_data(&reply_to(0x33, &[0x13], &standby(), &[0]));
        thinq.emit_data(&ok_reply(&[0x13, 0x13]));
        assert_eq!(thinq.outbox().len(), 1);
        thinq.emit_data(&ok_reply(&[0x13]));
        assert_eq!(thinq.outbox().len(), 2);
        assert_eq!(
            inner(&thinq.outbox()[1]),
            [0xf0, 0xe5, 0, 2, 1, 0xff, 1, 0x20, 4]
        );
        dev.drop_device();
    }

    #[test]
    fn built_in_course_action_only_start_pause_resume_and_power_use_explicit_writes() {
        let cases: [(&str, &str, &[u8]); 10] = [
            ("power", "ON", &[2, 1]),
            ("power", "OFF", &[2, 0]),
            ("course", "Normal", &[10, 46]),
            ("course", "Intimates", &[10, 100]),
            ("course", "Hand Wash", &[10, 33]),
            ("course", "Rinse + Spin", &[10, 55]),
            ("course", "Tub Clean", &[10, 85]),
            ("start", "PRESS", &[3, 1]),
            ("pause", "PRESS", &[3, 2]),
            ("resume", "PRESS", &[3, 3]),
        ];
        for (prop, value, pairs) in cases {
            let (_ha, thinq, dev) = make();
            thinq.emit_data(&frame(0xeb, &standby()));
            dev.set_property(prop, value);
            let count = (pairs.len() / 2) as u8;
            let expected = [&[0xf0, 0xe5, 0, 2, 1, 0xff, count][..], pairs].concat();
            assert_eq!(inner(&thinq.outbox()[0]), expected, "{prop} {value}");
            dev.drop_device();
        }
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(CAPTURED_PAUSED));
        dev.set_property("start", "PRESS");
        assert_eq!(
            inner(&thinq.outbox()[0]),
            [0xf0, 0xe5, 0, 2, 1, 0xff, 1, 3, 1]
        );
        dev.drop_device();
    }

    #[test]
    fn both_captured_downloaded_bundles_accept_only_the_specific_46_01_exception() {
        for (label, download, course, rinse, total) in [
            ("Spin Only", 0x81, 0x37, 0, 8),
            ("Active Wear", 0xee, 0x63, 2, 43),
        ] {
            let (ha, thinq, dev) = make();
            thinq.emit_data(&frame(0xeb, &standby()));
            dev.set_property("course", label);
            let pairs = [
                0x0b, download, 0x0a, 0xff, 0x0c, course, 0x20, rinse, 0x21, 0x13, 0x1f, 0, 0x46, 0,
            ];
            let expected = [&[0xf0, 0xe5, 0, 2, 1, 0xff, 7][..], &pairs].concat();
            assert_eq!(inner(&thinq.outbox()[0]), expected, "{label}");
            let mut current = command_status(1, 255, rinse);
            current[23] = download;
            current[16] = total;
            let ids = [0x0b, 0x0a, 0x0c, 0x20, 0x21, 0x1f, 0x46];
            thinq.emit_data(&reply(&ids, &current, &[0, 0, 0, 0, 0, 0, 1]));
            expect(&ha, "problem", "OFF");
            expect(&ha, "course", "Normal");
            dev.set_property("rinse", "3");
            assert_eq!(thinq.outbox().len(), 1);
            thinq.emit_data(&transition(&standby(), &current));
            expect(&ha, "course", label);
            expect(&ha, "rinse", &rinse.to_string());
            expect(&ha, "total_time", &total.to_string());
            dev.set_property("rinse", "3");
            assert_eq!(thinq.outbox().len(), 2);
            dev.drop_device();
        }
    }

    #[test]
    fn result_01_is_refused_outside_the_exact_downloaded_warm_water_exception() {
        for (prop, value, id) in [("warm_water", "OFF", 0x46), ("rinse", "3", 0x20)] {
            let (ha, thinq, dev) = make();
            thinq.emit_data(&frame(0xeb, &standby()));
            dev.set_property(prop, value);
            thinq.emit_data(&reply(&[id], &standby(), &[1]));
            expect(&ha, "problem", "ON");
            dev.drop_device();
        }
        let (ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        dev.set_property("course", "Spin Only");
        let ids = [0x0b, 0x0a, 0x0c, 0x20, 0x21, 0x1f, 0x46];
        thinq.emit_data(&reply(&ids, &standby(), &[1, 0, 0, 0, 0, 0, 1]));
        expect(&ha, "problem", "ON");
        dev.drop_device();
    }

    #[test]
    fn result_01_exception_requires_the_exact_captured_bundle_not_just_its_property_set() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        let near_bundle = [
            0xf0, 0xe5, 0, 2, 1, 0xff, 7, 0x0b, 0x81, 0x0a, 0xff, 0x0c, 0x37, 0x20, 1, 0x21, 0x13,
            0x1f, 0, 0x46, 0,
        ];
        thinq.send_packet(&wrap_aabb(&near_bundle));
        dev.set_property("rinse", "4");
        let ids = [0x0b, 0x0a, 0x0c, 0x20, 0x21, 0x1f, 0x46];
        thinq.emit_data(&reply(&ids, &standby(), &[0, 0, 0, 0, 0, 0, 1]));
        // A refused external bundle does not clear the fresh baseline as an accepted course write would.
        assert_eq!(thinq.outbox().len(), 2);
        assert_eq!(last_pair(&thinq.outbox()[1]), [0x20, 4]);
        dev.drop_device();
    }

    #[test]
    fn result_15_accepts_power_and_actions_only_and_never_publishes_embedded_reply_state() {
        for (prop, value, id) in [
            ("power", "ON", 2),
            ("power", "OFF", 2),
            ("start", "PRESS", 3),
            ("pause", "PRESS", 3),
            ("resume", "PRESS", 3),
        ] {
            let (ha, thinq, dev) = make();
            thinq.emit_data(&frame(0xeb, &standby()));
            dev.set_property(prop, value);
            let before = props(&ha);
            thinq.emit_data(&reply(&[id], &command_status(14, 255, 0), &[0x15]));
            expect(&ha, "problem", "OFF");
            assert_eq!(props(&ha), before, "{prop} {value}");
            dev.drop_device();
        }
        for (prop, value, id) in [
            ("rinse", "3", 0x20),
            ("buzzer", "Low", 0x13),
            ("warm_water", "ON", 0x46),
            ("melody", "Bubble", 0x50),
            ("course", "Tub Clean", 0x0a),
        ] {
            let (ha, thinq, dev) = make();
            thinq.emit_data(&frame(0xeb, &standby()));
            dev.set_property(prop, value);
            thinq.emit_data(&reply(&[id], &command_status(14, 255, 0), &[0x15]));
            expect(&ha, "problem", "ON");
            expect(&ha, "course", "Normal");
            expect(&ha, "status", "Standby");
            expect(&ha, "rinse", "2");
            dev.drop_device();
        }
    }

    #[test]
    fn result_17_refusal_leaves_state_and_the_future_command_baseline_unchanged_no_retry() {
        for (prop, value, id) in [
            ("rinse", "2", 0x20),
            ("warm_water", "ON", 0x46),
            ("start", "PRESS", 3),
        ] {
            let (ha, thinq, dev) = make();
            let mut baseline = command_status(1, 46, 1);
            baseline[16] = 39;
            thinq.emit_data(&frame(0xeb, &baseline));
            dev.set_property(prop, value);
            let mut refused = command_status(0, 85, 2);
            refused[16] = 51;
            refused[36] = 8;
            thinq.emit_data(&reply(&[id], &refused, &[0x17]));
            expect(&ha, "problem", "ON");
            expect(&ha, "power", "ON");
            expect(&ha, "course", "Normal");
            expect(&ha, "status", "Standby");
            expect(&ha, "rinse", "1");
            expect(&ha, "warm_water", "OFF");
            expect(&ha, "total_time", "39");
            assert_eq!(thinq.outbox().len(), 1);
            dev.set_property("rinse", "3");
            assert_eq!(thinq.outbox().len(), 2);
            assert_eq!(last_pair(&thinq.outbox()[1]), [0x20, 3]);
            dev.drop_device();
        }
    }

    #[test]
    fn rejects_invalid_values_and_unsupported_features_gates_settings_while_off_or_unknown() {
        let (_ha, thinq, dev) = make();
        dev.set_property("start", "PRESS");
        dev.set_property("rinse", "1");
        thinq.emit_data(&frame(0xeb, &command_status(0, 46, 2)));
        dev.set_property("course", "Normal");
        thinq.emit_data(&frame(0xeb, &standby()));
        for (prop, value) in [
            ("rinse", "-1"),
            ("rinse", "7"),
            ("rinse", "1.5"),
            ("rinse", ""),
            ("rinse", "NaN"),
            ("buzzer", "Very High"),
            ("course", "Downloaded"),
            ("power", "on"),
            ("warm_water", "on"),
            ("melody", "Unknown"),
            ("keep_remote_start", "on"),
            ("clock_display", "ON"),
            ("remote_maintain", "ON"),
        ] {
            dev.set_property(prop, value);
        }
        assert!(thinq.outbox().is_empty());
        dev.drop_device();
    }

    #[test]
    fn refused_writes_report_failure_and_late_replies_across_course_changes_restore_nothing() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        dev.set_property("rinse", "4");
        dev.set_property("rinse", "5");
        thinq.emit_data(&frame(0xeb, &command_status(1, 85, 2)));
        thinq.emit_data(&ok_reply(&[0x20]));
        assert_eq!(thinq.outbox().len(), 1);
        expect(&ha, "course", "Tub Clean");
        dev.set_property("buzzer", "Low");
        thinq.emit_data(&reply(&[0x13], &command_status(1, 85, 2), &[0x13]));
        expect(&ha, "problem", "ON");
        expect(&ha, "buzzer", "Very Loud");
        dev.drop_device();
    }

    #[test]
    fn replies_to_writes_sent_before_a_course_change_are_ignored() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        dev.set_property("rinse", "4");
        thinq.emit_data(&frame(0xeb, &command_status(1, 85, 2)));
        thinq.emit_data(&reply(&[0x20], &standby(), &[0x13]));
        expect(&ha, "problem", "OFF");
        dev.set_property("course", "Normal");
        thinq.emit_data(&frame(0xeb, &command_status(1, 33, 2)));
        thinq.emit_data(&ok_reply(&[0x0a]));
        // The accepted course write does not discard the newer report as the command baseline.
        dev.set_property("rinse", "3");
        assert_eq!(thinq.outbox().len(), 3);
        dev.drop_device();
    }

    #[test]
    fn tracks_bridge_writes_and_waits_for_fresh_status_after_course_acceptance() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&frame(0xeb, &standby()));
        thinq.send_packet(&wrap_aabb(&[0xf0, 0xe5, 0, 2, 1, 0xff, 1, 10, 85]));
        dev.set_property("rinse", "4");
        thinq.emit_data(&reply(&[10], &command_status(1, 85, 2), &[0]));
        assert_eq!(thinq.outbox().len(), 1);
        thinq.emit_data(&frame(0xeb, &command_status(1, 85, 2)));
        dev.set_property("rinse", "4");
        assert_eq!(thinq.outbox().len(), 2);
        dev.drop_device();
    }

    #[test]
    fn strict_timeout_blocks_further_writes_malformed_replies_do_not_settle() {
        let (ha, thinq, dev) = make_with_timeout(SHORT_TIMEOUT);
        thinq.emit_data(&frame(0xeb, &standby()));
        dev.set_property("buzzer", "Off");
        dev.set_property("rinse", "3");
        let mut corrupt = ok_reply(&[0x13]);
        let n = corrupt.len();
        corrupt[n - 2] ^= 1;
        thinq.emit_data(&corrupt);
        thinq.emit_data(&reply(&[0x13], &standby()[..48], &[0]));
        let mut wrong_marker = standby();
        wrong_marker[0] = 1;
        thinq.emit_data(&reply(&[0x13], &wrong_marker, &[0]));
        assert_eq!(thinq.outbox().len(), 1);
        wait_for(|| dev.writes.writes_blocked());
        thinq.emit_data(&ok_reply(&[0x13]));
        dev.set_property("power", "ON");
        assert_eq!(thinq.outbox().len(), 1);
        let reason = prop(&ha, "problem_reason").unwrap_or_default();
        assert!(reason.contains("reconnect required"), "{reason}");
        dev.drop_device();
    }

    #[test]
    fn retire_stops_the_write_timer() {
        let timeout = Duration::from_millis(300);
        let (ha, thinq, dev) = make_with_timeout(timeout);
        thinq.emit_data(&frame(0xeb, &standby()));
        dev.set_property("buzzer", "Off");
        dev.retire();
        thread::sleep(timeout * 2);
        expect(&ha, "problem_reason", "OK");
        assert!(!dev.writes.writes_blocked());
        dev.drop_device();
    }
}
