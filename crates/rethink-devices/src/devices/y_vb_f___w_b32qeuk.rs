//! Y_VB_F___W.B32QEUK — LG F4DR711S2BA washer/dryer combo.
//!
//! Extends `Y_VB_Y___W.B32QEUK` with the drying mode at `s[13]`, courses `Wash+Dry (6kg)` (0x13)
//! and `Dry Only` (0x18), panel course defaults for `F0 26`, `detergent_empty`, and tub / drying
//! air temperature readings from the 5-minute measurement records.
//!
//! Port of upstream PR #352.

use crate::device_trait::DeviceHandler;
use crate::devices::y_v8_y___w_b32qeuk::{Device as V8, Extension};
use crate::devices::y_vb_y___w_b32qeuk::Ezdispense;
use crate::washer_common::drying_mode;
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const COURSES: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x3a, "AI Wash"),
    (0x01, "Cotton"),
    (0x02, "Easy Care"),
    (0x04, "Eco 40-60"),
    (0x05, "Duvet"),
    (0x07, "Mixed Fabric"),
    (0x08, "Sports Wear"),
    (0x0c, "Speed 14"),
    (0x12, "Tub Clean"),
    (0x1b, "Hand/Wool"),
    (0x20, "Delicates"),
    (0x2d, "Allergy Care"),
    (0x31, "TurboWash 39"),
    (0x0e, "Rinse + Spin"),
    (0x17, "Spin only"),
    (0x09, "Silent Wash"),
    (0x30, "Quick Tub Clean"),
    (0x3b, "MicroPlastic Care"),
    (0x37, "Dispenser Clean"),
    (0x13, "Wash+Dry (6kg)"),
    (0x18, "Dry Only"),
];

const DRYING_MODES: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x02, "Auto"),
    (0x03, "00:30"),
    (0x04, "01:00"),
    (0x05, "01:30"),
    (0x06, "02:00"),
    (0x07, "02:30"),
    (0x0a, "Iron"),
    (0x0b, "Delicate"),
    (0x0c, "Eco"),
];

const SELECTABLE_COURSES: &[&str] = &[
    "Not selected",
    "AI Wash",
    "Cotton",
    "Eco 40-60",
    "TurboWash 39",
    "Mixed Fabric",
    "Easy Care",
    "Allergy Care",
    "Delicates",
    "Hand/Wool",
    "Speed 14",
    "Dry Only",
    "Wash+Dry (6kg)",
    "Tub Clean",
];

const SELECTABLE_SPINS: &[&str] = &[
    "Not selected",
    "No spin",
    "400 rpm",
    "800 rpm",
    "1000 rpm",
    "1200 rpm",
    "1400 rpm",
];

const SELECTABLE_TEMPS: &[&str] = &[
    "Not selected",
    "Cold",
    "20 °C",
    "30 °C",
    "40 °C",
    "60 °C",
    "95 °C",
];

const SELECTABLE_RINSES: &[&str] = &["Not selected", "Normal", "Rinse+"];

/// Panel defaults per course: [soil, spin, temp, rinse, dry, options, detergent, softener].
/// This model does not accept 0 as "course default" in F026.
const COURSE_DEFAULTS: &[(u8, [u8; 8])] = &[
    (0x3a, [3, 10, 4, 1, 0, 0x00, 2, 2]), // AI Wash
    (0x01, [3, 10, 4, 1, 0, 0x00, 2, 2]), // Cotton
    (0x04, [3, 10, 4, 1, 0, 0x00, 2, 2]), // Eco 40-60
    (0x31, [3, 9, 4, 1, 0, 0x01, 2, 2]),  // TurboWash 39
    (0x07, [3, 7, 4, 1, 0, 0x00, 2, 2]),  // Mixed Fabric
    (0x02, [3, 10, 4, 1, 0, 0x00, 2, 2]), // Easy Care
    (0x2d, [3, 10, 6, 1, 0, 0x80, 3, 3]), // Allergy Care
    (0x20, [3, 5, 2, 1, 0, 0x00, 1, 1]),  // Delicates
    (0x1b, [3, 5, 3, 1, 0, 0x00, 1, 1]),  // Hand/Wool
    (0x0c, [3, 2, 2, 1, 0, 0x01, 2, 2]),  // Speed 14
    (0x18, [0, 0, 0, 0, 2, 0x00, 0, 0]),  // Dry Only
    (0x13, [3, 10, 4, 1, 2, 0x00, 2, 2]), // Wash+Dry (6kg)
    (0x12, [3, 1, 6, 1, 0, 0x00, 0, 0]),  // Tub Clean
];

