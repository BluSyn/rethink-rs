//! RH90V9_WW — LG heat-pump dryer (AABB class 0x30).
//!
//! Port of closed upstream PR #55. Dual 0xEC records (header + A + B) and compact
//! 0xEB poll replies share the same Bd field map from modelJSON + wire captures.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::PropertyValue;
use rethink_core::{hex_decode, HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const FLAG14_ANTI_CREASE: u8 = 0x02;
const FLAG15_REMOTE_START: u8 = 0x01;
const PING: &[u8] = &[
    0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x04, 0x13, 0x14, 0x00, 0x00, 0x5a,
];

fn state_name(code: u8) -> String {
    match code {
        0 => "Off",
        1 => "Initial",
        2 => "Running",
        3 => "Paused",
        4 => "End",
        5 => "Error",
        8 => "Smart Diagnosis",
        100 => "Reserved",
        n => return format!("unknown ({n})"),
    }
    .into()
}

fn cycle_name(id: u8) -> &'static str {
    match id {
        0x02 => "Towels",
        0x04 => "Duvet",
        0x05 => "Easy Care",
        0x06 => "Mixed Fabric",
        0x07 => "Cotton",
        0x08 => "Sports Wear",
        0x09 => "Speed 30",
        0x0a => "Delicates",
        0x0b => "Wool",
        0x0c => "Rack Dry",
        0x0e => "Warm Air",
        0x10 => "Allergy Care",
        0x12 => "Condenser Care",
        0x13 => "Drum Care",
        0x19 => "Eco (Cotton+)",
        _ => "",
    }
}

fn cycle_id(name: &str) -> Option<u8> {
    match name {
        "Towels" => Some(0x02),
        "Duvet" => Some(0x04),
        "Easy Care" => Some(0x05),
        "Mixed Fabric" => Some(0x06),
        "Cotton" => Some(0x07),
        "Sports Wear" => Some(0x08),
        "Speed 30" => Some(0x09),
        "Delicates" => Some(0x0a),
        "Wool" => Some(0x0b),
        "Rack Dry" => Some(0x0c),
        "Warm Air" => Some(0x0e),
        "Allergy Care" => Some(0x10),
        "Condenser Care" => Some(0x12),
        "Drum Care" => Some(0x13),
        "Eco (Cotton+)" => Some(0x19),
        _ => None,
    }
}

const CYCLE_OPTIONS: &[&str] = &[
    "None",
    "Towels",
    "Duvet",
    "Easy Care",
    "Mixed Fabric",
    "Cotton",
    "Sports Wear",
    "Speed 30",
    "Delicates",
    "Wool",
    "Rack Dry",
    "Warm Air",
    "Allergy Care",
    "Condenser Care",
    "Drum Care",
    "Eco (Cotton+)",
];

fn dry_level_name(code: u8) -> String {
    match code {
        0 => "None",
        1 => "Iron Dry",
        3 => "Cupboard Dry",
        4 => "Extra Dry",
        n => return format!("unknown ({n})"),
    }
    .into()
}

fn dry_level_id(name: &str) -> Option<u8> {
    match name {
        "None" => Some(0),
        "Iron Dry" => Some(1),
        "Cupboard Dry" => Some(3),
        "Extra Dry" => Some(4),
        _ => None,
    }
}

fn eco_name(code: u8) -> String {
    match code {
        0 => "None",
        1 => "Energy",
        2 => "Normal",
        3 => "Time",
        n => return format!("unknown ({n})"),
    }
    .into()
}

fn eco_id(name: &str) -> Option<u8> {
    match name {
        "None" => Some(0),
        "Energy" => Some(1),
        "Normal" => Some(2),
        "Time" => Some(3),
        _ => None,
    }
}

fn process_name(code: u8) -> String {
    match code {
        0 => "Detecting",
        1 => "Steam",
        2 | 3 | 4 => "Dry",
        5 => "Cooling",
        6 => "Anti-crease",
        7 => "End",
        n => return format!("unknown ({n})"),
    }
    .into()
}

