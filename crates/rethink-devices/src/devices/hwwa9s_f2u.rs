//! HWWA9S_F2U — LG CordZero A9K stick vacuum (A9K-CORE3S, deviceType 504).
//!
//! AABB class 0xD2 from the Wi-Fi module in the handle. Read-only. Status is a 12-byte record, one
//! byte per field in the modelJson's MonitoringValue order, behind its 16-bit length (`00 0c`):
//! `d2 eb 00 0c <record>` answers the status request, `d2 ec 00 0c <previous> 00 0c <current>` is
//! pushed on every change.
//!
//! After connecting, the module runs a content sync: long-framed envelopes (`d2 0a ...`) with inner
//! types 0x85 and 0x86, both listing VC-1..VC-4. The cloud acks each one, then answers with a 0x87
//! envelope holding a single 00. Unacked, the module resends 0x85 every couple of seconds; if the
//! sync never completes after provisioning, it drops off Wi-Fi after 7 minutes.
//!
//! Port of upstream PR #276.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::crc16::crc16;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

const CLASS_BYTE: u8 = 0xd2;
const RECORD_LEN: usize = 12;
// a record together with its 16-bit length prefix
const BLOCK_LEN: usize = 2 + RECORD_LEN;

const ENVELOPE_TYPE: u8 = 0x0a;
const INNER_TYPE_OFFSET: usize = 10;
const SYNC_LAST_TYPE: u8 = 0x86;
const SYNC_REPLY_TYPE: u8 = 0x87;

// Status query, as the cloud sends it on connect. 0xF0ED only reads.
const STATUS_REQUEST: &[u8] = &[0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x00];

const STATUS_OFFSET: usize = 0;
const CLEAN_MODE_OFFSET: usize = 1;
const FILTER_OFFSET: usize = 2;
const CLOGGED_OFFSET: usize = 3;
const BATTERY_OFFSET: usize = 5;

const STATUS: &[(u8, &str)] = &[
    (1, "Ready"),
    (2, "Cleaning"),
    (3, "Charging"),
    (4, "Fully charged"),
];
const CLEAN_MODE: &[(u8, &str)] = &[
    (1, "Off"),
    (2, "Normal"),
    (3, "High"),
    (4, "Turbo"),
    (5, "Mop"),
];
const BATTERY: &[(u8, &str)] = &[(1, "High"), (2, "Medium"), (3, "Low"), (4, "Warning")];

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map_or("None", |(_, label)| *label)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, label)| *label).collect()
}

/// 2 is the fault, 1 is fine.
fn problem(code: u8) -> &'static str {
    match code {
        2 => "ON",
        1 => "OFF",
        _ => "None",
    }
}

/// `AA FF <inner> <crc16> BB`, where `inner[2..4]` is the whole frame length and the CRC-16/XMODEM
/// covers everything before it.
fn long_frame(inner: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(inner.len() + 5);
    packet.extend_from_slice(&[0xaa, 0xff]);
    packet.extend_from_slice(inner);
    let len = (packet.len() + 3) as u16;
    packet[4..6].copy_from_slice(&len.to_be_bytes());
    let crc = crc16(&packet);
    packet.extend_from_slice(&crc.to_be_bytes());
    packet.push(0xbb);
    packet
}

