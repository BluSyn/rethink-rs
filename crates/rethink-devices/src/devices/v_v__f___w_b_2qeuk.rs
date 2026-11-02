//! V_V__F___W.B_2QEUK — LG W4X1085NWH washer/dryer (modem clip_ble_v1.9.215).
//!
//! 39-byte records inside a class `0x20` envelope: [2] state, [7] course, [10] spin,
//! [11] temperature. The course table and phase codes differ from the other 39-byte
//! washers, so this is its own handler; temperature and spin use the shared tables.
//! Controls are power, Pause and Run programme, which replays the configuration
//! prepared on the washer. `F0 26` starts or restarts that programme; it does not
//! resume, and the washer rejected `F0 24 05`. `auto_ack` is on.
//!
//! Port of upstream PR #297.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_status;
use crate::washer_common::{spin_options, spin_value, temperature_value, TEMPERATURE_LABELS};
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const STATUS_LENGTH: usize = 39;

const STATE_OFF: u8 = 0;
const STATE_READY: u8 = 1;

/// Phase codes captured on this unit; other models disagree on some of them.
const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Ready"),
    (2, "Paused"),
    (6, "Washing"),
    (7, "Rinsing"),
    (8, "Spinning"),
    (10, "End"),
];

const COURSES: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x01, "Cotton"),
    (0x04, "Eco 40-60"),
    (0x07, "Mix"),
    (0x22, "Quick 30"),
    (0x02, "Synthetic"),
    (0x12, "Tub Clean"),
    (0x1b, "Hand wash/Wool"),
    (0x18, "Dry only"),
    (0x13, "Wash + Dry"),
];
const DOWNLOADED_PROGRAMME: &str = "Downloaded programme";

const DRYING: &[(u8, &str)] = &[(0, "Off"), (2, "Cupboard dry"), (11, "Low temperature")];

const FLAG_REMOTE_START: u8 = 0x02;
const FLAG_DOOR_LOCK: u8 = 0x40;
const FLAG_CHILD_LOCK: u8 = 0x80;

const POWER_ON: &[u8] = &[0xf0, 0x2a, 0x01, 0x00];
const POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];
const PAUSE: &[u8] = &[0xf0, 0x24, 0x04, 0x01, 0x00];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn hours_minutes(hours: u8, minutes: u8) -> i64 {
    i64::from(hours) * 60 + i64::from(minutes)
}

fn enumeration(name: &str, key: &str, options: Vec<&str>) -> Value {
    json!({"platform":"sensor","device_class":"enum","unique_id":format!("$deviceid-{key}"),"state_topic":format!("$this/{key}"),"name":name,"options":options})
}

fn duration(name: &str, key: &str) -> Value {
    json!({"platform":"sensor","device_class":"duration","unique_id":format!("$deviceid-{key}"),"state_topic":format!("$this/{key}"),"unit_of_measurement":"min","name":name})
}

fn button(name: &str, key: &str, icon: &str) -> Value {
    json!({"platform":"button","unique_id":format!("$deviceid-{key}"),"command_topic":format!("$this/{key}/set"),"payload_press":"PRESS","name":name,"icon":icon})
}

