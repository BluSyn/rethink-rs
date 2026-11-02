//! Shared helpers for in-module device tests (`#[cfg(test)]`).

use rethink_core::{
    wrap_aabb, HaConnection, MockHaConnection, MockThinq1Device, MockThinq2Device, Metadata,
    Thinq1Device, Thinq2Device,
};
use std::sync::Arc;

pub const DEVICE_ID: &str = "test-id";

pub fn meta(model_id: &str) -> Metadata {
    Metadata::new(model_id, model_id, "1.0")
}

pub fn make_t2<D>(
    model_id: &str,
    new: impl Fn(Arc<dyn HaConnection>, Arc<dyn Thinq2Device>, Metadata) -> Arc<D>,
) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<D>) {
    let ha = MockHaConnection::new();
    let m = meta(model_id);
    let thinq = MockThinq2Device::new(DEVICE_ID, m.clone());
    let ha_dyn: Arc<dyn HaConnection> = ha.clone();
    let tq_dyn: Arc<dyn Thinq2Device> = thinq.clone();
    let dev = new(ha_dyn, tq_dyn, m);
    (ha, thinq, dev)
}

pub fn make_t1<D>(
    model_id: &str,
    new: impl Fn(Arc<dyn HaConnection>, Arc<dyn Thinq1Device>, Metadata) -> Arc<D>,
) -> (Arc<MockHaConnection>, Arc<MockThinq1Device>, Arc<D>) {
    let ha = MockHaConnection::new();
    let m = meta(model_id);
    let thinq = MockThinq1Device::new(DEVICE_ID, m.clone());
    let ha_dyn: Arc<dyn HaConnection> = ha.clone();
    let tq_dyn: Arc<dyn Thinq1Device> = thinq.clone();
    let dev = new(ha_dyn, tq_dyn, m);
    (ha, thinq, dev)
}

pub fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
    ha.device(DEVICE_ID)?
        .properties
        .get(name)
        .map(|p| p.as_string())
}

pub fn emit_inner(thinq: &MockThinq2Device, inner: &[u8]) {
    thinq.emit_data(&wrap_aabb(inner));
}

/// The `XX EB` single-record frame carrying the current record of an `XX EC` pair.
pub fn eb_from_ec(ec_frame: &[u8], record_len: usize) -> Vec<u8> {
    let inner = &ec_frame[2..ec_frame.len() - 2];
    assert_eq!(inner.len(), 2 + 2 * record_len, "not an EC pair");
    assert_eq!(inner[1], 0xec, "not an EC pair");
    let mut eb = vec![inner[0], 0xeb];
    eb.extend_from_slice(&inner[2 + record_len..]);
    wrap_aabb(&eb)
}

/// True if any AABB-wrapped outbox packet contains this inner payload.
pub fn outbox_has_inner(thinq: &MockThinq2Device, inner: &[u8]) -> bool {
    thinq
        .outbox()
        .iter()
        .any(|p| p.len() >= 4 + inner.len() && p[2..2 + inner.len()] == *inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::hex_decode;

    #[test]
    fn prop_none_without_device() {
        let ha = MockHaConnection::new();
        assert!(prop(&ha, "power").is_none());
    }

    #[test]
    fn outbox_has_inner_matches_wrapped_send() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta("X"));
        let core = rethink_core::device_base::AabbDeviceCore::new(ha, thinq.clone());
        let inner = hex_decode("F0ED1121010000001800");
        core.send(&inner);
        assert!(outbox_has_inner(&thinq, &inner));
        assert!(!outbox_has_inner(&thinq, &[0x00, 0x01]));
    }

    #[test]
    fn eb_from_ec_keeps_the_current_record() {
        let ec = wrap_aabb(&hex_decode("20EC0102AABB"));
        assert_eq!(eb_from_ec(&ec, 2), wrap_aabb(&hex_decode("20EBAABB")));
    }
}
