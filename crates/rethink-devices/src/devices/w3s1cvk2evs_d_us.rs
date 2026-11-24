//! W3S1CVK2EVS_D_US — LG WKGX201HWA WashTower dryer half (AABB, class `0x34`).
//!
//! Port of upstream PR #300. Uses `rv13d5jsd_d_us` decoding with class `0x34`,
//! 30-byte blocks, marker `0x1c`, power on/off buttons (`F0 24 01 01 01`/`00`),
//! downloaded course, control lock, remote start, and cycle energy (Wh).

use crate::device_trait::DeviceHandler;
use crate::devices::rv13d5jsd_d_us::{self, VariantOptions};
use rethink_core::device_base::AabbDeviceCore;
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x34;
const STATUS_LENGTH: usize = 30;
const MARKER: u8 = 0x1c;

const FLAGS_OFFSET: usize = 15;
const OPT2_OFFSET: usize = 16;
const FLAG_CHILD_LOCK: u8 = 0x01;
const OPT2_REMOTE_START: u8 = 0x01;
const ENERGY_OFFSET: usize = 18;
const SELECTED_DOWNLOAD_OFFSET: usize = 21;
const DOWNLOADED_OFFSET: usize = 24;

const DOWNLOADED_COURSE: &[(u8, &str)] = &[
    (0x64, "Super Dry"),
    (0x65, "Denim"),
    (0x68, "Ultra Delicate"),
    (0x69, "Low Temp Dry"),
    (0x6c, "Blankets"),
    (0x6d, "Blanket Refresh"),
    (0x6f, "Deodorization"),
    (0x70, "Lingerie"),
    (0x72, "Overnight Dry"),
    (0x74, "Perm. Press"),
    (0x75, "Sportswear"),
    (0x76, "Antibacterial"),
    (0x77, "Steam Sanitary"),
    (0x78, "Speed Dry"),
    (0x79, "Air Dry"),
    (0x97, "Towels"),
    (0xc9, "Easy Ironing"),
    (0xca, "Wrinkle Prevention"),
    (0xcc, "Static Reduce"),
    (0xcd, "Half Load Dry"),
    (0xce, "Full Load Dry"),
    (0xd0, "Rack Dry"),
];

fn downloaded_name(code: u8) -> Option<&'static str> {
    DOWNLOADED_COURSE
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| *n)
}

fn extra_components(comps: &mut HashMap<String, Value>) {
    let dl_options: Vec<&str> = DOWNLOADED_COURSE.iter().map(|(_, n)| *n).collect();
    comps.insert(
        "power_on".into(),
        json!({
            "platform": "button",
            "unique_id": "$deviceid-power_on",
            "command_topic": "$this/power_on/set",
            "payload_press": "PRESS",
            "name": "Power on",
            "icon": "mdi:power",
            "availability": [{"topic": "$this/power", "payload_available": "OFF", "payload_not_available": "ON"}],
        }),
    );
    comps.insert(
        "remote_start".into(),
        json!({
            "platform": "binary_sensor",
            "unique_id": "$deviceid-remote_start",
            "state_topic": "$this/remote_start",
            "name": "Remote start",
            "icon": "mdi:remote",
        }),
    );
    comps.insert(
        "child_lock".into(),
        json!({
            "platform": "binary_sensor",
            "unique_id": "$deviceid-child_lock",
            "state_topic": "$this/child_lock",
            "name": "Control lock",
            "icon": "mdi:lock",
        }),
    );
    comps.insert(
        "downloaded_course".into(),
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-downloaded_course",
            "state_topic": "$this/downloaded_course",
            "name": "Downloaded course",
            "icon": "mdi:download",
            "device_class": "enum",
            "options": dl_options,
        }),
    );
    comps.insert(
        "energy".into(),
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-energy",
            "state_topic": "$this/energy",
            "name": "Energy",
            "device_class": "energy",
            "state_class": "total_increasing",
            "unit_of_measurement": "Wh",
            "icon": "mdi:lightning-bolt",
        }),
    );
}

fn course_override(rec: &[u8]) -> Option<&'static str> {
    if rec[1] != 0 && rec[SELECTED_DOWNLOAD_OFFSET] != 0 {
        Some(downloaded_name(rec[SELECTED_DOWNLOAD_OFFSET]).unwrap_or("None"))
    } else {
        None
    }
}

fn on_record(core: &AabbDeviceCore, rec: &[u8]) {
    core.publish_property(
        "downloaded_course",
        downloaded_name(rec[DOWNLOADED_OFFSET])
            .unwrap_or("None")
            .into(),
    );
    core.publish_on_off("child_lock", rec[FLAGS_OFFSET] & FLAG_CHILD_LOCK != 0);
    core.publish_on_off("remote_start", rec[OPT2_OFFSET] & OPT2_REMOTE_START != 0);
    let energy = u16::from_be_bytes([rec[ENERGY_OFFSET], rec[ENERGY_OFFSET + 1]]);
    core.publish_property("energy", PropertyValue::Int(i64::from(energy)));
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    rv13d5jsd_d_us::Device::with_variant(
        ha,
        thinq,
        meta,
        Some(VariantOptions {
            class_byte: CLASS_BYTE,
            status_length: STATUS_LENGTH,
            marker: MARKER,
            send_status_on_start: false,
            power_on_supported: true,
            omit_damp_dry_signal: true,
            extra_components,
            course_override,
            on_record,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const NORMAL: &str = "AA4234EC001C01000100010000000000040000004028020000000000006900000000001C01003B003B03000504000400000040280300000000000069000000002CBB";
    const SUPER_DRY_DOWNLOADED: &str = "aa4234ec001c01000100010000000000040000004028020000000000006900000000001c01003b003b1a00050500040000004028020000006400006400000000babb";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_status_downloaded_and_power_commands() {
        let meta = Metadata::new("W3S1CVK2EVS_D_US", "W3S1CVK2EVS_D_US", "0.0.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = create(ha.clone(), thinq.clone(), meta);

        dev.start();
        assert!(thinq.outbox().is_empty(), "no status request sent");

        dev.set_property("power_on", "PRESS");
        dev.set_property("power_off", "PRESS");
        dev.set_property("power_off", "not-a-press");
        let sent: Vec<String> = thinq.outbox().iter().map(|b| hex_encode(b)).collect();
        assert_eq!(sent, vec!["AA09F0240101019FBB", "AA09F0240101009CBB"]);

        thinq.emit_data(&hex_decode(NORMAL));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "downloaded_course").as_deref(), Some("Low Temp Dry"));
        assert!(prop(&ha, "damp_dry_signal").is_none());

        thinq.emit_data(&hex_decode(SUPER_DRY_DOWNLOADED));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Super Dry"));
        assert_eq!(prop(&ha, "downloaded_course").as_deref(), Some("Super Dry"));

        // RV13D5JSD class-0x30 frame is ignored.
        let ha2 = MockHaConnection::new();
        let thinq2 = MockThinq2Device::new(ID, Metadata::new("W3S1CVK2EVS_D_US", "W3S1CVK2EVS_D_US", "0.0.0"));
        let _dev2 = create(ha2.clone(), thinq2.clone(), Metadata::new("W3S1CVK2EVS_D_US", "W3S1CVK2EVS_D_US", "0.0.0"));
        thinq2.emit_data(&hex_decode(
            "AA2330EB001B000039003900000000000400000000A800000001000000CA000000B9BB",
        ));
        assert!(ha2.device(ID).unwrap().properties.is_empty());
    }
}