fn components() -> HashMap<String, Value> {
    let mut courses = labels(COURSES);
    courses.push(DOWNLOADED_PROGRAMME);
    [
        ("status", enumeration("Status", "status", labels(STATES))),
        ("course", enumeration("Programme", "course", courses)),
        ("temp", enumeration("Selected temperature", "temp", TEMPERATURE_LABELS.to_vec())),
        ("spin", enumeration("Selected spin speed", "spin", spin_options())),
        ("drying", enumeration("Drying mode", "drying", labels(DRYING))),
        ("remaining_time", duration("Remaining time", "remaining_time")),
        ("initial_time", duration("Initial time", "initial_time")),
        ("reserve_time", duration("Delayed finish", "reserve_time")),
        ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:remote"})),
        ("door_lock", json!({"platform":"binary_sensor","device_class":"lock","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:account-lock"})),
        ("power", json!({"platform":"binary_sensor","device_class":"power","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power"})),
        ("power_control", json!({"platform":"switch","unique_id":"$deviceid-power_control","state_topic":"$this/power","command_topic":"$this/power_control/set","name":"Power control","icon":"mdi:power"})),
        ("run_programme", button("Run programme", "run_programme", "mdi:play-box-outline")),
        ("pause", button("Pause", "pause", "mdi:pause-circle-outline")),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    /// The last Ready record. Later phases clear the temperature, rinse and spin
    /// fields, so Run programme replays this instead of the current record.
    programme: Mutex<Option<Vec<u8>>>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            programme: Mutex::new(None),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
        config.device.model = Some("W4X1085NWH".into());
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                if let Some(rec) = process_enveloped_status(inner, CLASS_BYTE, STATUS_LENGTH) {
                    dev.process_status(rec);
                }
            }
        }));
        this
    }

    fn process_status(&self, s: &[u8]) {
        match s[2] {
            STATE_READY => *self.programme.lock() = Some(s.to_vec()),
            STATE_OFF => *self.programme.lock() = None,
            _ => {}
        }
        let state = named(STATES, s[2]);
        self.core
            .publish_property("status", state.unwrap_or("None").into());
        match state {
            Some(_) => self.core.publish_on_off("power", s[2] != STATE_OFF),
            None => self.core.publish_property("power", "None".into()),
        }
        // The Download selector sets [22]. [25] holds the slot's contents even on
        // ordinary courses.
        let course = if s[22] > 0 && s[22] < 0xff {
            Some(DOWNLOADED_PROGRAMME)
        } else {
            named(COURSES, s[7])
        };
        self.core
            .publish_property("course", course.unwrap_or("None").into());
        self.core.publish_property(
            "temp",
            temperature_value(s[11] as usize).unwrap_or("None").into(),
        );
        self.core
            .publish_property("spin", spin_value(s[10] as usize).unwrap_or("None").into());
        self.core
            .publish_property("drying", named(DRYING, s[13]).unwrap_or("None").into());
        // Dry only and an unselected idle programme report a duration while the
        // panel shows dashes. It becomes an estimate once a cycle starts.
        let duration_known = s[7] != 0 && !(s[2] == STATE_READY && s[13] != 0);
        self.publish_minutes(
            "remaining_time",
            duration_known.then(|| hours_minutes(s[3], s[4])),
        );
        self.publish_minutes(
            "initial_time",
            duration_known.then(|| hours_minutes(s[5], s[6])),
        );
        self.core.publish_property(
            "reserve_time",
            PropertyValue::Int(hours_minutes(s[14], s[15])),
        );
        self.core
            .publish_on_off("remote_start", s[17] & FLAG_REMOTE_START != 0);
        // HA's lock class uses OFF for locked.
        self.core
            .publish_on_off("door_lock", s[17] & FLAG_DOOR_LOCK == 0);
        self.core
            .publish_on_off("child_lock", s[17] & FLAG_CHILD_LOCK != 0);
    }

    fn publish_minutes(&self, prop: &str, minutes: Option<i64>) {
        match minutes {
            Some(minutes) => self
                .core
                .publish_property(prop, PropertyValue::Int(minutes)),
            None => self.core.publish_property(prop, "None".into()),
        }
    }

    /// `F0 26` with the prepared configuration, or `None` when there is nothing safe to run.
    fn programme_command(&self) -> Option<[u8; 18]> {
        let programme = self.programme.lock();
        let s = programme.as_deref()?;
        // Downloaded programmes have a separate configuration path, unverified on
        // this model. Never substitute a guessed or zeroed programme.
        if s[7] == 0 || named(COURSES, s[7]).is_none() || s[22] != 0 {
            return None;
        }
        Some([
            0xf0, 0x26, s[7], s[9], s[10], s[11], s[12], s[13], s[14], s[15], 0, s[16], 0x03, 0, 0,
            0, 0, 0,
        ])
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        request_status(&self.core);
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        if prop == "power_control" {
            match value {
                "ON" => self.core.send(POWER_ON),
                "OFF" => self.core.send(POWER_OFF),
                _ => {}
            }
            return;
        }
        if value != "PRESS" {
            return;
        }
        match prop {
            "pause" => self.core.send(PAUSE),
            "run_programme" => {
                if let Some(cmd) = self.programme_command() {
                    self.core.send(&cmd);
                }
            }
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

    const ID: &str = "washer";

    /// A record from the W4X1085NWH capture fixture with its capture annotations.
    struct Sample {
        rx: &'static str,
        state: u8,
        remaining_minutes: i64,
        course: u8,
        drying: u8,
        delay_hours: u8,
        flags: u8,
    }

    // The ten selector positions with the programme each one reports.
    const SELECTOR_SWEEP: &[(&str, &str)] = &[
        ("Cotton", "AAFF200A0060002B39000100EC004E000001030803080000000000000000000000000004000003008800000000000000000000000000000001030903090100030A04010000000000000003000003008800000400000000000000000000C69CBB"),
        ("Eco 40-60", "AAFF200A0060002B3B000100EC004E000001030903090100030A04010000000000000003000003008800000400000000000000000000000001031D031D0400030A0401000000000000000300000300880000020000000000000000000014D4BB"),
        ("Mix", "AAFF200A0060002B3D000100EC004E000001031D031D0400030A04010000000000000003000003008800000200000000000000000000000001011A011A07000307040100000000000000020000030088000003000000000000000000009F26BB"),
        ("Quick 30", "AAFF200A0060002B3F000100EC004E000001011A011A0700030704010000000000000002000003008800000300000000000000000000000001001E001E220003050201000000000000000100000300880000020000000000000000000071ACBB"),
        ("Synthetic", "AAFF200A0060002B40000100EC004E000001001E001E2200030502010000000000000001000003008800000200000000000000000000000001022302230200030A04010000000000000003000003008800000300000000000000000000D815BB"),
        ("Downloaded programme", "AAFF200A0060002B42000100EC004E000001022302230200030A04010000000000000003000003008800000300000000000000000000000001032103212D00030A060100000080000000040088030088000005000000000000000000009368BB"),
        ("Tub Clean", "AAFF200A0060002B43000100EC004E000001032103212D00030A06010000008000000004008803008800000500000000000000000000000001011001101200030106010000000000000003000003008800000200000000000000000000CF00BB"),
        ("Hand wash/Wool", "AAFF200A0060002B45000100EC004E000001011001101200030106010000000000000003000003008800000200000000000000000000000001002700271B00030503010000000000000001000003008800000300000000000000000000E9F6BB"),
        ("Dry only", "AAFF200A0060002B47000100EC004E000001002700271B000305030100000000000000010000030088000003000000000000000000000000010032003218000000000002000000000000060000030088000000000000000000000000000BB5BB"),
        ("Wash + Dry", "AAFF200A0060002B4A000100EC004E000001003200321800000000000200000000000006000003008800000000000000000000000000000001060E060E1300030A04010200000000000004000003008800000400000000000000000000063DBB"),
    ];
    // Physical W4X1085NWH captures, clip_ble_v1.9.215, local provisioning.
    // Owner confirmed Cotton, temperature 60 C, spin 0 then 400, and off/on.
    const COTTON: &str = "aaff200a0060002a4d000100ec004e0039002a3b00010101002700000100000000000000000000000000000000000000000300880000000001030903090100030a040100000000000000030000030088000004000000000000000000005f01bb";
    const TEMP_60: &str = "aaff200a0060002aac000100ec004e0039002a7f000101010027000001030903090100030a0401000000000000000300000300880000000001030e030e0100030a06010000000000000004000003008800000400000000000000000000c04dbb";
    const NO_SPIN: &str = "aaff200a0060002abf000100ec004e000001030e030e0100030a060100000000000000040000030088000004000000000000000000000000010300030001000301060100000000000000030000030088000004000000000000000000008014bb";
    const SPIN_400: &str = "aaff200a0060002ac3000100ec004e000001030003000100030106010000000000000003000003008800000400000000000000000000000001030803080100030206010000000000000004000003008800000400000000000000000000bbf4bb";
    const OFF: &str = "aaff200a0060002ad2000100ec004e00000103080308010003020601000000000000000400000300880000040000000000000000000000000003080308010000000000000000000000000401000300880000040000000000000000000021ddbb";
    const IDLE: &str = "aaff200a0039002a2f000100eb00270000010000000000000000000000000000000000000000030088000000000000000000000000002deabb";
    // The records of the upstream W4X1085NWH.json fixture that the tests select, in
    // capture order, ending with a supervised Quick 30 cycle.
    const CAPTURE: &[Sample] = &[
        Sample { rx: "aaff200a0039002354000100eb00270000000000062213000000000000000000000000040a00050088000004000cd0000000000000009355bb", state: 0, remaining_minutes: 0, course: 19, drying: 0, delay_hours: 0, flags: 0 },
        Sample { rx: "aaff200a00600023b3000100ec004e0000010000000000000000000000000000000000000000060088000000000000000000000000000000010000000000000000000000000000800000000000060088000000000000000000000000001fe9bb", state: 1, remaining_minutes: 0, course: 0, drying: 0, delay_hours: 0, flags: 128 },
        Sample { rx: "aaff200a00600023d5000100ec004e000001000f000f0100000a00000000000000000001000006008800000000000000000000000000000001000f000f0100000a00000000000002000001000006008800000000000000000000000000cbcebb", state: 1, remaining_minutes: 15, course: 1, drying: 0, delay_hours: 0, flags: 2 },
        Sample { rx: "aaff200a00600023d7000100ec004e000001000f000f0100000a00000000000002000001000006008800000000000000000000000000000001000f000f0100000a00000000000042000001000006008800000000000000000000000000b7eabb", state: 1, remaining_minutes: 15, course: 1, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a00600023db000100ec004e000001000f000f0100000a00000000000042000001000006008800000000000000000000000000000001000f000f0100000a000000000000400000010000060088000000000000000000000000002ee9bb", state: 1, remaining_minutes: 15, course: 1, drying: 0, delay_hours: 0, flags: 64 },
        Sample { rx: "aaff200a006000240e000100ec004e000001022302230200030a04010000000000000003000006008800000300000000000000000000000001001e001e2200030502010000000000000001000006008800000200000000000000000000bce8bb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 0, flags: 0 },
        Sample { rx: "aaff200a006000243d000100ec004e000001001e001e2200030502010000000000000001000006008800000200000000000000000000000001001e001e2200030502010003000000000001000006008800000200000000000000000000ae80bb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 3, flags: 0 },
        Sample { rx: "aaff200a0060002464000100ec004e000001001e001e2200030502010003000000000001000006008800000200000000000000000000000001001e001e2200030502010004000000000001000006008800000200000000000000000000d25ebb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 4, flags: 0 },
        Sample { rx: "aaff200a0060002466000100ec004e000001001e001e2200030502010004000000000001000006008800000200000000000000000000000001001e001e2200030502010005000000000001000006008800000200000000000000000000b608bb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 5, flags: 0 },
        Sample { rx: "aaff200a0060002467000100ec004e000001001e001e2200030502010005000000000001000006008800000200000000000000000000000001001e001e2200030502010006000000000001000006008800000200000000000000000000ab89bb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 6, flags: 0 },
        Sample { rx: "aaff200a0060002468000100ec004e000001001e001e2200030502010006000000000001000006008800000200000000000000000000000001001e001e2200030502010007000000000001000006008800000200000000000000000000d7bfbb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 7, flags: 0 },
        Sample { rx: "aaff200a0060002469000100ec004e000001001e001e2200030502010007000000000001000006008800000200000000000000000000000001001e001e2200030502010008000000000001000006008800000200000000000000000000c12dbb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 8, flags: 0 },
        Sample { rx: "aaff200a006000246b000100ec004e000001001e001e2200030502010008000000000001000006008800000200000000000000000000000001001e001e2200030502010009000000000001000006008800000200000000000000000000372dbb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 9, flags: 0 },
        Sample { rx: "aaff200a006000246c000100ec004e000001001e001e2200030502010009000000000001000006008800000200000000000000000000000001001e001e220003050201000a000000000001000006008800000200000000000000000000048ebb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 10, flags: 0 },
        Sample { rx: "aaff200a006000246e000100ec004e000001001e001e220003050201000a000000000001000006008800000200000000000000000000000001001e001e220003050201000b00000000000100000600880000020000000000000000000029f3bb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 11, flags: 0 },
        Sample { rx: "aaff200a006000246f000100ec004e000001001e001e220003050201000b000000000001000006008800000200000000000000000000000001001e001e220003050201000c0000000000010000060088000002000000000000000000003283bb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 12, flags: 0 },
        Sample { rx: "aaff200a0060002470000100ec004e000001001e001e220003050201000c000000000001000006008800000200000000000000000000000001001e001e220003050201000d000000000001000006008800000200000000000000000000d76ebb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 13, flags: 0 },
        Sample { rx: "aaff200a0060002472000100ec004e000001001e001e220003050201000d000000000001000006008800000200000000000000000000000001001e001e220003050201000e000000000001000006008800000200000000000000000000ddfebb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 14, flags: 0 },
        Sample { rx: "aaff200a0060002473000100ec004e000001001e001e220003050201000e000000000001000006008800000200000000000000000000000001001e001e220003050201000f000000000001000006008800000200000000000000000000e792bb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 15, flags: 0 },
        Sample { rx: "aaff200a0060002474000100ec004e000001001e001e220003050201000f000000000001000006008800000200000000000000000000000001001e001e2200030502010010000000000001000006008800000200000000000000000000c4e6bb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 16, flags: 0 },
        Sample { rx: "aaff200a0060002475000100ec004e000001001e001e2200030502010010000000000001000006008800000200000000000000000000000001001e001e2200030502010011000000000001000006008800000200000000000000000000117abb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 17, flags: 0 },
        Sample { rx: "aaff200a0060002476000100ec004e000001001e001e2200030502010011000000000001000006008800000200000000000000000000000001001e001e220003050201001200000000000100000600880000020000000000000000000016e5bb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 18, flags: 0 },
        Sample { rx: "aaff200a0060002478000100ec004e000001001e001e2200030502010012000000000001000006008800000200000000000000000000000001001e001e220003050201001300000000000100000600880000020000000000000000000067dcbb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 19, flags: 0 },
        Sample { rx: "aaff200a006000271d000100ec004e0000010000000000000000000000000000000000000000060088000000000000000000000000000000010032003218000000000002000000000000060000060088000000000000000000000000006933bb", state: 1, remaining_minutes: 50, course: 24, drying: 2, delay_hours: 0, flags: 0 },
        Sample { rx: "aaff200a0060002744000100ec004e000001003200321800000000000200000000000006000006008800000000000000000000000000000001003200321800000000000b00000000000004000006008800000000000000000000000000b1a6bb", state: 1, remaining_minutes: 50, course: 24, drying: 11, delay_hours: 0, flags: 0 },
        Sample { rx: "aaff200a006000279b000100ec004e000000003200321800000000000000000000000006010006008800000000000000000000000000000001003200320000000000000000000000000006000006008800000000000000000000000000649ebb", state: 1, remaining_minutes: 50, course: 0, drying: 0, delay_hours: 0, flags: 0 },
        Sample { rx: "aaff200a00600027a5000100ec004e000001001e001e2200030502010000000002000001000006008800000200000000000000000000000001001e001e2200030502010000000042000001000006008800000200000000000000000000f1cebb", state: 1, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000280f000100ec004e000001001e001e2200030502010000000042000001000006008800000200000000000000000000000006001e001e2200030502010000000042200001010006008800000200000000000000000000774ebb", state: 6, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000281e000100ec004e000006001e001e2200030502010000000042200001010006008800000200000000000000000000000002001e001e2200030502010000000042000001060006008800000200000000000000000000a475bb", state: 2, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000284f000100ec004e000002001e001e2200030502010000000042000001060006008800000200000000000000000000000006001e001e22000305020100000000422000010200060088000002000000000000000000004707bb", state: 6, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002852000100ec004e000006001e001e2200030502010000000042200001020006008800000200000000000000000000000006001d001e22000305020100000000422000010200060088000002000000000000000000008e90bb", state: 6, remaining_minutes: 29, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002867000100ec004e000006001d001e2200030502010000000042000001020006008800000200000100000000000000000006001c001e22000305020100000000420000010200060088000002000001000000000000003e2ebb", state: 6, remaining_minutes: 28, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000286e000100ec004e000006001c001e2200030502010000000042000001020006008800000200000100000000000000000002001c001e2200030502010000000042000001060006008800000200000100000000000000bbbdbb", state: 2, remaining_minutes: 28, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000287a000100ec004e000002001c001e2200030502010000000042000001060006008800000200000100000000000000000006001e001e22000305020100000000420000010100060088000002000001000000000000006ac4bb", state: 6, remaining_minutes: 30, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000287e000100ec004e000006001e001e2200030502010000000042000001010006008800000200000000000000000000000006001c001e2200030502010000000042000001010006008800000200000000000000000000b2d1bb", state: 6, remaining_minutes: 28, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000288b000100ec004e000006001c001e2200030502010000000042000001010006008800000200000000000000000000000006001b001e2200030502010000000042000001010006008800000200000000000000000000a9a5bb", state: 6, remaining_minutes: 27, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002899000100ec004e000006001b001e2200030502010000000042000001010006008800000200000100000000000000000006001a001e22000305020100000000420000010100060088000002000001000000000000007929bb", state: 6, remaining_minutes: 26, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a00600028a7000100ec004e000006001a001e22000305020100000000420000010100060088000002000002000000000000000000060019001e22000305020100000000420000010100060088000002000002000000000000008863bb", state: 6, remaining_minutes: 25, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a00600028b5000100ec004e0000060019001e22000305020100000000420000010100060088000002000002000000000000000000060018001e2200030502010000000042000001010006008800000200000200000000000000268dbb", state: 6, remaining_minutes: 24, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a00600028e1000100ec004e0000060018001e22000305020100000000420000010100060088000002000020000000000000000000060017001e22000305020100000000420000010100060088000002000021000000000000006b2fbb", state: 6, remaining_minutes: 23, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000290c000100ec004e0000060017001e2200030502010000000042000001010006008800000200003f000000000000000000060016001e220003050201000000004200000101000600880000020000400000000000000032c4bb", state: 6, remaining_minutes: 22, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002936000100ec004e0000060016001e2200030502010000000042000001010006008800000200005f000000000000000000060015001e2200030502010000000042000001010006008800000200005f00000000000000dc5ebb", state: 6, remaining_minutes: 21, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002945000100ec004e0000060015001e22000305020100000000420000010100060088000002000060000000000000000000070014001e220003050001000000004200000106000600880000020000600000000000000001bbbb", state: 7, remaining_minutes: 20, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002962000100ec004e0000070014001e22000305000100000000420000010600060088000002000061000000000000000000070013001e2200030500010000000042000001060006008800000200006100000000000000eff4bb", state: 7, remaining_minutes: 19, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000296f000100ec004e0000070013001e22000305000100000000420000010600060088000002000061000000000000000000070012001e22000305000100000000420000010600060088000002000061000000000000002eecbb", state: 7, remaining_minutes: 18, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000297c000100ec004e0000070012001e22000305000100000000420000010600060088000002000062000000000000000000070011001e22000305000100000000420000010600060088000002000062000000000000003f2cbb", state: 7, remaining_minutes: 17, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a006000298a000100ec004e0000070011001e22000305000100000000420000010600060088000002000063000000000000000000070010001e22000305000100000000420000010600060088000002000063000000000000000aebbb", state: 7, remaining_minutes: 16, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002998000100ec004e0000070010001e2200030500010000000042000001060006008800000200006300000000000000000007000f001e22000305000100000000420000010600060088000002000063000000000000000efebb", state: 7, remaining_minutes: 15, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a00600029b9000100ec004e000007000f001e2200030500010000000042000001060006008800000200006400000000000000000007000e001e2200030500010000000042000001060006008800000200006400000000000000bd3fbb", state: 7, remaining_minutes: 14, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a00600029c7000100ec004e000007000e001e2200030500010000000042000001060006008800000200006500000000000000000007000d001e22000305000100000000420000010600060088000002000065000000000000003b95bb", state: 7, remaining_minutes: 13, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a00600029e3000100ec004e000007000d001e2200030500010000000042000001060006008800000200006600000000000000000007000c001e2200030500010000000042000001060006008800000200006600000000000000541abb", state: 7, remaining_minutes: 12, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a00600029f2000100ec004e000007000c001e2200030500010000000042000001060006008800000200006700000000000000000007000b001e2200030500010000000042000001060006008800000200006700000000000000d772bb", state: 7, remaining_minutes: 11, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002a00000100ec004e000007000b001e2200030500010000000042000001060006008800000200006700000000000000000008000a001e22000005000000000000420000010700060088000002000067000000000000001a47bb", state: 8, remaining_minutes: 10, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002a19000100ec004e000008000a001e22000005000000000000420000010700060088000002000068000000000000000000080009001e2200000500000000000042000001070006008800000200006800000000000000ee0bbb", state: 8, remaining_minutes: 9, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002a2c000100ec004e0000080009001e22000005000000000000420000010700060088000002000069000000000000000000080008001e22000005000000000000420000010700060088000002000069000000000000005838bb", state: 8, remaining_minutes: 8, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002a3c000100ec004e0000080008001e22000005000000000000420000010700060088000002000069000000000000000000080005001e22000005000000000000420000010700060088000002000069000000000000009bcfbb", state: 8, remaining_minutes: 5, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002a4b000100ec004e0000080005001e2200000500000000000042000001070006008800000200006b000000000000000000080004001e2200000500000000000042000001070006008800000200006b0000000000000034debb", state: 8, remaining_minutes: 4, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002a5a000100ec004e0000080004001e2200000500000000000042000001070006008800000200006d000000000000000000080003001e2200000500000000000042000001070006008800000200006d000000000000003142bb", state: 8, remaining_minutes: 3, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002a69000100ec004e0000080003001e2200000500000000000042000001070006008800000200006f000000000000000000080002001e22000005000000000000420000010700060088000002000070000000000000003704bb", state: 8, remaining_minutes: 2, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002a6e000100ec004e0000080002001e22000005000000000000420000010700060088000002000070000000000000000000080001001e22000005000000000000420000010700060088000002000070000000000000001decbb", state: 8, remaining_minutes: 1, course: 34, drying: 0, delay_hours: 0, flags: 66 },
        Sample { rx: "aaff200a0060002a82000100ec004e0000080001001e220000050000000000004200000107000600880000020000710000000000000000000a0000001e2200000000000000000040000001080006008800000200007100000000000000ab1dbb", state: 10, remaining_minutes: 0, course: 34, drying: 0, delay_hours: 0, flags: 64 },
        Sample { rx: "aaff200a0060002a84000100ec004e00000a0000001e220000000000000000004000000108000600880000020000710000000000000000000a0000001e2200000000000000000000000001080006008800000200007100000000000000418fbb", state: 10, remaining_minutes: 0, course: 34, drying: 0, delay_hours: 0, flags: 0 },
        Sample { rx: "aaff200a0060002a8b000100ec004e00000a0000001e22000000000000000000000000010800060088000002000071000000000000000000000000001e22000000000000000000000000010a0006008800000200007100000000000000ce66bb", state: 0, remaining_minutes: 0, course: 34, drying: 0, delay_hours: 0, flags: 0 },
    ];
    // Commands the washer accepted: power off, power on, Run programme (Quick 30), Pause.
    const COMMANDS: [&str; 4] = [
        "aa09f0240101009cbb",
        "aa08f02a010098bb",
        "aa16f0262203050201000000000003000000000053bb",
        "aa09f02404010099bb",
    ];

    fn meta() -> Metadata {
        Metadata::new("V_V__F___W.B_2QEUK", "V_V__F___W.B_2QEUK", "2.11.207")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    /// The first capture matching `pred`, in capture order.
    fn sample(pred: impl Fn(&Sample) -> bool) -> &'static Sample {
        CAPTURE.iter().find(|s| pred(s)).expect("captured sample")
    }

    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    #[test]
    fn physical_ten_position_selector_sweep_includes_the_downloaded_programme_flag() {
        let (ha, thinq, _) = make();
        for (programme, frame) in SELECTOR_SWEEP {
            emit(&thinq, frame);
            expect(&ha, "course", programme);
        }
    }

    #[test]
    fn production_registry_maps_the_model_and_decodes_a_captured_washer_status() {
        let factory = crate::registry::t2_factory("V_V__F___W.B_2QEUK").expect("registered");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let _dev = factory(ha.clone(), thinq.clone(), meta());
        emit(&thinq, IDLE);
        expect(&ha, "status", "Ready");
        expect(&ha, "course", "Not selected");
    }

    #[test]
    fn physical_settings_decode_from_enveloped_ec_current_records() {
        let (ha, thinq, _) = make();
        emit(&thinq, COTTON);
        expect(&ha, "course", "Cotton");
        emit(&thinq, TEMP_60);
        expect(&ha, "temp", "60 °C");
        emit(&thinq, NO_SPIN);
        expect(&ha, "spin", "No spin");
        emit(&thinq, SPIN_400);
        expect(&ha, "spin", "400 rpm");
        expect(&ha, "status", "Ready");
    }

    #[test]
    fn captured_power_off_and_eb_idle_records_decode_without_retaining_previous_settings() {
        let (ha, thinq, _) = make();
        emit(&thinq, OFF);
        expect(&ha, "status", "Off");
        expect(&ha, "power", "OFF");
        emit(&thinq, IDLE);
        expect(&ha, "status", "Ready");
        expect(&ha, "power", "ON");
        expect(&ha, "course", "Not selected");
    }

    #[test]
    fn power_pause_and_run_use_the_commands_accepted_by_the_physical_washer() {
        let (ha, thinq, dev) = make();
        let ready = sample(|s| s.state == 1 && s.course == 34 && s.flags == 66);
        emit(&thinq, ready.rx);
        thinq.reset_recorder();
        dev.set_property("power_control", "OFF");
        dev.set_property("power_control", "ON");
        dev.set_property("run_programme", "PRESS");
        dev.set_property("pause", "PRESS");
        let expected: Vec<Vec<u8>> = COMMANDS.iter().map(|c| hex_decode(c)).collect();
        assert_eq!(thinq.outbox(), expected);
        let config = ha.device(ID).unwrap().config.unwrap();
        assert_eq!(config.components["power"]["platform"], "binary_sensor");
    }

    #[test]
    fn unvalidated_resume_raw_command_payloads_and_an_unselected_programme_cannot_actuate() {
        let (_, thinq, dev) = make();
        dev.set_property("run_programme", "PRESS"); // no record received yet
        assert!(thinq.outbox().is_empty());
        emit(&thinq, IDLE);
        thinq.reset_recorder();
        for prop in ["start", "resume", "power", "set_course"] {
            dev.set_property(prop, "PRESS");
        }
        dev.set_property("run_programme", "PRESS"); // the panel has no selection
        dev.set_property("pause", "F024050100");
        dev.set_property("power_control", "F02A0100");
        assert!(thinq.outbox().is_empty());
        dev.start();
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1121010000001800B5BB"
        );
    }

    #[test]
    fn captured_child_lock_and_remote_door_lock_are_separate_flags_with_ha_lock_polarity() {
        let (ha, thinq, _) = make();
        let on_off = |on: bool| if on { "ON" } else { "OFF" };
        for flags in [128u8, 0, 2, 66, 64, 0] {
            emit(&thinq, sample(|s| s.flags == flags).rx);
            expect(&ha, "child_lock", on_off(flags & 128 != 0));
            expect(&ha, "remote_start", on_off(flags & 2 != 0));
            expect(&ha, "door_lock", on_off(flags & 64 == 0));
        }
    }

    #[test]
    fn dry_only_panel_dashes_do_not_become_a_spurious_50_minute_idle_countdown() {
        let (ha, thinq, _) = make();
        for (code, name) in [(2u8, "Cupboard dry"), (11, "Low temperature")] {
            emit(&thinq, sample(|s| s.course == 24 && s.drying == code).rx);
            expect(&ha, "drying", name);
            expect(&ha, "remaining_time", "None");
            expect(&ha, "initial_time", "None");
        }
        let idle = sample(|s| s.state == 1 && s.course == 0 && s.remaining_minutes == 50);
        emit(&thinq, idle.rx);
        expect(&ha, "remaining_time", "None");
    }

    #[test]
    fn physical_delay_sweep_is_independent_of_the_30_minute_programme_duration() {
        let (ha, thinq, _) = make();
        for hours in (3u8..=19).chain([0]) {
            let delayed = sample(|s| s.course == 34 && s.delay_hours == hours && s.state == 1);
            emit(&thinq, delayed.rx);
            expect(&ha, "reserve_time", &(i64::from(hours) * 60).to_string());
            expect(&ha, "remaining_time", "30");
        }
    }

    #[test]
    fn real_washing_and_paused_records_retain_the_captured_countdown_values() {
        let (ha, thinq, _) = make();
        let cycle: Vec<&Sample> = CAPTURE
            .iter()
            .filter(|s| s.course == 34 && [2, 6].contains(&s.state))
            .collect();
        assert!(cycle
            .iter()
            .any(|s| s.state == 2 && s.remaining_minutes == 28));
        assert!(cycle
            .iter()
            .any(|s| s.state == 6 && s.remaining_minutes < 30));
        for s in cycle {
            emit(&thinq, s.rx);
            expect(
                &ha,
                "status",
                if s.state == 2 { "Paused" } else { "Washing" },
            );
            expect(&ha, "remaining_time", &s.remaining_minutes.to_string());
            expect(&ha, "initial_time", "30");
        }
    }

    #[test]
    fn complete_physical_cycle_reaches_end_at_zero_unlocks_then_powers_off() {
        let (ha, thinq, _) = make();
        let expected = |state: u8| match state {
            7 => Some("Rinsing"),
            8 => Some("Spinning"),
            10 => Some("End"),
            0 => Some("Off"),
            _ => None,
        };
        let rinsing = CAPTURE.iter().position(|s| s.state == 7).unwrap();
        assert!(rinsing > 0);
        let mut seen = Vec::new();
        for s in &CAPTURE[rinsing..] {
            emit(&thinq, s.rx);
            if !seen.contains(&s.state) {
                seen.push(s.state);
            }
            assert_eq!(prop(&ha, "status").as_deref(), expected(s.state));
            expect(&ha, "remaining_time", &s.remaining_minutes.to_string());
            if s.state == 10 || s.state == 0 {
                expect(&ha, "remaining_time", "0");
            }
        }
        assert_eq!(seen, [7, 8, 10, 0]);
        expect(&ha, "power", "OFF");
        expect(&ha, "remote_start", "OFF");
        expect(&ha, "door_lock", "ON"); // HA: unlocked
    }

    #[test]
    fn run_programme_retains_the_prepared_configuration_when_later_phases_clear_settings() {
        let (_, thinq, dev) = make();
        let ready = sample(|s| s.state == 1 && s.course == 34 && s.flags == 66);
        let spinning = sample(|s| s.state == 8);
        emit(&thinq, ready.rx);
        emit(&thinq, spinning.rx);
        thinq.reset_recorder();
        dev.set_property("run_programme", "PRESS");
        assert_eq!(thinq.outbox()[0], hex_decode(COMMANDS[2]));

        // A fresh driver attached mid-cycle never received the selected configuration.
        let (_, attached_thinq, attached) = make();
        emit(&attached_thinq, spinning.rx);
        attached_thinq.reset_recorder();
        attached.set_property("run_programme", "PRESS");
        assert!(attached_thinq.outbox().is_empty());
    }

    #[test]
    fn captured_envelopes_receive_delivery_acks_malformed_lengths_publish_no_guessed_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, SPIN_400);
        assert!(thinq.sent().iter().any(|m| m.cmd == "ack"));
        let before = ha.device(ID).unwrap().properties;
        let mut bad = hex_decode(SPIN_400);
        bad[14] = 1;
        thinq.emit_data(&bad);
        assert_eq!(ha.device(ID).unwrap().properties, before);
    }

    #[test]
    fn discovery_keeps_the_upstream_entities() {
        let (ha, _, _) = make();
        let config = ha.device(ID).unwrap().config.unwrap();
        assert_eq!(config.device.model.as_deref(), Some("W4X1085NWH"));
        assert_eq!(config.device.name.as_deref(), Some("LG Washer"));
        let c = &config.components;
        assert_eq!(c.len(), 15);
        let courses = c["course"]["options"].as_array().unwrap();
        assert_eq!(courses.first().unwrap(), "Not selected");
        assert_eq!(courses.last().unwrap(), "Downloaded programme");
        assert_eq!(
            c["spin"]["options"].as_array().unwrap().last().unwrap(),
            "Max"
        );
        assert_eq!(
            c["status"]["options"],
            json!(["Off", "Ready", "Paused", "Washing", "Rinsing", "Spinning", "End"])
        );
        assert_eq!(c["power_control"]["state_topic"], "$this/power");
        assert_eq!(
            c["power_control"]["command_topic"],
            "$this/power_control/set"
        );
        assert_eq!(c["run_programme"]["payload_press"], "PRESS");
        assert_eq!(c["pause"]["command_topic"], "$this/pause/set");
        assert_eq!(c["reserve_time"]["name"], "Delayed finish");
    }
}
