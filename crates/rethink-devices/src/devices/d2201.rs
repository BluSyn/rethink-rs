//! D2201 dishwasher (LG XD3A15MB): D0211's records plus the Settings screen.
//!
//! Settings are one `F0 26 <rinse> <softening> <flags>` snapshot of every
//! cached value, echoed back in body bytes 13/14 and in flags1, whose bits
//! differ from the command's. A smart course is loaded with
//! `F0 25 03 06 <base course> <course> 00 00 00 <options>`; that only stores
//! the preset, the cycle is still started at the panel. Controls are not
//! optimistic: the appliance ignores commands while asleep, so HA shows what
//! it reports.
//!
//! Port of upstream PR #289.

use crate::device_trait::DeviceHandler;
use crate::devices::d0211::{Device as D0211, Extension};
use rethink_core::device_base::AabbDeviceCore;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const FLAG1_CHIME: u8 = 0x10;
const FLAG1_REMINDER: u8 = 0x20;
const FLAG1_CLEAN_LIGHT: u8 = 0x40;

const SET_REMINDER: u8 = 0x01;
const SET_CHIME: u8 = 0x04;
const SET_CLEAN_LIGHT: u8 = 0x10;

const MAX_LEVEL: i64 = 4;
// Byte 20 holds the smart course only while it is the selected course; byte 23 keeps the preset.
const STORED_SMART_COURSE: usize = 23;

const AUTO_DOOR: u8 = 0x01;
const EXTRA_DRY: u8 = 0x04;

// Number, name, and the modelJson's base course and options for each preset.
const SMART_COURSES: &[(u8, &str, u8, u8)] = &[
    (1, "Pots & Pans", 0x02, AUTO_DOOR | EXTRA_DRY),
    (2, "Casseroles", 0x02, AUTO_DOOR),
    (3, "Glassware", 0x03, AUTO_DOOR),
    (4, "Night Care", 0x03, AUTO_DOOR | EXTRA_DRY),
];

#[derive(Clone, Copy)]
struct Settings {
    rinse: u8,
    softening: u8,
    chime: bool,
    reminder: bool,
    clean_light: bool,
}

impl Settings {
    fn packet(&self) -> [u8; 5] {
        let mut flags = 0u8;
        if self.reminder {
            flags |= SET_REMINDER;
        }
        if self.chime {
            flags |= SET_CHIME;
        }
        if self.clean_light {
            flags |= SET_CLEAN_LIGHT;
        }
        [0xf0, 0x26, self.rinse, self.softening, flags]
    }
}

/// The Settings screen and smart course loader D2201 adds to D0211.
#[derive(Default)]
struct Extras {
    settings: Mutex<Option<Settings>>,
}

impl Extras {
    /// A settings write is a full snapshot, so it needs the read-back values first.
    fn write_setting(&self, core: &AabbDeviceCore, prop: &str, value: &str) {
        let pkt = {
            let mut guard = self.settings.lock();
            let Some(s) = guard.as_mut() else {
                tracing::debug!("D2201 refusing settings write: no status record received yet");
                return;
            };
            let on = value == "ON";
            match prop {
                "rinse_level" | "softening_level" => {
                    let Ok(level) = value.parse::<i64>() else {
                        return;
                    };
                    if !(0..=MAX_LEVEL).contains(&level) {
                        return;
                    }
                    if prop == "rinse_level" {
                        s.rinse = level as u8;
                    } else {
                        s.softening = level as u8;
                    }
                }
                "chime_sound" => s.chime = on,
                "machine_clean_reminder" => s.reminder = on,
                "clean_indicator_light" => s.clean_light = on,
                _ => return,
            }
            s.packet()
        };
        core.send(&pkt);
    }
}

impl Extension for Extras {
    fn components(&self) -> std::collections::HashMap<String, Value> {
        components()
    }

