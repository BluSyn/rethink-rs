//! Y_V8_F___W.B32QEUK — LG FWY916WBTN1 washer-dryer (AABB, 39-byte enveloped records).
//!
//! Port of upstream PR #316. Extends `y_v8_y___w_b32qeuk` with drying mode
//! (`s[13]`), Air condensing (`s[16]` bit `0x08`), downloaded programme select
//! via `F0 25 03 15`, `F0 26` resume button, `last_command` ack status, power-on
//! guard, and suppression of zero `cycles`/`energy` when no programme is selected.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::set_power_start_pause;
use crate::devices::y_v8_y___w_b32qeuk::{self, Device, Extension};
use crate::washer_common::drying_mode;

const DRYING_MODE_OPTIONS: &[&str] = &[
    "Off", "Auto", "00:30", "01:00", "01:30", "02:00", "02:30", "Iron", "Delicate", "Eco",
];
use rethink_core::{hex_decode, HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const SELECT_PROGRAMME: &[u8] = &[0xf0, 0x25, 0x03, 0x15];
const STANDBY: u8 = 1;
const PAUSED: u8 = 2;

const COURSES: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x3a, "AI Wash"),
    (0x01, "Cotton"),
    (0x04, "Eco 40-60"),
    (0x31, "TurboWash 39"),
    (0x07, "Mixed Fabric"),
    (0x02, "Easy Care"),
    (0x2d, "Allergy Care"),
    (0x20, "Delicates"),
    (0x1b, "Hand/Wool"),
    (0x0c, "Speed 14"),
    (0x18, "Dry Only"),
    (0x13, "Wash+Dry"),
    (0x12, "Tub Clean"),
    (0x05, "Duvet"),
    (0x08, "Sports Wear"),
    (0x0d, "Steam Refresh"),
    (0x0e, "Rinse + Spin"),
    (0x17, "Spin + Drain"),
    (0x24, "Quick Wash + Dry"),
    (0x30, "Quick Tub Clean"),
    (0x3b, "Microplastic Care"),
];

const DIAL_BODIES: &[(&str, &str)] = &[
    ("AI Wash", "3A030A04010000000000020000000000"),
    ("Cotton", "01030A04010000000000020000000000"),
    ("Eco 40-60", "04030A04010000000000020000000000"),
    ("TurboWash 39", "31030904010000000001020000000000"),
    ("Mixed Fabric", "07030704010000000000020000000000"),
    ("Easy Care", "02030A04010000000000020000000000"),
    ("Allergy Care", "2D030A06010000000080020000000000"),
    ("Delicates", "20030502010000000000020000000000"),
    ("Hand/Wool", "1B030503010000000000020000000000"),
    ("Speed 14", "0C030202010000000001020000000000"),
    ("Dry Only", "18000000000200000000020000000000"),
    ("Wash+Dry", "13030A04010200000000020000000000"),
    ("Tub Clean", "12030106010000000000020000000000"),
];

const DOWNLOAD_BODIES: &[(&str, &str)] = &[
    ("Baby Care", "01030706020000000050020047000000"),
    ("Cold Wash", "0103FF0101000000000002004D000000"),
    ("Colour Protection", "2003050201000000000002003F000000"),
    ("Drain", "17000100000000000000020079000000"),
    ("Dry 30", "18000000000300000000020043000000"),
    ("Dry 60", "18000000000400000000020044000000"),
    ("Duvet", "05030701010000000000020081000000"),
    ("Hygiene", "2D03FF06010000000080020048000000"),
    ("Jeans", "2003FF02010000000000020039000000"),
    ("Juice and Food Stains", "0103070401000000004002006C000000"),
    ("Kids Wear", "01030906010000000040020034000000"),
    ("Lightly Soiled Items", "1B030202010000000000020065000000"),
    ("Lingerie", "2003050201000000000002004A000000"),
    ("Microplastic Care", "3B030704010000000000020086000000"),
    ("Minimize Detergent Residue", "0103FF04020000000000020066000000"),
    ("Minimize Wrinkles", "0103FF0401000000000002006F000000"),
    ("Quick Tub Clean", "30030201000000000000020071000000"),
    ("Quick Wash + Dry", "2403FF01010300000000020042000000"),
    ("Rainy Season", "0103FF04010000000000020037000000"),
    ("Rinse + Spin", "0E03FF00010000000000020064000000"),
    ("Shirt Dry", "18000000000A00000000020046000000"),
    ("Single Garment", "0C03020201000000000002003E000000"),
    ("Sleeve Hems and Collars", "0103070601000000004002006B000000"),
    ("Small Load", "0C030202010000000000020049000000"),
    ("Spin", "1700FF0000000000000002007A000000"),
    ("Sports Wear", "08030504010000000000020082000000"),
    ("Steam Refresh", "0D000000000000000380020085000000"),
    ("Sweat Stain", "0103FF0401000000000002003B000000"),
    ("Swimming Wear", "1B030202010000000000020036000000"),
    ("Turbo Dry", "18000000000200000000020045000000"),
];