const ENVELOPE: u8 = 0x0a;
const RECORD_MARKER: [u8; 3] = [0x0d, 0x01, 0x02];
const KIND_LOG: u8 = 0x65;
const LOG_TUB: usize = 15;
const LOG_DRYING: usize = 56;
const LOG_LENGTH: usize = 61;
const SAMPLES: usize = 5;
const NO_SAMPLE: u8 = 0xff;

fn course_defaults(code: u8) -> Option<[u8; 8]> {
    COURSE_DEFAULTS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, d)| *d)
}

fn course_code(name: &str) -> Option<u8> {
    COURSES.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

fn drying_code(name: &str) -> Option<u8> {
    DRYING_MODES
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(c, _)| *c)
}

fn drying_options() -> Vec<&'static str> {
    DRYING_MODES.iter().map(|(_, n)| *n).collect()
}

/// The samples form a ring with no start marker; the largest step is where it wraps.
fn newest(samples: &[u8]) -> u8 {
    let mut at = 0;
    let mut step = -1i32;
    for i in 0..samples.len() {
        let next = samples[(i + 1) % samples.len()];
        let diff = (i32::from(next) - i32::from(samples[i])).abs();
        if diff > step {
            step = diff;
            at = i;
        }
    }
    samples[at]
}

#[derive(Default)]
struct ComboState {
    powered_on: bool,
    reported_dry: u8,
    pending_dry: Option<u8>,
    pending_dose: Option<(u8, u8)>,
}

#[derive(Default)]
struct Combo {
    ez: Ezdispense,
    state: Mutex<ComboState>,
}

impl Combo {
    fn process_measurements(&self, dev: &V8, buf: &[u8]) {
        if buf.len() < 13 || buf[0] != 0x20 || buf[1] != ENVELOPE || buf[9] != 0x01 {
            return;
        }
        if buf[10] != 0x02 && buf[10] != 0x03 {
            return;
        }
        let inner_len = usize::from(u16::from_be_bytes([buf[11], buf[12]]));
        let Some(records) = buf.get(13..13 + inner_len) else {
            return;
        };
        let marker = if records.starts_with(&RECORD_MARKER) {
            0
        } else if records.get(1..).is_some_and(|r| r.starts_with(&RECORD_MARKER)) {
            1
        } else {
            return;
        };
        if records.len() < marker + 8 {
            return;
        }
        let kind = records[marker + 3];
        let body = &records[marker + 8..];
        if kind != KIND_LOG || body.len() < LOG_LENGTH {
            return;
        }
        let series = |at: usize| -> Option<u8> {
            let slice = &body[at..at + SAMPLES];
            if slice.contains(&NO_SAMPLE) {
                None
            } else {
                Some(newest(slice))
            }
        };
        if let Some(tub) = series(LOG_TUB) {
            dev.core()
                .publish_property("tub_temperature", PropertyValue::Int(i64::from(tub)));
        }
        if let Some(dry) = series(LOG_DRYING) {
            dev.core()
                .publish_property("drying_temperature", PropertyValue::Int(i64::from(dry)));
        }
    }

    fn allowed_select(prop: &str, value: &str) -> bool {
        let list = match prop {
            "set_course" => SELECTABLE_COURSES,
            "set_spin" => SELECTABLE_SPINS,
            "set_temp" => SELECTABLE_TEMPS,
            "set_rinse" => SELECTABLE_RINSES,
            _ => return true,
        };
        list.contains(&value)
    }
}

