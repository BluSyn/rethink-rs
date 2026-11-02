//! HWWA9K_F2 — LG CordZero A9 stick vacuum (deviceType 504).
//!
//! State is class 0xd2 followed by one or two 14-byte records, each an index, a field count and
//! twelve one-byte fields; the trailing record is current. The separate 0xd2/0x0a frame is a
//! capability list, not live values, and does not match this record shape. Offsets and codes
//! were confirmed by injecting single-byte-changed frames and reading LG's cloud decode back. The
//! five settings take the captured one-byte command `f0 24 <controlDataType> 01 <value>`.
//!
//! Port of upstream PR #116.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_TAG: u8 = 0xd2;
const RECORDS_OFF: usize = 2;
const RECORD_LEN: usize = 14;
const FIELD_COUNT: u8 = 12;

// Offsets within a record.
const OFF_MON_STATUS: usize = 2;
const OFF_CLEAN_MODE: usize = 3;
const OFF_FILTER_STATE: usize = 4;
const OFF_PASSAGE_CLOGGED: usize = 5;
const OFF_NOZZLE: usize = 6;
const OFF_BATTERY_LEVEL: usize = 7;
const OFF_MOP_WITH_SUCKING: usize = 8;
const OFF_COMPLETE_CLEAN: usize = 9;
const OFF_SUCTION_FORCE: usize = 10;
const OFF_CHARGING_MELODY: usize = 11;
const OFF_VOLUME: usize = 12;
const OFF_BRIGHTNESS: usize = 13;

// modelJSON `ControlWifi` controlDataType ids.
const CTRL_MOP_SETTING: u8 = 0x01;
const CTRL_SUCTION_FORCE: u8 = 0x02;
const CTRL_CHARGING_MELODY: u8 = 0x03;
const CTRL_VOLUME: u8 = 0x04;
const CTRL_BRIGHTNESS: u8 = 0x05;

// modelJSON `MonitoringValue.<field>.valueMapping`, with English labels from the model's language
// pack, or the mapping's own `_comment` where LG left the label blank.

const MON_STATUS: &[(u8, &str)] = &[
    (1, "Standby"),
    (2, "Cleaning"),
    (3, "Charging"),
    (4, "Charging complete"),
];

/// The suction level running right now.
const CLEAN_MODE: &[(u8, &str)] = &[
    (1, "Off"),
    (2, "Standard"),
    (3, "High"),
    (4, "Turbo"),
    (5, "Mop"),
    (6, "Auto"),
];

const FILTER_STATE: &[(u8, &str)] = &[(1, "Normal"), (2, "Cleaning needed")];

const PASSAGE: &[(u8, &str)] = &[(1, "Normal"), (2, "Check for debris")];

/// Which head is attached.
const NOZZLE: &[(u8, &str)] = &[
    (0, "Auxiliary inlet"),
    (1, "PowerDrive Floor"),
    (2, "PowerDrive Carpet"),
    (3, "PowerDrive Mop"),
];

/// This unit reports 0 while docked, which LG surfaces as NOT_USE.
const BATTERY: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "High"),
    (2, "Mid"),
    (3, "Low"),
    (4, "Warning"),
];

const MOP_WITH_SUCKING: &[(u8, &str)] = &[(1, "Mop only"), (2, "Mop and suction together")];

/// The default suction level the product starts at.
const SUCTION_FORCE: &[(u8, &str)] = &[(1, "Standard"), (2, "High"), (3, "Turbo")];

const CHARGING_MELODY: &[(u8, &str)] = &[
    (1, "Lucky"),
    (2, "Marble"),
    (3, "Ice"),
    (4, "Breeze"),
    (5, "Nebula"),
];

const VOLUME: &[(u8, &str)] = &[(1, "High"), (2, "Normal"), (3, "Low")];

const BRIGHTNESS: &[(u8, &str)] = &[
    (1, "Very bright"),
    (2, "Bright"),
    (3, "Normal"),
    (4, "Dim"),
    (5, "Off"),
];

