//! WMVEF1337F — LG MVEF1337F over-the-range microwave/hood.
//!
//! Same protocol and offsets as `WMVEL2137`, with 53-byte status records instead of 46 and
//! `auto_ack` enabled so the `41 0a` content-sync envelope is acknowledged.
//!
//! Port of upstream PR #339.

use super::wmvel2137::Device;
use crate::device_trait::DeviceHandler;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use std::sync::Arc;

const RECORD_LEN: usize = 53;

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::with_options(ha, thinq, meta, RECORD_LEN, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";

    // Real frames from an LG MVEF1337F (53-byte records).
    const INITIAL_STATUS: &str = "AA3B41EB003100015500000000000000FF030C000000000000000000000000000000C300000000530000808080800100000000000000000000E8BB";
    const LAMP_LOW: &str = "AA7041EC003100015500000000000000FF030C000000000000000000000000000000C300000000530000808080800100000000000000000000003000015500000000000000FF030C000000000000000000000000000000C300000000531000808080800100000000000000000000FBBB";
    const VENT_4_LAMP_LOW: &str = "AA7041EC003000015500000000000000FF030C000000000000000000000000000000C300000000531300808080800100000000000000000000003000015500000000000000FF030C000000000000000000000000000000C30000000053140080808080010000000000000000000091BB";
    const COOK_START: &str = "AA7041EC003000015500000000000000FF030D000000000000000000000000000000C300000000530000808080800100000000000000000000023000004401000000000A00FF030D000100000100000000000000000000C700010000530001808080800100000000000000000000F7BB";
    const COOK_TICK: &str = "AA7041EC023000004401000000000A00FF030D000100000100000000000000000000C700010000530001808080800100000000000000000000023000004401000000000A00FF030D00003B000100000000000000000000C7000100005300018080808001000000000000000000008ABB";
    const COOK_PAUSED: &str = "AA7041EC023000004401000000000A00FF030D00003B000100000000000000000000C700010000530001808080800100000000000000000000043000004401000000000A00FF030D00001A000100000000000000000000C700010000530001808080800100000000000000000000AFBB";
    const COOK_CANCELLED: &str = "AA7041EC043000004401000000000A00FF030D00001A000100000000000000000000C700010000530001808080800100000000000000000000003000015500000000000000FF030D000000000000000000000000000000C300000000530000808080800100000000000000000000E8BB";
    const CONTENT_SYNC: &str = "AAFF410A003000000100010A85001E0004053330322D3100053330322D3200053330322D3300053330322D340088EABB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<dyn DeviceHandler>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("WMVEF1337F", "WMVEF1337F", "20240722");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = create(ha.clone(), thinq.clone(), meta);
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }

    #[test]
    fn decodes_53_byte_records_cook_cycle_and_acks_content_sync() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(CONTENT_SYNC));
        thinq.emit_data(&hex_decode(INITIAL_STATUS));
        thinq.emit_data(&hex_decode(LAMP_LOW));

        // Only the 41 0a content-sync envelope is acked by sequence; short status frames are not.
        let acks: Vec<_> = thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data)
            .collect();
        assert_eq!(
            acks,
            vec![serde_json::json!(hex_encode(&wrap_aabb(&hex_decode("f0000a040001"))))]
        );
        assert!(thinq.outbox().is_empty());

        assert_eq!(prop(&ha, "light_level").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(VENT_4_LAMP_LOW));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(COOK_START));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Cooking"));
        assert_eq!(prop(&ha, "cook_mode").as_deref(), Some("Microwave"));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("100"));
        assert_eq!(prop(&ha, "cook_time").as_deref(), Some("60"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("60"));

        thinq.emit_data(&hex_decode(COOK_TICK));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("59"));

        thinq.emit_data(&hex_decode(COOK_PAUSED));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Paused"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("26"));

        thinq.emit_data(&hex_decode(COOK_CANCELLED));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));

        // Vent speed 5 clamps to 4.
        dev.set_property("fan_speed", "5");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220401048080808043BB"
        );
    }

    #[test]
    fn wmvel2137_rejects_53_byte_records() {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("WMVEL2137", "WMVEL2137", "20240722");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let _dev = super::super::wmvel2137::Device::new(ha.clone(), thinq.clone(), meta);
        thinq.emit_data(&hex_decode(INITIAL_STATUS));
        thinq.emit_data(&hex_decode(LAMP_LOW));
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());
    }
}
