//! RH10V9_CH heat-pump dryer (AABB, kind 0x30).
//!
//! Status record is 27 bytes (no 0x1b marker used by some NA RV13 models).
//! Live 2026-08-11 capture while running refined the field map:
//!
//! | Offset | Meaning |
//! |--------|---------|
//! | 0–1    | Programmed / frozen H:M estimate (often static during auto dry) |
//! | 2      | Phase (0 Off, 1 Initial, **2 Drying**, 3 Pause, 4 End, 0x32/0x33 alt) |
//! | 3      | Session option A (0/1 in captures; diagnostic) |
//! | 4      | **Live remaining minutes** (decrements ~1/min wall-clock while drying) |
//! | 6      | Course code |
//! | 7      | Dry level (1–5 style) |
//! | 10–11  | Temperature / option codes |
//! | 17     | Flags bitfield |
//! | 19     | Session option B (3/4 in captures; mirrors 0x3e[2]) |
//! | 20     | ~6 s tick counter while active |
//! | 21     | Diagnostic |
//! | 25     | Constant 0x75 in captures |
//!
//! Dual EC frames carry prev+cur; we publish **cur**. Subtype 0x3e is a short
//! telemetry burst: `u16be` candidate + option echo — diagnostic only (not proven Wh/W).
//!
//! Progress uses max(live remaining seen this cycle, programmed H:M) as baseline so
//! extended sensor-dry times (remaining > programmed) still make sense.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};

use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use rethink_util::sync::Mutex;

const RECORD_LEN: usize = 27;

/// Interval for monitor-enable retries (ms). Tests may lower this.
pub static MONITOR_INTERVAL_MS: AtomicU64 = AtomicU64::new(15_000);

fn status_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x01, "Initial"),
        // Heat-pump RH10 uses 0x02 for active drying in live captures (not 0x32).
        (0x02, "Drying"),
        (0x03, "Pause"),
        (0x04, "End"),
        (0x32, "Drying"),
        (0x33, "Cooling"),
    ])
}

fn course_map() -> HashMap<u8, &'static str> {
    // Sparse map; unknown codes published as "Course 0xNN".
    HashMap::from([
        (0x01, "Cotton"),
        (0x02, "Mixed"),
        (0x03, "Easy Care"),
        (0x04, "Delicates"),
        (0x05, "Wool"),
        (0x06, "Sportswear"),
        (0x07, "Duvet"),
        (0x08, "Quick"),
        (0x09, "Time Dry"),
        (0x0a, "Air Dry"),
        (0x0b, "Eco"),
        (0x10, "Speed Dry"),
        (0x12, "Time Dry"),
        (0x37, "Auto / Sensor"), // seen in heat-pump run capture
    ])
}

fn dry_level_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0, "—"),
        (1, "Damp"),
        (2, "Less"),
        (3, "Normal"),
        (4, "More"),
        (5, "Very"),
    ])
}