const NOT_SEEN_PROPS: &[&str] = &[
    "child_lock",
    "wrinkle_care",
    "crease_care",
    "steam_softener",
    "set_child_lock",
    "set_wrinkle_care",
    "set_crease_care",
    "set_steam_softener",
];

const COMMANDS: &[&str] = &[
    "Power on",
    "Power off",
    "Pause",
    "Resume",
    "Set course",
    "Run programme",
];

fn download_name(code: u8) -> Option<&'static str> {
    DOWNLOAD_BODIES.iter().find_map(|(name, hex)| {
        let body = hex_decode(hex);
        (body.get(12) == Some(&code)).then_some(*name)
    })
}

fn download_load_item(code: u8) -> u8 {
    DOWNLOAD_BODIES
        .iter()
        .find_map(|(_, hex)| {
            let body = hex_decode(hex);
            (body.get(12) == Some(&code)).then(|| body[8])
        })
        .unwrap_or(0)
}

fn programme_body(name: &str) -> Option<Vec<u8>> {
    DIAL_BODIES
        .iter()
        .chain(DOWNLOAD_BODIES.iter())
        .find(|(n, _)| *n == name)
        .map(|(_, hex)| hex_decode(hex))
}

fn course_name(code: u8) -> Option<&'static str> {
    COURSES.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

struct Fwy916 {
    state: Mutex<Option<Vec<u8>>>,
    last_command: Mutex<Option<&'static str>>,
}

impl Fwy916 {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
            last_command: Mutex::new(None),
        }
    }

    fn current_programme(&self) -> Option<&'static str> {
        let guard = self.state.lock();
        let s = guard.as_deref()?;
        let name = if s[22] != 0 {
            download_name(s[22])
        } else {
            course_name(s[7])
        }?;
        programme_body(name).map(|_| name)
    }
}