fn components() -> HashMap<String, Value> {
    [
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:vacuum","device_class":"enum","options":labels(STATUS)})),
        ("battery", json!({"platform":"sensor","unique_id":"$deviceid-battery","state_topic":"$this/battery","name":"Battery","icon":"mdi:battery","device_class":"enum","options":labels(BATTERY)})),
        ("clean_mode", json!({"platform":"sensor","unique_id":"$deviceid-clean_mode","state_topic":"$this/clean_mode","name":"Power mode","icon":"mdi:fan","device_class":"enum","options":labels(CLEAN_MODE)})),
        ("filter", json!({"platform":"binary_sensor","unique_id":"$deviceid-filter","state_topic":"$this/filter","name":"Filter needs cleaning","device_class":"problem"})),
        ("clogged", json!({"platform":"binary_sensor","unique_id":"$deviceid-clogged","state_topic":"$this/clogged","name":"Air path clogged","device_class":"problem"})),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    sync_seq: AtomicU16,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            sync_seq: AtomicU16::new(0),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Stick Vacuum"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(block) = process_common_status(buf, CLASS_BYTE, BLOCK_LEN) {
            self.process_status(block);
            return;
        }
        if buf.first() == Some(&CLASS_BYTE)
            && buf.get(1) == Some(&ENVELOPE_TYPE)
            && buf.get(INNER_TYPE_OFFSET) == Some(&SYNC_LAST_TYPE)
        {
            self.complete_sync();
        }
    }

    fn complete_sync(&self) {
        let seq = self
            .sync_seq
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let [seq_hi, seq_lo] = seq.to_be_bytes();
        // f0 0b <frame length> 00 <seq> 00 01 0a, then the inner type, its 16-bit length and a 00
        let mut inner = vec![
            0xf0, 0x0b, 0x00, 0x00, 0x00, seq_hi, seq_lo, 0x00, 0x01, 0x0a,
        ];
        inner.extend_from_slice(&[SYNC_REPLY_TYPE, 0x00, 0x01, 0x00]);
        self.core.thinq.send_packet(&long_frame(&inner));
    }

    fn process_status(&self, block: &[u8]) {
        let rec = &block[2..];
        self.core
            .publish_property("status", named(STATUS, rec[STATUS_OFFSET]).into());
        self.core.publish_property(
            "clean_mode",
            named(CLEAN_MODE, rec[CLEAN_MODE_OFFSET]).into(),
        );
        self.core
            .publish_property("battery", named(BATTERY, rec[BATTERY_OFFSET]).into());
        self.core
            .publish_property("filter", problem(rec[FILTER_OFFSET]).into());
        self.core
            .publish_property("clogged", problem(rec[CLOGGED_OFFSET]).into());
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(STATUS_REQUEST);
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, _prop: &str, _value: &str) {}
    fn publish_config(&self) {
        self.core.republish_config();
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, PropertyValue};

    // Real captures from an A9K-CORE3S.
    // Docked: charging, power mode off, filter and air path fine, battery low.
    const SAMPLE_INITIAL: &str = "AA14D2EB000C03010101FF03010101020101C3BB";
    // Battery taken out: ready, power mode off, battery unknown.
    const SAMPLE_DELTA_BATTERY_OUT: &str =
        "AA22D2EC000C03010101FF03010101020101000C01010101FF00010101020101EEBB";
    // Running: power mode stepped up from High to Turbo, battery medium.
    const SAMPLE_DELTA_TURBO: &str =
        "AA22D2EC000C02030101FF02010101020101000C02040101FF0201010102010194BB";
    // Last content sync envelope, seq 0003, listing VC-1..VC-4.
    const SAMPLE_SYNC_LAST: &str =
        "AAFFD20A002B00000300010A860019040456432D31000456432D32000456432D33000456432D34000645BB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("HWWA9S_F2U", Device::new)
    }

    fn outbox_hex(thinq: &MockThinq2Device) -> Vec<String> {
        thinq.outbox().iter().map(|p| hex_encode(p)).collect()
    }

    fn sent(thinq: &MockThinq2Device) -> Vec<(String, i32, Value)> {
        thinq
            .sent()
            .into_iter()
            .map(|m| (m.cmd, m.msg_type, m.data))
            .collect()
    }

    #[test]
    fn start_requests_status() {
        let (_, thinq, dev) = make();
        dev.start();
        assert_eq!(outbox_hex(&thinq), vec!["AA0EF0ED1121010000001800B5BB"]);
    }

    #[test]
    fn initial_status() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        let expected: HashMap<String, PropertyValue> = [
            ("status", "Charging"),
            ("clean_mode", "Off"),
            ("battery", "Low"),
            ("filter", "OFF"),
            ("clogged", "OFF"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.into()))
        .collect();
        assert_eq!(ha.device(DEVICE_ID).unwrap().properties, expected);
    }

    #[test]
    fn status_delta_uses_the_current_record() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DELTA_BATTERY_OUT));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "battery").as_deref(), Some("None"));
    }

    #[test]
    fn running() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DELTA_TURBO));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Cleaning"));
        assert_eq!(prop(&ha, "clean_mode").as_deref(), Some("Turbo"));
        assert_eq!(prop(&ha, "battery").as_deref(), Some("Medium"));
    }

    #[test]
    fn status_frames_are_not_acked() {
        let (_, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        assert!(thinq.sent().is_empty());
    }

    #[test]
    fn content_sync_is_acked_and_answered_with_0x87() {
        let (_, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_SYNC_LAST));
        assert_eq!(
            sent(&thinq),
            vec![("ack".to_string(), 1, json!("AA0AF0000A040003E0BB"))]
        );
        assert_eq!(thinq.outbox().len(), 1);
    }

    #[test]
    fn reply_0x87_matches_the_cloud_byte_for_byte() {
        let (_, thinq, dev) = make();
        dev.sync_seq.store(0xaedd, Ordering::Relaxed);
        thinq.emit_data(&hex_decode(SAMPLE_SYNC_LAST));
        assert_eq!(
            outbox_hex(&thinq),
            vec!["AAFFF00B001300AEDE00010A870001005CF8BB"]
        );
    }

    #[test]
    fn earlier_sync_envelope_is_only_acked() {
        // 0x85 sync envelope captured from the sibling HWWA9K_F2 handle module.
        const SYNC_FIRST: &str = "aaffd20a002c00005200010a85001a00040456432d31010456432d32010456432d33010456432d34014880bb";
        let (_, thinq, _) = make();
        thinq.emit_data(&hex_decode(SYNC_FIRST));
        assert_eq!(
            sent(&thinq),
            vec![("ack".to_string(), 1, json!("AA0AF0000A04005251BB"))]
        );
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("HWWA9S_F2U").is_some());
    }
}