fn temp_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0, "—"),
        (1, "Low"),
        (2, "Medium Low"),
        (3, "Medium"),
        (4, "Medium High"),
        (5, "High"),
    ])
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    stop: Arc<AtomicBool>,
    monitor_thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// Last status phase (for cycle-complete edge).
    last_phase: Mutex<Option<u8>>,
    /// Peak remaining minutes this cycle (for progress when time extends).
    max_remaining: Mutex<i64>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            monitor_thread: Mutex::new(None),
            last_phase: Mutex::new(None),
            max_remaining: Mutex::new(0),
        });

        let mut base = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        let mut components = Map::new();
        let comps = [
            (
                "power",
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-power",
                    "state_topic": "$this/power",
                    "name": "Running",
                    "icon": "mdi:tumble-dryer",
                    "device_class": "running",
                }),
            ),
            (
                "status",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-status",
                    "state_topic": "$this/status",
                    "name": "Status",
                    "icon": "mdi:state-machine",
                }),
            ),
            (
                "course",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-course",
                    "state_topic": "$this/course",
                    "name": "Course",
                    "icon": "mdi:pin-outline",
                }),
            ),
            (
                "remaining_time",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-remaining_time",
                    "state_topic": "$this/remaining_time",
                    "name": "Remaining time",
                    "icon": "mdi:timer-outline",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                }),
            ),
            (
                "initial_time",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-initial_time",
                    "state_topic": "$this/initial_time",
                    "name": "Programmed time",
                    "icon": "mdi:clock-outline",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "cycle_baseline",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-cycle_baseline",
                    "state_topic": "$this/cycle_baseline",
                    "name": "Cycle baseline",
                    "icon": "mdi:timer-sand",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "progress",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-progress",
                    "state_topic": "$this/progress",
                    "name": "Progress",
                    "icon": "mdi:progress-clock",
                    "unit_of_measurement": "%",
                    "state_class": "measurement",
                }),
            ),
            (
                "dry_level",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-dry_level",
                    "state_topic": "$this/dry_level",
                    "name": "Dry level",
                    "icon": "mdi:water-percent",
                }),
            ),
            (
                "temp",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-temp",
                    "state_topic": "$this/temp",
                    "name": "Temperature",
                    "icon": "mdi:thermometer",
                }),
            ),
            (
                "child_lock",
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-child_lock",
                    "state_topic": "$this/child_lock",
                    "name": "Child lock",
                    "icon": "mdi:lock",
                    "entity_category": "diagnostic",
                    "payload_on": "ON",
                    "payload_off": "OFF",
                }),
            ),
            (
                "damp_dry_signal",
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-damp_dry_signal",
                    "state_topic": "$this/damp_dry_signal",
                    "name": "Damp dry signal",
                    "icon": "mdi:water-alert-outline",
                    "payload_on": "ON",
                    "payload_off": "OFF",
                }),
            ),
            (
                "flags",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-flags",
                    "state_topic": "$this/flags",
                    "name": "Flags (raw)",
                    "icon": "mdi:flag",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "tick",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-tick",
                    "state_topic": "$this/tick",
                    "name": "Activity tick",
                    "icon": "mdi:counter",
                    "entity_category": "diagnostic",
                    "state_class": "total_increasing",
                }),
            ),
            (
                "option_a",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-option_a",
                    "state_topic": "$this/option_a",
                    "name": "Option A (rec3)",
                    "icon": "mdi:tune",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "option_b",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-option_b",
                    "state_topic": "$this/option_b",
                    "name": "Option B (rec19)",
                    "icon": "mdi:tune-variant",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "telemetry",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-telemetry",
                    "state_topic": "$this/telemetry",
                    "name": "Telemetry (0x3e hex)",
                    "icon": "mdi:sine-wave",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "telemetry_u16",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-telemetry_u16",
                    "state_topic": "$this/telemetry_u16",
                    "name": "Telemetry u16 (0x3e)",
                    "icon": "mdi:numeric",
                    "entity_category": "diagnostic",
                    "state_class": "measurement",
                }),
            ),
            (
                "telemetry_opt",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-telemetry_opt",
                    "state_topic": "$this/telemetry_opt",
                    "name": "Telemetry option (0x3e)",
                    "icon": "mdi:numeric",
                    "entity_category": "diagnostic",
                }),
            ),
        ];
        for (k, v) in comps {
            components.insert(k.into(), v);
        }
        base.components = components.into_iter().collect();
        let (k, v) = rethink_core::notification_event(
            "cycle_complete",
            "Cycle complete",
            &["cycle_complete"],
            None,
        );
        base.components.insert(k, v);
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn send_monitor_enable(core: &AabbDeviceCore) {
        tracing::debug!("RH10V9: monitor enable");
        crate::devices::washer_ctrl::request_status(&core);
    }

    fn process_record(&self, rec: &[u8]) {
        if rec.len() < RECORD_LEN {
            return;
        }
        let phase = rec[2];
        let option_a = rec[3] as i64;
        let programmed = rec[0] as i64 * 60 + rec[1] as i64;
        // Live countdown lives at rec[4] while drying (1 min wall-clock ≈ 1 unit).
        // When zero (idle/initial), fall back to H:M at rec[0..1].
        let remaining = if rec[4] > 0 {
            rec[4] as i64
        } else {
            programmed
        };
        let course = rec[6];
        let dry_level = rec[7];
        let temp = rec[10];
        let flags = rec[17];
        let option_b = rec[19] as i64;
        let tick = rec[20] as i64;
        let status = status_map()
            .get(&phase)
            .map(|s| (*s).to_string())
            .unwrap_or_else(|| format!("0x{phase:02x}"));
        let course_s = course_map()
            .get(&course)
            .map(|s| (*s).to_string())
            .unwrap_or_else(|| format!("Course 0x{course:02x}"));
        let dry_s = dry_level_map()
            .get(&dry_level)
            .map(|s| (*s).to_string())
            .unwrap_or_else(|| format!("0x{dry_level:02x}"));
        let temp_s = temp_map()
            .get(&temp)
            .map(|s| (*s).to_string())
            .unwrap_or_else(|| format!("0x{temp:02x}"));

        let prev_phase = *self.last_phase.lock();
        // Reset cycle baseline when leaving Off / End into a new run.
        let restart = matches!(prev_phase, None | Some(0x00) | Some(0x04))
            && phase != 0x00
            && phase != 0x04;
        if restart || phase == 0x00 {
            *self.max_remaining.lock() = 0;
        }
        if remaining > 0 {
            let mut mx = self.max_remaining.lock();
            if remaining > *mx {
                *mx = remaining;
            }
        }
        let baseline = {
            let mx = *self.max_remaining.lock();
            mx.max(programmed).max(remaining)
        };
        let progress = if phase == 0x04 {
            100
        } else if baseline > 0 && remaining <= baseline {
            (((baseline - remaining) as f64 / baseline as f64) * 100.0).clamp(0.0, 100.0) as i64
        } else {
            0
        };

        *self.last_phase.lock() = Some(phase);

        let running = phase != 0x00;
        self.core.publish_on_off("power", running);
        self.core.publish_property("status", status.into());
        self.core.publish_property("course", course_s.into());
        self.core
            .publish_property("remaining_time", remaining.into());
        self.core
            .publish_property("initial_time", programmed.into());
        self.core
            .publish_property("cycle_baseline", baseline.into());
        self.core.publish_property("progress", progress.into());
        self.core.publish_property("dry_level", dry_s.into());
        self.core.publish_property("temp", temp_s.into());
        self.core
            .publish_property("flags", (flags as i64).into());
        self.core.publish_property("tick", tick.into());
        self.core.publish_property("option_a", option_a.into());
        self.core.publish_property("option_b", option_b.into());
        // Flag bits (aligned with other LG laundry layouts where known).
        self.core.publish_on_off("child_lock", flags & 0x01 != 0);
        self.core.publish_on_off("damp_dry_signal", flags & 0x08 != 0);

        if phase == 0x04 && prev_phase != Some(0x04) {
            self.core.ha.fire_notification_event(
                &self.core.id,
                "cycle_complete",
                "cycle_complete",
            );
        }
    }

    /// Short 0x3e telemetry: hex + structured fields (not proven energy/power).
    fn process_telemetry(&self, body: &[u8]) {
        // body: 30 3e | u16be | opt | x | y
        if body.len() < 3 {
            return;
        }
        let payload = &body[2..];
        self.core
            .publish_property("telemetry", rethink_core::hex_encode(payload).into());
        if payload.len() >= 2 {
            let u16v = u16::from_be_bytes([payload[0], payload[1]]) as i64;
            self.core.publish_property("telemetry_u16", u16v.into());
        }
        if payload.len() >= 3 {
            self.core
                .publish_property("telemetry_opt", (payload[2] as i64).into());
        }
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.is_empty() || buf[0] != 0x30 {
            return;
        }
        if buf.len() > 1 && buf[1] == 0x31 {
            return; // identity / serial noise
        }
        if buf.len() > 1 && buf[1] == 0x3e {
            self.process_telemetry(buf);
            return;
        }
        if buf.len() > 1 && buf[1] == 0xeb && buf.len() == 2 + RECORD_LEN {
            self.process_record(&buf[2..2 + RECORD_LEN]);
        } else if buf.len() > 1 && buf[1] == 0xec && buf.len() == 2 + 2 * RECORD_LEN {
            // Dual records: previous then current — use current.
            self.process_record(&buf[2 + RECORD_LEN..2 + 2 * RECORD_LEN]);
        } else {
            tracing::debug!("RH10V9 unhandled AABB {:02x?}", buf);
        }
    }

    pub fn set_property(&self, _prop: &str, _mqtt_value: &str) {}
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.stop.store(false, Ordering::SeqCst);
        Self::send_monitor_enable(&self.core);
        let core = self.core.clone();
        let stop = self.stop.clone();
        let handle = thread::spawn(move || {
            let mut n = 0u32;
            loop {
                let ms = MONITOR_INTERVAL_MS.load(Ordering::SeqCst);
                thread::sleep(Duration::from_millis(ms));
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                Self::send_monitor_enable(&core);
                n += 1;
                if n >= 8 {
                    break;
                }
            }
        });
        *self.monitor_thread.lock() = Some(handle);
    }
    fn drop_device(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.monitor_thread.lock().take() {
            let _ = h.join();
        }
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
    use rethink_core::{
        hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Metadata, Thinq2Device,
    };
    use crate::device_trait::DeviceHandler;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata {
        Metadata::new("RH10V9_CH", "RH10V9_CH", "2.10.114")
    }
    fn make() -> (
        std::sync::Arc<MockHaConnection>,
        std::sync::Arc<MockThinq2Device>,
        std::sync::Arc<Device>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?
            .properties
            .get(n)
            .map(|p| p.as_string())
    }

    #[test]
    fn start_monitor_enable() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        MONITOR_INTERVAL_MS.store(5, Ordering::SeqCst);
        dev.start();
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1121010000001800B5BB");
        thread::sleep(Duration::from_millis(80));
        let n = thinq.outbox().len();
        assert!(n >= 9, "expected >=9 got {n}");
        let after = n;
        thread::sleep(Duration::from_millis(40));
        assert_eq!(thinq.outbox().len(), after);
        dev.drop_device();
        MONITOR_INTERVAL_MS.store(15_000, Ordering::SeqCst);
    }

    #[test]
    fn status_decode_initial_eb() {
        let (ha, thinq, dev) = make();
        // Idle/initial: H:M remaining, phase Initial, rec[4]=0
        thinq.emit_data(&hex_decode(
            "AA2130EB00190100000000000000000000000000000000000000000000750020BB",
        ));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Initial"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("25"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("25"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "flags").as_deref(), Some("0"));
        dev.drop_device();
    }

    #[test]
    fn live_drying_ec_uses_rec4_remaining_and_phase02() {
        let (ha, thinq, dev) = make();
        // Cur record from 2026-08-11 capture mid-run:
        // programmed 25 min, phase 0x02 Drying, live remaining rec[4]=0x1b (27), course 0x37, dry 4
        thinq.emit_data(&hex_decode(
            "aa3c30ec001902011b0237040000030300000000001900030a010000007500001902011b0237040000030300000000001900030b0100000075007abb",
        ));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("27"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("25"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Auto / Sensor"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("More"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON")); // flags 0x19 bit0
        assert_eq!(prop(&ha, "damp_dry_signal").as_deref(), Some("ON")); // bit3
        assert_eq!(prop(&ha, "tick").as_deref(), Some("11"));
        // baseline = max(27 remaining, 25 programmed) = 27 → progress 0
        assert_eq!(prop(&ha, "cycle_baseline").as_deref(), Some("27"));
        assert_eq!(prop(&ha, "progress").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "option_a").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "option_b").as_deref(), Some("3"));

        // Later frame: remaining 16 min
        thinq.emit_data(&hex_decode(
            "aa3c30ec001902011002370400000303000000000019000377010000007500001902011002370400000303000000000019000378010000007500a6bb",
        ));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("16"));
        assert_eq!(prop(&ha, "tick").as_deref(), Some("120"));
        // progress (27-16)/27 ≈ 40%
        assert_eq!(prop(&ha, "progress").as_deref(), Some("40"));
        assert_eq!(prop(&ha, "cycle_baseline").as_deref(), Some("27"));
        dev.drop_device();
    }

    #[test]
    fn flags_and_end_event() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(
            "AA2130EB00190100000000000000000000000000000800000000000000750028BB",
        ));
        assert_eq!(prop(&ha, "flags").as_deref(), Some("8"));
        assert_eq!(prop(&ha, "damp_dry_signal").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));

        // phase End → cycle_complete event
        let mut rec = vec![0u8; 27];
        rec[2] = 0x04; // End
        rec[25] = 0x75;
        let mut frame = vec![0x30, 0xeb];
        frame.extend_from_slice(&rec);
        // wrap AABB
        let mut pkt = vec![0xaa, (frame.len() + 4) as u8];
        pkt.extend_from_slice(&frame);
        pkt.push(0);
        pkt.push(0xbb);
        let sum: u32 = pkt.iter().take(pkt.len() - 2).map(|&b| u32::from(b)).sum();
        let last = pkt.len() - 2;
        pkt[last] = ((sum & 0xff) as u8) ^ 0x55;
        thinq.emit_data(&pkt);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        let events = &ha.device(DEVICE_ID).unwrap().events;
        assert!(
            events
                .iter()
                .any(|(t, p)| t == "events/cycle_complete" && p.contains("cycle_complete")),
            "cycle complete event: {events:?}"
        );
        dev.drop_device();
    }

    #[test]
    fn telemetry_0x3e() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode("aa0b303e0092031b068cbb"));
        assert_eq!(prop(&ha, "telemetry").as_deref(), Some("0092031B06"));
        assert_eq!(prop(&ha, "telemetry_u16").as_deref(), Some("146"));
        assert_eq!(prop(&ha, "telemetry_opt").as_deref(), Some("3"));
        // Second session payload from later capture
        thinq.emit_data(&hex_decode("aa0b303e00970448085bbb"));
        assert_eq!(prop(&ha, "telemetry_u16").as_deref(), Some("151"));
        assert_eq!(prop(&ha, "telemetry_opt").as_deref(), Some("4"));
        dev.drop_device();
    }

    #[test]
    fn progress_when_remaining_exceeds_programmed() {
        let (ha, thinq, dev) = make();
        // programmed 25, live remaining 56 (extended sensor dry) — second capture style
        thinq.emit_data(&hex_decode(
            "aa3c30ec00190200380237040000030300000000001900044001000000750000190200380237040000030300000000001900044001000000750099bb",
        ));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("56"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("25"));
        assert_eq!(prop(&ha, "cycle_baseline").as_deref(), Some("56"));
        assert_eq!(prop(&ha, "progress").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "option_a").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "option_b").as_deref(), Some("4"));
        // remaining drops to 53
        thinq.emit_data(&hex_decode(
            "aa3c30ec00190200350237040000030300000000001900045b01000000750000190200350237040000030300000000001900045c01000000750050bb",
        ));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("53"));
        assert_eq!(prop(&ha, "cycle_baseline").as_deref(), Some("56"));
        // (56-53)/56 ≈ 5%
        assert_eq!(prop(&ha, "progress").as_deref(), Some("5"));
        dev.drop_device();
    }

    #[test]
    fn config_exposes_new_entities() {
        let (ha, _, dev) = make();
        let cfg = ha.device(DEVICE_ID).unwrap().config.unwrap();
        for k in [
            "course",
            "dry_level",
            "temp",
            "progress",
            "initial_time",
            "cycle_baseline",
            "option_a",
            "option_b",
            "telemetry_u16",
            "child_lock",
            "cycle_complete",
        ] {
            assert!(cfg.components.contains_key(k), "missing component {k}");
        }
        dev.drop_device();
    }
}
