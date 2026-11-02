//! DUCT_626301_WW — LG ducted air conditioner (PWFMDD200 / LCW-005 Wi-Fi module).
//!
//! The RAC_056905_WW handler with the ducted unit's fan speeds, 2/4/6 = low/medium/high. Changing
//! from off to a mode needs power=1 in the write, which RAC_056905_WW mode writes already carry.
//! Port of upstream PR #251.

use super::rac_056905_ww::{Device, FanModes};
use crate::device_trait::DeviceHandler;
use rethink_core::ha::HaConnection;
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use std::sync::Arc;

const FAN_MODES: FanModes = &[("low", 2), ("medium", 4), ("high", 6)];

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::with_fan_modes(ha, thinq, meta, FAN_MODES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device, PropertyValue};
    use rethink_util::tlv;
    use serde_json::json;

    const DEVICE_ID: &str = "test-id";

    // Captured from a DUCT_626301_WW (upstream PR #211): power off, fan_only, fan low, 25.5 °C
    // current and 26 °C set.
    const CAPS_RESPONSE_HEX: &str = "000004000000A702010076B005B05057B09054B0C1B103B4D011B710FEBB82BBC0BC41BC89BCC1B340B380B54EB2C4B740B85020B8903CB8D020B9103CBAD024BB103CBD41B5C0B642B61034B5C1B642B61034B5C2B642B61034B5C6B642B61034B5C4B642B61034B69036B6F0626301E5E0AE5AE5E05C97D400BD30800000DD006EC7";
    const QUERY_RESPONSE_HEX: &str = "000004000000A7020400567DC07E427E827F50337F90347F0086808840D4C0D500C84081408180C9408340838083C08FC0CD40CD00CCC0CD80ADA01995ACD01AAD5035B54ED5600457D5A00960BC89D5D03CD61020C9009C4087CCA880AC40E9C08F08";

    fn meta() -> Metadata {
        Metadata::new("DUCT_626301_WW", "DUCT_626301_WW", "626301")
    }

    fn make_ready_device() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::with_fan_modes(ha.clone(), thinq.clone(), meta(), FAN_MODES);
        let d = dev.clone();
        ha.on_set_property(move |id, prop, value| {
            if id == DEVICE_ID {
                d.set_property(prop, value);
            }
        });
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        // the first values arrive before the fields exist
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    /// Upstream PR #251's representative state: power off, cool, fan low, 24 °C set.
    fn set_representative_state(dev: &Device) {
        dev.set_raw(0x1f7, 0);
        dev.set_raw(0x1f9, 0);
        dev.set_raw(0x1fa, 2);
        dev.set_raw(0x1fe, 48);
    }

    fn value_for(packet: &[u8], id: u16) -> Option<u32> {
        tlv::parse(&packet[11..packet.len() - 2])
            .into_iter()
            .find(|e| e.t == id)
            .map(|e| e.v)
    }

    #[test]
    fn exposes_the_three_fan_modes_supported_by_the_ducted_unit() {
        let (ha, _thinq, _dev) = make_ready_device();
        let c = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(c["climate"]["fan_modes"], json!(["low", "medium", "high"]));
    }

    #[test]
    fn maps_low_medium_and_high_fan_modes_to_2_4_and_6() {
        let (ha, thinq, dev) = make_ready_device();
        set_representative_state(&dev);
        for (mode, expected) in [("low", 2), ("medium", 4), ("high", 6)] {
            thinq.reset_recorder();
            ha.set_property(DEVICE_ID, "climate", "fan_mode_command", mode);
            let out = thinq.outbox();
            assert_eq!(out.len(), 1);
            assert_eq!(value_for(&out[0], 0x1fa), Some(expected), "{mode}");
        }
    }

    #[test]
    fn selecting_an_operating_mode_explicitly_powers_on_the_unit() {
        let (ha, thinq, dev) = make_ready_device();
        set_representative_state(&dev);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "cool");
        let out = thinq.outbox();
        assert_eq!(out.len(), 1);
        assert_eq!(value_for(&out[0], 0x1f7), Some(1));
        assert_eq!(value_for(&out[0], 0x1f9), Some(0));
        assert_eq!(value_for(&out[0], 0x1fa), Some(2));
        assert_eq!(value_for(&out[0], 0x1fe), Some(48));
    }

    #[test]
    fn selecting_off_sends_power_off() {
        let (ha, thinq, dev) = make_ready_device();
        set_representative_state(&dev);
        dev.set_raw(0x1f7, 1);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "off");
        let out = thinq.outbox();
        assert_eq!(out.len(), 1);
        assert_eq!(value_for(&out[0], 0x1f7), Some(0));
    }

    #[test]
    fn captured_state_publishes_expected_climate_values() {
        let (ha, _thinq, _dev) = make_ready_device();
        let get = |topic: &str| ha.get_property(DEVICE_ID, "climate", topic);
        assert_eq!(get("current_temperature"), Some(PropertyValue::from(25.5)));
        assert_eq!(get("temperature_state"), Some(PropertyValue::from(26.0)));
        assert_eq!(get("fan_mode_state"), Some("low".into()));
        assert_eq!(get("mode_state"), Some("off".into()));
    }

    #[test]
    fn fan_mode_mapping_matches_the_device_values() {
        let (ha, _thinq, dev) = make_ready_device();
        let get = || ha.get_property(DEVICE_ID, "climate", "fan_mode_state");
        for (raw, mode) in [(2, "low"), (4, "medium"), (6, "high")] {
            dev.core.process_key_value(0x1fa, raw);
            assert_eq!(get(), Some(mode.into()));
        }
        // PR #211 read 8 as auto; PR #251 offers no auto
        dev.core.process_key_value(0x1fa, 8);
        assert_eq!(get(), Some("high".into()));
    }
}