impl Extension for Fwy916 {
    fn courses_override(&self) -> Option<&'static [(u8, &'static str)]> {
        Some(COURSES)
    }

    fn components(&self, comps: &mut Map<String, Value>) {
        let download_options: Vec<&str> = DOWNLOAD_BODIES.iter().map(|(n, _)| *n).collect();
        let programme_options: Vec<&str> = DIAL_BODIES
            .iter()
            .chain(DOWNLOAD_BODIES.iter())
            .map(|(n, _)| *n)
            .collect();
        let command_results: Vec<String> = COMMANDS
            .iter()
            .flat_map(|c| [format!("{c}: accepted"), format!("{c}: refused")])
            .collect();

        comps.insert(
            "drying_mode".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-drying-mode",
                "state_topic": "$this/drying_mode",
                "name": "Drying mode",
                "icon": "mdi:tumble-dryer",
                "device_class": "enum",
                "options": DRYING_MODE_OPTIONS,
            }),
        );
        comps.insert(
            "air_condensing".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-air_condensing",
                "state_topic": "$this/air_condensing",
                "name": "Air condensing",
                "icon": "mdi:weather-windy",
            }),
        );
        comps.insert(
            "downloaded_program".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-downloaded_program",
                "state_topic": "$this/downloaded_program",
                "name": "Downloaded program",
                "icon": "mdi:download",
                "device_class": "enum",
                "options": download_options,
            }),
        );
        comps.insert(
            "resume".into(),
            json!({
                "platform": "button",
                "unique_id": "$deviceid-resume",
                "command_topic": "$this/resume/set",
                "payload_press": "",
                "name": "Resume",
                "icon": "mdi:play-pause",
            }),
        );
        comps.insert(
            "last_command".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-last_command",
                "state_topic": "$this/last_command",
                "name": "Last command",
                "icon": "mdi:message-reply-text-outline",
                "device_class": "enum",
                "options": command_results,
            }),
        );

        if let Some(sc) = comps.get_mut("set_course").and_then(Value::as_object_mut) {
            sc.insert("options".into(), json!(programme_options));
        }
        if let Some(smart) = comps.get_mut("smart_course").and_then(Value::as_object_mut) {
            smart.insert("options".into(), json!(download_options));
        }
        comps.remove("start");
        for key in NOT_SEEN_PROPS {
            comps.remove(*key);
        }
    }

    fn before_state(&self, s: &[u8]) {
        *self.state.lock() = Some(s.to_vec());
    }

    fn filter_publish(&self, prop: &str, value: PropertyValue) -> Option<PropertyValue> {
        if NOT_SEEN_PROPS.contains(&prop) {
            return None;
        }
        let guard = self.state.lock();
        let s = guard.as_deref();
        if (prop == "cycles" || prop == "energy")
            && matches!(value, PropertyValue::Int(0))
            && s.is_some_and(|st| st[7] == 0)
        {
            return None;
        }
        if prop == "set_course" {
            let name = s.and_then(|st| {
                let n = if st[22] != 0 {
                    download_name(st[22])
                } else {
                    course_name(st[7])
                }?;
                programme_body(n).map(|_| n)
            });
            return Some(name.unwrap_or("None").into());
        }
        if prop == "smart_course" {
            let name = s.and_then(|st| download_name(st[22]));
            return Some(name.unwrap_or("None").into());
        }
        if prop == "course" {
            if let Some(st) = s {
                if st[22] != 0 {
                    return Some(download_name(st[22]).unwrap_or("None").into());
                }
            }
        }
        Some(value)
    }

    fn process_state(&self, dev: &Device, s: &[u8]) {
        dev.core().publish_property(
            "downloaded_program",
            download_name(s[25]).unwrap_or("None").into(),
        );
        dev.core().publish_property(
            "drying_mode",
            drying_mode(u32::from(s[13])).unwrap_or("None").into(),
        );
        dev.core()
            .publish_on_off("air_condensing", s[16] & 0x08 != 0);
    }

    fn course_setting(&self, _dev: &Device) -> Option<&'static str> {
        self.current_programme()
    }

    fn process_aabb(&self, dev: &Device, inner: &[u8]) {
        if inner.len() == 4 && inner[0] == 0x20 && inner[1] == 0x00 {
            let last = self.last_command.lock().take();
            if let Some(cmd) = last {
                let outcome = if inner[3] != 0 { "refused" } else { "accepted" };
                dev.core()
                    .publish_property("last_command", format!("{cmd}: {outcome}").into());
            }
        }
    }

    fn programme_command(&self, dev: &Device, cmd: &mut [u8; 18]) {
        let guard = self.state.lock();
        let Some(s) = guard.as_deref() else {
            return;
        };
        let download = s[22];
        cmd[2 + 5] = s[13];
        cmd[2 + 8] = download_load_item(download);
        cmd[2 + 10] = if dev.effective_delay_end() != 0.0 {
            0x03
        } else {
            0x02
        };
        cmd[2 + 12] = download;
    }

    fn set_property(&self, dev: &Device, prop: &str, value: &str) -> bool {
        if prop == "start" {
            return true;
        }
        if prop == "power" && value == "ON" {
            if self.state.lock().as_deref().is_some_and(|s| s[2] != 0) {
                return true;
            }
            *self.last_command.lock() = Some("Power on");
            set_power_start_pause(dev.core(), prop, value);
            return true;
        }
        if prop == "set_course" {
            let in_standby = self.state.lock().as_deref().is_some_and(|s| s[2] == STANDBY);
            if in_standby {
                if let Some(body) = programme_body(value) {
                    *self.last_command.lock() = Some("Set course");
                    let mut frame = Vec::with_capacity(SELECT_PROGRAMME.len() + body.len());
                    frame.extend_from_slice(SELECT_PROGRAMME);
                    frame.extend_from_slice(&body);
                    dev.core().send(&frame);
                }
            }
            dev.publish_settings();
            return true;
        }
        if prop == "resume" {
            let is_paused = self.state.lock().as_deref().is_some_and(|s| s[2] == PAUSED);
            if !is_paused {
                return true;
            }
            dev.clear_pending();
            let mut cmd = dev.preview_programme_command();
            cmd[2 + 6] = 0xff;
            cmd[2 + 7] = 0xff;
            cmd[2 + 10] = 0x02;
            *self.last_command.lock() = Some("Resume");
            dev.core().send(&cmd);
            return true;
        }
        if prop == "power" {
            *self.last_command.lock() = Some("Power off");
        } else if prop == "pause" {
            *self.last_command.lock() = Some("Pause");
        } else if prop == "run_programme" {
            *self.last_command.lock() = Some("Run programme");
        }
        false
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    y_v8_y___w_b32qeuk::Device::with_extension(ha, thinq, meta, Fwy916::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const ECO_STANDBY: &str = "AAFF200A003900AE45000100EB0027000001032603260400030A0401000000000000000300000E000000000200000000000000000000D0C7BB";
    const ECO_END: &str = "AAFF200A006000B748000100EC004E0000080001021B0400000A0000000000004000000107000E00000000020000FE0000000000000000000A0000021B040000000000000000004000000108000E00000000020000FE00000000000000CE60BB";
    const OFF_ALL_ZERO: &str = "AAFF200A006000E9CB000100EC004E00000000000000000000000000000000000000000001000D0071000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000008F07BB";
    const BABY_CARE: &str = "AAFF200A006000A41B000100EC004E000001023002302D00030A0601000000804200000400000E00790000050000000000000000000000000104320432010003070602000000504200000500470E0047000005000000000000000000002C95BB";
    const AIR_CONDENSING: &str = "AAFF200A006000A312000100EC004E00000100370037180000000000020000000000000600000E00790000000000000000000000000000000101000100180000000000020000080000000600000E007900000000000000000000000000CF99BB";
    const PAUSED_FRAME: &str = "AAFF200A006000E84A000100EC004E000006021B021B0400030A0401000000004020000104000D007100000200000000000000000000000002021B021B0400030A0401000000004000000206000D00710000020000000000000000000013DDBB";
    const RINSE_SPIN: &str = "AAFF200A006000A671000100EC004E000001040D040D010003090601000000400000000400340E003400000500000000000000000000000001001500150E00030A0001000000000000000100640E0064000001000000000000000000007555BB";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_fwy916_status_and_emits_captured_commands() {
        let meta = Metadata::new("Y_V8_F___W.B32QEUK", "Y_V8_F___W.B32QEUK", "2.11.207");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = create(ha.clone(), thinq.clone(), meta);

        // Energy and cycles don't reset to 0 on OFF_ALL_ZERO.
        thinq.emit_data(&hex_decode(ECO_END));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("254"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("14"));
        thinq.emit_data(&hex_decode(OFF_ALL_ZERO));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("254"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("14"));

        // Downloaded programme names course and smart_course; air condensing + drying mode decode.
        thinq.emit_data(&hex_decode(BABY_CARE));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Baby Care"));
        assert_eq!(prop(&ha, "downloaded_program").as_deref(), Some("Baby Care"));
        thinq.emit_data(&hex_decode(AIR_CONDENSING));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "air_condensing").as_deref(), Some("ON"));

        // Select, run, resume, and ack tracking.
        thinq.emit_data(&hex_decode(ECO_STANDBY));
        thinq.reset_recorder();
        dev.set_property("set_course", "Eco 40-60");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA18F025031504030A0401000000000002000000000052BB"
        );
        thinq.emit_data(&hex_decode("AA0820002500A2BB"));
        assert_eq!(prop(&ha, "last_command").as_deref(), Some("Set course: accepted"));

        thinq.reset_recorder();
        dev.set_property("run_programme", "");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA16F02604030A04010000000000020000000000BBBB"
        );

        thinq.reset_recorder();
        dev.set_property("set_delay_end", "3");
        dev.set_property("run_programme", "");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA16F02604030A04010003000000030000000000A7BB"
        );

        thinq.emit_data(&hex_decode(RINSE_SPIN));
        thinq.reset_recorder();
        dev.set_property("run_programme", "");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA16F0260E030A000100000000000200640000000DBB"
        );

        thinq.emit_data(&hex_decode(PAUSED_FRAME));
        thinq.reset_recorder();
        dev.set_property("resume", "");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA16F02604030A040100FFFF0000020000000000B9BB"
        );
    }
}
