//! RH90V9_WW — LG RC10V9AV2W heat-pump dryer.
//!
//! Uses `rc90u2_ww`'s records and writes byte for byte, with two extra courses
//! (`Cotton+` and `Tub Clean`) that `RC90U2_WW` does not offer. Withdraws the
//! entities from the earlier standalone `rh90v9_ww` handler.
//!
//! Port of upstream master `8e3db5d`.

use super::rc90u2_ww::Device;
use crate::device_trait::DeviceHandler;
use crate::timers::ThreadTimers;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use std::sync::Arc;

const EXTRA_COURSES: &[(u8, &str)] = &[
    (0x19, "Cotton+"),
    (0x13, "Tub Clean"), // the Drum Care button
];

/// Entities the earlier RH90V9_WW handler published that this handler does not have.
const TOMBSTONES: &[(&str, &str)] = &[
    ("cycle", "sensor"),
    ("reservation", "sensor"),
    ("pause", "button"),
    ("ping", "button"),
    ("run_state", "sensor"),
    ("process_state", "sensor"),
    ("run_completed", "binary_sensor"),
    ("cycle_start_time", "sensor"),
    ("cycle_end_time", "sensor"),
    ("cycle_duration", "sensor"),
    ("downloaded_cycle_id", "sensor"),
];

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::with_variant(
        ha,
        thinq,
        meta,
        Arc::new(ThreadTimers),
        EXTRA_COURSES,
        TOMBSTONES,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ManualTimers;
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device, PropertyValue};
    use serde_json::json;

    const DEVICE_ID: &str = "test-id";

    // Captures from a bridged RC10V9AV2W.
    const EB_READY: &str =
        "AA2130EB0019010000000000000000000000000000080000000000000073002EBB";
    const SPORTS_WEAR: &str = "AA3C30EC001901003000000A0000010200000000000800000000000000730000190100320000080000010200000000000820000000000000730093BB";
    const DOWNLOADED_SHOES: &str = "AA3C30EC001901030000000C00000102000000000008000000000000007300001901030000000C0000010200000000000800000000730000730096BB";
    const COTTON_PLUS: &str = "AA3C30EC001901022D00000700030302000000000008000000000000007300001901022D00001900030102000000000008000000000000007300EDBB";
    const TUB_CLEAN: &str = "AA3C30EC001901030000000C000001020000000008080000000000000073000019010300000013000003030000000008080000000000000073003FBB";
    const DELAY_END_ON: &str = "AA3C30EC0019010300000013000003030000000000080000000000000073000019010300000013000003030300030001080000000000000073003EBB";
    const START: &str = "AA3C30EC001901030000001300000303000000000008000000000000007300001902030003001300000303000000000008500000010000007300ECBB";
    const PAUSE: &str = "AA3C30EC0019020300030013000003030000000000085000000100000073000019030300030013000003030000000008080000000200000073009DBB";
    const POWER_OFF: &str = "AA3C30EC0019030300030013000003030000000008080000000200000073000019000000000000000000000000000000080000000300000073000CBB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("RH90V9_WW", "RH90V9_WW", "2.10.123");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = Device::with_variant(
            ha.clone(),
            thinq.clone(),
            meta,
            Arc::new(ManualTimers::default()),
            EXTRA_COURSES,
            TOMBSTONES,
        );
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> PropertyValue {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .get(name)
            .cloned()
            .unwrap()
    }

    #[test]
    fn knob_delay_end_start_pause_and_power_off() {
        let (ha, thinq, _dev) = make();
        thinq.emit_data(&hex_decode(EB_READY));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Standby".into());
        assert_eq!(prop(&ha, "course"), "Cotton".into());

        thinq.emit_data(&hex_decode(SPORTS_WEAR));
        assert_eq!(prop(&ha, "course"), "Sports Wear".into());
        assert_eq!(prop(&ha, "dry_level"), "Not selected".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Eco".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(50));

        thinq.emit_data(&hex_decode(DOWNLOADED_SHOES));
        assert_eq!(prop(&ha, "course"), "Shoes/Fabric Doll".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(180));

        thinq.emit_data(&hex_decode(COTTON_PLUS));
        assert_eq!(prop(&ha, "course"), "Cotton+".into());
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Eco".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(165));

        thinq.emit_data(&hex_decode(TUB_CLEAN));
        assert_eq!(prop(&ha, "course"), "Tub Clean".into());
        assert_eq!(prop(&ha, "dry_level"), "Not selected".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Turbo".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(180));

        thinq.emit_data(&hex_decode(DELAY_END_ON));
        assert_eq!(prop(&ha, "delay"), "ON".into());
        assert_eq!(prop(&ha, "reserve_time"), PropertyValue::Int(180));
        assert_eq!(prop(&ha, "anti_crease"), "No".into());

        thinq.emit_data(&hex_decode(START));
        assert_eq!(prop(&ha, "status"), "Drying".into());
        assert_eq!(prop(&ha, "phase"), "Drying".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(180));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(180));
        assert_eq!(prop(&ha, "delay"), "OFF".into());

        thinq.emit_data(&hex_decode(PAUSE));
        assert_eq!(prop(&ha, "status"), "Paused".into());
        assert_eq!(prop(&ha, "phase"), "Drying".into());
        assert_eq!(prop(&ha, "error"), "OFF".into());

        thinq.emit_data(&hex_decode(POWER_OFF));
        assert_eq!(prop(&ha, "power"), "OFF".into());
        assert_eq!(prop(&ha, "status"), "Off".into());
        assert_eq!(prop(&ha, "course"), "Tub Clean".into());
    }

    #[test]
    fn course_options_include_extra_courses_and_withdraw_earlier_entities() {
        let (ha, _thinq, _dev) = make();
        let components = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        let courses = components["course"]["options"].as_array().unwrap();
        assert!(courses.contains(&json!("Cotton+")));
        assert!(courses.contains(&json!("Tub Clean")));

        let mut tombstones: Vec<_> = components
            .iter()
            .filter(|(_, c)| c.as_object().is_some_and(|o| o.len() == 1))
            .map(|(k, c)| (k.as_str(), c["platform"].as_str().unwrap()))
            .collect();
        tombstones.sort();
        let mut expected = TOMBSTONES.to_vec();
        expected.sort();
        assert_eq!(tombstones, expected);

        for key in [
            "power",
            "remote_start",
            "dry_level",
            "eco_hybrid",
            "anti_crease",
            "start",
            "remaining_time",
            "initial_time",
            "error",
            "error_message",
        ] {
            assert!(components[key].get("unique_id").is_some(), "{key}");
        }
    }
}
