//! RC90U2_WW on ThinQ1 — the Qualcomm `QC_Modem` module (fw QC_Modem_1.2.80).
//!
//! Same model id as the AABB handler in `rc90u2_ww`, registered on the ThinQ1 factory like
//! RAC_056905_WW. After `Mon Start` the module pushes the status about once a second as a bare
//! 24-byte payload: the AABB record without its last byte. Each payload is rebuilt into the
//! `30 EB` status frame and handed to the `rc90u2_ww` handler, so both variants share one
//! decoder. That handler's `F0` frames are translated to the module's ThinQ1 controls (modelJSON
//! TITAN2.0_D_WIFI_CZ, ControlWifi): power off is Power/Off, power on Operation/WakeUp,
//! Pause/Resume Operation/Stop or Operation/Start with the paused cycle's settings, and the
//! `F0 26` remote start Operation/Start with the same payload minus its header. A start sent
//! while the appliance is off would be kept by the module and applied at the next power-on from
//! the panel, so the appliance is woken first and the start follows once it reports Ready; it is
//! dropped if that takes more than 20 s.
//!
//! Port of upstream PR #240.

use crate::device_trait::DeviceHandler;
use crate::devices::rc90u2_ww::{
    self, CLASS_BYTE, POWER_OFF, POWER_ON, RECORD_MARKER, START_PAUSE, STATUS_LENGTH,
};
use crate::devices::washer_ctrl::STATUS_QUERY;
use crate::timers::{ThreadTimers, Timers};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rethink_core::device_base::{unwrap_aabb, wrap_aabb};
use rethink_core::{hex_encode, HaConnection, Metadata, Thinq1Device, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

const PAYLOAD_LEN: usize = 24;
/// After the `00 19` block header.
const PAYLOAD_OFFSET: usize = 2;
const FRAME_TYPE_STATUS: u8 = 0xeb;

// Payload offsets, named as in the modelJSON Monitoring section.
const STATE: usize = 0;
const COURSE: usize = 5;
const DRY_LEVEL: usize = 7;
const ECO_HYBRID: usize = 8;
const OPTION1: usize = 14;
const OPTION2: usize = 15;
const SMART_COURSE: usize = 20;
const STATE_OFF: u8 = 0x00;
const STATE_RUNNING: u8 = 0x02;
const STATE_PAUSE: u8 = 0x03;
/// Wrinkle Care.
const OPTION1_ANTI_CREASE: u8 = 0x02;
const OPTION2_REMOTE_START: u8 = 0x01;

const START_COURSE: u8 = 0x26;
const START_DATA_LEN: usize = 14;

/// WakeUp has taken 1 to 8 s to power the dryer on, and now and then only lights the Remote
/// Start icon instead.
const WAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Seen working 5 s after Ready: give the main board a moment before the start.
const START_AFTER_WAKE: Duration = Duration::from_secs(2);

fn control(cmd_opt: &str, value: &str, data: Option<&[u8]>) -> Value {
    json!({
        "Cmd": "Control",
        "CmdOpt": cmd_opt,
        "Value": value,
        "Format": "B64",
        "Data": data.map(|d| STANDARD.encode(d)).unwrap_or_default(),
    })
}

/// OperationStart: [Course, DryLevel, EcoHybrid, MoreLessTime, 0, 0, Reserve_Time_H, 0, 0,
/// Option1, Option2, 0, SmartCourse, 0]. Resuming a paused cycle repeats what it is running.
fn resume_data(status: &[u8]) -> [u8; START_DATA_LEN] {
    let mut data = [0; START_DATA_LEN];
    data[0] = status[COURSE];
    data[1] = status[DRY_LEVEL];
    data[2] = status[ECO_HYBRID];
    data[9] = status[OPTION1] & OPTION1_ANTI_CREASE;
    data[10] = status[OPTION2] & OPTION2_REMOTE_START;
    data[12] = status[SMART_COURSE];
    data
}

/// The ThinQ1 control for an `F0` frame of the AABB handler, if there is one.
fn translate(inner: &[u8], status: Option<&[u8]>) -> Option<Value> {
    if inner == POWER_OFF {
        return Some(control("Power", "Off", None));
    }
    // The modelJSON has no power on: WakeUp powers the dryer on from standby.
    if inner == POWER_ON {
        return Some(control("Operation", "WakeUp", None));
    }
    // A single toggle on the AABB variant; ThinQ1 has separate Stop and Start.
    if inner == START_PAUSE {
        let status = status?;
        return match status[STATE] {
            STATE_RUNNING => Some(control("Operation", "Stop", None)),
            STATE_PAUSE => Some(control("Operation", "Start", Some(&resume_data(status)))),
            _ => None,
        };
    }
    if inner.len() == 2 + START_DATA_LEN && inner[0] == 0xf0 && inner[1] == START_COURSE {
        return Some(control("Operation", "Start", Some(&inner[2..])));
    }
    None
}

struct Pending {
    cmd: Value,
    seq: u64,
}

#[derive(Default)]
struct Link {
    status: Option<Vec<u8>>,
    /// A start waiting for the appliance to wake up.
    pending: Option<Pending>,
    seq: u64,
}

type DataHandler = Box<dyn Fn(&[u8]) + Send + Sync>;

/// What the AABB handler sees in place of a ThinQ2 device.
struct Transport {
    thinq: Arc<dyn Thinq1Device>,
    timers: Arc<dyn Timers>,
    link: Arc<Mutex<Link>>,
    handlers: Mutex<Vec<DataHandler>>,
}

impl Transport {
    fn deliver(&self, payload: &[u8]) {
        // The module also passes the DevInfo text through here.
        if payload.len() != PAYLOAD_LEN {
            return;
        }
        let woken = {
            let mut link = self.link.lock();
            link.status = Some(payload.to_vec());
            if payload[STATE] != STATE_OFF {
                link.pending.take()
            } else {
                None
            }
        };
        if let Some(pending) = woken {
            let thinq = self.thinq.clone();
            self.timers
                .after(START_AFTER_WAKE, Box::new(move || thinq.send(pending.cmd)));
        }
        let mut rec = [0u8; STATUS_LENGTH];
        rec[1] = RECORD_MARKER;
        rec[PAYLOAD_OFFSET..PAYLOAD_OFFSET + PAYLOAD_LEN].copy_from_slice(payload);
        let mut inner = vec![CLASS_BYTE, FRAME_TYPE_STATUS];
        inner.extend_from_slice(&rec);
        let frame = wrap_aabb(&inner);
        for handler in self.handlers.lock().iter() {
            handler(&frame);
        }
    }

    /// A start while the appliance is off is refused, yet the module keeps it for the next
    /// power-on from the panel: wake the appliance and send the start once it is up.
    fn start_when_awake(&self, cmd: Value) {
        let seq = {
            let mut link = self.link.lock();
            link.seq += 1;
            let seq = link.seq;
            link.pending = Some(Pending { cmd, seq });
            seq
        };
        let link = self.link.clone();
        let thinq = self.thinq.clone();
        self.timers.after(
            WAKE_TIMEOUT,
            Box::new(move || {
                let mut link = link.lock();
                if link.pending.as_ref().is_some_and(|p| p.seq == seq) {
                    link.pending = None;
                    tracing::info!(
                        model = %thinq.meta().model_id,
                        id = %thinq.id(),
                        "the appliance did not power on, start dropped"
                    );
                }
            }),
        );
        self.thinq.send(control("Operation", "WakeUp", None));
    }
}

impl Thinq2Device for Transport {
    fn id(&self) -> &str {
        self.thinq.id()
    }
    fn meta(&self) -> &Metadata {
        self.thinq.meta()
    }
    fn send_packet(&self, packet: &[u8]) {
        let Some(inner) = unwrap_aabb(packet) else {
            return;
        };
        // `Mon Start` replaces the status request.
        if inner == STATUS_QUERY {
            return;
        }
        let (cmd, off) = {
            let mut link = self.link.lock();
            if inner == POWER_OFF {
                link.pending = None;
            }
            let status = link.status.as_deref();
            (
                translate(inner, status),
                status.is_none_or(|s| s[STATE] == STATE_OFF),
            )
        };
        match cmd {
            Some(cmd) if inner[1] == START_COURSE && off => self.start_when_awake(cmd),
            Some(cmd) => self.thinq.send(cmd),
            None => tracing::info!(
                model = %self.thinq.meta().model_id,
                id = %self.thinq.id(),
                "no ThinQ1 equivalent for {}, dropped",
                hex_encode(inner)
            ),
        }
    }
    // Status pushes over ThinQ1 are not acked by the cloud.
    fn send_ack(&self, _buf: &[u8]) {}
    // The AABB layer sends no CLIP commands.
    fn send(&self, _cmd: &str, _msg_type: i32, _data: Value) {}
    fn on_data(&self, handler: DataHandler) {
        self.handlers.lock().push(handler);
    }
    fn emit_data(&self, buf: &[u8]) {
        for handler in self.handlers.lock().iter() {
            handler(buf);
        }
    }
}

pub struct Device {
    dryer: Arc<rc90u2_ww::Device>,
    thinq: Arc<dyn Thinq1Device>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq1Device>,
        meta: Metadata,
        timers: Arc<dyn Timers>,
    ) -> Arc<Self> {
        let transport = Arc::new(Transport {
            thinq: thinq.clone(),
            timers: timers.clone(),
            link: Arc::default(),
            handlers: Mutex::new(Vec::new()),
        });
        let t = transport.clone();
        thinq.on_data(Box::new(move |payload| t.deliver(payload)));
        let dryer = rc90u2_ww::Device::new(ha, transport, meta, timers);
        Arc::new(Self { dryer, thinq })
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        self.dryer.id()
    }
    fn start(&self) {
        self.dryer.start();
        self.thinq.send(json!({ "Cmd": "Mon", "CmdOpt": "Start" }));
    }
    fn take_over(&self, previous: &dyn DeviceHandler) {
        let previous: &dyn Any = previous;
        if let Some(previous) = previous.downcast_ref::<Device>() {
            self.dryer.take_over(previous.dryer.as_ref());
        }
    }
    fn drop_device(&self) {
        self.dryer.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        self.dryer.set_property(prop, value);
    }
    fn publish_config(&self) {
        self.dryer.publish_config();
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta, Arc::new(ThreadTimers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{prop, ManualTimers, DEVICE_ID};
    use rethink_core::{hex_decode, MockHaConnection, MockThinq1Device};

    // Real status payloads from an RC90U2 with the QC_Modem module, decoded from the `Data` of
    // the ThinQ1 status pushes. payload[i] is byte i of the AABB variant's record.

    // Panel idle on Quick Dry: 0:30, no dry level (fixed on that course), Time.
    const SAMPLE_IDLE_QUICK30: &str =
        "01 001e 0000 09 00 00 03 02 00000000 00 04 00 0000 00 000000 6b";
    // Cotton, Extra, Time, right after Start: 2:40 left of 2:40.
    const SAMPLE_RUNNING: &str = "02 0228 0228 07 00 04 03 02 00000000 00 04 00 0000 01 000000 6b";
    // The same cycle paused: event byte 0x60.
    const SAMPLE_PAUSED: &str = "03 0228 0228 07 00 04 03 02 00000000 00 04 60 0000 02 000000 6b";
    // Powered off, Remote Start icon lit after a WakeUp.
    const SAMPLE_OFF: &str = "00 0000 0000 00 00 00 00 00 00000000 00 04 00 0000 01 000000 6b";
    // What a WakeUp powers it on to: Ready, no course yet, Remote Start armed.
    const SAMPLE_WOKEN: &str = "01 0000 0000 00 00 00 00 00 00000000 00 05 00 0000 00 000000 6b";
    // The DevInfo text comes through the same event.
    const SAMPLE_DEVINFO: &[u8] = b"RuleVer=1.3,FwVer=QC_Modem_1.2.80,regFail=N";

    fn meta() -> Metadata {
        Metadata::new("RC90U2_WW", "RC90U2_WW", "QC_Modem_1.2.80")
    }

    fn make() -> (
        Arc<MockHaConnection>,
        Arc<MockThinq1Device>,
        Arc<Device>,
        Arc<ManualTimers>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq1Device::new(DEVICE_ID, meta());
        let timers = Arc::new(ManualTimers::default());
        let dev = Device::new(ha.clone(), thinq.clone(), meta(), timers.clone());
        (ha, thinq, dev, timers)
    }

    fn emit(thinq: &MockThinq1Device, payload: &str) {
        thinq.emit_data(&hex_decode(payload));
    }

    fn control_data(cmd_opt: &str, value: &str, data: &str) -> Value {
        json!({
            "Cmd": "Control",
            "CmdOpt": cmd_opt,
            "Value": value,
            "Format": "B64",
            "Data": STANDARD.encode(hex_decode(data)),
        })
    }

    fn power_off() -> Value {
        json!({"Cmd": "Control", "CmdOpt": "Power", "Value": "Off", "Format": "B64", "Data": ""})
    }

    fn wake_up() -> Value {
        json!({"Cmd": "Control", "CmdOpt": "Operation", "Value": "WakeUp", "Format": "B64", "Data": ""})
    }

    // Cotton, Cupboard, Eco, Remote Start
    fn start_cotton() -> Value {
        control_data(
            "Operation",
            "Start",
            "07 03 01 00 00 00 00 00 00 00 01 00 00 00",
        )
    }

    #[test]
    fn registered_for_the_thinq1_module_only() {
        let t1 = crate::t1_factory("RC90U2_WW").unwrap();
        assert!(std::ptr::fn_addr_eq(
            t1,
            create as crate::device_trait::T1Factory
        ));
        let t2 = crate::t2_factory("RC90U2_WW").unwrap();
        assert!(std::ptr::fn_addr_eq(
            t2,
            rc90u2_ww::create as crate::device_trait::T2Factory
        ));
    }

    #[test]
    fn start_subscribes_to_the_monitoring_feed() {
        let (_ha, thinq, dev, _timers) = make();
        dev.start();
        assert_eq!(thinq.sent(), vec![json!({"Cmd": "Mon", "CmdOpt": "Start"})]);
    }

    #[test]
    fn idle_payload_is_decoded_with_the_aabb_layout() {
        let (ha, thinq, _dev, _timers) = make();
        emit(&thinq, SAMPLE_IDLE_QUICK30);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Quick Dry"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Time"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("30"));
        // Filled in when a cycle starts.
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("0"));
        // Bit 0x04 of the remote byte is set here, but only bit 0 is Remote Start.
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn running_then_paused() {
        let (ha, thinq, _dev, _timers) = make();
        emit(&thinq, SAMPLE_RUNNING);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Extra"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("160"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("160"));

        emit(&thinq, SAMPLE_PAUSED);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Paused"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("160"));
    }

    #[test]
    fn the_panel_is_mirrored_into_the_remote_selects() {
        let (ha, thinq, _dev, _timers) = make();
        emit(&thinq, SAMPLE_RUNNING);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Extra"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Time"));
    }

    #[test]
    fn devinfo_text_is_ignored() {
        let (ha, thinq, _dev, _timers) = make();
        thinq.emit_data(SAMPLE_DEVINFO);
        assert_eq!(prop(&ha, "status"), None);
    }

    #[test]
    fn power_off_is_power_off_and_on_is_a_wake_up() {
        let (_ha, thinq, dev, _timers) = make();
        dev.set_property("power", "OFF");
        dev.set_property("power", "ON");
        assert_eq!(thinq.sent(), vec![power_off(), wake_up()]);
    }

    #[test]
    fn start_pause_stops_a_running_cycle() {
        let (_ha, thinq, dev, _timers) = make();
        emit(&thinq, SAMPLE_RUNNING);
        dev.set_property("start_pause", "PRESS");
        assert_eq!(
            thinq.sent(),
            vec![
                json!({"Cmd": "Control", "CmdOpt": "Operation", "Value": "Stop", "Format": "B64", "Data": ""})
            ]
        );
    }

    #[test]
    fn start_pause_resumes_a_paused_cycle_with_its_own_settings() {
        let (_ha, thinq, dev, _timers) = make();
        emit(&thinq, SAMPLE_PAUSED);
        dev.set_property("start_pause", "PRESS");
        // Cotton, Extra, Time; no Anti crease, Remote Start not armed
        assert_eq!(
            thinq.sent(),
            vec![control_data(
                "Operation",
                "Start",
                "07 04 03 00 00 00 00 00 00 00 00 00 00 00"
            )]
        );
    }

    #[test]
    fn start_pause_does_nothing_when_idle() {
        let (_ha, thinq, dev, _timers) = make();
        emit(&thinq, SAMPLE_IDLE_QUICK30);
        dev.set_property("start_pause", "PRESS");
        assert!(thinq.sent().is_empty());
    }

    #[test]
    fn start_selected_cycle_is_operation_start_with_the_f0_26_payload() {
        let (_ha, thinq, dev, _timers) = make();
        emit(&thinq, SAMPLE_IDLE_QUICK30);
        dev.set_property("course", "Cotton");
        dev.set_property("dry_level", "Extra");
        dev.set_property("eco_hybrid", "Eco");
        dev.set_property("anti_crease", "Yes");
        dev.set_property("start_delay_end", "5");
        dev.set_property("start", "PRESS");
        // [Course, DryLevel, EcoHybrid, MoreLessTime, 0, 0, Reserve_Time_H, 0, 0, Option1,
        // Option2, 0, SmartCourse, 0]
        assert_eq!(
            thinq.sent(),
            vec![control_data(
                "Operation",
                "Start",
                "07 04 01 00 00 00 05 00 00 02 01 00 00 00"
            )]
        );
    }

    #[test]
    fn start_selected_cycle_on_an_off_appliance_wakes_it_up_first() {
        let (_ha, thinq, dev, timers) = make();
        emit(&thinq, SAMPLE_OFF);
        dev.set_property("start", "PRESS");
        assert_eq!(
            thinq.sent(),
            vec![wake_up()],
            "no Start while off: the module would keep it for later"
        );

        emit(&thinq, SAMPLE_WOKEN);
        timers.tick(1900);
        assert_eq!(thinq.sent(), vec![wake_up()]);
        timers.tick(200);
        assert_eq!(thinq.sent(), vec![wake_up(), start_cotton()]);
    }

    #[test]
    fn the_start_is_dropped_when_the_appliance_does_not_wake_up() {
        let (_ha, thinq, dev, timers) = make();
        emit(&thinq, SAMPLE_OFF);
        dev.set_property("start", "PRESS");
        timers.tick(20000);

        emit(&thinq, SAMPLE_WOKEN);
        timers.tick(5000);
        assert_eq!(thinq.sent(), vec![wake_up()]);
    }

    #[test]
    fn power_off_cancels_a_pending_start() {
        let (_ha, thinq, dev, timers) = make();
        emit(&thinq, SAMPLE_OFF);
        dev.set_property("start", "PRESS");
        dev.set_property("power", "OFF");

        emit(&thinq, SAMPLE_WOKEN);
        timers.tick(5000);
        assert_eq!(thinq.sent(), vec![wake_up(), power_off()]);
    }

    #[test]
    fn publish_config_restores_availability_and_discovery() {
        let (ha, _thinq, dev, _timers) = make();
        dev.drop_device();
        assert_eq!(
            ha.device(DEVICE_ID).unwrap().availability.as_deref(),
            Some("offline")
        );
        dev.publish_config();
        let info = ha.device(DEVICE_ID).unwrap();
        assert_eq!(info.availability.as_deref(), Some("online"));
        assert!(info.config.unwrap().components.contains_key("start"));
    }
}
