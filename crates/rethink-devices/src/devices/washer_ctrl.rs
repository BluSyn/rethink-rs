//! Shared control packets for F/Y-class AABB washers.

use rethink_core::device_base::AabbDeviceCore;

/// Family-wide "report your state" request used by F/Y washers, US laundry, RH10, H11.
pub const STATUS_QUERY: &[u8] = &[
    0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x00,
];

/// Fridge AABB status poll (`F0 ED 12 11 …`).
pub const FRIDGE_STATUS_QUERY: &[u8] = &[
    0xf0, 0xed, 0x12, 0x11, 0x01, 0x00, 0x00, 0x01, 0x04, 0x00,
];

pub fn request_status(core: &AabbDeviceCore) {
    core.send(STATUS_QUERY);
}

pub fn request_fridge_status(core: &AabbDeviceCore) {
    core.send(FRIDGE_STATUS_QUERY);
}

const POWER_ON: &[u8] = &[0xf0, 0x2a, 0x01, 0x00];
const POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];
const PAUSE: &[u8] = &[0xf0, 0x24, 0x04, 0x01, 0x00];
const START: &[u8] = &[0xf0, 0x24, 0x05, 0x01, 0x00];

pub fn set_power_start_pause(core: &AabbDeviceCore, prop: &str, mqtt_value: &str) {
    if prop == "power" {
        if mqtt_value == "ON" {
            core.send(POWER_ON);
        } else if mqtt_value == "OFF" {
            core.send(POWER_OFF);
        }
    }
    if prop == "pause" {
        core.send(PAUSE);
    }
    // The button payload is not a command. Every F/Y washer sends this fixed start.
    if prop == "start" {
        core.send(START);
    }
}

pub fn pub_temp_spin(core: &AabbDeviceCore, temp_idx: u8, spin_idx: u8) {
    use crate::washer_common::{spin_value, temperature_value};
    core.publish_property(
        "temp",
        temperature_value(temp_idx as usize)
            .unwrap_or("None")
            .into(),
    );
    core.publish_property(
        "spin",
        spin_value(spin_idx as usize).unwrap_or("None").into(),
    );
}

pub fn pub_error_status(core: &AabbDeviceCore, error: u8, status: u8, name: fn(u8) -> &'static str) {
    use crate::washer_common::error_message;
    core.publish_property("error_message", error_message(error as usize).into());
    core.publish_on_off("error", error != 0);
    core.publish_property("status", name(status).into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device, Metadata};

    fn core() -> (
        std::sync::Arc<MockHaConnection>,
        std::sync::Arc<MockThinq2Device>,
        std::sync::Arc<AabbDeviceCore>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new("id", Metadata::new("M", "M", "1"));
        let c = AabbDeviceCore::new(ha.clone(), thinq.clone());
        (ha, thinq, c)
    }

    #[test]
    fn request_status_sends_laundry_query() {
        let (_, thinq, c) = core();
        request_status(&c);
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1121010000001800B5BB"
        );
    }

    #[test]
    fn request_fridge_status_sends_fridge_query() {
        let (_, thinq, c) = core();
        request_fridge_status(&c);
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1211010000010400EBBB"
        );
    }

    #[test]
    fn power_on_off_and_start_pause() {
        let (_, thinq, c) = core();
        set_power_start_pause(&c, "power", "ON");
        set_power_start_pause(&c, "power", "OFF");
        set_power_start_pause(&c, "pause", "");
        set_power_start_pause(&c, "start", "");
        let hexes: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert!(hexes.iter().any(|h| h.contains("F02A0100")));
        assert!(hexes.iter().any(|h| h.contains("F024010100")));
        assert!(hexes.iter().any(|h| h.contains("F024040100")));
        assert!(hexes.iter().any(|h| h.contains("F024050100")));
    }

    #[test]
    fn start_payload_is_ignored() {
        // Master always sends F024050100. A button payload must not become the packet.
        let models = [
            "F_C__Y___W.A__QEUK",
            "F_V8_Y___W.B_2QEUK",
            "F_VB_F___W.B_2QEUK",
            "F_VB_Y___W.B_2QEUK",
            "F_V__F___W.B_1QEUK",
            "Y_V8_Y___W.B32QEUK",
            "Y_V8_F___W.B_2QEUK",
            "G2V7_Y___W.B_2QEUK",
        ];
        for model in models {
            let ha = MockHaConnection::new();
            let meta = Metadata::new(model, model, "1");
            let thinq = MockThinq2Device::new("id", meta.clone());
            let factory = crate::registry::t2_factory(model).expect(model);
            let dev = factory(ha, thinq.clone(), meta);
            dev.set_property("start", "DEADBEEF");
            let hexes: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
            assert!(
                hexes.iter().any(|h| h.contains("F024050100")),
                "{model} start packet: {hexes:?}"
            );
            assert!(
                hexes.iter().all(|h| !h.contains("DEAD")),
                "{model} echoed the payload: {hexes:?}"
            );
        }
    }

    #[test]
    fn pub_error_status_ok_and_fault() {
        let (ha, _, c) = core();
        pub_error_status(&c, 0, 1, crate::washer_common::fy_steam_name);
        assert_eq!(
            ha.device("id").unwrap().properties.get("error").map(|p| p.as_string()),
            Some("OFF".into())
        );
        assert_eq!(
            ha.device("id")
                .unwrap()
                .properties
                .get("status")
                .map(|p| p.as_string()),
            Some("Standby".into())
        );
        pub_error_status(&c, 1, 18, crate::washer_common::fy_steam_name);
        assert_eq!(
            ha.device("id").unwrap().properties.get("error").map(|p| p.as_string()),
            Some("ON".into())
        );
        assert_eq!(
            ha.device("id")
                .unwrap()
                .properties
                .get("error_message")
                .map(|p| p.as_string()),
            Some("Door lock error (DE2)".into())
        );
    }
}
