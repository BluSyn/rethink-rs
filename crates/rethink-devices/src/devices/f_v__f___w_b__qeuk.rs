//! F_V__F___W.B__QEUK — LG CV9011EC4 washer/dryer.
//!
//! Same 39-byte class `0x20` status record as `F_V__F___W.B_1QEUK`, except records also arrive
//! inside the `0x20 0x0a` envelope and dial code `0x04` is `Cotton+`.
//!
//! Port of upstream PR #341.

use super::f_v__f___w_b_1qeuk::Device;
use crate::device_trait::DeviceHandler;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use std::sync::Arc;

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::with_options(ha, thinq, meta, true, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::device_base::{unwrap_aabb, wrap_aabb};
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    const CAPTURE: &str = "AA5420EC002501040C040C0100030904010400000000010002000010006400000400000000000000000000002501030603060100030704010400000100010002000010006400000400000000000000000000FCBB";

    fn record_cotton_plus() -> Vec<u8> {
        let frame = hex_decode(CAPTURE);
        let inner = unwrap_aabb(&frame).unwrap();
        let mut rec = inner[2 + 39..].to_vec();
        rec[7] = 0x04;
        rec
    }

    fn bare(record: &[u8]) -> Vec<u8> {
        let mut body = vec![0x20, 0xeb];
        body.extend_from_slice(record);
        wrap_aabb(&body)
    }

    fn enveloped(record: &[u8]) -> Vec<u8> {
        let mut body = vec![
            0x20,
            0x0a,
            0,
            0,
            0,
            0,
            1,
            0,
            0,
            0,
            0xeb,
            0,
            record.len() as u8,
        ];
        body.extend_from_slice(record);
        wrap_aabb(&body)
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<dyn DeviceHandler>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("F_V__F___W.B__QEUK", "CV9011EC4", "1.0");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = create(ha.clone(), thinq.clone(), meta);
        (ha, thinq, dev)
    }

    #[test]
    fn course_0x04_is_cotton_plus_and_enveloped_matches_bare() {
        let rec = record_cotton_plus();
        let (ha_bare, thinq_bare, _) = make();
        thinq_bare.emit_data(&bare(&rec));
        let (ha_env, thinq_env, _) = make();
        thinq_env.emit_data(&enveloped(&rec));

        let props_bare = ha_bare.device(DEVICE_ID).unwrap().properties;
        let props_env = ha_env.device(DEVICE_ID).unwrap().properties;
        assert_eq!(props_bare.get("course"), Some(&"Cotton+".into()));
        assert_eq!(props_env.get("status"), Some(&"Standby".into()));
        assert_eq!(props_env, props_bare);
    }
}