fn options(table: &[(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, label)| *label).collect()
}

fn label(table: &[(u8, &'static str)], raw: u8) -> Option<&'static str> {
    table.iter().find(|(code, _)| *code == raw).map(|(_, l)| *l)
}

fn enum_sensor(id: &str, name: &str, table: &[(u8, &'static str)], icon: &str) -> Value {
    json!({
        "platform": "sensor",
        "unique_id": format!("$deviceid-{id}"),
        "state_topic": format!("$this/{id}"),
        "name": name,
        "device_class": "enum",
        "options": options(table),
        "icon": icon,
    })
}

/// The options are the labels of the setting's own read table, so every reported state is one.
fn select(id: &str, name: &str, table: &[(u8, &'static str)], icon: &str) -> Value {
    json!({
        "platform": "select",
        "unique_id": format!("$deviceid-{id}"),
        "state_topic": format!("$this/{id}"),
        "command_topic": format!("$this/{id}/set"),
        "name": name,
        "options": options(table),
        "optimistic": true,
        "icon": icon,
    })
}

fn config_select(id: &str, name: &str, table: &[(u8, &'static str)], icon: &str) -> Value {
    let mut component = select(id, name, table, icon);
    component["entity_category"] = json!("config");
    component
}

fn components() -> HashMap<String, Value> {
    [
        ("status", enum_sensor("status", "Status", MON_STATUS, "mdi:robot-vacuum")),
        ("clean_mode", enum_sensor("clean_mode", "Operation level", CLEAN_MODE, "mdi:fan")),
        ("suction_force", select("suction_force", "Default suction power", SUCTION_FORCE, "mdi:weather-windy")),
        ("battery", enum_sensor("battery", "Battery", BATTERY, "mdi:battery")),
        ("nozzle", enum_sensor("nozzle", "Attached nozzle", NOZZLE, "mdi:vacuum")),
        ("filter_state", enum_sensor("filter_state", "Filter state", FILTER_STATE, "mdi:air-filter")),
        ("passage", enum_sensor("passage", "Inlet blocked", PASSAGE, "mdi:pipe-disconnected")),
        ("mop_with_sucking", select("mop_with_sucking", "Mop usage mode", MOP_WITH_SUCKING, "mdi:water")),
        ("charging_melody", config_select("charging_melody", "Charging melody", CHARGING_MELODY, "mdi:music-note")),
        ("volume", config_select("volume", "Volume", VOLUME, "mdi:volume-high")),
        ("brightness", config_select("brightness", "LED Brightness", BRIGHTNESS, "mdi:brightness-6")),
        ("complete_clean", json!({"platform":"binary_sensor","unique_id":"$deviceid-complete-clean","state_topic":"$this/complete_clean","name":"Cleaning complete","icon":"mdi:check-circle-outline","payload_on":"ON","payload_off":"OFF"})),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });

        let mut config =
            default_config(&meta, Some(json!({"name": "LG CordZero A9 Stick Vacuum"})));
        config.components = components();
        core.set_config(config);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first() != Some(&CLASS_TAG) {
            return;
        }
        let payload = buf.len().saturating_sub(RECORDS_OFF);
        if payload == 0 || !payload.is_multiple_of(RECORD_LEN) {
            return;
        }
        // The trailing record is the current state; a leading one, when present, is the previous.
        let rec = &buf[buf.len() - RECORD_LEN..];
        // A different count is a different record shape; leave the previous values standing.
        if rec[1] != FIELD_COUNT {
            return;
        }

        let publish_enum = |prop: &str, table: &[(u8, &'static str)], raw: u8| {
            self.core
                .publish_property(prop, label(table, raw).unwrap_or("None").into());
        };
        publish_enum("status", MON_STATUS, rec[OFF_MON_STATUS]);
        publish_enum("clean_mode", CLEAN_MODE, rec[OFF_CLEAN_MODE]);
        publish_enum("battery", BATTERY, rec[OFF_BATTERY_LEVEL]);
        publish_enum("nozzle", NOZZLE, rec[OFF_NOZZLE]);
        publish_enum("filter_state", FILTER_STATE, rec[OFF_FILTER_STATE]);
        publish_enum("passage", PASSAGE, rec[OFF_PASSAGE_CLOGGED]);
        // LG's own polarity: 1 = not finished, 2 = finished.
        self.core
            .publish_on_off("complete_clean", rec[OFF_COMPLETE_CLEAN] == 2);

        // A select state must be one of its options, so an unlisted code leaves the previous one.
        let publish_option = |prop: &str, table: &[(u8, &'static str)], raw: u8| {
            if let Some(l) = label(table, raw) {
                self.core.publish_property(prop, l.into());
            }
        };
        publish_option("suction_force", SUCTION_FORCE, rec[OFF_SUCTION_FORCE]);
        publish_option(
            "mop_with_sucking",
            MOP_WITH_SUCKING,
            rec[OFF_MOP_WITH_SUCKING],
        );
        publish_option("charging_melody", CHARGING_MELODY, rec[OFF_CHARGING_MELODY]);
        publish_option("volume", VOLUME, rec[OFF_VOLUME]);
        publish_option("brightness", BRIGHTNESS, rec[OFF_BRIGHTNESS]);
    }

    /// `f0 24 <controlDataType> 01 <value>`, framed as LG's own command frames.
    fn set_field(&self, typ: u8, value: u8) {
        self.core.send(&[0xf0, 0x24, typ, 0x01, value]);
    }

    fn write(&self, typ: u8, table: &[(u8, &str)], what: &str, value: &str) {
        match table.iter().find(|(_, l)| *l == value) {
            Some((code, _)) => self.set_field(typ, *code),
            None => tracing::debug!("{}: unknown {what} {value}", self.core.id),
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {}
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        match prop {
            "suction_force" => self.write(
                CTRL_SUCTION_FORCE,
                SUCTION_FORCE,
                "Default suction power",
                value,
            ),
            "mop_with_sucking" => {
                self.write(CTRL_MOP_SETTING, MOP_WITH_SUCKING, "Mop usage mode", value)
            }
            "charging_melody" => self.write(
                CTRL_CHARGING_MELODY,
                CHARGING_MELODY,
                "Charging melody",
                value,
            ),
            "volume" => self.write(CTRL_VOLUME, VOLUME, "Volume", value),
            "brightness" => self.write(CTRL_BRIGHTNESS, BRIGHTNESS, "LED Brightness", value),
            _ => tracing::debug!("{}: item does not support writing {prop}", self.core.id),
        }
    }
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
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};

    // STATE was captured from the appliance; the single-byte variants were injected through LG's
    // cloud decoder to confirm which field and value each offset represents.

    // Docked and fully charged. LG's snapshot for this exact frame read monStatus
    // CHARGING_COMPLETE, cleanMode OFF, filterState NORMAL, passageClogged NORMAL, nozzle IGNORE,
    // batteryLevel NOT_USE, mopWithSucking OFF, completeClean OFF, suctionForce HIGH,
    // chargingMelody MELODY_1, volume HIGH, brightness LOW.
    const STATE: &str = "aa14d2eb000c04010101ff00010102010103c3bb";

    const MON_CHARGING: &str = "aa14d2eb000c03010101ff00010102010103c0bb"; // byte[6]=3 -> monStatus CHARGING
    const SUCTION_TURBO: &str = "aa14d2eb000c04010101ff00010103010103c2bb"; // byte[14]=3 -> suctionForce TURBO
    const BRIGHT_HIGH: &str = "aa14d2eb000c04010101ff00010102010102c0bb"; // byte[17]=2 -> brightness HIGH
    const NOZZLE_MOP: &str = "aa14d2eb000c040101010300010102010103cfbb"; // byte[10]=3 -> nozzle WATER_MOP
                                                                         // completeClean's offset is from the probe; its ON code is LG's own valueMapping (ON = 2,
                                                                         // OFF = 1).
    const CLEAN_DONE: &str = "aa14d2eb000c04010101ff00010202010103c2bb"; // byte[13]=2 -> completeClean ON

    // Captured echo after five settings were written: the previous record then the current one,
    // under d2/ec.
    const ECHO_TWO_RECORDS: &str =
        "aa22d2ec000c04010101ff00020101030304000c04010101ff000201020303049ebb";

    // The 44-byte settings frame the appliance also sends: a capability list, not values (all four
    // "VC-n" records read 01 while the state frame says suction 2 and brightness 3).
    const CAPABILITY: &str =
        "aaffd20a002c00005200010a85001a00040456432d31010456432d32010456432d33010456432d34014880bb";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("HWWA9K_F2", Device::new)
    }

    fn components(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID).unwrap().config.unwrap().components
    }

    fn props(ha: &MockHaConnection) -> HashMap<String, String> {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .into_iter()
            .map(|(k, v)| (k, v.as_string()))
            .collect()
    }

    /// `frame` with one byte changed, and the checksum recomputed when `checksum` is set.
    fn with_byte(frame: &str, index: usize, value: u8, checksum: bool) -> Vec<u8> {
        let mut packet = hex_decode(frame);
        packet[index] = value;
        if checksum {
            packet = wrap_aabb(&packet[2..packet.len() - 2]);
        }
        packet
    }

    #[test]
    fn publishes_only_entities_the_12_field_frame_actually_fills() {
        let (ha, _, _) = make();
        let components = components(&ha);
        for c in [
            "status",
            "clean_mode",
            "suction_force",
            "battery",
            "nozzle",
            "filter_state",
            "passage",
            "mop_with_sucking",
            "charging_melody",
            "volume",
            "brightness",
            "complete_clean",
        ] {
            assert!(components.contains_key(c), "component {c} present");
        }
        let real = components
            .values()
            .filter(|c| c.get("unique_id").is_some())
            .count();
        assert_eq!(real, 12);
    }

    #[test]
    fn closed_set_sensors_declare_their_enum_contract() {
        let (ha, _, _) = make();
        let components = components(&ha);
        for (name, options) in [
            (
                "status",
                json!(["Standby", "Cleaning", "Charging", "Charging complete"]),
            ),
            (
                "clean_mode",
                json!(["Off", "Standard", "High", "Turbo", "Mop", "Auto"]),
            ),
            ("battery", json!(["Off", "High", "Mid", "Low", "Warning"])),
            (
                "nozzle",
                json!([
                    "Auxiliary inlet",
                    "PowerDrive Floor",
                    "PowerDrive Carpet",
                    "PowerDrive Mop"
                ]),
            ),
            ("filter_state", json!(["Normal", "Cleaning needed"])),
            ("passage", json!(["Normal", "Check for debris"])),
        ] {
            assert_eq!(
                components[name]["device_class"], "enum",
                "{name} is an enum sensor"
            );
            assert_eq!(
                components[name]["options"], options,
                "{name} declares every value"
            );
        }
    }

    #[test]
    fn exactly_the_fields_with_a_captured_command_frame_are_writable() {
        const WRITABLE: [&str; 5] = [
            "suction_force",
            "mop_with_sucking",
            "charging_melody",
            "volume",
            "brightness",
        ];
        let (ha, _, _) = make();
        for (name, comp) in components(&ha) {
            if comp.get("unique_id").is_none() {
                continue;
            }
            if WRITABLE.contains(&name.as_str()) {
                assert_eq!(comp["command_topic"], format!("$this/{name}/set"), "{name}");
                assert_eq!(comp["platform"], "select", "{name}");
            } else {
                assert!(
                    comp.get("command_topic").is_none(),
                    "{name} stays read-only"
                );
                let platform = comp["platform"].as_str().unwrap();
                assert!(
                    platform == "sensor" || platform == "binary_sensor",
                    "{name} is a sensor platform, got {platform}"
                );
            }
        }
    }

    #[test]
    fn every_selects_options_are_exactly_the_labels_it_can_report() {
        let (ha, _, _) = make();
        let components = components(&ha);
        for (name, options) in [
            ("suction_force", json!(["Standard", "High", "Turbo"])),
            (
                "mop_with_sucking",
                json!(["Mop only", "Mop and suction together"]),
            ),
            (
                "charging_melody",
                json!(["Lucky", "Marble", "Ice", "Breeze", "Nebula"]),
            ),
            ("volume", json!(["High", "Normal", "Low"])),
            (
                "brightness",
                json!(["Very bright", "Bright", "Normal", "Dim", "Off"]),
            ),
        ] {
            assert_eq!(components[name]["options"], options, "{name}");
            assert_eq!(components[name]["optimistic"], true, "{name}");
        }
    }

    // Each frame is what LG's cloud sent the appliance when its capability API was asked to change
    // the setting (every request answered CL-0000), relayed through rethink on 2026-07-28.
    #[test]
    fn each_write_reproduces_the_frame_lg_itself_sent_for_that_command() {
        for (key, value, want) in [
            // MOP_SETTING (controlDataType 0x01)
            ("mop_with_sucking", "Mop only", "aa09f0240101019fbb"),
            (
                "mop_with_sucking",
                "Mop and suction together",
                "aa09f0240101029ebb",
            ),
            // SUCTION_FORCE (0x02)
            ("suction_force", "Standard", "aa09f0240201019ebb"),
            ("suction_force", "High", "aa09f02402010299bb"),
            ("suction_force", "Turbo", "aa09f02402010398bb"),
            // CHARGING_MELODY (0x03)
            ("charging_melody", "Lucky", "aa09f02403010199bb"),
            ("charging_melody", "Marble", "aa09f02403010298bb"),
            ("charging_melody", "Ice", "aa09f0240301039bbb"),
            ("charging_melody", "Breeze", "aa09f0240301049abb"),
            ("charging_melody", "Nebula", "aa09f02403010585bb"),
            // VOLUME (0x04)
            ("volume", "High", "aa09f02404010198bb"),
            ("volume", "Normal", "aa09f0240401029bbb"),
            ("volume", "Low", "aa09f0240401039abb"),
            // BRIGHTNESS (0x05)
            ("brightness", "Very bright", "aa09f0240501019bbb"),
            ("brightness", "Bright", "aa09f0240501029abb"),
            ("brightness", "Normal", "aa09f02405010385bb"),
            ("brightness", "Dim", "aa09f02405010484bb"),
            ("brightness", "Off", "aa09f02405010587bb"),
        ] {
            let (ha, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property(key, value);
            let outbox = thinq.outbox();
            assert_eq!(outbox.len(), 1, "{key}={value} sent one frame");
            assert_eq!(hex_encode(&outbox[0]), want.to_uppercase(), "{key}={value}");
            // The select is optimistic in HA; the state comes from the appliance's own report.
            assert_eq!(prop(&ha, key), None, "{key}={value}");
        }
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_guessed() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("suction_force", "InvalidForce");
        dev.set_property("brightness", "InvalidBrightness");
        dev.set_property("status", "Charging"); // read-only
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn real_captured_frame_decodes_to_what_lg_read_from_the_same_frame() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Charging complete"));
        assert_eq!(prop(&ha, "clean_mode").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "filter_state").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "passage").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "mop_with_sucking").as_deref(), Some("Mop only"));
        assert_eq!(prop(&ha, "suction_force").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "charging_melody").as_deref(), Some("Lucky"));
        assert_eq!(prop(&ha, "volume").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "brightness").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "complete_clean").as_deref(), Some("OFF"));
        // nozzle 0xff and batteryLevel 0 are LG's IGNORE sentinels on this model, not real values.
        assert_eq!(prop(&ha, "nozzle").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "battery").as_deref(), Some("Off"));
    }

    #[test]
    fn each_probed_offset_moves_exactly_the_field_lg_said_it_moves() {
        for (frame, key, want) in [
            (MON_CHARGING, "status", "Charging"),
            (SUCTION_TURBO, "suction_force", "Turbo"),
            (BRIGHT_HIGH, "brightness", "Bright"),
            (NOZZLE_MOP, "nozzle", "PowerDrive Mop"),
            (CLEAN_DONE, "complete_clean", "ON"),
        ] {
            let (ha, thinq, _) = make();
            thinq.emit_data(&hex_decode(STATE));
            thinq.emit_data(&hex_decode(frame));
            assert_eq!(
                prop(&ha, key).as_deref(),
                Some(want),
                "{key} after single-byte probe"
            );
        }
    }

    #[test]
    fn a_single_byte_probe_changes_nothing_else() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE));
        let before = props(&ha);
        thinq.emit_data(&hex_decode(SUCTION_TURBO));
        let after = props(&ha);
        for (key, value) in &before {
            if key != "suction_force" {
                assert_eq!(after.get(key), Some(value), "{key} unchanged");
            }
        }
    }

    #[test]
    fn the_44_byte_capability_frame_is_ignored_not_decoded_as_state() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE));
        let before = props(&ha);
        thinq.emit_data(&hex_decode(CAPABILITY));
        assert_eq!(props(&ha), before);
    }

    #[test]
    fn the_two_record_echo_is_read_and_its_last_record_is_the_current_state() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE));
        thinq.emit_data(&hex_decode(ECHO_TWO_RECORDS));
        // The trailing record's five settings, not the leading record's (which still says
        // mopWithSucking 1 and suctionForce 1).
        assert_eq!(
            prop(&ha, "mop_with_sucking").as_deref(),
            Some("Mop and suction together")
        );
        assert_eq!(prop(&ha, "suction_force").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "charging_melody").as_deref(), Some("Ice"));
        assert_eq!(prop(&ha, "volume").as_deref(), Some("Low"));
        assert_eq!(prop(&ha, "brightness").as_deref(), Some("Dim"));
        // ...and the rest of the record still decodes on the same offsets.
        assert_eq!(prop(&ha, "status").as_deref(), Some("Charging complete"));
        assert_eq!(prop(&ha, "clean_mode").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "battery").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "nozzle").as_deref(), Some("None"));
    }

    #[test]
    fn an_unrecognized_enum_code_publishes_none() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&with_byte(STATE, 6, 9, false));
        assert_eq!(prop(&ha, "status").as_deref(), Some("None"));
    }

    #[test]
    fn a_code_the_table_does_not_list_leaves_the_select_where_it_was() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE));
        assert_eq!(prop(&ha, "suction_force").as_deref(), Some("High"));
        // Not a suctionForce code; publishing it would be an invalid select state.
        thinq.emit_data(&with_byte(STATE, 14, 9, true));
        assert_eq!(prop(&ha, "suction_force").as_deref(), Some("High"));
    }

    #[test]
    fn a_frame_whose_field_count_is_not_12_is_left_alone() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STATE));
        let before = props(&ha);
        thinq.emit_data(&with_byte(STATE, 5, 3, false));
        assert_eq!(props(&ha), before);
    }

    #[test]
    fn no_read_only_component_claims_entity_category_config() {
        let (ha, _, _) = make();
        for (name, comp) in components(&ha) {
            if comp["platform"] != "sensor" && comp["platform"] != "binary_sensor" {
                continue;
            }
            assert_ne!(
                comp.get("entity_category"),
                Some(&json!("config")),
                "{name}"
            );
        }
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("HWWA9K_F2").is_some());
    }
}
