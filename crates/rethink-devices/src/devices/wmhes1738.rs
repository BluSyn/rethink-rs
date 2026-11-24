//! WMHES1738 — LG MHES1738F over-the-range microwave.
//!
//! Shares WMVEM1825's status records, door events, and hood commands, with five vent fan speeds
//! instead of two.
//!
//! Port of upstream PR #323.

use super::wmvem1825::Device;
use crate::device_trait::DeviceHandler;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use std::sync::Arc;

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::with_max_fan_speed(ha, thinq, meta, 5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    // Real captures from an LG MHES1738F microwave (firmware 20210927).
    const SAMPLE_INITIAL_OFF: &str = "AA3441EB003000015500000000000000FF030C000000000000000000000000000000C3000000005300008080808001000000E0BB";
    const SAMPLE_FAN_1_TO_3: &str = "AA6241EC003000015500000000000000FF030C000000000000000000000000000000C3000000005301008080808001000000003000015500000000000000FF030C000000000000000000000000000000C3000000005303008080808001000000C6BB";
    const SAMPLE_FAN_3_TO_5: &str = "AA6241EC003000015500000000000000FF030C000000000000000000000000000000C3000000005303008080808001000000003000015500000000000000FF030C000000000000000000000000000000C3000000005305008080808001000000C2BB";
    const SAMPLE_LIGHT_HIGH: &str = "AA6241EC003000015500000000000000FF030C000000000000000000000000000000C3000000005300008080808001000000003000015500000000000000FF030C000000000000000000000000000000C3000000005320008080808001000000FABB";
    const SAMPLE_COOKING_20S_P5: &str = "AA6241EC073000004001000000000000FF030C000000000000000000000000000000C3000100005300008080808001000000023000004401000000000500FF030C000014000014000000000000000000C7000100005300018080808001000000F3BB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<dyn DeviceHandler>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("WMHES1738", "WMHES1738", "20210927");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = create(ha.clone(), thinq.clone(), meta);
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }

    #[test]
    fn advertises_five_fan_speeds_and_decodes_captures() {
        let (ha, thinq, dev) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fan_power"]["speed_range_max"], 5);

        thinq.emit_data(&hex_decode(SAMPLE_INITIAL_OFF));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("0"));

        thinq.emit_data(&hex_decode(SAMPLE_FAN_1_TO_3));
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("3"));

        thinq.emit_data(&hex_decode(SAMPLE_FAN_3_TO_5));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("5"));

        thinq.emit_data(&hex_decode(SAMPLE_LIGHT_HIGH));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("2"));

        thinq.emit_data(&hex_decode(SAMPLE_COOKING_20S_P5));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Cooking"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("20"));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("50"));

        // Fan speed writes clamp at 5 (not 2).
        thinq.reset_recorder();
        dev.set_property("fan_speed", "5");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220401050000808042BB"
        );
        thinq.reset_recorder();
        dev.set_property("fan_speed", "9");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220401050000808042BB"
        );
    }
}