    fn process_status(&self, core: &AabbDeviceCore, record: &[u8]) {
        let data = &record[2..];
        let s = Settings {
            rinse: data[13],
            softening: data[14],
            chime: data[11] & FLAG1_CHIME != 0,
            reminder: data[11] & FLAG1_REMINDER != 0,
            clean_light: data[11] & FLAG1_CLEAN_LIGHT != 0,
        };
        *self.settings.lock() = Some(s);
        core.publish_property("rinse_level", i64::from(s.rinse).into());
        core.publish_property("softening_level", i64::from(s.softening).into());
        core.publish_on_off("chime_sound", s.chime);
        core.publish_on_off("machine_clean_reminder", s.reminder);
        core.publish_on_off("clean_indicator_light", s.clean_light);
        let stored = data[STORED_SMART_COURSE];
        let name = SMART_COURSES
            .iter()
            .find(|(code, ..)| *code == stored)
            .map_or("None", |(_, name, ..)| *name);
        core.publish_property("downloaded_cycle", name.into());
    }

    fn set_property(&self, core: &AabbDeviceCore, prop: &str, value: &str) {
        match prop {
            "downloaded_cycle" => {
                let Some(&(course, _, base, options)) =
                    SMART_COURSES.iter().find(|(_, name, ..)| *name == value)
                else {
                    return;
                };
                core.send(&[
                    0xf0, 0x25, 0x03, 0x06, base, course, 0x00, 0x00, 0x00, options,
                ]);
            }
            "rinse_level"
            | "softening_level"
            | "chime_sound"
            | "machine_clean_reminder"
            | "clean_indicator_light" => self.write_setting(core, prop, value),
            _ => {}
        }
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    D0211::with_extension(ha, thinq, meta, Some(Box::new(Extras::default())))
}

fn components() -> std::collections::HashMap<String, Value> {
    let smart_courses: Vec<&str> = SMART_COURSES.iter().map(|(_, name, ..)| *name).collect();
    let mut c = Map::new();
    let add = |c: &mut Map<String, Value>, k: &str, v: Value| {
        c.insert(k.into(), v);
    };
    add(
        &mut c,
        "rinse_level",
        json!({"platform":"number","icon":"mdi:water-plus","entity_category":"config","unique_id":"$deviceid-rinse_level","state_topic":"$this/rinse_level","command_topic":"$this/rinse_level/set","name":"Rinse aid dispenser level","min":0,"max":MAX_LEVEL,"step":1}),
    );
    add(
        &mut c,
        "softening_level",
        json!({"platform":"number","icon":"mdi:water-opacity","entity_category":"config","unique_id":"$deviceid-softening_level","state_topic":"$this/softening_level","command_topic":"$this/softening_level/set","name":"Softening level","min":0,"max":MAX_LEVEL,"step":1}),
    );
    add(
        &mut c,
        "chime_sound",
        json!({"platform":"switch","icon":"mdi:volume-high","entity_category":"config","unique_id":"$deviceid-chime_sound","state_topic":"$this/chime_sound","command_topic":"$this/chime_sound/set","name":"Chime sound"}),
    );
    add(
        &mut c,
        "machine_clean_reminder",
        json!({"platform":"switch","icon":"mdi:bell-outline","entity_category":"config","unique_id":"$deviceid-machine_clean_reminder","state_topic":"$this/machine_clean_reminder","command_topic":"$this/machine_clean_reminder/set","name":"Machine clean reminder"}),
    );
    add(
        &mut c,
        "clean_indicator_light",
        json!({"platform":"switch","icon":"mdi:lightbulb","entity_category":"config","unique_id":"$deviceid-clean_indicator_light","state_topic":"$this/clean_indicator_light","command_topic":"$this/clean_indicator_light/set","name":"Clean indicator light"}),
    );
    add(
        &mut c,
        "downloaded_cycle",
        json!({"platform":"select","icon":"mdi:download-circle-outline","unique_id":"$deviceid-downloaded_cycle","state_topic":"$this/downloaded_cycle","command_topic":"$this/downloaded_cycle/set","name":"Downloaded cycle","options":smart_courses}),
    );
    c.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const ID: &str = "test-id";
    // Each status frame is the 0xEC change that followed the command above it. The records
    // start in Ready with Turbo selected, so only the settings differ between them.
    const TX_RINSE_4: &str = "AA09F02604000098BB";
    const RX_RINSE_4: &str = "AA3A32EC00180100000123040001230000020400000100000000000000010018010000012304000123000002040400010000000000000001B5BB";
    const TX_SOFTENING_1: &str = "AA09F0260001009FBB";
    const RX_SOFTENING_1: &str = "AA3A32EC0018010000012304000123000002040000010000000000000001001801000001230400012300000A040001010000000000000001B0BB";
    const TX_CHIME_ON: &str = "AA09F0260200049ABB";
    const RX_CHIME_ON: &str = "AA3A32EC00180100000123040001230000020402000100000000000000010018010000012304000123000012040200010000000000000001A5BB";
    const TX_CHIME_OFF: &str = "AA09F0260200009EBB";
    const RX_CHIME_OFF: &str = "AA3A32EC00180100000123040001230000120402000100000000000000010018010000012304000123000002040200010000000000000001A5BB";
    const TX_REMINDER_ON: &str = "AA09F02602000199BB";
    const RX_REMINDER_ON: &str = "AA3A32EC0018010000012304000123000002040200010000000000000001001801000001230400012300002204020001000000000000000155BB";
    const TX_LIGHT_ON: &str = "AA09F0260200108EBB";
    const RX_LIGHT_ON: &str = "AA3A32EC0018010000012304000123000002040200010000000000000001001801000001230400012300004204020001000000000000000175BB";
    /// Smart course, the command the app sent, and the status record that followed.
    const SMART_COURSE_LOADS: &[(&str, &str, &str)] = &[
        (
            "Pots & Pans",
            "AA0EF02503060201000000058BBB",
            "AA3A32EC00180100000130030101300000120402000100000000040000040018010000022A0201022A00001204020001000000000100000164BB",
        ),
        (
            "Casseroles",
            "AA0EF02503060202000000018EBB",
            "AA3A32EC0018010000020A0500020A0000120002000100000000000000040018010000021702010217000012000200010000000002000002EDBB",
        ),
        (
            "Glassware",
            "AA0EF025030603030000000188BB",
            "AA3A32EC00180100000217020102170000120002000100000000020000020018010000011E0301011E0000120002000100000000030000038ABB",
        ),
        (
            "Night Care",
            "AA0EF0250306030400000005B7BB",
            "AA3A32EC0018010000011E0301011E00001200020001000000000300000300180100000130030101300000120402000100000000040000044DBB",
        ),
    ];

    fn meta() -> Metadata {
        Metadata::new("D2201", "D2201", "1")
    }

    fn make() -> (
        Arc<MockHaConnection>,
        Arc<MockThinq2Device>,
        Arc<dyn DeviceHandler>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = create(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn first_packet(thinq: &MockThinq2Device) -> String {
        hex_encode(&thinq.outbox()[0])
    }

    /// The starting record holds the settings the appliance had when the app sent the
    /// command, so a snapshot that dropped one of them would not match.
    fn assert_write(status: &str, setting: &str, value: &str, expected: &str) {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(status));
        thinq.reset_recorder();
        dev.set_property(setting, value);
        assert_eq!(first_packet(&thinq), expected);
    }

    fn assert_smart_course_load(index: usize) {
        let (name, command, status) = SMART_COURSE_LOADS[index];
        let (ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("downloaded_cycle", name);
        assert_eq!(first_packet(&thinq), command);

        thinq.emit_data(&hex_decode(status));
        assert_eq!(prop(&ha, "downloaded_cycle").as_deref(), Some(name));
    }

    #[test]
    fn the_settings_are_read_back_from_the_status_record() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&hex_decode(RX_RINSE_4));
        assert_eq!(prop(&ha, "rinse_level").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "softening_level").as_deref(), Some("0"));

        thinq.emit_data(&hex_decode(RX_SOFTENING_1));
        assert_eq!(prop(&ha, "softening_level").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "salt_refill").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(RX_CHIME_ON));
        assert_eq!(prop(&ha, "chime_sound").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(RX_CHIME_OFF));
        assert_eq!(prop(&ha, "chime_sound").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(RX_REMINDER_ON));
        assert_eq!(prop(&ha, "machine_clean_reminder").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "clean_indicator_light").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(RX_LIGHT_ON));
        assert_eq!(prop(&ha, "clean_indicator_light").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "machine_clean_reminder").as_deref(), Some("OFF"));
    }

    #[test]
    fn writing_rinse_level_4_reproduces_the_command_the_lg_app_sent() {
        assert_write(RX_CHIME_OFF, "rinse_level", "4", TX_RINSE_4);
    }

    #[test]
    fn writing_chime_on_reproduces_the_command_the_lg_app_sent() {
        assert_write(RX_CHIME_OFF, "chime_sound", "ON", TX_CHIME_ON);
    }

    #[test]
    fn writing_machine_clean_reminder_on_reproduces_the_command_the_lg_app_sent() {
        assert_write(RX_CHIME_OFF, "machine_clean_reminder", "ON", TX_REMINDER_ON);
    }

    #[test]
    fn writing_clean_indicator_light_on_reproduces_the_command_the_lg_app_sent() {
        assert_write(RX_CHIME_OFF, "clean_indicator_light", "ON", TX_LIGHT_ON);
    }

    #[test]
    fn writing_chime_off_reproduces_the_command_the_lg_app_sent() {
        assert_write(RX_CHIME_ON, "chime_sound", "OFF", TX_CHIME_OFF);
    }

    #[test]
    fn writing_softening_level_1_reproduces_the_command_the_lg_app_sent() {
        let (_, thinq, dev) = make();
        // The app sent it with the rinse level at 0, so bring the level there first.
        thinq.emit_data(&hex_decode(RX_RINSE_4));
        dev.set_property("rinse_level", "0");
        thinq.reset_recorder();
        dev.set_property("softening_level", "1");
        assert_eq!(first_packet(&thinq), TX_SOFTENING_1);
    }

    #[test]
    fn loading_pots_and_pans_reproduces_the_command_the_lg_app_sent_and_reads_back() {
        assert_smart_course_load(0);
    }

    #[test]
    fn loading_casseroles_reproduces_the_command_the_lg_app_sent_and_reads_back() {
        assert_smart_course_load(1);
    }

    #[test]
    fn loading_glassware_reproduces_the_command_the_lg_app_sent_and_reads_back() {
        assert_smart_course_load(2);
    }

    #[test]
    fn loading_night_care_reproduces_the_command_the_lg_app_sent_and_reads_back() {
        assert_smart_course_load(3);
    }

    #[test]
    fn the_stored_downloaded_cycle_is_reported_while_another_course_is_selected() {
        // Byte 20 is 0 here, but byte 23 still holds the preset the app showed.
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(RX_CHIME_OFF));
        assert_eq!(
            prop(&ha, "downloaded_cycle").as_deref(),
            Some("Pots & Pans")
        );
    }

    #[test]
    fn a_write_carries_the_settings_the_appliance_already_has() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(RX_CHIME_ON));
        thinq.reset_recorder();

        // Rinse level 2 and the chime on: changing the softening level must not clear the chime.
        dev.set_property("softening_level", "1");
        assert_eq!(
            hex_encode(&thinq.outbox()[0][2..7]).to_ascii_lowercase(),
            "f026020104"
        );
    }

    #[test]
    fn a_settings_write_is_refused_until_a_status_record_has_been_read() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("chime_sound", "ON");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn a_record_d0211_rejects_is_not_read_for_settings() {
        // Synthetic: the current record's initial-time minute set out of range.
        let mut bad_minute = hex_decode(RX_CHIME_ON);
        bad_minute[36] = 0x3c;
        let (ha, thinq, dev) = make();
        thinq.emit_data(&bad_minute);
        assert_eq!(prop(&ha, "chime_sound"), None);
        dev.set_property("chime_sound", "OFF");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn out_of_range_levels_are_not_sent() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(RX_CHIME_OFF));
        thinq.reset_recorder();
        dev.set_property("rinse_level", "5");
        dev.set_property("softening_level", "-1");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("D2201").is_some());
    }
}
