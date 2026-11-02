//! WLREL6323S — LG LREL6323S electric range (AABB class 0x40).
//!
//! Same 62-byte 0x40EB/EC record as WFV474PGV but single cavity: door bit
//! 0x08 at offset 11, and one aggregate cooktop status (normal elements set
//! bytes 36 and 38, the warming zone 37 and 39). The only writes are the clock,
//! clock format and beeper fields of the F043210E Preferences frame shared with
//! the WFV474PGV oven, verified on a live range, which reports none of them.
//!
//! Port of upstream commit eedaf3a (PR #159) and PR #183.

use crate::device_trait::DeviceHandler;
use chrono::Timelike;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x40;
const STATUS_RECORD_LEN: usize = 62;
const OVEN_STATE: usize = 0;
const OVEN_MODE: usize = 1;
const OVEN_SET_TEMP: usize = 5;
const OVEN_DOOR_FLAGS: usize = 11;
const OVEN_DOOR_OPEN: u8 = 0x08;
const COOKTOP_OFFSETS: [usize; 4] = [36, 37, 38, 39];
const STATUS_QUERY: &str =
    "F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000";

const OVEN_STATES: &[(u8, &str)] = &[
    (0x00, "Idle"),
    (0x01, "Preheating"),
    (0x02, "Cooking"),
    (0x04, "Cooling"),
    (0x09, "Cleaning"),
];

const OVEN_MODES: &[(u8, &str)] = &[
    (0x00, "None"),
    (0x01, "Bake"),
    (0x03, "Convection Bake"),
    (0x04, "Convection Roast"),
    (0x07, "Broil"),
    (0x08, "Warm"),
    (0x0d, "EasyClean"),
    (0x0f, "Self Clean"),
    (0x10, "Air Fry"),
];

// Fields a write does not set go out as 0x80 "no change", index 11 as 0xFF. Payload 0 and 1 are
// the clock hour and minute and 2 its format, which travels with every clock write; index 11 is
// the panel beeper. The double oven's beeper at index 8 was acknowledged but changed nothing.
const PREFERENCE_COMMAND_PREFIX: [u8; 4] = [0xf0, 0x43, 0x21, 0x0e];
const PREFERENCE_PAYLOAD_LEN: usize = 13;
const PREFERENCE_NO_CHANGE: u8 = 0x80;
const PREFERENCE_FF_INDEX: usize = 11;
const PREFERENCE_FF_NO_CHANGE: u8 = 0xff;
const CLOCK_HOURS_INDEX: usize = 0;
const CLOCK_MINUTES_INDEX: usize = 1;
const CLOCK_FORMAT_INDEX: usize = 2;
const CLOCK_FORMAT_12H: u8 = 0x01;
const CLOCK_FORMAT_24H: u8 = 0x00;
const CLOCK_FORMATS: &[(u8, &str)] =
    &[(CLOCK_FORMAT_12H, "12-hour"), (CLOCK_FORMAT_24H, "24-hour")];
const BEEPER_VOLUME_INDEX: usize = 11;
const BEEPER_VOLUMES: &[(u8, &str)] = &[(0x02, "High"), (0x00, "Mute")];

// Write acknowledgement: 40 00 <command family> <result>, result 0x00 on success.
const ACK_FRAME_TYPE: u8 = 0x00;
const PREFERENCE_COMMAND_FAMILY: u8 = 0x43;

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn code(table: &'static [(u8, &'static str)], label: &str) -> Option<u8> {
    table
        .iter()
        .find(|(_, name)| *name == label)
        .map(|(c, _)| *c)
}

fn preference_payload() -> [u8; PREFERENCE_PAYLOAD_LEN] {
    let mut payload = [PREFERENCE_NO_CHANGE; PREFERENCE_PAYLOAD_LEN];
    payload[PREFERENCE_FF_INDEX] = PREFERENCE_FF_NO_CHANGE;
    payload
}

// HA rejects values outside the declared options, so an undecoded byte publishes "None".
fn oven_state(value: u8) -> Option<&'static str> {
    let state = named(OVEN_STATES, value);
    if state.is_none() {
        tracing::debug!("WLREL6323S undecoded oven state {value:x}");
    }
    state
}

fn oven_mode(value: u8) -> Option<&'static str> {
    let mode = named(OVEN_MODES, value);
    if mode.is_none() {
        tracing::debug!("WLREL6323S undecoded oven mode {value:x}");
    }
    mode
}

fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    // The range cannot be asked for its format: assume 12-hour (the US default and the tested
    // unit's setting) until HA selects otherwise.
    clock_format: Mutex<u8>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let mut cfg = default_config(&meta, Some(json!({"name": "LG Electric Range"})));
        cfg.components = [
            (
                "cooktop_status".into(),
                json!({
                    "platform": "binary_sensor",
                    "icon": "mdi:stove",
                    "unique_id": "$deviceid-cooktop_status",
                    "state_topic": "$this/cooktop_status",
                    "name": "Cooktop",
                    "payload_on": "ON",
                    "payload_off": "OFF",
                }),
            ),
            (
                "oven_status".into(),
                json!({
                    "platform": "sensor",
                    "device_class": "enum",
                    "icon": "mdi:stove",
                    "unique_id": "$deviceid-oven_status",
                    "state_topic": "$this/oven_status",
                    "name": "Oven Status",
                    "options": labels(OVEN_STATES),
                }),
            ),
            // Withdraws the entity this handler published before oven_status took its place.
            ("oven_state".into(), json!({"platform": "sensor"})),
            (
                "oven_mode".into(),
                json!({
                    "platform": "sensor",
                    "device_class": "enum",
                    "icon": "mdi:chef-hat",
                    "unique_id": "$deviceid-oven_mode",
                    "state_topic": "$this/oven_mode",
                    "name": "Oven Mode",
                    "options": labels(OVEN_MODES),
                }),
            ),
            (
                "oven_set_temperature".into(),
                json!({
                    "platform": "sensor",
                    "device_class": "temperature",
                    "icon": "mdi:thermometer-chevron-up",
                    "unique_id": "$deviceid-oven_set_temperature",
                    "state_topic": "$this/oven_set_temperature",
                    "name": "Oven Set Temperature",
                    "unit_of_measurement": "°F",
                }),
            ),
            (
                "oven_door".into(),
                json!({
                    "platform": "binary_sensor",
                    "device_class": "door",
                    "unique_id": "$deviceid-oven_door",
                    "state_topic": "$this/oven_door",
                    "name": "Oven Door",
                    "payload_on": "ON",
                    "payload_off": "OFF",
                }),
            ),
            (
                "clock_sync".into(),
                json!({
                    "platform": "button",
                    "icon": "mdi:clock-check-outline",
                    "unique_id": "$deviceid-clock_sync",
                    "command_topic": "$this/clock_sync/set",
                    "payload_press": "PRESS",
                    "name": "Sync Clock",
                }),
            ),
            // Write-only: selecting a format re-sends the current time in it.
            (
                "clock_format".into(),
                json!({
                    "platform": "select",
                    "icon": "mdi:clock-outline",
                    "unique_id": "$deviceid-clock_format",
                    "command_topic": "$this/clock_format/set",
                    "options": labels(CLOCK_FORMATS),
                    "optimistic": true,
                    "name": "Clock Format",
                }),
            ),
            (
                "beeper_volume".into(),
                json!({
                    "platform": "select",
                    "icon": "mdi:volume-high",
                    "unique_id": "$deviceid-beeper_volume",
                    "command_topic": "$this/beeper_volume/set",
                    "options": labels(BEEPER_VOLUMES),
                    "optimistic": true,
                    "name": "Beeper Volume",
                }),
            ),
        ]
        .into_iter()
        .collect();
        core.set_config(cfg);

        let this = Arc::new(Self {
            core: core.clone(),
            clock_format: Mutex::new(CLOCK_FORMAT_12H),
        });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 4
            && buf[0] == CLASS_BYTE
            && buf[1] == ACK_FRAME_TYPE
            && buf[2] == PREFERENCE_COMMAND_FAMILY
        {
            if buf[3] != 0x00 {
                tracing::debug!("WLREL6323S command rejected by the range {:02x?}", buf);
            }
            return;
        }
        match process_common_status(buf, CLASS_BYTE, STATUS_RECORD_LEN) {
            Some(current) => self.publish_status(current),
            None => tracing::debug!("WLREL6323S undecoded frame {:02x?}", buf),
        }
    }

    fn publish_status(&self, current: &[u8]) {
        self.core.publish_property(
            "oven_status",
            oven_state(current[OVEN_STATE]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "oven_mode",
            oven_mode(current[OVEN_MODE]).unwrap_or("None").into(),
        );
        // Zero is no setpoint (nothing programmed, or a mode without one), not a reading.
        let set = be16(current, OVEN_SET_TEMP);
        if set == 0 {
            self.core
                .publish_property("oven_set_temperature", "None".into());
        } else {
            self.core
                .publish_property("oven_set_temperature", i64::from(set).into());
        }
        self.core
            .publish_on_off("oven_door", current[OVEN_DOOR_FLAGS] & OVEN_DOOR_OPEN != 0);
        let cooktop = COOKTOP_OFFSETS.iter().any(|&off| current[off] != 0);
        self.core.publish_on_off("cooktop_status", cooktop);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "clock_sync" => {
                if mqtt_value == "PRESS" {
                    self.sync_clock();
                }
            }
            "clock_format" => {
                if let Some(format) = code(CLOCK_FORMATS, mqtt_value) {
                    *self.clock_format.lock() = format;
                    self.sync_clock();
                }
            }
            "beeper_volume" => {
                if let Some(volume) = code(BEEPER_VOLUMES, mqtt_value) {
                    self.send_beeper_volume(volume);
                }
            }
            _ => {}
        }
    }

    fn send_preference(&self, payload: &[u8; PREFERENCE_PAYLOAD_LEN]) {
        let mut body = PREFERENCE_COMMAND_PREFIX.to_vec();
        body.extend_from_slice(payload);
        self.core.send(&body);
    }

    fn sync_clock(&self) {
        let now = chrono::Local::now();
        let format = *self.clock_format.lock();
        self.send_clock(now.hour() as u8, now.minute() as u8, format);
    }

    /// `hours` is always 0-23; the 12-hour format carries it as 1-12.
    fn send_clock(&self, hours: u8, minutes: u8, format: u8) {
        let mut payload = preference_payload();
        payload[CLOCK_HOURS_INDEX] = if format == CLOCK_FORMAT_12H {
            match hours % 12 {
                0 => 12,
                h => h,
            }
        } else {
            hours
        };
        payload[CLOCK_MINUTES_INDEX] = minutes;
        payload[CLOCK_FORMAT_INDEX] = format;
        self.send_preference(&payload);
    }

    fn send_beeper_volume(&self, volume: u8) {
        let mut payload = preference_payload();
        payload[BEEPER_VOLUME_INDEX] = volume;
        self.send_preference(&payload);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode(STATUS_QUERY));
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        Device::set_property(self, prop, value);
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
    use crate::device_trait::DeviceHandler;
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, Thinq2Device};
    use std::collections::BTreeMap;

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("WLREL6323S", Device::new)
    }

    fn props(ha: &rethink_core::MockHaConnection) -> BTreeMap<String, String> {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.as_string()))
            .collect()
    }

    // Real captures from the physical panel, 2026-08-30. EC frames carry the previous 62-byte
    // record followed by the current one.
    const IDLE_INITIAL_STATUS: &str =
        "AA4440EB0000000000000000000000000000000E0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000072BB";
    // Left-front, left-rear and right-front elements all produced this same aggregate transition.
    const COOKTOP_OFF_TO_ON: &str =
        "AA8240EC0000000000000000000000000000000E0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000001000100000000000000000000000000000000000000000000003BBB";
    const COOKTOP_ON_TO_OFF: &str =
        "AA8240EC00000000000000000000000000000006000000000000000000000000000000000000000001000100000000000000000000000000000000000000000000000000000000000000000000000000000E000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003BBB";
    // The center-rear warming zone sets the adjacent pair of aggregate bytes (37 and 39).
    const WARMING_ZONE_OFF_TO_ON: &str =
        "AA8240EC0000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000010001000000000000000000000000000000000000000000000033BB";
    const OVEN_DOOR_CLOSED_TO_OPEN: &str =
        "aa8240ec0000000000000000000000000000000e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080000000e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000029bb";
    const OVEN_DOOR_OPEN_TO_CLOSED: &str =
        "aa8240ec0000000000000000000000080000000e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000029bb";
    // Panel-started Bake at 350°F (0x015E), cancelled ten seconds later.
    const BAKE_350_PREHEATING: &str =
        "aa8240ec00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000101000000015e0000000000000000230000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000089bb";
    const BAKE_CANCELLED: &str =
        "aa8240ec0101000000015e000000000000000023000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000bfbb";
    // Further panel-started modes, 2026-09-07.
    const CONVECTION_BAKE_PREHEATING: &str =
        "aa8240ec0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010300000001450000000000000000230000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000090bb";
    const CONVECTION_ROAST_PREHEATING: &str =
        "aa8240ec0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010400000001450000000000000000230000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000093bb";
    const AIR_FRY_COOKING: &str =
        "aa8240ec000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002100000000190000000000000000023000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004bbb";
    const BROIL_COOKING: &str =
        "aa8240ec00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000207000000000000000000000000004200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f6bb";
    const WARM_COOKING: &str =
        "aa8240ec00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000208000000000000000000000000004200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f1bb";
    const EASY_CLEAN: &str =
        "aa8240ec0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000090d000a00000000000000000000004200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000efbb";
    const SELF_CLEAN: &str =
        "aa8240ec0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000090f000004000000000000000000004200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e3bb";
    const SELF_CLEAN_CANCELLED: &str =
        "aa8240ec090f313b0300000000000001000000420000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004000000000000000000000100000002000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000007cbb";

    #[test]
    fn publishes_read_only_oven_and_aggregate_cooktop_components() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        assert_eq!(comps["cooktop_status"]["platform"], "binary_sensor");
        assert_eq!(comps["oven_status"]["device_class"], "enum");
        assert_eq!(
            comps["oven_status"]["options"],
            json!(["Idle", "Preheating", "Cooking", "Cooling", "Cleaning"])
        );
        assert_eq!(comps["oven_mode"]["device_class"], "enum");
        assert_eq!(
            comps["oven_mode"]["options"],
            json!([
                "None",
                "Bake",
                "Convection Bake",
                "Convection Roast",
                "Broil",
                "Warm",
                "EasyClean",
                "Self Clean",
                "Air Fry",
            ])
        );
        assert_eq!(comps["oven_set_temperature"]["unit_of_measurement"], "°F");
        assert_eq!(comps["oven_door"]["device_class"], "door");
        assert!(comps["oven_door"].get("icon").is_none());
        assert!(comps["oven_status"].get("command_topic").is_none());
        assert_eq!(comps["clock_sync"]["platform"], "button");
        assert_eq!(comps["clock_sync"]["command_topic"], "$this/clock_sync/set");
        assert_eq!(comps["clock_format"]["platform"], "select");
        assert_eq!(
            comps["clock_format"]["options"],
            json!(["12-hour", "24-hour"])
        );
        assert_eq!(comps["clock_format"]["optimistic"], true);
        assert_eq!(comps["beeper_volume"]["platform"], "select");
        assert_eq!(comps["beeper_volume"]["options"], json!(["High", "Mute"]));
        assert_eq!(comps["beeper_volume"]["optimistic"], true);
    }

    #[test]
    fn withdraws_the_entity_renamed_to_oven_status() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        assert_eq!(comps["oven_state"], json!({"platform": "sensor"}));
        assert_eq!(comps["oven_status"]["unique_id"], "$deviceid-oven_status");
    }

    #[test]
    fn start_sends_the_captured_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA28F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000F3BB"
        );
    }

    #[test]
    fn decodes_the_initial_idle_status() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(IDLE_INITIAL_STATUS));
        let expected: BTreeMap<String, String> = [
            ("oven_status", "Idle"),
            ("oven_mode", "None"),
            ("oven_set_temperature", "None"),
            ("oven_door", "OFF"),
            ("cooktop_status", "OFF"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(props(&ha), expected);
    }

    #[test]
    fn decodes_aggregate_cooktop_transitions_from_the_current_ec_record() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(COOKTOP_OFF_TO_ON));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(COOKTOP_ON_TO_OFF));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("OFF"));
    }

    #[test]
    fn includes_the_warming_zone_in_aggregate_cooktop_status() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(WARMING_ZONE_OFF_TO_ON));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("ON"));
    }

    #[test]
    fn decodes_oven_door_open_and_closed_transitions() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(OVEN_DOOR_CLOSED_TO_OPEN));
        assert_eq!(prop(&ha, "oven_door").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(OVEN_DOOR_OPEN_TO_CLOSED));
        assert_eq!(prop(&ha, "oven_door").as_deref(), Some("OFF"));
    }

    #[test]
    fn decodes_panel_started_bake_at_350_and_cancellation() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(BAKE_350_PREHEATING));
        assert_eq!(prop(&ha, "oven_status").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "oven_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "oven_set_temperature").as_deref(), Some("350"));

        thinq.emit_data(&hex_decode(BAKE_CANCELLED));
        assert_eq!(prop(&ha, "oven_status").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "oven_mode").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "oven_set_temperature").as_deref(), Some("None"));
    }

    #[test]
    fn decodes_every_captured_cooking_mode() {
        let (ha, thinq, _) = make();
        for (packet, status, mode, temperature) in [
            (
                CONVECTION_BAKE_PREHEATING,
                "Preheating",
                "Convection Bake",
                "325",
            ),
            (
                CONVECTION_ROAST_PREHEATING,
                "Preheating",
                "Convection Roast",
                "325",
            ),
            (AIR_FRY_COOKING, "Cooking", "Air Fry", "400"),
            (BROIL_COOKING, "Cooking", "Broil", "None"),
            (WARM_COOKING, "Cooking", "Warm", "None"),
            (EASY_CLEAN, "Cleaning", "EasyClean", "None"),
            (SELF_CLEAN, "Cleaning", "Self Clean", "None"),
            (SELF_CLEAN_CANCELLED, "Cooling", "None", "None"),
        ] {
            thinq.emit_data(&hex_decode(packet));
            assert_eq!(prop(&ha, "oven_status").as_deref(), Some(status), "{mode}");
            assert_eq!(prop(&ha, "oven_mode").as_deref(), Some(mode));
            assert_eq!(
                prop(&ha, "oven_set_temperature").as_deref(),
                Some(temperature),
                "{mode}"
            );
        }
    }

    #[test]
    fn does_not_mask_unknown_oven_mode_bits_into_a_known_mode() {
        assert_eq!(oven_mode(0x81), None);
    }

    // Sent to a live WLREL6323S, both acknowledged 40 00 43 00. 13:11 with byte 2 = 0x00 switched
    // the display to 24-hour; 1:13 with byte 2 = 0x01 restored 12-hour.
    #[test]
    fn clock_writes_reproduce_both_frames_verified_on_the_appliance() {
        let (_, thinq, dev) = make();
        dev.send_clock(13, 11, 0x00);
        dev.send_clock(13, 13, 0x01);
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(
            sent,
            [
                "AA15F043210E0D0B008080808080808080FF80EDBB",
                "AA15F043210E010D018080808080808080FF80FABB",
            ]
        );
    }

    #[test]
    fn twelve_hour_clock_writes_carry_midnight_and_noon_as_12() {
        let (_, thinq, dev) = make();
        dev.send_clock(0, 5, 0x01);
        dev.send_clock(12, 5, 0x01);
        let outbox = thinq.outbox();
        assert_eq!(outbox[0][6], 12);
        assert_eq!(outbox[1][6], 12);
    }

    #[test]
    fn clock_sync_keeps_the_12_hour_format_until_another_is_selected() {
        let (_, thinq, dev) = make();
        let payload = |frame: &[u8]| frame[6..frame.len() - 2].to_vec();

        dev.set_property("clock_sync", "PRESS");
        let first = payload(&thinq.outbox()[0]);
        assert_eq!(first[2], 0x01);
        assert!((1..=12).contains(&first[0]));
        assert_eq!(
            first[3..],
            [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0xff, 0x80]
        );

        dev.set_property("clock_format", "24-hour");
        dev.set_property("clock_sync", "PRESS");
        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 3);
        assert_eq!(payload(&outbox[1])[2], 0x00);
        assert_eq!(payload(&outbox[2])[2], 0x00);
        assert!(payload(&outbox[2])[0] < 24);
    }

    // Mute was sent live and silenced the panel keys; High (0x02) brought them back.
    #[test]
    fn beeper_volume_writes_only_payload_index_11() {
        let (_, thinq, dev) = make();
        dev.set_property("beeper_volume", "Mute");
        dev.set_property("beeper_volume", "High");
        let outbox = thinq.outbox();
        assert_eq!(
            hex_encode(&outbox[0]),
            "AA15F043210E8080808080808080808080008074BB"
        );
        let high = &outbox[1][6..outbox[1].len() - 2];
        for (index, &value) in high.iter().enumerate() {
            let expected = if index == 11 { 0x02 } else { 0x80 };
            assert_eq!(value, expected, "payload index {index}");
        }
    }

    #[test]
    fn invalid_preference_values_send_nothing() {
        let (_, thinq, dev) = make();
        dev.set_property("clock_sync", "not-a-press");
        dev.set_property("clock_format", "36-hour");
        dev.set_property("beeper_volume", "Deafening");
        assert_eq!(thinq.outbox().len(), 0);
    }

    #[test]
    fn write_acknowledgements_are_consumed_without_publishing() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA084000430060BB"));
        assert!(props(&ha).is_empty());
    }

    #[test]
    fn ignores_frames_outside_the_envelope_and_unexpected_status_lengths() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("001122"));
        thinq.emit_data(&hex_decode("AA0840EB000073BB"));
        assert!(props(&ha).is_empty());
    }
}