fn error_name(code: u8) -> String {
    match code {
        0 => "-",
        1 => "TE1 (temperature)",
        2 => "TE2 (temperature)",
        4 => "TE4 (temperature)",
        7 => "CE1 (communication)",
        13 => "OE (drain motor)",
        14 => "Empty water tank",
        15 => "dE (door open)",
        17 => "No filter",
        19 => "F1",
        20 => "LE2 (motor)",
        21 => "AE",
        30 => "LE1 (motor)",
        37 => "DE4 (door)",
        42 => "DE2 (door lock)",
        n => return format!("unknown (0x{n:x})"),
    }
    .into()
}

fn reservation_name(h: u8) -> String {
    if h == 0 {
        "Off".into()
    } else if (3..=19).contains(&h) {
        format!("{h}h")
    } else {
        "Off".into()
    }
}

fn reservation_id(label: &str) -> Option<u8> {
    if label == "Off" {
        return Some(0);
    }
    label.strip_suffix('h')?.parse().ok()
}

struct Downloaded {
    label: &'static str,
    base_id: u8,
    default_dry: u8,
    default_eco: u8,
    default_time: u8,
}

fn downloaded(id: u8) -> Option<Downloaded> {
    match id {
        0x65 => Some(Downloaded { label: "Baby Wear", base_id: 0x02, default_dry: 0, default_eco: 3, default_time: 130 }),
        0x66 => Some(Downloaded { label: "Gym Clothes", base_id: 0x08, default_dry: 0, default_eco: 1, default_time: 60 }),
        0x67 => Some(Downloaded { label: "Blanket", base_id: 0x04, default_dry: 0, default_eco: 3, default_time: 165 }),
        0x68 => Some(Downloaded { label: "Blanket Refresh", base_id: 0x00, default_dry: 0, default_eco: 3, default_time: 30 }),
        0x69 => Some(Downloaded { label: "Rainy Day", base_id: 0x0e, default_dry: 0, default_eco: 3, default_time: 30 }),
        0x6a => Some(Downloaded { label: "Single Garments", base_id: 0x0e, default_dry: 0, default_eco: 3, default_time: 40 }),
        0x6b => Some(Downloaded { label: "Deodorization", base_id: 0x00, default_dry: 0, default_eco: 3, default_time: 39 }),
        0x6c => Some(Downloaded { label: "Small Load", base_id: 0x0e, default_dry: 0, default_eco: 3, default_time: 50 }),
        0x6d => Some(Downloaded { label: "Lingerie", base_id: 0x0a, default_dry: 0, default_eco: 1, default_time: 50 }),
        0x6e => Some(Downloaded { label: "Easy Ironing", base_id: 0x19, default_dry: 1, default_eco: 1, default_time: 110 }),
        0x6f => Some(Downloaded { label: "Super Dry", base_id: 0x19, default_dry: 4, default_eco: 3, default_time: 160 }),
        0x70 => Some(Downloaded { label: "Economic Dry", base_id: 0x19, default_dry: 3, default_eco: 1, default_time: 150 }),
        0x71 => Some(Downloaded { label: "Big Size Item", base_id: 0x04, default_dry: 0, default_eco: 3, default_time: 165 }),
        0x72 => Some(Downloaded { label: "Minimize Wrinkles", base_id: 0x19, default_dry: 3, default_eco: 3, default_time: 130 }),
        0x73 => Some(Downloaded { label: "Shoes / Fabric Doll", base_id: 0x0c, default_dry: 0, default_eco: 1, default_time: 180 }),
        0x74 => Some(Downloaded { label: "Full Size Load", base_id: 0x19, default_dry: 4, default_eco: 3, default_time: 160 }),
        _ => None,
    }
}

fn schema_dry_levels(cycle: u8) -> &'static [u8] {
    match cycle {
        0x05 => &[1, 3],
        0x06 | 0x19 => &[1, 3, 4],
        0x07 => &[1, 3, 4],
        _ => &[],
    }
}

fn schema_eco(cycle: u8) -> &'static [u8] {
    match cycle {
        0x05 | 0x06 | 0x0e | 0x19 => &[1, 3],
        0x08 | 0x0a | 0x0b | 0x0c => &[1],
        _ => &[3],
    }
}

fn schema_default_dry(cycle: u8) -> u8 {
    match cycle {
        0x05 | 0x06 | 0x07 | 0x19 => 3,
        _ => 0,
    }
}

fn schema_default_eco(cycle: u8) -> u8 {
    match cycle {
        0x08 | 0x0a | 0x0b | 0x0c | 0x0e | 0x19 => 1,
        _ => 3,
    }
}