impl Extension for Combo {
    fn courses_override(&self) -> Option<&'static [(u8, &'static str)]> {
        Some(COURSES)
    }

    fn components(&self, components: &mut Map<String, Value>) {
        self.ez.components(components);
        for (name, options) in [
            ("set_course", SELECTABLE_COURSES),
            ("set_spin", SELECTABLE_SPINS),
            ("set_temp", SELECTABLE_TEMPS),
            ("set_rinse", SELECTABLE_RINSES),
        ] {
            if let Some(Value::Object(obj)) = components.get_mut(name) {
                obj.insert("options".into(), json!(options));
            }
        }
        // F024 04 (pause) is ignored and F024 05 (start) is refused on this model.
        components.insert("pause".into(), json!({"platform": "button"}));
        components.insert("start".into(), json!({"platform": "button"}));
        let dry_opts = drying_options();
        for (k, v) in [
            (
                "drying_mode",
                json!({"platform":"sensor","unique_id":"$deviceid-drying_mode","state_topic":"$this/drying_mode","name":"Drying mode","icon":"mdi:tumble-dryer","device_class":"enum","options":dry_opts}),
            ),
            (
                "tub_temperature",
                json!({"platform":"sensor","unique_id":"$deviceid-tub_temperature","state_topic":"$this/tub_temperature","name":"Tub temperature","device_class":"temperature","state_class":"measurement","unit_of_measurement":"°C"}),
            ),
            (
                "drying_temperature",
                json!({"platform":"sensor","unique_id":"$deviceid-drying_temperature","state_topic":"$this/drying_temperature","name":"Drying air temperature","device_class":"temperature","state_class":"measurement","unit_of_measurement":"°C"}),
            ),
            (
                "detergent_empty",
                json!({"platform":"binary_sensor","unique_id":"$deviceid-detergent_empty","state_topic":"$this/detergent_empty","name":"Detergent empty","device_class":"problem"}),
            ),
            (
                "set_drying_mode",
                json!({"platform":"select","unique_id":"$deviceid-set_drying_mode","state_topic":"$this/set_drying_mode","command_topic":"$this/set_drying_mode/set","name":"Set drying mode","icon":"mdi:tumble-dryer","options":dry_opts,"entity_category":"config"}),
            ),
        ] {
            components.insert(k.into(), v);
        }
    }

    fn process_aabb(&self, dev: &V8, inner: &[u8]) {
        self.ez.process_aabb(dev, inner);
        self.process_measurements(dev, inner);
    }

    fn process_state(&self, dev: &V8, s: &[u8]) {
        {
            let mut st = self.state.lock();
            st.powered_on = s[2] > 0;
            st.reported_dry = s[13];
        }
        self.ez.process_state(dev, s);
        let core = dev.core();
        core.publish_property(
            "drying_mode",
            drying_mode(u32::from(s[13])).unwrap_or("None").into(),
        );
        core.publish_on_off("detergent_empty", s[37] & 0x01 != 0);
        self.publish_settings(dev);
    }

    fn course_setting(&self, dev: &V8) -> Option<&'static str> {
        self.ez.course_setting(dev)
    }

    fn publish_settings(&self, dev: &V8) {
        self.ez.publish_settings(dev);
        let dry = {
            let st = self.state.lock();
            st.pending_dry.unwrap_or(st.reported_dry)
        };
        dev.core().publish_property(
            "set_drying_mode",
            drying_mode(u32::from(dry)).unwrap_or("None").into(),
        );
    }

    fn programme_command(&self, dev: &V8, cmd: &mut [u8; 18]) {
        self.ez.programme_command(dev, cmd);
        if let Some(defaults) = course_defaults(cmd[2]) {
            for i in 0..4 {
                if cmd[3 + i] == 0 {
                    cmd[3 + i] = defaults[i];
                }
            }
        }
        let st = self.state.lock();
        cmd[7] = st.pending_dry.unwrap_or(st.reported_dry);
        if let Some((det, soft)) = st.pending_dose {
            cmd[15] = det;
            cmd[16] = soft;
        }
    }

    fn set_property(&self, dev: &V8, prop: &str, value: &str) -> bool {
        // F02A switches a running panel off on this model.
        if prop == "power" && value == "ON" && self.state.lock().powered_on {
            return true;
        }
        if prop == "run_programme" {
            let cmd = dev.preview_programme_command();
            if course_defaults(cmd[2]).is_none() && cmd[2..7].contains(&0) {
                return true;
            }
            let handled = self.ez.set_property(dev, prop, value);
            if !handled {
                dev.run_programme();
            }
            let mut st = self.state.lock();
            st.pending_dry = None;
            st.pending_dose = None;
            drop(st);
            dev.publish_settings();
            return true;
        }
        if !Self::allowed_select(prop, value) {
            return true;
        }
        if prop == "set_drying_mode" {
            if let Some(code) = drying_code(value) {
                self.state.lock().pending_dry = Some(code);
            }
            dev.publish_settings();
            return true;
        }
        if prop == "set_course" {
            if let Some(defaults) = course_code(value).and_then(course_defaults) {
                let [soil, spin, temp, rinse, dry, options, detergent, softener] = defaults;
                dev.set_pending(|p| {
                    p.soil = Some(soil);
                    p.spin = Some(spin);
                    p.temp = Some(temp);
                    p.rinse = Some(rinse);
                    p.options = Some(options);
                });
                let mut st = self.state.lock();
                st.pending_dry = Some(dry);
                st.pending_dose = Some((detergent, softener));
            }
        }
        self.ez.set_property(dev, prop, value)
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    V8::with_extension(ha, thinq, meta, Combo::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    const SAMPLE_STANDBY: &str = "AAFF200A006000396C000100EC004E000001000000000000000000000000000000000000000008004A0000000000000202322D004101000001000000000000000000000000000000000000000008004A0000000000000202322D0001011266BB";
    const SAMPLE_WASH_DRY_AUTO: &str = "AAFF200A00600039E8000100EC004E000001010C010C1200030106010000000000000003000008004A0000020000000000322D000001000001050F050F1300030A04010200000000000004000008004A0000040000000202322D00010193FBBB";
    const SAMPLE_WASH_DRY_1H: &str = "AAFF200A00600039FD000100EC004E000001032C032C1300030A04010300000000000002000008004A0000040000000202322D000101000001040E040E1300030A04010400000000000002000008004A0000040000000202322D0001012A96BB";
    const SAMPLE_WASH_DRY_IRON: &str = "AAFF200A0060003A01000100EC004E000001050E050E1300030A04010600000000000003000008004A0000040000000202322D000101000001041D041D1300030A04010A00000000000003000008004A0000040000000202322D000101EEACBB";
    const SAMPLE_WASH_DRY_DELICATE: &str = "AAFF200A0060003A03000100EC004E000001041D041D1300030A04010A00000000000003000008004A0000040000000202322D000101000001050205021300030A04010B00000000000003000008004A0000040000000202322D000101CC4BBB";
    const SAMPLE_COTTON: &str = "AAFF200A00600039D3000100EC004E000001003B003B3A00030A04010000000000010002000008004A0000040000000202322D000101000001033103310100030A04010000000000000003000008004A0000040000000202322D0001014917BB";
    const LOG_HEATING: &str = "AAFF200A0061004A8C00010102004F0D01026500EF001A0600280402042C000008494849474632342D2F31EFEDEFEEEC000000000000000000000000000000000000000000000000000000000000001F1F1C1C1E014C00000000000F0400D37BBB";
    const LOG_DRYING: &str = "AAFF200A006100562400010102004F0D01026500EF0097090090003A031300001D3F3F3D3E3F35383C3D31FFFFFFFFFF0000000000000000000000000000000000000000000000000000000000000851565A5E4C014C00000000000F1B00FD95BB";
    const DIAL_DRY_ONLY: &str = "AAFF200A00600070B3000100EC004E000001050F050F1300030A0401020000000000000400000A004A0000040000000202322D00000100000100370037180000000000020000000000000600000A004A0000000000000000322D000001DE78BB";
    const STATUS_DETERGENT_EMPTY: &str = "AAFF200A00600043EE000100EC004E000001033B033B0700030A06010B00004000000004000009004A0000040000000202322D000101000001033B033B0700030A06010B00004000000004000009004A0000040000000202322D0041019B50BB";
    const STATUS_DETERGENT_REFILLED: &str = "AAFF200A00600043FF000100EC004E000001033B033B0700030A06010B00004000000004000009004A0000040000000202322D000001000004033B033B0700030A06010B00004002200001010009004A0000040000000202322D000001658CBB";

    fn make(frame: &str) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<dyn DeviceHandler>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("Y_VB_F___W.B32QEUK", "Y_VB_F___W.B32QEUK", "2.11.224");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = create(ha.clone(), thinq.clone(), meta);
        thinq.emit_data(&hex_decode(frame));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }

    #[test]
    fn decodes_wash_dry_courses_detergent_empty_and_temperature_logs() {
        let (ha, thinq, _) = make(SAMPLE_STANDBY);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "ezdispense_detergent").as_deref(), Some("50"));
        assert_eq!(prop(&ha, "ezdispense_softener").as_deref(), Some("45"));

        thinq.emit_data(&hex_decode(SAMPLE_COTTON));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Off"));

        thinq.emit_data(&hex_decode(SAMPLE_WASH_DRY_AUTO));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Wash+Dry (6kg)"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));
        thinq.emit_data(&hex_decode(SAMPLE_WASH_DRY_1H));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("01:00"));
        thinq.emit_data(&hex_decode(SAMPLE_WASH_DRY_IRON));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Iron"));
        thinq.emit_data(&hex_decode(SAMPLE_WASH_DRY_DELICATE));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Delicate"));
        thinq.emit_data(&hex_decode(DIAL_DRY_ONLY));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Dry Only"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));

        thinq.emit_data(&hex_decode(STATUS_DETERGENT_EMPTY));
        assert_eq!(prop(&ha, "detergent_empty").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(STATUS_DETERGENT_REFILLED));
        assert_eq!(prop(&ha, "detergent_empty").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(LOG_HEATING));
        assert_eq!(prop(&ha, "tub_temperature").as_deref(), Some("52"));
        assert_eq!(prop(&ha, "drying_temperature").as_deref(), Some("31"));

        thinq.emit_data(&hex_decode(LOG_DRYING));
        assert_eq!(prop(&ha, "tub_temperature").as_deref(), Some("61"));
        assert_eq!(prop(&ha, "drying_temperature").as_deref(), Some("94"));
    }

    #[test]
    fn stages_course_defaults_gates_panel_values_and_guards_power_on() {
        let (ha, thinq, dev) = make(SAMPLE_STANDBY);
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert!(comps["pause"].get("command_topic").is_none());
        assert!(comps["start"].get("command_topic").is_none());
        assert!(!comps["set_spin"]["options"]
            .as_array()
            .unwrap()
            .contains(&json!("1600 rpm")));

        // Power ON while already on sends nothing; OFF sends F024010100.
        dev.set_property("power", "ON");
        assert!(thinq.outbox().is_empty());
        dev.set_property("power", "OFF");
        assert!(hex_encode(&thinq.outbox()[0]).contains("F024010100"));
        thinq.reset_recorder();

        // Start with no course selected is blocked.
        dev.set_property("run_programme", "");
        assert!(thinq.outbox().is_empty());

        // Choosing TurboWash 39 stages its defaults and builds the expected F026.
        dev.set_property("set_course", "TurboWash 39");
        assert_eq!(prop(&ha, "set_spin").as_deref(), Some("1200 rpm"));
        assert_eq!(prop(&ha, "set_temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "set_turbowash").as_deref(), Some("ON"));
        dev.set_property("run_programme", "");
        assert!(hex_encode(thinq.outbox().last().unwrap())
            .contains("F02631030904010000000001030000020200"));

        // Wash+Dry carries drying mode.
        dev.set_property("set_course", "Wash+Dry (6kg)");
        assert_eq!(prop(&ha, "set_drying_mode").as_deref(), Some("Auto"));
        dev.set_property("set_drying_mode", "01:00");
        dev.set_property("run_programme", "");
        assert!(hex_encode(thinq.outbox().last().unwrap())
            .contains("F02613030A04010400000000030000020200"));

        // Dry Only sends zeros and no dosing.
        dev.set_property("set_course", "Dry Only");
        dev.set_property("run_programme", "");
        assert!(hex_encode(thinq.outbox().last().unwrap())
            .contains("F02618000000000200000000030000000000"));

        // Out-of-panel selections are dropped.
        dev.set_property("set_course", "Mixed Fabric");
        dev.set_property("set_spin", "1600 rpm");
        dev.set_property("set_temp", "50 °C");
        assert_eq!(prop(&ha, "set_spin").as_deref(), Some("1000 rpm"));
        assert_eq!(prop(&ha, "set_temp").as_deref(), Some("40 °C"));
    }
}
