//! Y_V8_Y___W.B_2QEUK — LG F2WR709S2W washer.
//!
//! A complete Cotton cycle confirmed the Y_V8_Y___W.B32QEUK telemetry and core
//! command layout, so this is that handler with fewer controls. Pause worked only
//! while Remote Start was armed and the washer rejected every Start/resume; Delay
//! End and the wash-option switches were not validated. Those entities are
//! withdrawn from discovery and their commands are ignored.
//!
//! Port of upstream PR #265.

use crate::device_trait::DeviceHandler;
use crate::devices::y_v8_y___w_b32qeuk::{dial_courses, Device as V8, Extension};
use rethink_core::device_base::AabbDeviceCore;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

/// Withheld controls and their platforms. HA removes a component whose discovery
/// carries only `platform`.
const WITHHELD: &[(&str, &str)] = &[
    ("start", "button"),
    ("pause", "button"),
    ("set_delay_end", "number"),
    ("set_steam", "switch"),
    ("set_prewash", "switch"),
    ("set_turbowash", "switch"),
    ("set_crease_care", "switch"),
    ("set_steam_softener", "switch"),
    ("set_medic_rinse", "switch"),
];

fn extension(_core: Arc<AabbDeviceCore>) -> Extension {
    Extension {
        courses: dial_courses(),
        extra_courses: &[],
        extra_components: WITHHELD
            .iter()
            .map(|(key, platform)| (key.to_string(), json!({ "platform": platform })))
            .collect(),
        after_state: None,
        patch_command: None,
        handle_set: Some(Box::new(|_, prop, _| {
            WITHHELD.iter().any(|(key, _)| *key == prop)
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    const MODEL_ID: &str = "Y_V8_Y___W.B_2QEUK";

    // Captured locally from an LG F2WR709S2W on 2026-10-24 while powered on and idle.
    // The physical panel showed no programme selected and the door and child locks were off.
    const SAMPLE_IDLE: &str = "AAFF200A0039009D73000100EB0027000001000000000000000000000000000000000000000002006400000000000000000000000000EF0FBB";
    // Captured from the same washer on 2026-10-25 after starting Cotton / Cold /
    // No spin / Normal rinse / Normal soil through rethink. The physical appliance
    // first measured the empty load, then entered Washing with a 39-minute estimate.
    const SAMPLE_MEASURING: &str = "AAFF200A006000D4FA000100EC004E0000010112011200000000000000000000000000010000030064000000000000000000000000000000040112011201000301010100000000002000010100030064000000000000000000000000004B97BB";
    const SAMPLE_WASHING: &str = "AAFF200A006000D50B000100EC004E000004011201120100030101010000000040200001010003006400000000000000000000000000000006002700270100030101010000000040200001040003006400000000000000000000000000C3C0BB";
    // Guarded empty-cycle testing on 2026-10-25. These are the exact transitions
    // around a physical pause, a physical resume and a remote pause made while the
    // appliance still reported Remote Start armed.
    const SAMPLE_PHYSICAL_PAUSE: &str = "AAFF200A006000DFA7000100EC004E000006011201120100030101010000000040200001040003006400000000000000000000000000000002011201120100030101010000000040000001060003006400000400000000000000000000BB64BB";
    const SAMPLE_PHYSICAL_RESUME: &str = "AAFF200A006000DFBE000100EC004E000002011201120100030101010000000040000001060003006400000400000000000000000000000006011201120100030101010000000002200001020003006400000400000000000000000000DA3BBB";
    const SAMPLE_RESUME_DOOR_LOCK: &str = "AAFF200A006000DFC0000100EC004E00000601120112010003010101000000000220000102000300640000040000000000000000000000000601120112010003010101000000004220000102000300640000040000000000000000000009BABB";
    const SAMPLE_REMOTE_PAUSE_ARMED: &str = "AAFF200A006000DFC8000100EC004E000006011101120100030101010000000042200001020003006400000400000000000000000000000002011101120100030101010000000042000001060003006400000400000000000000000000C90CBB";
    // Appliance-to-cloud event markers around the guarded control session. C9
    // coincided with physical resume; C8 appeared in the later power-off sequence.
    // They must not be mistaken for commands that can be sent the other way.
    const EVENT_MARKER_C8: &str = "AA09207200C80058BB";
    const PHYSICAL_RESUME_MARKER: &str = "AA09207200C9005BBB";
    // Immediate appliance responses to the inherited Pause and Start packets. 00
    // accompanied a working armed Pause; FF accompanied every rejected Start/resume.
    const COMMAND_ACCEPTED: &str = "AA0820002400A3BB";
    const COMMAND_REJECTED: &str = "AA08200024FFA0BB";

    fn meta() -> Metadata {
        Metadata::new(MODEL_ID, MODEL_ID, "2.11.207")
    }

    fn make() -> (
        Arc<MockHaConnection>,
        Arc<MockThinq2Device>,
        Arc<dyn DeviceHandler>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = create(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?
            .properties
            .get(name)
            .map(|p| p.as_string())
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    fn last_packet(thinq: &MockThinq2Device) -> String {
        hex_encode(thinq.outbox().last().unwrap())
    }

    #[test]
    fn exposes_validated_core_controls_and_withholds_unconfirmed_controls() {
        let (ha, _, _) = make();
        let components = ha.device(DEVICE_ID).unwrap().config.unwrap().components;

        for key in [
            "power",
            "set_course",
            "set_temp",
            "set_spin",
            "set_rinse",
            "set_soil",
            "run_programme",
        ] {
            let topic = components[key]["command_topic"].as_str();
            assert!(
                topic.is_some_and(|t| !t.is_empty()),
                "{key} has a command topic"
            );
        }

        for (key, platform) in [
            ("start", "button"),
            ("pause", "button"),
            ("set_delay_end", "number"),
            ("set_steam", "switch"),
            ("set_prewash", "switch"),
            ("set_turbowash", "switch"),
            ("set_crease_care", "switch"),
            ("set_steam_softener", "switch"),
            ("set_medic_rinse", "switch"),
        ] {
            let keys: Vec<&String> = components[key].as_object().unwrap().keys().collect();
            assert_eq!(keys, ["platform"], "{key} is a removal tombstone only");
            assert_eq!(components[key]["platform"], platform, "{key}");
        }
    }

    #[test]
    fn real_powered_on_idle_frame_matches_the_physical_panel() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_IDLE);

        expect(&ha, "power", "ON");
        expect(&ha, "status", "Standby");
        expect(&ha, "course", "Not selected");
        expect(&ha, "temp", "Not selected");
        expect(&ha, "spin", "Not selected");
        expect(&ha, "cycles", "2");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "error", "OFF");
        expect(&ha, "error_message", "OK");
        // This protocol uses the inverted lock convention: ON means unlocked.
        expect(&ha, "door_lock", "ON");
        expect(&ha, "child_lock", "ON");
        expect(&ha, "remote_start", "OFF");
    }

    #[test]
    fn real_empty_cycle_frames_decode_detecting_then_washing_with_the_selected_programme() {
        let (ha, thinq, _) = make();

        emit(&thinq, SAMPLE_MEASURING);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Detecting");
        expect(&ha, "course", "Cotton");
        expect(&ha, "temp", "Cold");
        expect(&ha, "spin", "No spin");
        expect(&ha, "rinse", "Normal");
        expect(&ha, "soil", "Normal");

        emit(&thinq, SAMPLE_WASHING);
        expect(&ha, "status", "Washing");
        expect(&ha, "initial_time", "39");
        expect(&ha, "remaining_time", "39");
        expect(&ha, "error_message", "OK");
    }

    #[test]
    fn real_physical_pause_and_resume_frames_preserve_the_arming_and_lock_transitions() {
        let (ha, thinq, _) = make();

        emit(&thinq, SAMPLE_PHYSICAL_PAUSE);
        expect(&ha, "status", "Paused");
        expect(&ha, "remaining_time", "78");
        expect(&ha, "remote_start", "OFF");

        emit(&thinq, SAMPLE_PHYSICAL_RESUME);
        expect(&ha, "status", "Washing");
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "ON"); // transiently unlocked

        emit(&thinq, SAMPLE_RESUME_DOOR_LOCK);
        expect(&ha, "status", "Washing");
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "OFF"); // locked once motion/water resumes
    }

    #[test]
    fn real_armed_remote_pause_transitions_to_paused_while_remote_start_remains_armed() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_REMOTE_PAUSE_ARMED);

        expect(&ha, "status", "Paused");
        expect(&ha, "remaining_time", "77");
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "OFF");
    }

    #[test]
    fn physical_action_markers_are_acknowledged_but_do_not_change_decoded_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING);
        let before = ha.device(DEVICE_ID).unwrap().properties;
        thinq.reset_recorder();

        emit(&thinq, EVENT_MARKER_C8);
        emit(&thinq, PHYSICAL_RESUME_MARKER);
        assert_eq!(ha.device(DEVICE_ID).unwrap().properties, before);
        let acks: Vec<String> = thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(acks, ["AA08F00072044DBB", "AA08F00072044DBB"]);
    }

    #[test]
    fn observed_command_response_records_do_not_mutate_appliance_properties() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING);
        let before = ha.device(DEVICE_ID).unwrap().properties;

        emit(&thinq, COMMAND_ACCEPTED);
        emit(&thinq, COMMAND_REJECTED);
        assert_eq!(ha.device(DEVICE_ID).unwrap().properties, before);
    }

    #[test]
    fn encodes_the_commands_accepted_by_the_real_appliance() {
        let (_, thinq, dev) = make();

        dev.set_property("power", "ON");
        assert_eq!(last_packet(&thinq), "AA08F02A010098BB");

        dev.set_property("set_course", "Cotton");
        dev.set_property("set_soil", "Normal");
        dev.set_property("set_spin", "No spin");
        dev.set_property("set_temp", "Cold");
        dev.set_property("set_rinse", "Normal");
        dev.set_property("run_programme", "");
        assert_eq!(
            last_packet(&thinq),
            "AA16F02601030101010000000000030000000000B5BB"
        );

        dev.set_property("power", "OFF");
        assert_eq!(last_packet(&thinq), "AA09F0240101009CBB");
    }

    #[test]
    fn withheld_controls_ignore_their_commands() {
        let (ha, thinq, dev) = make();
        let before = ha.device(DEVICE_ID).unwrap().properties;
        for (key, value) in [
            ("start", ""),
            ("start", "F024050100"),
            ("pause", ""),
            ("set_delay_end", "5"),
            ("set_steam", "ON"),
            ("set_prewash", "ON"),
            ("set_turbowash", "ON"),
            ("set_crease_care", "ON"),
            ("set_steam_softener", "ON"),
            ("set_medic_rinse", "ON"),
        ] {
            dev.set_property(key, value);
        }
        assert!(thinq.outbox().is_empty());
        assert_eq!(ha.device(DEVICE_ID).unwrap().properties, before);

        dev.set_property("set_course", "Cotton");
        dev.set_property("set_soil", "Normal");
        dev.set_property("set_spin", "No spin");
        dev.set_property("set_temp", "Cold");
        dev.set_property("set_rinse", "Normal");
        dev.set_property("run_programme", "");
        // Neither the delay nor the option bits reach the programme frame.
        assert_eq!(
            last_packet(&thinq),
            "AA16F02601030101010000000000030000000000B5BB"
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory(MODEL_ID).is_some());
    }
}