struct Inner {
    last_downloaded: u8,
    last_cycle: u8,
    last_dry: u8,
    last_eco: u8,
    last_anti_crease: bool,
    last_reservation: u8,
    cycle_start_ms: Option<i64>,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    inner: Mutex<Inner>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            inner: Mutex::new(Inner {
                last_downloaded: 0,
                last_cycle: 0,
                last_dry: 0,
                last_eco: 0,
                last_anti_crease: false,
                last_reservation: 0,
                cycle_start_ms: None,
            }),
        });

        let mut base = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        base.components = [
            ("power", json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"","icon":"mdi:tumble-dryer"})),
            ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote-start","state_topic":"$this/remote_start","name":"Remote Start","icon":"mdi:remote"})),
            ("cycle", json!({"platform":"sensor","unique_id":"$deviceid-cycle","state_topic":"$this/cycle","name":"Cycle","icon":"mdi:pin-outline","device_class":"enum","options":CYCLE_OPTIONS})),
            ("dry_level", json!({"platform":"sensor","unique_id":"$deviceid-dry-level","state_topic":"$this/dry_level","name":"Dry level","icon":"mdi:water-percent","device_class":"enum","options":["None","Iron Dry","Cupboard Dry","Extra Dry"]})),
            ("eco_hybrid", json!({"platform":"sensor","unique_id":"$deviceid-eco-hybrid","state_topic":"$this/eco_hybrid","name":"Eco hybrid","icon":"mdi:leaf","device_class":"enum","options":["None","Energy","Normal","Time"]})),
            ("anti_crease", json!({"platform":"switch","unique_id":"$deviceid-anti-crease","state_topic":"$this/anti_crease","command_topic":"$this/anti_crease/set","name":"Anti-crease","icon":"mdi:iron-outline"})),
            ("reservation", json!({"platform":"sensor","unique_id":"$deviceid-reservation","state_topic":"$this/reservation","name":"Delayed end","icon":"mdi:clock-end"})),
            ("start", json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
            ("pause", json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
            ("ping", json!({"platform":"button","unique_id":"$deviceid-ping","command_topic":"$this/ping/set","payload_press":"","name":"Poll (Debug)","icon":"mdi:connection","entity_category":"diagnostic"})),
            ("run_state", json!({"platform":"sensor","unique_id":"$deviceid-run-state","state_topic":"$this/run_state","name":"Run state","icon":"mdi:state-machine"})),
            ("process_state", json!({"platform":"sensor","unique_id":"$deviceid-process-state","state_topic":"$this/process_state","name":"Process state","icon":"mdi:cog-outline"})),
            ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining-time","state_topic":"$this/remaining_time","name":"Remaining time","device_class":"duration","unit_of_measurement":"min"})),
            ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial-time","state_topic":"$this/initial_time","name":"Initial time","device_class":"duration","unit_of_measurement":"min"})),
            ("run_completed", json!({"platform":"binary_sensor","unique_id":"$deviceid-run-completed","state_topic":"$this/run_completed","name":"Run completed","icon":"mdi:check-circle-outline"})),
            ("cycle_start_time", json!({"platform":"sensor","unique_id":"$deviceid-cycle-start-time","state_topic":"$this/cycle_start_time","name":"Cycle start time","device_class":"timestamp"})),
            ("cycle_end_time", json!({"platform":"sensor","unique_id":"$deviceid-cycle-end-time","state_topic":"$this/cycle_end_time","name":"Cycle end time","device_class":"timestamp"})),
            ("cycle_duration", json!({"platform":"sensor","unique_id":"$deviceid-cycle-duration","state_topic":"$this/cycle_duration","name":"Cycle duration","device_class":"duration","unit_of_measurement":"min"})),
            ("error", json!({"platform":"binary_sensor","unique_id":"$deviceid-error","state_topic":"$this/error","name":"Error state","device_class":"problem","entity_category":"diagnostic"})),
            ("error_message", json!({"platform":"sensor","unique_id":"$deviceid-error-message","state_topic":"$this/error_message","name":"Error message","entity_category":"diagnostic"})),
            ("downloaded_cycle_id", json!({"platform":"sensor","unique_id":"$deviceid-downloaded-cycle-id","state_topic":"$this/downloaded_cycle_id","name":"Downloaded cycle ID","entity_category":"diagnostic"})),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.is_empty() || buf[0] != 0x30 {
            return;
        }
        let bd: &[u8] = if buf.len() == 56 && buf[1] == 0xec {
            if buf.len() < 56 {
                return;
            }
            &buf[31..]
        } else if buf.len() == 29 && buf[1] == 0xeb {
            &buf[4..]
        } else {
            return;
        };
        if bd.len() < 24 {
            return;
        }

        let state = bd[0];
        let remaining = bd[1] as i64 * 60 + bd[2] as i64;
        let initial = bd[3] as i64 * 60 + bd[4] as i64;
        let cycle = bd[5];
        let error_code = bd[6];
        let dry = bd[7];
        let eco = bd[8];
        let process = bd[9];
        let reservation = bd[10];
        let anti = bd[14] & FLAG14_ANTI_CREASE != 0;
        let remote = bd[15] & FLAG15_REMOTE_START != 0;
        let downloaded_id = bd[23];

        let now = chrono::Utc::now().timestamp_millis();
        {
            let mut inner = self.inner.lock();
            if downloaded_id != 0 {
                inner.last_downloaded = downloaded_id;
            }
            if cycle > 1 {
                inner.last_cycle = cycle;
                inner.last_dry = dry;
                inner.last_eco = eco;
                inner.last_anti_crease = anti;
                inner.last_reservation = reservation;
            }
            if state == 2 && inner.cycle_start_ms.is_none() {
                inner.cycle_start_ms = Some(now);
                drop(inner);
                let iso = chrono::Utc::now().to_rfc3339();
                self.core.publish_property("cycle_start_time", iso.into());
            } else if state == 0 || state == 4 {
                inner.cycle_start_ms = None;
            }
        }

        let is_off = state == 0;
        self.core.publish_on_off("power", !is_off);
        self.core.publish_property("run_state", state_name(state).into());
        self.core.publish_property(
            "process_state",
            if is_off {
                "-".into()
            } else {
                process_name(process).into()
            },
        );
        self.core
            .publish_property("remaining_time", PropertyValue::Int(remaining));
        self.core
            .publish_property("initial_time", PropertyValue::Int(initial));
        self.core.publish_on_off("remote_start", remote);
        self.core.publish_on_off("run_completed", state == 4);

        if let Some(start) = self.inner.lock().cycle_start_ms {
            let duration = ((now - start).max(0) / 60_000) as i64;
            let end = chrono::Utc::now() + chrono::Duration::minutes(remaining);
            self.core
                .publish_property("cycle_duration", PropertyValue::Int(duration));
            self.core
                .publish_property("cycle_end_time", end.to_rfc3339().into());
        } else {
            self.core
                .publish_property("cycle_duration", PropertyValue::Int(0));
            self.core.publish_property("cycle_end_time", "-".into());
        }

        self.core.publish_on_off("error", error_code != 0);
        self.core
            .publish_property("error_message", error_name(error_code).into());

        let cycle_label = if cycle <= 1 {
            "None".to_string()
        } else {
            let n = cycle_name(cycle);
            if n.is_empty() {
                format!("unknown (0x{cycle:x})")
            } else {
                n.into()
            }
        };
        self.core.publish_property("cycle", cycle_label.into());

        let dl = if downloaded_id == 0 {
            "-".into()
        } else {
            let label = downloaded(downloaded_id)
                .map(|d| d.label)
                .unwrap_or("unknown");
            format!("0x{downloaded_id:x} ({label})")
        };
        self.core.publish_property("downloaded_cycle_id", dl.into());
        self.core
            .publish_property("dry_level", dry_level_name(dry).into());
        self.core
            .publish_property("eco_hybrid", eco_name(eco).into());
        self.core.publish_on_off("anti_crease", anti);
        self.core
            .publish_property("reservation", reservation_name(reservation).into());
    }

    fn build_f025(&self, smart_id: u8) -> Option<Vec<u8>> {
        let course = downloaded(smart_id)?;
        let mut inner = vec![0u8; 25];
        inner[0] = 0xf0;
        inner[1] = 0x25;
        inner[2] = course.default_eco;
        inner[3] = 0x15;
        inner[5] = course.default_eco;
        inner[6] = course.default_time;
        inner[14] = course.base_id;
        inner[15] = smart_id;
        inner[19] = course.default_dry;
        Some(inner)
    }

    fn build_start(&self, cycle: u8, dry: u8, eco: u8, anti: bool, reservation: u8) -> Vec<u8> {
        let mut dry = dry;
        let mut eco = eco;
        let allowed_dry = schema_dry_levels(cycle);
        if allowed_dry.is_empty() {
            dry = 0;
        } else if !allowed_dry.contains(&dry) {
            dry = schema_default_dry(cycle);
        }
        let allowed_eco = schema_eco(cycle);
        if !allowed_eco.contains(&eco) {
            eco = schema_default_eco(cycle);
        }
        let mut inner = vec![0u8; 16];
        inner[0] = 0xf0;
        inner[1] = 0x26;
        inner[2] = cycle;
        inner[3] = dry;
        inner[4] = eco;
        inner[8] = reservation;
        inner[11] = if anti { 0x02 } else { 0x00 };
        inner[12] = 0x03;
        inner
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "power" => {
                if mqtt_value == "ON" {
                    let id = self.inner.lock().last_downloaded;
                    if let Some(wake) = self.build_f025(id) {
                        self.core.send(&wake);
                        let core = self.core.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(500));
                            core.send(&hex_decode("F02A0100"));
                        });
                    } else {
                        self.core.send(&hex_decode("F02A0100"));
                    }
                } else if mqtt_value == "OFF" {
                    self.core.send(&hex_decode("F024010100"));
                }
            }
            "ping" => self.core.send(PING),
            "anti_crease" => {
                self.inner.lock().last_anti_crease = mqtt_value == "ON";
                self.core.publish_on_off("anti_crease", mqtt_value == "ON");
            }
            "pause" => self.core.send(&hex_decode("F024040100")),
            "start" => {
                let mut cycle;
                let mut dry;
                let mut eco;
                let mut anti;
                let mut reservation;
                {
                    let inner = self.inner.lock();
                    cycle = inner.last_cycle;
                    dry = inner.last_dry;
                    eco = inner.last_eco;
                    anti = inner.last_anti_crease;
                    reservation = inner.last_reservation;
                }
                if !mqtt_value.trim().is_empty() {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(mqtt_value) {
                        if let Some(c) = payload.get("cycle").and_then(|v| v.as_str()) {
                            if let Some(id) = cycle_id(c) {
                                cycle = id;
                            }
                        }
                        if let Some(c) = payload.get("dry_level").and_then(|v| v.as_str()) {
                            if let Some(id) = dry_level_id(c) {
                                dry = id;
                            }
                        }
                        if let Some(c) = payload.get("eco_hybrid").and_then(|v| v.as_str()) {
                            if let Some(id) = eco_id(c) {
                                eco = id;
                            }
                        }
                        if let Some(v) = payload.get("anti_crease") {
                            anti = v.as_bool().unwrap_or(anti);
                        }
                        if let Some(c) = payload.get("reservation").and_then(|v| v.as_str()) {
                            if let Some(id) = reservation_id(c) {
                                reservation = id;
                            }
                        }
                    }
                }
                if cycle < 2 {
                    return;
                }
                self.core
                    .send(&self.build_start(cycle, dry, eco, anti, reservation));
            }
            _ => {}
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
    use rethink_core::{hex_decode, hex_encode, Thinq2Device};
    use std::thread;
    use std::time::Duration;

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("RH90V9_WW", Device::new)
    }

    const SAMPLE_OFF: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001900000000000000000000000000000000000000000000000000D3BB";
    const SAMPLE_INITIAL: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001901000000000600030300000000000000000000000000000000C6BB";
    const SAMPLE_RUNNING: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001902003000300600030302000000000001000000000000000000A2BB";
    const SAMPLE_PAUSED: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001903001E00000600030300000000000000000000000000000000E6BB";
    const SAMPLE_END: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001904000000000600000000000000000000000000000000000000C5BB";
    const SAMPLE_ERROR: &str = "AA3830EC00190000000000000000000000000000000000000000000000000000190500000000060F000000000000000000000000000000000000F5BB";
    const SAMPLE_ANTI_CREASE: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001902000000000600000000000000000200000000000000000000C5BB";
    const SAMPLE_REMOTE_START: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001901000000000600000000000000000001000000000000000000DBBB";
    const SAMPLE_RESERVATION: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001901000000000600000000040000000000000000000000000000C4BB";
    const SAMPLE_DOWNLOADED: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001902000000000800000000000000000000000000000000006600A3BB";
    const SAMPLE_SPEED30: &str = "AA3830EC001900000000000000000000000000000000000000000000000000001901000000000900000300000000000000000000000000000000C6BB";

    fn emit(thinq: &rethink_core::MockThinq2Device, hex: &str) {
        thinq.emit_data(&hex_decode(hex));
    }

    fn pkt_inner(thinq: &rethink_core::MockThinq2Device, i: usize) -> Vec<u8> {
        let box_ = thinq.outbox();
        let pkt = &box_[i];
        if pkt.first() == Some(&0xaa) {
            pkt[2..pkt.len() - 2].to_vec()
        } else {
            pkt.to_vec()
        }
    }

    #[test]
    fn config_exposes_core_entities() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        for k in [
            "power",
            "remote_start",
            "cycle",
            "dry_level",
            "eco_hybrid",
            "anti_crease",
            "reservation",
            "start",
            "pause",
            "ping",
            "run_state",
            "process_state",
            "remaining_time",
            "initial_time",
            "run_completed",
            "error",
            "error_message",
            "downloaded_cycle_id",
        ] {
            assert!(comps.contains_key(k), "missing {k}");
        }
        assert_eq!(comps["cycle"]["platform"], "sensor");
        assert!(comps["cycle"].get("command_topic").is_none());
        let options = comps["cycle"]["options"].as_array().unwrap();
        for c in [
            "Towels",
            "Duvet",
            "Easy Care",
            "Mixed Fabric",
            "Cotton",
            "Sports Wear",
            "Speed 30",
            "Delicates",
            "Wool",
            "Rack Dry",
            "Warm Air",
            "Allergy Care",
            "Condenser Care",
            "Drum Care",
            "Eco (Cotton+)",
        ] {
            assert!(options.iter().any(|v| v.as_str() == Some(c)), "missing {c}");
        }
    }

    #[test]
    fn off_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "run_state").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "run_completed").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "cycle").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "process_state").as_deref(), Some("-"));
    }

    #[test]
    fn initial_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_INITIAL);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "run_state").as_deref(), Some("Initial"));
        assert_eq!(prop(&ha, "cycle").as_deref(), Some("Mixed Fabric"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Cupboard Dry"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Time"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
    }

    #[test]
    fn running_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_RUNNING);
        assert_eq!(prop(&ha, "run_state").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("48"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("48"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "process_state").as_deref(), Some("Dry"));
    }

    #[test]
    fn paused_end_error_flags() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_PAUSED);
        assert_eq!(prop(&ha, "run_state").as_deref(), Some("Paused"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("30"));
        emit(&thinq, SAMPLE_END);
        assert_eq!(prop(&ha, "run_state").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "run_completed").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_ERROR);
        assert_eq!(prop(&ha, "run_state").as_deref(), Some("Error"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("dE (door open)"));
    }

    #[test]
    fn flag_and_reservation_decode() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_ANTI_CREASE);
        assert_eq!(prop(&ha, "anti_crease").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_REMOTE_START);
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_RESERVATION);
        assert_eq!(prop(&ha, "reservation").as_deref(), Some("4h"));
    }

    #[test]
    fn downloaded_and_speed30() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DOWNLOADED);
        assert_eq!(
            prop(&ha, "downloaded_cycle_id").as_deref(),
            Some("0x66 (Gym Clothes)")
        );
        assert_eq!(prop(&ha, "cycle").as_deref(), Some("Sports Wear"));
        emit(&thinq, SAMPLE_SPEED30);
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Time"));
    }

    #[test]
    fn ping_power_off_pause() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("ping", "");
        assert!(hex_encode(&thinq.outbox()[0]).contains("F0ED1121010000001804131400005A"));
        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert!(hex_encode(&thinq.outbox()[0]).contains("F024010100"));
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert!(hex_encode(&thinq.outbox()[0]).contains("F024040100"));
    }

    #[test]
    fn start_requires_cycle() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn start_json_and_fallback() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property(
            "start",
            r#"{"cycle":"Mixed Fabric","dry_level":"Cupboard Dry","eco_hybrid":"Time"}"#,
        );
        let inner = pkt_inner(&thinq, 0);
        assert_eq!(inner[0], 0xf0);
        assert_eq!(inner[1], 0x26);
        assert_eq!(inner[2], 0x06);
        assert_eq!(inner[3], 3);
        assert_eq!(inner[4], 3);
        assert_eq!(inner[12], 0x03);

        emit(&thinq, SAMPLE_INITIAL);
        thinq.reset_recorder();
        dev.set_property("start", "");
        let inner = pkt_inner(&thinq, 0);
        assert_eq!(inner[2], 0x06);
        assert_eq!(inner[3], 3);
        assert_eq!(inner[4], 3);
    }

    #[test]
    fn start_reservation_and_anti_crease() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("start", r#"{"cycle":"Mixed Fabric","reservation":"4h"}"#);
        assert_eq!(pkt_inner(&thinq, 0)[8], 4);
        thinq.reset_recorder();
        dev.set_property("start", r#"{"cycle":"Mixed Fabric","anti_crease":true}"#);
        assert_eq!(pkt_inner(&thinq, 0)[11], 0x02);
    }

    #[test]
    fn anti_crease_switch_is_start_fallback() {
        let (ha, thinq, dev) = make();
        dev.set_property("anti_crease", "ON");
        assert_eq!(prop(&ha, "anti_crease").as_deref(), Some("ON"));
        thinq.reset_recorder();
        dev.set_property("start", r#"{"cycle":"Mixed Fabric"}"#);
        assert_eq!(pkt_inner(&thinq, 0)[11], 0x02);
    }

    #[test]
    fn start_enforces_speed30_schema() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("start", r#"{"cycle":"Speed 30","dry_level":"Cupboard Dry"}"#);
        assert_eq!(pkt_inner(&thinq, 0)[3], 0);
    }

    #[test]
    fn unknown_property_sends_nothing() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("does-not-exist", "whatever");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn cycle_timing() {
        let (ha, thinq, _) = make();
        let before = chrono::Utc::now().timestamp_millis();
        emit(&thinq, SAMPLE_RUNNING);
        let start = prop(&ha, "cycle_start_time").unwrap();
        assert_ne!(start, "-");
        let parsed = chrono::DateTime::parse_from_rfc3339(&start).unwrap().timestamp_millis();
        assert!(parsed >= before - 1000);
        let end = prop(&ha, "cycle_end_time").unwrap();
        let end_ms = chrono::DateTime::parse_from_rfc3339(&end).unwrap().timestamp_millis();
        let expected = before + 48 * 60 * 1000;
        assert!((end_ms - expected).abs() < 5000);
        assert!(prop(&ha, "cycle_duration").unwrap().parse::<i64>().unwrap() >= 0);
        emit(&thinq, SAMPLE_END);
        assert_eq!(prop(&ha, "cycle_duration").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "cycle_end_time").as_deref(), Some("-"));
    }

    #[test]
    fn start_time_stable_across_running_frames() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_RUNNING);
        let first = prop(&ha, "cycle_start_time");
        emit(&thinq, SAMPLE_RUNNING);
        assert_eq!(prop(&ha, "cycle_start_time"), first);
        emit(&thinq, SAMPLE_OFF);
        assert_eq!(prop(&ha, "cycle_duration").as_deref(), Some("0"));
    }

    #[test]
    fn power_on_wake_then_f02a() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DOWNLOADED);
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        let inner = pkt_inner(&thinq, 0);
        assert_eq!(inner[0], 0xf0);
        assert_eq!(inner[1], 0x25);
        assert_eq!(inner[15], 0x66);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(thinq.outbox().len(), 2);
        let inner2 = pkt_inner(&thinq, 1);
        assert_eq!(inner2[0], 0xf0);
        assert_eq!(inner2[1], 0x2a);
    }

    #[test]
    fn power_on_without_download_sends_f02a() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        let inner = pkt_inner(&thinq, 0);
        assert_eq!(inner[0], 0xf0);
        assert_eq!(inner[1], 0x2a);
    }

    #[test]
    fn junk_packets_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_OFF);
        let before = prop(&ha, "power");
        emit(&thinq, "AA083000250052BB");
        assert_eq!(prop(&ha, "power"), before);
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("RH90V9_WW").is_some());
    }
}
