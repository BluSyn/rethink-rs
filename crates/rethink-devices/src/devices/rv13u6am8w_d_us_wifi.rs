//! RV13U6AM8W_D_US_WIFI dryer (AABB).

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

fn map_status() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x01, "Starting"),
        (0x03, "Paused"),
        (0x32, "Drying"),
        (0x33, "Cooldown"),
        (0x04, "Finishing"),
    ])
}
fn map_cycles() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x01, "Heavy Duty"),
        (0x03, "Normal"),
        (0x04, "Perm. Press"),
        (0x05, "Delicates"),
        (0x07, "Bedding"),
        (0x10, "Speed Dry"),
        (0x11, "Air Dry"),
        (0x12, "Manual"),
    ])
}
fn map_temps() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x01, "Ultra Low"),
        (0x02, "Low"),
        (0x03, "Medium"),
        (0x04, "Med High"),
        (0x05, "High"),
    ])
}
fn map_dry() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x01, "Damp"),
        (0x02, "Less"),
        (0x03, "Normal"),
        (0x04, "More"),
        (0x05, "Very"),
    ])
}

fn unique_values(m: &HashMap<u8, &'static str>, order: &[u8]) -> Vec<&'static str> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for k in order {
        if let Some(v) = m.get(k) {
            if seen.insert(*v) {
                out.push(*v);
            }
        }
    }
    out
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self { core: core.clone() });

        let status = map_status();
        let cycles = map_cycles();
        let temps = map_temps();
        let dry = map_dry();

        let mut base = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        let mut components = Map::new();
        components.insert(
            "status".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-status",
                "state_topic": "$this/status",
                "name": "Status",
                "icon": "mdi:state-machine",
                "device_class": "enum",
                "options": unique_values(&status, &[0x00,0x01,0x03,0x32,0x33,0x04]),
            }),
        );
        components.insert(
            "remaining_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-remaining_time",
                "state_topic": "$this/remaining_time",
                "name": "Remaining time",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        );
        components.insert(
            "power".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "name": "Power",
                "icon": "mdi:tumble-dryer",
                "device_class": "running",
            }),
        );
        components.insert(
            "drum_running".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-drum_running",
                "state_topic": "$this/drum_running",
                "name": "Drum running",
                "icon": "mdi:rotate-3d-variant",
            }),
        );
        components.insert(
            "cycle".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-cycle",
                "state_topic": "$this/cycle",
                "name": "Cycle",
                "icon": "mdi:tumble-dryer",
                "device_class": "enum",
                "options": unique_values(&cycles, &[0x00,0x01,0x03,0x04,0x05,0x07,0x10,0x11,0x12]),
            }),
        );
        components.insert(
            "temp".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-temp",
                "state_topic": "$this/temp",
                "name": "Temperature",
                "icon": "mdi:thermometer",
                "device_class": "enum",
                "options": unique_values(&temps, &[0x00,0x01,0x02,0x03,0x04,0x05]),
            }),
        );
        components.insert(
            "dry_level".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-dry_level",
                "state_topic": "$this/dry_level",
                "name": "Dry level",
                "icon": "mdi:water-percent",
                "device_class": "enum",
                "options": unique_values(&dry, &[0x00,0x01,0x02,0x03,0x04,0x05]),
            }),
        );
        base.components = components.into_iter().collect();
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_record(&self, rec: &[u8]) {
        if rec.len() < 18 {
            return;
        }
        let phase = rec[2];
        // Off and Finishing keep a stale countdown in rec[4].
        let mins = if phase == 0x00 || phase == 0x04 {
            0
        } else {
            rec[4] as i64
        };
        self.core.publish_property(
            "status",
            map_status().get(&phase).copied().unwrap_or("None").into(),
        );
        self.core.publish_property("remaining_time", mins.into());
        self.core.publish_on_off("power", phase != 0);
        self.core.publish_on_off("drum_running", rec[17] & 0x01 != 0);
        self.core.publish_property(
            "cycle",
            map_cycles()
                .get(&rec[7])
                .copied()
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "temp",
            map_temps()
                .get(&rec[10])
                .copied()
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "dry_level",
            map_dry().get(&rec[9]).copied().unwrap_or("None").into(),
        );
    }

    fn process_aabb(&self, buf: &[u8]) {
        // 29-byte records. 0xEC is previous then current.
        if let Some(rec) = process_common_status(buf, 0x30, 29) {
            self.process_record(rec);
        }
    }

    pub fn set_property(&self, _prop: &str, _mqtt_value: &str) {}
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode("F0ED1121010000001800"));
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
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};

    const SAMPLE_EB_OFF: &str =
        "AA2330EB000000000000000000000000000000000000000000000000000000000000BB";
    const SAMPLE_EB_STARTING: &str =
        "AA2330EB000001003C00000000000000000000000000000000000000000000000000BB";
    const SAMPLE_EB_DRYING: &str =
        "AA2330EB000032002D00000000000000000000000000000000000000000000000000BB";
    const SAMPLE_EC_DRYING: &str =
        "AA4030EC0000000000000000000000000000000000000000000000000000000000000032001E00000000000000000000000000000000000000000000000003BB";
    const SAMPLE_EB_RECONNECT: &str =
        "AA2330EB001B010001000100000000000100000000A8000000000000006400000046BB";
    const SAMPLE_EC_HEAVY_DUTY: &str =
        "AA4030EC001B320036003601000305000100000000A90000000100000064000000001B320035003601000305000100000000A90000530100000064000000AFBB";
    const SAMPLE_EC_MANUAL_STARTING: &str =
        "AA4030EC001B010014001412000005010100000040A80000000000000064000000001B010014001412000001010100000040A8000000000000006400000001BB";
    const SAMPLE_EB_PAUSED: &str =
        "AA2330EB001B030012001412000002010100000040A8000074320000006400000061BB";
    const SAMPLE_EC_RESUME: &str =
        "AA4030EC001B030012001412000002010100000040A80000743200000064000000001B320012001412000002010100000000A900007403000000640000000ABB";
    const SAMPLE_EC_COOLDOWN: &str =
        "AA4030EC000000000000000000000000000000000000000000000000000000000000003300010000000000000000000000000000000000000000000000006FBB";
    const SAMPLE_EC_COOLDOWN_TRUNCATED: &str =
        "AA4030EC001B330001010301000305000100000040A8000BFD33000000640000000BBB";
    const SAMPLE_EC_POWER_OFF: &str =
        "AA4030EC001B320012001412000002010100000000A900007403000000640000000F001412000002010100000000A900009F0300000064000000001B00000F000F00000000000100000040A80000A13200000064000000C1BB";
    const SAMPLE_IDENTITY: &str =
        "AA3730310201534141333839363434323600002DF000008000000000000253414133383936343331300000AEAD000040000000000020BB";
    const LIVE_EB: &str = "AA2330EB001B32001E002603000304000400000000AB0002090100000064000000F7BB";
    const LIVE_EC: [&str; 3] = [
        "AA4030EC001B32001E002603000304000400000000AB0002090100000064000000001B32001E002603000304000400000000AB00020A01000000640000002EBB",
        "AA4030EC001B32001E002603000304000400000000AB00020A0100000064000000001B32001D002603000304000400000000AB00020A01000000640000002EBB",
        "AA4030EC001B32001D002603000304000400000000AB00020A0100000064000000001B32001D002603000304000400000000AB00020B01000000640000002EBB",
    ];
    const LIVE_END: [&str; 2] = [
        "AA4030EC001B330001002C03000304000400000000AB00055B3200010064000000001B040001002C03000304000400000040AA00055B330001006400000038BB",
        "AA4030EC001B040001002C03000304000400000040AA00055B3300010064000000001B000001000100000000000400000040A800055B040001006400000041BB",
    ];
    const LIVE_OFF_RECONNECT: &str =
        "AA2330EB001B000001000100000000000400000040A800055B0400010064000000EFBB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("RV13U6AM8W_D_US_WIFI", Device::new)
    }

    fn emit(thinq: &MockThinq2Device, hex: &str) {
        thinq.emit_data(&hex_decode(hex));
    }

    fn feed(hex: &str) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        emit(&thinq, hex);
        ha
    }

    fn values(ha: &MockHaConnection, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|n| prop(ha, n).unwrap_or_default())
            .collect()
    }

    #[test]
    fn config_exposes_expected_components() {
        let (ha, _, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        for c in [
            "status",
            "remaining_time",
            "power",
            "drum_running",
            "cycle",
            "temp",
            "dry_level",
        ] {
            assert!(comps.contains_key(c), "component {c} present");
        }
        assert!(!comps.contains_key("start_time"));
        assert!(!comps.contains_key("stop_time"));
        let status = comps["status"]["options"].as_array().unwrap();
        assert!(status.iter().any(|o| o == "Drying"));
    }

    #[test]
    fn enum_options_list_every_label_in_table_order() {
        let (ha, _, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(
            comps["status"]["options"],
            json!([
                "Off",
                "Starting",
                "Paused",
                "Drying",
                "Cooldown",
                "Finishing"
            ])
        );
        assert_eq!(
            comps["cycle"]["options"],
            json!([
                "Off",
                "Heavy Duty",
                "Normal",
                "Perm. Press",
                "Delicates",
                "Bedding",
                "Speed Dry",
                "Air Dry",
                "Manual"
            ])
        );
        assert_eq!(
            comps["temp"]["options"],
            json!(["Off", "Ultra Low", "Low", "Medium", "Med High", "High"])
        );
        assert_eq!(
            comps["dry_level"]["options"],
            json!(["Off", "Damp", "Less", "Normal", "More", "Very"])
        );
    }

    #[test]
    fn eb_off_frame_publishes_power_off_and_off_phase() {
        let ha = feed(SAMPLE_EB_OFF);
        assert_eq!(
            values(&ha, &["power", "status", "remaining_time"]),
            ["OFF", "Off", "0"]
        );
    }

    #[test]
    fn eb_starting_frame_publishes_starting_and_60_min() {
        let ha = feed(SAMPLE_EB_STARTING);
        assert_eq!(
            values(&ha, &["power", "status", "remaining_time"]),
            ["ON", "Starting", "60"]
        );
    }

    #[test]
    fn eb_drying_frame_publishes_drying_and_45_min() {
        let ha = feed(SAMPLE_EB_DRYING);
        assert_eq!(
            values(&ha, &["power", "status", "remaining_time"]),
            ["ON", "Drying", "45"]
        );
    }

    #[test]
    fn ec_dual_record_frame_uses_the_current_record_only() {
        let ha = feed(SAMPLE_EC_DRYING);
        assert_eq!(values(&ha, &["status", "remaining_time"]), ["Drying", "30"]);
    }

    #[test]
    fn real_post_reconnect_eb_publishes_starting_1_min() {
        let ha = feed(SAMPLE_EB_RECONNECT);
        assert_eq!(
            values(&ha, &["status", "remaining_time", "power"]),
            ["Starting", "1", "ON"]
        );
    }

    #[test]
    fn real_heavy_duty_ec_publishes_its_current_record() {
        let ha = feed(SAMPLE_EC_HEAVY_DUTY);
        assert_eq!(
            values(&ha, &["status", "remaining_time", "power"]),
            ["Drying", "53", "ON"]
        );
    }

    #[test]
    fn real_manual_cycle_ec_publishes_starting_20_min() {
        let ha = feed(SAMPLE_EC_MANUAL_STARTING);
        assert_eq!(
            values(&ha, &["status", "remaining_time", "power"]),
            ["Starting", "20", "ON"]
        );
    }

    #[test]
    fn real_resume_ec_publishes_its_current_record() {
        let ha = feed(SAMPLE_EC_RESUME);
        assert_eq!(
            values(&ha, &["status", "remaining_time", "power"]),
            ["Drying", "18", "ON"]
        );
    }

    #[test]
    fn paused_record_publishes_paused_18_min_power_on() {
        let ha = feed(SAMPLE_EB_PAUSED);
        assert_eq!(
            values(&ha, &["status", "remaining_time", "power"]),
            ["Paused", "18", "ON"]
        );
    }

    #[test]
    fn ec_cooldown_frame_publishes_cooldown() {
        let ha = feed(SAMPLE_EC_COOLDOWN);
        assert_eq!(
            values(&ha, &["status", "remaining_time", "power"]),
            ["Cooldown", "1", "ON"]
        );
    }

    #[test]
    fn real_heavy_duty_ec_reports_cycle_temp_and_dry_level() {
        let ha = feed(SAMPLE_EC_HEAVY_DUTY);
        assert_eq!(
            values(&ha, &["cycle", "temp", "dry_level"]),
            ["Heavy Duty", "High", "Normal"]
        );
    }

    #[test]
    fn manual_cycle_reports_no_dry_level_and_the_selected_heat() {
        let ha = feed(SAMPLE_EC_MANUAL_STARTING);
        assert_eq!(
            values(&ha, &["cycle", "dry_level", "temp"]),
            ["Manual", "Off", "Ultra Low"]
        );
        let ha = feed(SAMPLE_EC_RESUME);
        assert_eq!(
            values(&ha, &["cycle", "dry_level", "temp"]),
            ["Manual", "Off", "Low"]
        );
    }

    #[test]
    fn drum_running_on_while_drying_and_off_when_paused() {
        assert_eq!(
            prop(&feed(SAMPLE_EC_HEAVY_DUTY), "drum_running").as_deref(),
            Some("ON")
        );
        assert_eq!(
            prop(&feed(SAMPLE_EB_PAUSED), "drum_running").as_deref(),
            Some("OFF")
        );
    }

    #[test]
    fn drum_running_off_while_starting() {
        assert_eq!(
            prop(&feed(SAMPLE_EC_MANUAL_STARTING), "drum_running").as_deref(),
            Some("OFF")
        );
    }

    #[test]
    fn start_requests_status_with_the_read_only_query() {
        let (_, thinq, dev) = make();
        dev.start();
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(sent, ["AA0EF0ED1121010000001800B5BB"]);
    }

    #[test]
    fn acks_identity_and_heartbeat_but_not_status_records() {
        let (_, thinq, _) = make();
        emit(&thinq, SAMPLE_IDENTITY);
        emit(&thinq, "AA09307200C9004BBB");
        emit(&thinq, LIVE_EB);
        for f in LIVE_EC {
            emit(&thinq, f);
        }
        let acks: Vec<String> = thinq
            .sent()
            .iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap().to_string())
            .collect();
        assert_eq!(acks, ["AA08F000310482BB", "AA08F00072044DBB"]);
    }

    #[test]
    fn consecutive_ec_frames_chain() {
        let inners: Vec<Vec<u8>> = LIVE_EC
            .iter()
            .map(|f| {
                let b = hex_decode(f);
                b[2..b.len() - 2].to_vec()
            })
            .collect();
        for i in 1..inners.len() {
            assert_eq!(inners[i][2..31], inners[i - 1][31..60]);
        }
    }

    #[test]
    fn live_snapshot_then_updates_count_down() {
        let (ha, thinq, _) = make();
        emit(&thinq, LIVE_EB);
        assert_eq!(
            values(
                &ha,
                &[
                    "status",
                    "remaining_time",
                    "cycle",
                    "temp",
                    "dry_level",
                    "drum_running"
                ]
            ),
            ["Drying", "30", "Normal", "Med High", "Normal", "ON"]
        );
        let mins: Vec<String> = LIVE_EC
            .iter()
            .map(|f| {
                emit(&thinq, f);
                prop(&ha, "remaining_time").unwrap()
            })
            .collect();
        assert_eq!(mins, ["30", "29", "29"]);
    }

    #[test]
    fn end_of_cycle_publishes_finishing_then_off_with_0_min() {
        let (ha, thinq, _) = make();
        emit(&thinq, LIVE_EC[2]);
        emit(&thinq, LIVE_END[0]);
        assert_eq!(
            values(&ha, &["status", "remaining_time", "drum_running"]),
            ["Finishing", "0", "OFF"]
        );
        emit(&thinq, LIVE_END[1]);
        assert_eq!(
            values(
                &ha,
                &[
                    "status",
                    "remaining_time",
                    "power",
                    "cycle",
                    "temp",
                    "dry_level"
                ]
            ),
            ["Off", "0", "OFF", "Off", "Off", "Off"]
        );
    }

    #[test]
    fn reconnect_snapshot_while_off_publishes_0_min() {
        let ha = feed(LIVE_OFF_RECONNECT);
        assert_eq!(
            values(&ha, &["status", "remaining_time", "cycle", "dry_level"]),
            ["Off", "0", "Off", "Off"]
        );
    }

    #[test]
    fn no_published_value_is_none() {
        let (ha, thinq, _) = make();
        let frames = [LIVE_EB]
            .into_iter()
            .chain(LIVE_EC)
            .chain(LIVE_END)
            .chain([LIVE_OFF_RECONNECT]);
        for f in frames {
            emit(&thinq, f);
        }
        for (k, v) in ha.device(DEVICE_ID).unwrap().properties {
            assert_ne!(v.as_string(), "None", "{k}");
        }
    }

    #[test]
    fn frames_with_another_class_byte_are_ignored() {
        let ha = feed("AA2320EB000000000000000000000000000000000000000000000000000000000000BB");
        assert!(prop(&ha, "power").is_none());
    }

    #[test]
    fn frames_that_are_too_short_are_ignored() {
        assert!(prop(&feed("AA0430EB00BB"), "power").is_none());
    }

    #[test]
    fn truncated_cooldown_capture_is_ignored() {
        assert!(prop(&feed(SAMPLE_EC_COOLDOWN_TRUNCATED), "power").is_none());
    }

    #[test]
    fn three_record_power_off_capture_is_ignored() {
        assert!(prop(&feed(SAMPLE_EC_POWER_OFF), "power").is_none());
    }

    #[test]
    fn identity_packet_is_ignored() {
        assert!(prop(&feed(SAMPLE_IDENTITY), "power").is_none());
    }

    #[test]
    fn undecoded_status_value_publishes_none() {
        let ha = feed("AA2330EB0000FF000000000000000000000000000000000000000000000000000000BB");
        assert_eq!(prop(&ha, "status").as_deref(), Some("None"));
    }
}
