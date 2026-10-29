//! Y_VB_Y___W.B32QEUK washer (LG F4W1175YW), the ezDispense sibling of Y_V8.
//!
//! Status uses the Y_V8 39-byte current record. Dose levels, millilitre
//! amounts, the download slot, and the drawer bit are this model's fields.
//! A programme start repeats the last reported dose levels. `delay_end` is
//! withdrawn; reserve time already carries the minutes.

use crate::device_trait::DeviceHandler;
use crate::devices::y_v8_y___w_b32qeuk::{smart_course_name, Device as V8, Extension};
use crate::washer_common::DOSES;
use rethink_core::device_base::AabbDeviceCore;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const EXTRA_COURSES: &[(u8, &str)] = &[(0x37, "Dispenser Clean")];

fn dose_name(code: u8) -> &'static str {
    DOSES.get(code as usize).copied().unwrap_or("None")
}

fn parse_ez(value: &str) -> Option<u8> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let ml = value.parse::<i64>().ok()?;
    if (9..=150).contains(&ml) {
        Some(ml as u8)
    } else {
        None
    }
}

fn extension(_core: Arc<AabbDeviceCore>) -> Extension {
    let dose = Arc::new(Mutex::new([0u8; 3]));
    let dose_state = dose.clone();
    let dose_cmd = dose.clone();
    Extension {
        courses: crate::devices::y_v8_y___w_b32qeuk::dial_courses(),
        extra_courses: EXTRA_COURSES,
        extra_components: vec![
            (
                "downloaded_program".into(),
                json!({"platform":"sensor","unique_id":"$deviceid-downloaded_program","state_topic":"$this/downloaded_program","name":"Downloaded program","icon":"mdi:download"}),
            ),
            (
                "detergent".into(),
                json!({"platform":"sensor","unique_id":"$deviceid-detergent","state_topic":"$this/detergent","name":"Detergent dose","icon":"mdi:cup","device_class":"enum","options":DOSES}),
            ),
            (
                "softener".into(),
                json!({"platform":"sensor","unique_id":"$deviceid-softener","state_topic":"$this/softener","name":"Softener dose","icon":"mdi:cup-outline","device_class":"enum","options":DOSES}),
            ),
            (
                "ezdispense_detergent".into(),
                json!({"platform":"number","unique_id":"$deviceid-ezdispense_detergent","state_topic":"$this/ezdispense_detergent","command_topic":"$this/ezdispense_detergent/set","name":"ezDispense detergent amount","icon":"mdi:cup","unit_of_measurement":"mL","min":9,"max":150,"step":1,"mode":"box","entity_category":"config"}),
            ),
            (
                "ezdispense_softener".into(),
                json!({"platform":"number","unique_id":"$deviceid-ezdispense_softener","state_topic":"$this/ezdispense_softener","command_topic":"$this/ezdispense_softener/set","name":"ezDispense softener amount","icon":"mdi:cup-outline","unit_of_measurement":"mL","min":9,"max":150,"step":1,"mode":"box","entity_category":"config"}),
            ),
            (
                "dispenser_drawer".into(),
                json!({"platform":"binary_sensor","unique_id":"$deviceid-dispenser_drawer","state_topic":"$this/dispenser_drawer","name":"Dispenser drawer","device_class":"opening"}),
            ),
            ("delay_end".into(), json!({"platform":"sensor"})),
        ],
        after_state: Some(Box::new(move |core, s| {
            if s.len() < 38 {
                return;
            }
            core.publish_property("downloaded_program", smart_course_name(s[25]).into());
            core.publish_property("detergent", dose_name(s[32]).into());
            core.publish_property("softener", dose_name(s[33]).into());
            core.publish_property("ezdispense_detergent", i64::from(s[34]).into());
            core.publish_property("ezdispense_softener", i64::from(s[35]).into());
            core.publish_on_off("dispenser_drawer", s[37] & 0x04 != 0);
            *dose_state.lock() = [s[32], s[33], s[36]];
        })),
        patch_command: Some(Box::new(move |cmd| {
            let d = dose_cmd.lock();
            cmd[15] = d[0];
            cmd[16] = d[1];
            cmd[17] = d[2];
        })),
        handle_set: Some(Box::new(move |core, prop, value| {
            let parameter = match prop {
                "ezdispense_detergent" => 0x0d,
                "ezdispense_softener" => 0x0e,
                _ => return false,
            };
            if let Some(ml) = parse_ez(value) {
                core.send(&[0xf0, 0x24, parameter, 0x01, ml]);
            }
            true
        })),
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    V8::with_extension(ha, thinq, meta, extension)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_trait::DeviceHandler;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const ID: &str = "test-id";
    const INITIAL: &str = "aaff200a006000f52c000100ec004e00000100000000000000000000000000000000000000001f006400000000000002022a1e00800100000100000000000000000000000000000000000000001f006400000000000002022a1e0000010ee4bb";
    const POWER_OFF: &str = "aaff200a006000f531000100ec004e00000100000000000000000000000000000000000000001f006400000000000002022a1e00000100000000000000000000000000000000000000000001001f006400000000000002022a1e000001605cbb";
    const MEASURING: &str = "aaff200a00600008b5000100ec004e000001000000000000000000000000000000000000000020006400000000012402022a1e004001000004010c010c1200030106010000000042000001010020006400000200000000002a1e0000019db0bb";
    const RUNNING: &str = "aaff200a006000f98f000100ec004e0000060219021b0400030a0401000000004220000104001f006400000200000103012a1e0040010000060219021b0400030a0401000000004220000104001f006400000200000103012a1e00000164afbb";
    const RINSING: &str = "aaff200a00600002da000100ec004e0000070022021b0400030a0001000000004200000106001f00640000020000b903012a1e0000010000070022021b0400030a0001000000004200000106001f00640000020000ba03012a1e000001ce60bb";
    const SPINNING: &str = "aaff200a006000cc44000100ec004e000007001d021b0400030a0001000000004200000106001100640000020000c102002a1e004001000008001c021b0400000a0000000000004200000107001100640000020000c202002a1e000001dc6cbb";
    const SINGLE: &str = "aaff200a003900c2a6000100eb00270000060216021b0400030a04010000000042200001040011006400000200000602002a1e00400189a4bb";
    const UNKNOWN: &str = "aaff200a003900ce8600010ae20027000004032603260400030a04010000000002200001010011006400000200011602002a1e000001dbabbb";
    const DUVET: &str = "aaff200a006000e811000100ec004e000001010c010c1200030106010000000000000003000005003a00000200000000002a1e000001000001011c011c0500030201010000000000000001003a05003a00000300000002022a1e000001990cbb";
    const EZ_42_30: &str = "aaff200a0060000400000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e004001000001000000000000000000000000000000000000000005003400000000000002022a1e00800151adbb";
    const EZ_43_30: &str = "aaff200a0060000440000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e000001000001000000000000000000000000000000000000000005003400000000000002022b1e0000016e6bbb";
    const EZ_43_31: &str = "aaff200a006000045a000100ec004e000001000000000000000000000000000000000000000005003400000000000002022b1e000001000001000000000000000000000000000000000000000005003400000000000002022b1f000001fda5bb";
    const DRAWER_OPEN: &str = "aaff200a0060004cd1000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e000001000001000000000000000000000000000000000000000005003400000000000002022a1e0004014a1dbb";
    const DRAWER_PULSE_ON: &str = "aaff200a0060004cd4000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e000401000001000000000000000000000000000000000000000005003400000000000002022a1e004401bd20bb";
    const DRAWER_PULSE_OFF: &str = "aaff200a0060004cff000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e004401000001000000000000000000000000000000000000000005003400000000000002022a1e00040183e8bb";
    const DRAWER_CLOSED: &str = "aaff200a0060004d02000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e000401000001000000000000000000000000000000000000000005003400000000000002022a1e000001c8b2bb";
    const COTTON_DOSED: &str = "aaff200a0039040081000100eb0027000001032603260100000000000000000002000003000011007100000000000003010000000000557abb";

    fn meta() -> Metadata {
        Metadata::new("Y_VB_Y___W.B32QEUK", "Y_VB_Y___W.B32QEUK", "2.11.207")
    }

    fn feed(
        frames: &[&str],
    ) -> (
        Arc<MockHaConnection>,
        Arc<MockThinq2Device>,
        Arc<dyn DeviceHandler>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = create(ha.clone(), thinq.clone(), meta());
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    #[test]
    fn initial_state_includes_the_download_slot_and_doses() {
        let (ha, _, _) = feed(&[INITIAL]);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("31"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "reserve_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(
            prop(&ha, "downloaded_program").as_deref(),
            Some("Rinse + Spin")
        );
        let info = ha.device(ID).unwrap();
        assert_eq!(
            info.config.as_ref().unwrap().components["delay_end"],
            json!({"platform": "sensor"})
        );
    }

    #[test]
    fn downloaded_program_cycle_and_drawer_follow_the_current_record() {
        let (ha, _, _) = feed(&[DUVET]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Duvet"));
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("Blanket"));
        assert_eq!(prop(&ha, "downloaded_program").as_deref(), Some("Blanket"));
        let mut frame = hex_decode(DUVET);
        frame[76] = 0x86;
        frame[79] = 0x86;
        let (ha, thinq, _) = feed(&[]);
        thinq.emit_data(&frame);
        assert_eq!(
            prop(&ha, "smart_course").as_deref(),
            Some("MicroPlastic Care")
        );
        assert_eq!(
            prop(&ha, "downloaded_program").as_deref(),
            Some("MicroPlastic Care")
        );
        assert_eq!(prop(&ha, "spin").as_deref(), Some("400 rpm"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("88"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Medium"));

        let (ha, thinq, _) = feed(&[]);
        for (frame, status) in [
            (MEASURING, "Detecting"),
            (RUNNING, "Washing"),
            (RINSING, "Rinsing"),
            (SPINNING, "Spinning"),
            (POWER_OFF, "Off"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            assert_eq!(prop(&ha, "status").as_deref(), Some(status));
        }

        let (ha, thinq, _) = feed(&[INITIAL]);
        assert_eq!(prop(&ha, "dispenser_drawer").as_deref(), Some("OFF"));
        for (frame, drawer) in [
            (DRAWER_OPEN, "ON"),
            (DRAWER_PULSE_ON, "ON"),
            (DRAWER_PULSE_OFF, "ON"),
            (DRAWER_CLOSED, "OFF"),
            (SINGLE, "OFF"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            assert_eq!(prop(&ha, "dispenser_drawer").as_deref(), Some(drawer));
        }
    }

    #[test]
    fn cycle_fields_and_ez_amounts_match_the_captures() {
        let (ha, _, _) = feed(&[POWER_OFF]);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));

        let (ha, _, _) = feed(&[MEASURING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Detecting"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Tub Clean"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("No spin"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("72"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("72"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("32"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Off"));

        let (ha, _, _) = feed(&[RUNNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Eco 40-60"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("147"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("145"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Low"));

        let (ha, _, _) = feed(&[RINSING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Eco 40-60"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("34"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("186"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("147"));

        let (ha, _, _) = feed(&[SPINNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Spinning"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Eco 40-60"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("147"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("28"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("194"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("17"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Off"));

        let (ha, _, _) = feed(&[SINGLE]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Eco 40-60"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("142"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("6"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Off"));

        let (ha, thinq, _) = feed(&[]);
        thinq.emit_data(&hex_decode(UNKNOWN));
        thinq.emit_data(&hex_decode("001122"));
        assert!(prop(&ha, "power").is_none());

        let (ha, thinq, _) = feed(&[]);
        for (frame, det, soft) in [
            (EZ_42_30, "42", "30"),
            (EZ_43_30, "43", "30"),
            (EZ_43_31, "43", "31"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            assert_eq!(prop(&ha, "ezdispense_detergent").as_deref(), Some(det));
            assert_eq!(prop(&ha, "ezdispense_softener").as_deref(), Some(soft));
            assert_eq!(prop(&ha, "detergent").as_deref(), Some("Medium"));
            assert_eq!(prop(&ha, "softener").as_deref(), Some("Medium"));
        }
    }

    #[test]
    fn ez_writes_and_a_dosed_programme_match_the_app() {
        let (_, thinq, dev) = feed(&[]);
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1121010000001800B5BB"
        );
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA08F02A010098BB");
        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F0240101009CBB");
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F02404010099BB");
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F02405010098BB");

        thinq.reset_recorder();
        dev.set_property("ezdispense_detergent", "43");
        dev.set_property("ezdispense_softener", "31");
        assert_eq!(
            thinq
                .outbox()
                .iter()
                .map(|p| hex_encode(p))
                .collect::<Vec<_>>(),
            vec![
                "AA09F0240D012B55BB".to_string(),
                "AA09F0240E011FA0BB".to_string()
            ]
        );
        thinq.reset_recorder();
        dev.set_property("ezdispense_detergent", "9");
        dev.set_property("ezdispense_softener", "150");
        let inners: Vec<Vec<u8>> = thinq
            .outbox()
            .iter()
            .map(|p| p[2..p.len() - 2].to_vec())
            .collect();
        assert_eq!(
            inners,
            vec![
                vec![0xf0, 0x24, 0x0d, 0x01, 9],
                vec![0xf0, 0x24, 0x0e, 0x01, 150]
            ]
        );
        thinq.reset_recorder();
        for value in ["", " ", "0", "8", "151", "256", "42.5", "-1", "abc"] {
            dev.set_property("ezdispense_detergent", value);
            dev.set_property("ezdispense_softener", value);
        }
        assert!(thinq.outbox().is_empty());
        dev.set_property("does-not-exist", "whatever");
        assert!(thinq.outbox().is_empty());

        let mut clean = hex_decode(COTTON_DOSED);
        clean[2 + 13 + 7] = 0x37;
        let (ha, thinq, _) = feed(&[]);
        thinq.emit_data(&clean);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Dispenser Clean"));

        let (ha, thinq, dev) = feed(&[COTTON_DOSED]);
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Low"));
        thinq.reset_recorder();
        dev.set_property("set_course", "Cotton");
        dev.set_property("set_soil", "Normal");
        dev.set_property("set_spin", "Max");
        dev.set_property("set_temp", "40 °C");
        dev.set_property("set_rinse", "Rinse+");
        dev.set_property("run_programme", "");
        assert_eq!(
            hex_encode(thinq.outbox().last().unwrap()),
            "AA16F0260103FF04020000000000030000030100B3BB"
        );

        let mut typed = hex_decode(COTTON_DOSED);
        typed[2 + 13 + 36] = 2;
        let (_, thinq, dev) = feed(&[]);
        thinq.emit_data(&typed);
        thinq.reset_recorder();
        dev.set_property("run_programme", "");
        let packet = thinq.outbox();
        let inner = &packet.last().unwrap()[2..];
        assert_eq!(inner[15], 3);
        assert_eq!(inner[16], 1);
        assert_eq!(inner[17], 2);
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("Y_VB_Y___W.B32QEUK").is_some());
    }
}
