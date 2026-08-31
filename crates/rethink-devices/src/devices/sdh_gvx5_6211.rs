//! SDH_GVX5_6211 — LG RHX5009NHB heat-pump dryer (AABB class 0x30).
//!
//! Port of unmerged upstream PR #150. 13-byte header, 50-byte blocks led by
//! 0x0a; 0xEC current block is LAST. Offsets from 2026-08-26 live captures.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const HEADER_LENGTH: usize = 13;
const BLOCK_LENGTH: usize = 50;
const BLOCK_MARKER: u8 = 0x0a;
const STATUS_REQUEST: &[u8] = &[0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x00];
const RUN_TOGGLE: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x03, 0x02];
const START_COMMIT: &[u8] = &[0xf0, 0x24, 0x12, 0x01, 0x14];
const COURSE_PROGRAM_PREFIX: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x02, 0x0a];
const COURSE_PROGRAM_SUFFIX: &[u8] = &[0x03, 0x01];

const DRY_LEVEL_OFFSET: usize = 1;
const DRY_MODE_OFFSET: usize = 2;
const DELAY_OFFSET: usize = 7;
const REMAIN_TIME_OFFSET: usize = 9;
const INITIAL_TIME_OFFSET: usize = 11;
const PHASE_OFFSET: usize = 13;
const ERROR_OFFSET: usize = 15;
const ENERGY_OFFSET: usize = 17;
const FLAGS0_OFFSET: usize = 19;
const COURSE_OFFSET: usize = 20;
const FLAGS_OFFSET: usize = 24;
const FLAGS2_OFFSET: usize = 26;
const POWER_OFFSET: usize = 27;
const PHASE_OFF: u8 = 0x00;
const PHASE_ERROR: u8 = 0x05;
const POWER_BIT: u8 = 0x80;
const FLAG0_SOUND: u8 = 0x04;
const FLAG_ANTI_CREASE: u8 = 0x08;
const FLAG_DRUM_LIGHT: u8 = 0x20;
const FLAG_MOISTURE_ALERT: u8 = 0x40;
const FLAG2_DELAY: u8 = 0x08;
const FLAG2_CHILD_LOCK: u8 = 0x10;
const FLAG2_ACTIVE: u8 = 0x40;

fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

fn dry_level(v: u8) -> &'static str {
    match v {
        0x01 => "Iron",
        0x03 => "Cupboard",
        0x05 => "Extra",
        _ => "unknown",
    }
}

fn dry_mode(v: u8) -> &'static str {
    match v {
        0x02 => "Efficiency",
        0x03 => "Turbo",
        _ => "unknown",
    }
}

fn phase_name(v: u8) -> &'static str {
    match v {
        0x00 => "Off",
        0x01 => "Initial",
        0x03 => "Pause",
        0x04 => "End",
        0x05 => "Error",
        0x07 => "Drying",
        0x08 => "Finishing",
        0x11 => "Cooling",
        _ => "Running",
    }
}

fn course_name(v: u8) -> &'static str {
    match v {
        0x02 => "Towels",
        0x05 => "Synthetics",
        0x06 => "Mixed Fabric",
        0x07 => "Cotton",
        0x08 => "Sportswear",
        0x0a => "My Program",
        0x0b => "Wool",
        0x10 => "Allergy Care",
        0x12 => "Condenser Cleaning",
        0x13 => "Drum Cleaning",
        0x15 => "Time Dry",
        0x19 => "Eco",
        0x1c => "AI Dry",
        0x26 => "Turbo Dry",
        _ => "unknown",
    }
}

fn error_name(v: u8) -> String {
    match v {
        0x10 => "dE1 (door)".into(),
        n => format!("unknown (0x{n:x})"),
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    last_course: Mutex<u8>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let mut cfg = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        cfg.components = [
            ("power".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:power"})),
            ("power_button".into(), json!({"platform":"button","unique_id":"$deviceid-power_button","command_topic":"$this/power_button/set","payload_press":"","name":"Power","icon":"mdi:power"})),
            ("start".into(), json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
            ("pause".into(), json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
            ("status".into(), json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine"})),
            ("error".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-error","state_topic":"$this/error","name":"Error","icon":"mdi:alert-circle-outline","device_class":"problem","entity_category":"diagnostic"})),
            ("error_message".into(), json!({"platform":"sensor","unique_id":"$deviceid-error-message","state_topic":"$this/error_message","name":"Error message","icon":"mdi:alert-circle-outline","entity_category":"diagnostic"})),
            ("course".into(), json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
            ("dry_level".into(), json!({"platform":"sensor","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","name":"Dry level","icon":"mdi:water-percent"})),
            ("dry_mode".into(), json!({"platform":"sensor","unique_id":"$deviceid-dry_mode","state_topic":"$this/dry_mode","name":"Drying mode","icon":"mdi:fan"})),
            ("remaining_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
            ("energy".into(), json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
            ("initial_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
            ("delay".into(), json!({"platform":"sensor","unique_id":"$deviceid-delay","state_topic":"$this/delay","name":"Delay start","icon":"mdi:clock-start","device_class":"duration","unit_of_measurement":"min"})),
            ("anti_crease".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-anti_crease","state_topic":"$this/anti_crease","name":"Anti-crease","icon":"mdi:tshirt-crew-outline"})),
            ("moisture_alert".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-moisture_alert","state_topic":"$this/moisture_alert","name":"Moisture alert","icon":"mdi:water-alert-outline"})),
            ("drum_light".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-drum_light","state_topic":"$this/drum_light","name":"Drum light","device_class":"light"})),
            ("sound".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-sound","state_topic":"$this/sound","name":"Sound","icon":"mdi:volume-high","entity_category":"diagnostic"})),
            ("child_lock".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:account-lock","entity_category":"diagnostic"})),
            ("remote_start".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
        ]
        .into_iter()
        .collect();
        core.set_config(cfg);

        let this = Arc::new(Self {
            core: core.clone(),
            last_course: Mutex::new(0),
        });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first() != Some(&0x30) || buf.len() < HEADER_LENGTH + BLOCK_LENGTH {
            return;
        }
        let payload_len = be16(buf, 11) as usize;
        if buf.len() < HEADER_LENGTH + payload_len {
            return;
        }
        if buf[10] == 0xec && payload_len == BLOCK_LENGTH * 2 {
            self.process_status(&buf[HEADER_LENGTH + BLOCK_LENGTH..HEADER_LENGTH + BLOCK_LENGTH * 2]);
        } else if buf[10] == 0xeb && payload_len == BLOCK_LENGTH {
            self.process_status(&buf[HEADER_LENGTH..HEADER_LENGTH + BLOCK_LENGTH]);
        }
    }

    fn process_status(&self, rec: &[u8]) {
        if rec.first() != Some(&BLOCK_MARKER) || rec.len() < BLOCK_LENGTH {
            return;
        }
        *self.last_course.lock() = rec[COURSE_OFFSET];
        let phase = rec[PHASE_OFFSET];
        let is_off = rec[POWER_OFFSET] & POWER_BIT == 0 || phase == PHASE_OFF;
        let error = if phase == PHASE_ERROR {
            rec[ERROR_OFFSET]
        } else {
            0
        };

        self.core.publish_on_off("power", !is_off);
        self.core.publish_property(
            "error_message",
            if error != 0 {
                error_name(error)
            } else {
                "none".into()
            }
            .into(),
        );
        self.core.publish_on_off("error", error != 0);
        self.core.publish_property(
            "status",
            if is_off { "Off" } else { phase_name(phase) }.into(),
        );
        self.core
            .publish_property("course", course_name(rec[COURSE_OFFSET]).into());
        self.core
            .publish_property("dry_level", dry_level(rec[DRY_LEVEL_OFFSET]).into());
        self.core
            .publish_property("dry_mode", dry_mode(rec[DRY_MODE_OFFSET]).into());
        self.core.publish_property(
            "remaining_time",
            if is_off {
                0i64
            } else {
                i64::from(be16(rec, REMAIN_TIME_OFFSET))
            }
            .into(),
        );
        self.core.publish_property(
            "initial_time",
            if is_off {
                0i64
            } else {
                i64::from(be16(rec, INITIAL_TIME_OFFSET))
            }
            .into(),
        );
        self.core
            .publish_property("energy", i64::from(be16(rec, ENERGY_OFFSET)).into());

        let flags2 = rec[FLAGS2_OFFSET];
        self.core.publish_property(
            "delay",
            if flags2 & FLAG2_DELAY != 0 {
                i64::from(be16(rec, DELAY_OFFSET))
            } else {
                0
            }
            .into(),
        );
        if phase == 0x01 || is_off {
            self.core
                .publish_on_off("remote_start", flags2 & FLAG2_ACTIVE != 0);
        }
        let flags = rec[FLAGS_OFFSET];
        self.core
            .publish_on_off("anti_crease", flags & FLAG_ANTI_CREASE != 0);
        self.core
            .publish_on_off("moisture_alert", flags & FLAG_MOISTURE_ALERT != 0);
        self.core
            .publish_on_off("drum_light", flags & FLAG_DRUM_LIGHT != 0);
        self.core
            .publish_on_off("sound", rec[FLAGS0_OFFSET] & FLAG0_SOUND != 0);
        self.core
            .publish_on_off("child_lock", flags2 & FLAG2_CHILD_LOCK != 0);
    }

    pub fn set_property(&self, prop: &str, _mqtt_value: &str) {
        if prop == "power_button" {
            self.core.send(&hex_decode("F02A0100"));
        }
        if prop == "pause" {
            self.core.send(RUN_TOGGLE);
        }
        if prop == "start" {
            let course = *self.last_course.lock();
            if course == 0 {
                return;
            }
            let mut pkt = COURSE_PROGRAM_PREFIX.to_vec();
            pkt.push(course);
            pkt.extend_from_slice(COURSE_PROGRAM_SUFFIX);
            self.core.send(&pkt);
            self.core.send(RUN_TOGGLE);
            self.core.send(START_COMMIT);
        }
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

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("SDH_GVX5_6211", Device::new)
    }

    const SAMPLE_EB_OFF: &str =
        "AAFF300A00440008A2000100EB00320A000000000000000000000000010000000000040000008F20000000050000000600000000000000000000000000000000006B0DBB";
    const SAMPLE_EC_POWERED_IDLE: &str =
        "AAFF300A00760008A9000100EC00640A000000000000000000000000010000000000040000008F20000000050000000600000000000000000000000000000000000A000000000000000000000000010000000000040000008F2000008005000000060000000000000000000000000000000000CD40BB";
    const SAMPLE_EC_DELAY_8H: &str =
        "AAFF300A00760008CD000100EC00640A030300000700016800960096010000020000040700008F00000880050000000600000000000000000000000000000000000A03030000070001E000960096010000020000040700008F0000088005000000060000000000000000000000000000000000DFD6BB";
    const SAMPLE_EC_ERROR_DE1: &str =
        "AAFF300A00760008DB000100EC00640A030300000700000000960096010000020000040700008F00000080050000000600000000000000000000000000000000000A030300000700000000960096050110020000040700008F00000080050000000600000000000000000000000000000000002472BB";
    const SAMPLE_EC_SOUND_OFF: &str =
        "AAFF300A0076000037000100EC00640A000300001200000000410041010000030000041200008F00004080050000000600000000000000000000000000000000000A000300001200000000410041010000030000001200008F0000408005000000060000000000000000000000000000000000B31ABB";
    const SAMPLE_EC_CHILD_LOCK: &str =
        "AAFF300A0076000039000100EC00640A000300001200000000410041010000030000041200008F00004080050000000600000000000000000000000000000000000A000300001200000000410041010000030000041200008F0000508005000000060000000000000000000000000000000000A304BB";
    const SAMPLE_EC_ANTI_CREASE: &str =
        "AAFF300A00760008E9000100EC00640A030300000700000000960096010000020000040700008F20000080050000000600000000000000000000000000000000000A030300000700000000960096010000020000040700008F2800008005000000060000000000000000000000000000000000DFF9BB";
    const SAMPLE_EC_REMOTE_START: &str =
        "AAFF300A00760008F1000100EC00640A030300000700000000960096010000020000040700008F00000080050000000600000000000000000000000000000000000A030300000700000000960096010000020000040700008F000040800500000006000000000000000000000000000000000005F7BB";
    const SAMPLE_EC_DRYING: &str =
        "AAFF300A0076000901000100EC00640A000300001500000000140014070100020000041500008F00004080050000000600000000000000000000000000000000000A000300001500000000130014070100020000041500008F0000408005000000060000000000000000000000000000000000F59CBB";
    const SAMPLE_EC_PAUSED: &str =
        "AAFF300A0076000913000100EC00640A00030000150000000011001407010002000D041500008F00004080050000000600000000000000000000000000000000000A00030000150000000011001403070002000D041500008F20000080050000000600000000000000000000000000000000003498BB";
    const SAMPLE_EC_OFF_AFTER_USE: &str =
        "AAFF300A00760009AC000100EC00640A000000000000000000000000010000000000040000008F20000080050000000600000000000000000000000000000000000A000000000000000000000000000100000000040000008F00000080050000000600000000000000000000000000000000001912BB";
    const SAMPLE_EC_END_284WH: &str =
        "AAFF300A007600065C000100EC00640A00030002150000000001001E08110005011B041500008F00004080050000000600000000000000000000000000000000000A00030002150000000001001E04080007011C041500008F2000008005000000060000000000000000000000000000000000B4F2BB";
    const SAMPLE_EC_END_144WH: &str =
        "AAFF300A007600071D000100EC00640A00030000150000000001001411070005008E041500008F00006080050000000600000000000000000000000000000000000A000300001500000000010014081100050090041500008F000040800500000006000000000000000000000000000000000023DFBB";

    #[test]
    fn config_and_energy() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        for c in [
            "power", "power_button", "start", "pause", "status", "error", "error_message", "course",
            "dry_level", "dry_mode", "remaining_time", "initial_time", "energy", "delay",
            "anti_crease", "moisture_alert", "drum_light", "sound", "child_lock", "remote_start",
        ] {
            assert!(comps.contains_key(c), "missing {c}");
        }
        assert_eq!(comps["power"]["platform"], "binary_sensor");
        assert_eq!(comps["power_button"]["platform"], "button");

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_END_284WH));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("284"));
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_END_144WH));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("144"));
    }

    #[test]
    fn start_query_and_off_states() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1121010000001800B5BB");

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EB_OFF));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_POWERED_IDLE));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Initial"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("unknown"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_OFF_AFTER_USE));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
    }

    #[test]
    fn delay_error_flags_and_run() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_DELAY_8H));
        assert_eq!(prop(&ha, "delay").as_deref(), Some("480"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Cupboard"));
        assert_eq!(prop(&ha, "dry_mode").as_deref(), Some("Turbo"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("150"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_ERROR_DE1));
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("dE1 (door)"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Error"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_ANTI_CREASE));
        assert_eq!(prop(&ha, "anti_crease").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "moisture_alert").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "drum_light").as_deref(), Some("ON"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_SOUND_OFF));
        assert_eq!(prop(&ha, "sound").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Condenser Cleaning"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_CHILD_LOCK));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_REMOTE_START));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_DRYING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Time Dry"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("19"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("20"));
        assert!(prop(&ha, "remote_start").is_none());

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_PAUSED));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Pause"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("17"));
    }

    #[test]
    fn writes() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power_button", "");
        dev.set_property("nonsense", "");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA08F02A010098BB");

        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0DF0E5000201FF010302C1BB");

        thinq.reset_recorder();
        dev.set_property("start", "");
        assert!(thinq.outbox().is_empty());

        thinq.emit_data(&hex_decode(SAMPLE_EC_DRYING));
        thinq.reset_recorder();
        dev.set_property("start", "");
        let out = thinq.outbox();
        assert_eq!(out.len(), 3);
        assert_eq!(hex_encode(&out[0]), "AA0FF0E5000201FF020A150301E0BB");
        assert_eq!(hex_encode(&out[1]), "AA0DF0E5000201FF010302C1BB");
        assert_eq!(hex_encode(&out[2]), "AA09F024120114BBBB");
    }

    #[test]
    fn ignores_non_30() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA08F02A010098BB"));
        assert!(prop(&ha, "status").is_none());
    }
}
