//! RH16_T_KR (LG RH16VT) 16 kg heat-pump dryer, deviceType 202. AABB class byte 0x30.
//!
//! Only fields this appliance's own captures ground are exposed. EB carries one status block and
//! EC the previous then the current one; a block is a 0x00 byte and a 26-byte record from its
//! 0x19 marker. Frames with a bad checksum are dropped. A native course starts with the app's
//! captured `F0 26` frame for it, with the reserve hours (0 or 3-19) and only the options its
//! model Course table makes selectable folded in; Resume replays that frame. The ten download
//! courses install with their captured `F0 25 03 15` frames and start with an `F0 26` frame
//! built from the install body; no downloaded-course resume was captured, so none is sent. A
//! download's (execution id, signature) pair reads back at record bytes 6 and 21 only while it
//! is reserved or running, so the installed course and the selects are kept in process and in
//! the course memory file, and installs the LG app tunnels through the bridge are recognised
//! from the packets sent to the appliance. `auto_ack` stays off.
//!
//! Port of upstream PR #182.

use crate::course_memory::CourseMemory;
use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{hex_decode, unwrap_aabb, HaConnection, Metadata, Thinq2Device};
use rethink_util::packet_codec::aabb_checksum;
use rethink_util::sync::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

const DEVICE_TYPE: u8 = 0x30;
const STATUS_LEN: usize = 27;
const RECORD_MARKER: u8 = 0x19;

// Record offsets, counted from the marker.
const STATE_OFFSET: usize = 1;
const REMAIN_HOUR_OFFSET: usize = 2;
const REMAIN_MINUTE_OFFSET: usize = 3;
const INITIAL_HOUR_OFFSET: usize = 4;
const INITIAL_MINUTE_OFFSET: usize = 5;
// The native course, or a running download's execution id.
const COURSE_OFFSET: usize = 6;
const ERROR_OFFSET: usize = 7;
const DRY_LEVEL_OFFSET: usize = 8;
const ECO_HYBRID_OFFSET: usize = 9;
const PROCESS_STATE_OFFSET: usize = 10;
const RESERVE_REMAIN_HOUR_OFFSET: usize = 11;
const RESERVE_REMAIN_MINUTE_OFFSET: usize = 12;
const RESERVE_SET_HOUR_OFFSET: usize = 13;
const RESERVE_SET_MINUTE_OFFSET: usize = 14;
const FLAGS_OFFSET: usize = 15;
const REMOTE_START_OFFSET: usize = 16;
const STEAM_OFFSET: usize = 17;
// This-cycle Wh, 16-bit big-endian; it freezes at completion.
const ENERGY_OFFSET: usize = 18;
const DOWNLOAD_SIGNATURE_OFFSET: usize = 21;

const RESERVATION_FLAG: u8 = 0x01;
const ANTI_CREASE_FLAG: u8 = 0x02;
// Not 0x08, which is also set in captures where the lock was off.
const CHILD_LOCK_FLAG: u8 = 0x10;
const REMOTE_START_FLAG: u8 = 0x01;
const STEAM_FLAG: u8 = 0x08;

const STATE_POWEROFF: u8 = 0;
const STATE_RUNNING: u8 = 2;
const STATE_PAUSE: u8 = 3;
const STATE_DIAGNOSIS: u8 = 8;

const STATUS_REQUEST: &str = "F0ED1121010000001800";
const PAUSE_COMMAND: &str = "F024040100";
// The app's power-off, MCP toDevice seq 222.
const POWER_OFF_COMMAND: &str = "F024010100";

const STANDARD: u8 = 7;
// The app's start frame per course (MCP toDevice seq), then the dry level, Eco Hybrid and
// anti-crease the app shows once the course is selected. The Standard frame is the captured
// no-reserve one the app also replays for Resume.
const NATIVE_COURSES: &[(u8, &str, u8, u8, u8)] = &[
    (1, "f0260100020000000300000003080000", 0, 2, 0), // seq190 Steam Refresh, reserve 3
    (2, "f0260200020000000300000003000000", 0, 2, 0), // seq150 Towel, reserve 3
    (4, "f0260400030000000400000203000000", 0, 3, 1), // seq156 Bulky Item, reserve 4, anti-crease on
    (5, "f0260503020000000300000003000000", 3, 2, 0), // seq163 Easy Care, dry Standard, reserve 3
    (7, "f0260703020000000000000001000000", 3, 2, 0), // captured no-reserve base = Resume frame
    (8, "f0260800010000000400000003000000", 0, 1, 0), // seq94 Sports Wear, reserve 4
    (9, "f0260900030000000300000203000000", 0, 3, 1), // seq101 Quick Dry, reserve 3, anti-crease on
    (11, "f0260b00020000000300000003000000", 0, 2, 0), // seq107 Wool, reserve 3
    (15, "f0260f00030000000400000003000000", 0, 3, 0), // seq120 Bedding Brush, reserve 4
    (16, "f0261000030000000300000003080000", 0, 3, 0), // seq126 Allergy Care, reserve 3
    (18, "f0261200030000000000000003080000", 0, 3, 0), // seq212 Condenser Care, immediate
    (19, "f0261300030000000300000003080000", 0, 3, 0), // seq218 Tub Clean, reserve 3
    (20, "f0261400030000000300000003000000", 0, 3, 0), // seq196 Padding Refresh, reserve 3
    (21, "f0261500021e00000300000003000000", 0, 2, 0), // seq114 Time Dry 30min, reserve 3
    (22, "f0261600033c00000300000003000000", 0, 3, 0), // seq206 Outdoor Refresh, reserve 3
    (23, "f0261700020000000400000003000000", 0, 2, 0), // seq144 Baby Wear, reserve 4
];
const TEMPLATE_DRY_OFFSET: usize = 3;
const TEMPLATE_ECO_OFFSET: usize = 4;
const TEMPLATE_RESERVE_OFFSET: usize = 8;
const TEMPLATE_AC_OFFSET: usize = 11;
const AC_BYTE_ON: u8 = 0x02;
// Values the course's model table makes selectable. Anything else keeps the captured byte.
const DRY_WRITABLE: &[(u8, &[u8])] = &[(7, &[1, 2, 3, 4, 5]), (5, &[2, 3])];
const ECO_WRITABLE: &[(u8, &[u8])] = &[(7, &[1, 2, 3])];
const AC_WRITABLE: &[u8] = &[1, 2, 4, 5, 7, 8, 9, 11, 15, 16, 20, 21, 22, 23];

// Install frames the app sent while each course was downloaded, replayed verbatim. The app shows
// Refresh as 리프레쉬 and Easy Iron (model EASYIRON) as 촉촉 건조.
const DOWNLOAD_COURSES: &[(&str, &str)] = &[
    (
        "Powerful Dry",
        "f0250315000264000000000000001177000000000000000000",
    ),
    (
        "Wrinkle Care Dry",
        "f025031500025a000000000200000772000000030000000000",
    ),
    (
        "Full Size Load",
        "f0250315000364000000000000000774000000050000000000",
    ),
    (
        "Refresh",
        "f0250315000314000000000000000f6b000000000000000000",
    ),
    (
        "Small Load",
        "f025031500031e000000000000000e6c000000000000000000",
    ),
    (
        "Gym Clothes",
        "f025031500013d000000000000000866000000000000000000",
    ),
    (
        "Rainy Season",
        "f0250315000328000000000000000e69000000000000000000",
    ),
    (
        "Economic Dry",
        "f025031500017d000000000000000770000000030000000000",
    ),
    (
        "Easy Iron",
        "f025031500034100000000000000076e000000010000000000",
    ),
    (
        "Big Size Item",
        "f02503150003af000000000000000471000000000000000000",
    ),
];
// Several downloads share an execution id; with the signature the pair is unique.
const INSTALL_EXECUTION_ID: usize = 14;
const INSTALL_SIGNATURE: usize = 15;

const DOWNLOADED_COURSE: &str = "Downloaded Course";

// Codes outside these tables publish "None", HA's unknown.
// Courses isolated from labelled app starts; RACKDRY and COOLAIR have no code yet.
const COURSES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Steam Refresh"),
    (2, "Towel"),
    (4, "Bulky Item"),
    (5, "Easy Care"),
    (7, "Standard"),
    (8, "Sports Wear"),
    (9, "Quick Dry"),
    (11, "Wool"),
    (15, "Bedding Brush"),
    (16, "Allergy Care"),
    (18, "Condenser Care"),
    (19, "Tub Clean"),
    (20, "Padding Refresh"),
    (21, "Time Dry"),
    (22, "Outdoor Refresh"),
    (23, "Baby Wear"),
];
// The model's dryLevel table under the app's names; 0 is a course without a dry level.
const DRY_LEVELS: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Delicate"),
    (2, "Light"),
    (3, "Standard"),
    (4, "Standard+"),
    (5, "Strong"),
];
// The model's ecoHybrid table: NORMAL is the app's Auto and TURBO its Speed.
const ECO_HYBRIDS: &[(u8, &str)] = &[(0, "Off"), (1, "Energy"), (2, "Auto"), (3, "Speed")];
const STATES: &[(u8, &str)] = &[
    (0, "Power off"),
    (1, "Standby"),
    (2, "Drying"),
    (3, "Pause"),
    (4, "Complete"),
    (5, "Error"),
    (8, "Smart diagnosis"),
    (100, "Reserved"),
];
// MonitoringValue.processState while Running, folded into Status like a washer's phases.
const PROCESS_STATES: &[(u8, &str)] = &[
    (0, "Detecting"),
    (1, "Steam"),
    (2, "Drying"),
    (3, "Drying"),
    (4, "Drying"),
    (5, "Cooling"),
    (6, "Anti crease"),
    (7, "Complete"),
];
const ERRORS: &[(u8, &str)] = &[
    (0, "Normal"),
    (1, "tE1"),
    (2, "tE2"),
    (4, "tE4"),
    (5, "tE5"),
    (6, "tE6"),
    (7, "CE1"),
    (13, "OE Drain motor"),
    (14, "Empty water"),
    (15, "dE Door"),
    (16, "Filter clogging"),
    (17, "No filter"),
    (19, "F1"),
    (20, "LE2"),
    (21, "AE"),
    (30, "LE1"),
    (37, "dE4"),
    (39, "LE3"),
    (42, "dE2"),
];

/// The armed download per HA connection and device id, so a handler rebuilt in this process
/// keeps it. Holding the connection weakly also keeps its address from being reused while it is
/// listed.
type ArmedDownloads = HashMap<String, (String, bool)>;
static ARMED: Mutex<Vec<(Weak<dyn HaConnection>, ArmedDownloads)>> = Mutex::new(Vec::new());

fn armed_download(ha: &Arc<dyn HaConnection>, id: &str) -> Option<(String, bool)> {
    let mut all = ARMED.lock();
    all.retain(|(connection, _)| connection.strong_count() > 0);
    all.iter()
        .find(|(connection, _)| std::ptr::addr_eq(connection.as_ptr(), Arc::as_ptr(ha)))
        .and_then(|(_, devices)| devices.get(id).cloned())
}

fn arm_download(ha: &Arc<dyn HaConnection>, id: &str, course: &str, downloaded: bool) {
    let mut all = ARMED.lock();
    all.retain(|(connection, _)| connection.strong_count() > 0);
    let index = match all
        .iter()
        .position(|(connection, _)| std::ptr::addr_eq(connection.as_ptr(), Arc::as_ptr(ha)))
    {
        Some(index) => index,
        None => {
            all.push((Arc::downgrade(ha), HashMap::new()));
            all.len() - 1
        }
    };
    all[index]
        .1
        .insert(id.to_string(), (course.to_string(), downloaded));
}

/// What Start course and Resume send, and what a restart restores, in upstream's course memory
/// format.
#[derive(Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Selection {
    #[serde(skip_serializing_if = "Option::is_none")]
    downloaded_course: Option<String>,
    use_downloaded_course: bool,
    selected_course: u8,
    reserve_hours: u8,
    dry_code: u8,
    eco_code: u8,
    anti_crease_code: u8,
}

impl Default for Selection {
    /// The captured no-reserve Standard frame: dry Standard, Eco Hybrid Auto, anti-crease off.
    fn default() -> Self {
        Self {
            downloaded_course: None,
            use_downloaded_course: false,
            selected_course: STANDARD,
            reserve_hours: 0,
            dry_code: 3,
            eco_code: 2,
            anti_crease_code: 0,
        }
    }
}

impl Selection {
    fn native_start_frame(&self) -> Option<Vec<u8>> {
        let course = self.selected_course;
        let &(_, template, ..) = native_course(course)?;
        let mut frame = hex_decode(template);
        frame[TEMPLATE_RESERVE_OFFSET] = self.reserve_hours;
        if selectable(DRY_WRITABLE, course, self.dry_code) {
            frame[TEMPLATE_DRY_OFFSET] = self.dry_code;
        }
        if selectable(ECO_WRITABLE, course, self.eco_code) {
            frame[TEMPLATE_ECO_OFFSET] = self.eco_code;
        }
        if AC_WRITABLE.contains(&course) {
            frame[TEMPLATE_AC_OFFSET] = if self.anti_crease_code == 1 {
                AC_BYTE_ON
            } else {
                0x00
            };
        }
        Some(frame)
    }

    /// Execution id, dry level, Eco Hybrid, initial time, anti-crease and signature come from
    /// the course's install body; the two captured downloaded starts line up with it.
    fn download_start_frame(&self) -> Option<Vec<u8>> {
        let install = hex_decode(download_install(self.downloaded_course.as_deref()?)?);
        Some(vec![
            0xf0,
            0x26,
            install[INSTALL_EXECUTION_ID],
            install[19], // dry level
            install[5],  // Eco Hybrid
            install[6],  // initial time
            install[7],
            install[8],
            self.reserve_hours,
            install[10],
            install[12],
            install[11], // anti-crease
            0x03,
            install[13],
            install[INSTALL_SIGNATURE],
            0x00,
        ])
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    memory: CourseMemory,
    selection: Mutex<Selection>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        memory_file: PathBuf,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let memory = CourseMemory::new(memory_file, &core.id);
        // A rebuild in this process restores only the armed download; a restart restores all.
        let selection = match armed_download(&core.ha, &core.id) {
            Some((course, downloaded)) => Selection {
                downloaded_course: Some(course),
                use_downloaded_course: downloaded,
                ..Selection::default()
            },
            None => memory.load().unwrap_or_default(),
        };
        let this = Arc::new(Self {
            core: core.clone(),
            memory,
            selection: Mutex::new(selection),
        });
        let mut base = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        base.components = components();
        core.set_config(base);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| dev.process_data(data)));
        let dev = this.clone();
        thinq.on_send_packet(Box::new(move |packet| dev.observe_outgoing(packet)));
        this.publish_selection();
        this
    }

    fn publish_selection(&self) {
        let s = self.selection.lock();
        if let Some(course) = &s.downloaded_course {
            self.core
                .publish_property("smart_course_select", course.as_str().into());
            self.core
                .publish_property("smart_course", course.as_str().into());
        }
        let course = if s.use_downloaded_course && s.downloaded_course.is_some() {
            DOWNLOADED_COURSE
        } else {
            named(COURSES, s.selected_course)
        };
        self.core.publish_property("course_select", course.into());
        self.core
            .publish_property("reserve_hours", i64::from(s.reserve_hours).into());
        self.core
            .publish_property("dry_level_select", named(DRY_LEVELS, s.dry_code).into());
        self.core
            .publish_property("eco_hybrid_select", named(ECO_HYBRIDS, s.eco_code).into());
        self.core.publish_property(
            "anti_crease_select",
            anti_crease_label(s.anti_crease_code).into(),
        );
    }

    fn remember(&self, s: &Selection) {
        if let Some(course) = &s.downloaded_course {
            arm_download(
                &self.core.ha,
                &self.core.id,
                course,
                s.use_downloaded_course,
            );
        }
        self.memory.save(s);
    }

    fn process_data(&self, buf: &[u8]) {
        // Unlike the shared base, the checksum is verified on receive.
        if buf.len() < 4 || buf[buf.len() - 2] != aabb_checksum(&buf[..buf.len() - 2]) {
            return;
        }
        if let Some(inner) = self.core.process_data_envelope(buf) {
            self.process_aabb(inner);
        }
    }

    /// Installs the LG app makes in bridge mode reach the appliance without set_property; a
    /// packet matching a known install body updates HA the same way.
    fn observe_outgoing(&self, packet: &[u8]) {
        let Some(inner) = unwrap_aabb(packet) else {
            return;
        };
        let Some(&(name, _)) = DOWNLOAD_COURSES
            .iter()
            .find(|(_, install)| hex_decode(install) == inner)
        else {
            return;
        };
        let mut s = self.selection.lock();
        if s.downloaded_course.as_deref() == Some(name) && s.use_downloaded_course {
            return;
        }
        s.downloaded_course = Some(name.to_string());
        s.use_downloaded_course = true;
        self.remember(&s);
        self.core
            .publish_property("smart_course_select", name.into());
        self.core
            .publish_property("course_select", DOWNLOADED_COURSE.into());
        self.core.publish_property("smart_course", name.into());
    }

    fn process_aabb(&self, buf: &[u8]) {
        let Some(status) = process_common_status(buf, DEVICE_TYPE, STATUS_LEN) else {
            return;
        };
        let rec = &status[1..];
        if rec[0] != RECORD_MARKER {
            return;
        }
        let core = &self.core;
        let state = rec[STATE_OFFSET];
        let error = rec[ERROR_OFFSET];
        core.publish_on_off("power", state != STATE_POWEROFF);
        core.publish_property("status", status_of(rec).into());
        core.publish_flag("child_lock", rec[FLAGS_OFFSET], CHILD_LOCK_FLAG);
        core.publish_flag("anti_crease", rec[FLAGS_OFFSET], ANTI_CREASE_FLAG);
        core.publish_flag("reservation", rec[FLAGS_OFFSET], RESERVATION_FLAG);
        core.publish_flag("remote_start", rec[REMOTE_START_OFFSET], REMOTE_START_FLAG);
        core.publish_on_off("error", error != 0);
        core.publish_property("error_message", named(ERRORS, error).into());
        core.publish_on_off("smart_diagnosis", state == STATE_DIAGNOSIS);

        let smart_course = smart_course_of(rec);
        let raw_course = rec[COURSE_OFFSET];
        let mut s = self.selection.lock();
        if let Some(name) = smart_course {
            s.downloaded_course = Some(name.to_string());
            s.use_downloaded_course = true;
            self.remember(&s);
            core.publish_property("smart_course_select", name.into());
            core.publish_property("course_select", DOWNLOADED_COURSE.into());
            core.publish_property("smart_course", name.into());
        } else if native_course(raw_course).is_some() {
            s.selected_course = raw_course;
            s.use_downloaded_course = false;
            self.remember(&s);
            core.publish_property("course_select", named(COURSES, raw_course).into());
            // Running a native course does not uninstall the download, and an idle frame is no
            // evidence either way, so smart_course is never cleared.
            if let Some(course) = &s.downloaded_course {
                core.publish_property("smart_course_select", course.as_str().into());
                core.publish_property("smart_course", course.as_str().into());
            }
        }
        let course = if smart_course.is_some() || s.use_downloaded_course {
            DOWNLOADED_COURSE
        } else {
            named(COURSES, raw_course)
        };
        drop(s);
        core.publish_property("course", course.into());
        core.publish_property("dry_level", named(DRY_LEVELS, rec[DRY_LEVEL_OFFSET]).into());
        core.publish_property(
            "eco_hybrid",
            named(ECO_HYBRIDS, rec[ECO_HYBRID_OFFSET]).into(),
        );
        core.publish_flag("steam", rec[STEAM_OFFSET], STEAM_FLAG);
        core.publish_property(
            "remaining_time",
            minutes(rec[REMAIN_HOUR_OFFSET], rec[REMAIN_MINUTE_OFFSET]).into(),
        );
        core.publish_property(
            "initial_time",
            minutes(rec[INITIAL_HOUR_OFFSET], rec[INITIAL_MINUTE_OFFSET]).into(),
        );
        core.publish_property(
            "reserve_time",
            minutes(
                rec[RESERVE_REMAIN_HOUR_OFFSET],
                rec[RESERVE_REMAIN_MINUTE_OFFSET],
            )
            .into(),
        );
        let energy = u16::from_be_bytes([rec[ENERGY_OFFSET], rec[ENERGY_OFFSET + 1]]);
        core.publish_property("energy", i64::from(energy).into());
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        match prop {
            "pause" => self.core.send(&hex_decode(PAUSE_COMMAND)),
            "power_off" => self.core.send(&hex_decode(POWER_OFF_COMMAND)),
            "course_select" => self.select_course(value),
            "smart_course_select" => self.install_download(value),
            "reserve_hours" => {
                // LG declares 0 (start now) or 3-19 hours.
                let Some(hours) = crate::js::number(value)
                    .filter(|h| *h == 0.0 || (h.fract() == 0.0 && (3.0..=19.0).contains(h)))
                else {
                    return;
                };
                let mut s = self.selection.lock();
                s.reserve_hours = hours as u8;
                self.remember(&s);
                self.core
                    .publish_property("reserve_hours", i64::from(s.reserve_hours).into());
            }
            "dry_level_select" => {
                let Some(dry) = code(DRY_LEVELS, value) else {
                    return;
                };
                let mut s = self.selection.lock();
                s.dry_code = dry;
                self.remember(&s);
                self.core.publish_property("dry_level_select", value.into());
            }
            "eco_hybrid_select" => {
                let Some(eco) = code(ECO_HYBRIDS, value) else {
                    return;
                };
                let mut s = self.selection.lock();
                s.eco_code = eco;
                self.remember(&s);
                self.core
                    .publish_property("eco_hybrid_select", value.into());
            }
            "anti_crease_select" => {
                let anti_crease = match value {
                    "Off" => 0,
                    "On" => 1,
                    _ => return,
                };
                let mut s = self.selection.lock();
                s.anti_crease_code = anti_crease;
                self.remember(&s);
                self.core
                    .publish_property("anti_crease_select", value.into());
            }
            "start_course" | "resume" => self.send_start(prop == "resume"),
            _ => {}
        }
    }

    fn select_course(&self, value: &str) {
        let mut s = self.selection.lock();
        if value == DOWNLOADED_COURSE {
            s.use_downloaded_course = true;
            self.remember(&s);
            self.core.publish_property("course_select", value.into());
            return;
        }
        let Some(&(course, _, dry, eco, anti_crease)) =
            code(COURSES, value).and_then(native_course)
        else {
            return;
        };
        s.use_downloaded_course = false;
        s.selected_course = course;
        s.dry_code = dry;
        s.eco_code = eco;
        s.anti_crease_code = anti_crease;
        self.remember(&s);
        self.core.publish_property("course_select", value.into());
        self.core.publish_property(
            "dry_level_select",
            label(DRY_LEVELS, dry).unwrap_or("Off").into(),
        );
        self.core.publish_property(
            "eco_hybrid_select",
            label(ECO_HYBRIDS, eco).unwrap_or("Auto").into(),
        );
        self.core
            .publish_property("anti_crease_select", anti_crease_label(anti_crease).into());
    }

    fn install_download(&self, value: &str) {
        let Some(install) = download_install(value) else {
            return;
        };
        {
            let mut s = self.selection.lock();
            s.downloaded_course = Some(value.to_string());
            s.use_downloaded_course = true;
            self.remember(&s);
            self.core
                .publish_property("smart_course_select", value.into());
            self.core
                .publish_property("course_select", DOWNLOADED_COURSE.into());
            self.core.publish_property("smart_course", value.into());
        }
        self.core.send(&hex_decode(install));
    }

    fn send_start(&self, resume: bool) {
        let frame = {
            let s = self.selection.lock();
            if s.use_downloaded_course {
                // No downloaded-course resume has been captured.
                if resume {
                    return;
                }
                s.download_start_frame()
            } else {
                s.native_start_frame()
            }
        };
        if let Some(frame) = frame {
            self.core.send(&frame);
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode(STATUS_REQUEST));
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
    Device::new(ha, thinq, meta, CourseMemory::default_file())
}

/// A reservation counts down in Running or Pause, so the reserve bytes win over both.
fn status_of(rec: &[u8]) -> &'static str {
    let state = rec[STATE_OFFSET];
    let reserve_pending = [
        RESERVE_REMAIN_HOUR_OFFSET,
        RESERVE_REMAIN_MINUTE_OFFSET,
        RESERVE_SET_HOUR_OFFSET,
        RESERVE_SET_MINUTE_OFFSET,
    ]
    .iter()
    .any(|&offset| rec[offset] != 0);
    if reserve_pending && (state == STATE_RUNNING || state == STATE_PAUSE) {
        return "Reserved";
    }
    if state == STATE_RUNNING {
        return label(PROCESS_STATES, rec[PROCESS_STATE_OFFSET]).unwrap_or("Drying");
    }
    named(STATES, state)
}

fn smart_course_of(rec: &[u8]) -> Option<&'static str> {
    DOWNLOAD_COURSES
        .iter()
        .find(|(_, install)| {
            let install = hex_decode(install);
            install[INSTALL_EXECUTION_ID] == rec[COURSE_OFFSET]
                && install[INSTALL_SIGNATURE] == rec[DOWNLOAD_SIGNATURE_OFFSET]
        })
        .map(|&(name, _)| name)
}

fn native_course(course: u8) -> Option<&'static (u8, &'static str, u8, u8, u8)> {
    NATIVE_COURSES.iter().find(|(code, ..)| *code == course)
}

fn download_install(name: &str) -> Option<&'static str> {
    DOWNLOAD_COURSES
        .iter()
        .find(|(course, _)| *course == name)
        .map(|&(_, install)| install)
}

fn selectable(writable: &[(u8, &[u8])], course: u8, value: u8) -> bool {
    writable
        .iter()
        .any(|&(c, values)| c == course && values.contains(&value))
}

fn minutes(hours: u8, minutes: u8) -> i64 {
    i64::from(hours) * 60 + i64::from(minutes)
}

fn anti_crease_label(code: u8) -> &'static str {
    if code == 1 {
        "On"
    } else {
        "Off"
    }
}

fn label(table: &[(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|&(_, l)| l)
}

/// The label for `code`, or "None", which HA reads as unknown.
fn named(table: &[(u8, &'static str)], code: u8) -> &'static str {
    label(table, code).unwrap_or("None")
}

fn code(table: &[(u8, &'static str)], label: &str) -> Option<u8> {
    table.iter().find(|(_, l)| *l == label).map(|&(c, _)| c)
}

/// Each label once, in table order.
fn labels(table: &[(u8, &'static str)]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for &(_, label) in table {
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

fn components() -> HashMap<String, Value> {
    let download_courses: Vec<&str> = DOWNLOAD_COURSES.iter().map(|&(name, _)| name).collect();
    let course_options = [vec![DOWNLOADED_COURSE], labels(COURSES)].concat();
    let smart_course_options = [vec!["Unknown"], download_courses.clone()].concat();
    // Only courses with a captured start frame, and the downloaded one.
    let course_select_options: Vec<&str> = COURSES
        .iter()
        .filter(|&&(course, _)| native_course(course).is_some())
        .map(|&(_, label)| label)
        .chain([DOWNLOADED_COURSE])
        .collect();
    let status_options = labels(&[STATES, PROCESS_STATES].concat());
    let comps = [
        (
            "pause",
            json!({
                "platform": "button",
                "unique_id": "$deviceid-pause",
                "command_topic": "$this/pause/set",
                "payload_press": "",
                "name": "Pause",
                "icon": "mdi:pause-circle-outline",
            }),
        ),
        (
            "power",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "name": "Power",
                "payload_on": "ON",
                "payload_off": "OFF",
                "icon": "mdi:power",
            }),
        ),
        (
            "power_off",
            json!({
                "platform": "button",
                "unique_id": "$deviceid-power_off",
                "command_topic": "$this/power_off/set",
                "payload_press": "",
                "name": "Power off",
                "icon": "mdi:power",
            }),
        ),
        (
            "course",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-course",
                "state_topic": "$this/course",
                "name": "Course",
                "device_class": "enum",
                "options": course_options,
                "icon": "mdi:playlist-check",
            }),
        ),
        (
            "smart_course",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-smart_course",
                "state_topic": "$this/smart_course",
                "name": "Smart course",
                "device_class": "enum",
                "options": smart_course_options,
                "icon": "mdi:playlist-star",
            }),
        ),
        (
            "dry_level",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-dry_level",
                "state_topic": "$this/dry_level",
                "name": "Dry level",
                "device_class": "enum",
                "options": labels(DRY_LEVELS),
                "icon": "mdi:thermometer",
            }),
        ),
        (
            "eco_hybrid",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-eco_hybrid",
                "state_topic": "$this/eco_hybrid",
                "name": "Eco hybrid",
                "device_class": "enum",
                "options": labels(ECO_HYBRIDS),
                "icon": "mdi:leaf",
            }),
        ),
        (
            "steam",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-steam",
                "state_topic": "$this/steam",
                "name": "Steam",
                "payload_on": "ON",
                "payload_off": "OFF",
                "icon": "mdi:cloud-outline",
            }),
        ),
        (
            "course_select",
            json!({
                "platform": "select",
                "unique_id": "$deviceid-course_select",
                "state_topic": "$this/course_select",
                "command_topic": "$this/course_select/set",
                "name": "Course select",
                "options": course_select_options,
                "icon": "mdi:playlist-edit",
            }),
        ),
        (
            "smart_course_select",
            json!({
                "platform": "select",
                "unique_id": "$deviceid-smart_course_select",
                "state_topic": "$this/smart_course_select",
                "command_topic": "$this/smart_course_select/set",
                "name": "Smart course select",
                "options": download_courses,
                "icon": "mdi:playlist-edit",
            }),
        ),
        (
            // HA's number cannot leave out 1 and 2; set_property refuses them.
            "reserve_hours",
            json!({
                "platform": "number",
                "unique_id": "$deviceid-reserve_hours",
                "state_topic": "$this/reserve_hours",
                "command_topic": "$this/reserve_hours/set",
                "name": "Reserve hours",
                "min": 0,
                "max": 19,
                "step": 1,
                "icon": "mdi:calendar-clock",
                "entity_category": "config",
            }),
        ),
        (
            "dry_level_select",
            json!({
                "platform": "select",
                "unique_id": "$deviceid-dry_level_select",
                "state_topic": "$this/dry_level_select",
                "command_topic": "$this/dry_level_select/set",
                "name": "Dry level select",
                "options": labels(DRY_LEVELS),
                "icon": "mdi:thermometer",
                "entity_category": "config",
            }),
        ),
        (
            "eco_hybrid_select",
            json!({
                "platform": "select",
                "unique_id": "$deviceid-eco_hybrid_select",
                "state_topic": "$this/eco_hybrid_select",
                "command_topic": "$this/eco_hybrid_select/set",
                "name": "Eco hybrid select",
                "options": labels(ECO_HYBRIDS),
                "icon": "mdi:leaf",
                "entity_category": "config",
            }),
        ),
        (
            "anti_crease_select",
            json!({
                "platform": "select",
                "unique_id": "$deviceid-anti_crease_select",
                "state_topic": "$this/anti_crease_select",
                "command_topic": "$this/anti_crease_select/set",
                "name": "Anti crease select",
                "options": ["Off", "On"],
                "icon": "mdi:tshirt-crew-outline",
                "entity_category": "config",
            }),
        ),
        (
            "start_course",
            json!({
                "platform": "button",
                "unique_id": "$deviceid-start_course",
                "command_topic": "$this/start_course/set",
                "payload_press": "",
                "name": "Start course",
                "icon": "mdi:play-circle-outline",
            }),
        ),
        (
            "resume",
            json!({
                "platform": "button",
                "unique_id": "$deviceid-resume",
                "command_topic": "$this/resume/set",
                "payload_press": "",
                "name": "Resume",
                "icon": "mdi:play-pause",
            }),
        ),
        (
            "status",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-status",
                "state_topic": "$this/status",
                "name": "Status",
                "device_class": "enum",
                "options": status_options,
                "icon": "mdi:tumble-dryer",
            }),
        ),
        (
            "child_lock",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-child_lock",
                "state_topic": "$this/child_lock",
                "name": "Child lock",
                "payload_on": "ON",
                "payload_off": "OFF",
                "icon": "mdi:lock",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "anti_crease",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-anti_crease",
                "state_topic": "$this/anti_crease",
                "name": "Anti crease",
                "payload_on": "ON",
                "payload_off": "OFF",
                "icon": "mdi:tshirt-crew-outline",
            }),
        ),
        (
            "reservation",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-reservation",
                "state_topic": "$this/reservation",
                "name": "Reservation",
                "payload_on": "ON",
                "payload_off": "OFF",
                "icon": "mdi:calendar-clock",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "remote_start",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-remote_start",
                "state_topic": "$this/remote_start",
                "name": "Remote start",
                "payload_on": "ON",
                "payload_off": "OFF",
                "icon": "mdi:cellphone-check",
            }),
        ),
        (
            "error",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-error",
                "state_topic": "$this/error",
                "name": "Error",
                "payload_on": "ON",
                "payload_off": "OFF",
                "device_class": "problem",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "error_message",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-error_message",
                "state_topic": "$this/error_message",
                "name": "Error message",
                "device_class": "enum",
                "options": labels(ERRORS),
                "icon": "mdi:alert-circle-outline",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "smart_diagnosis",
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-smart_diagnosis",
                "state_topic": "$this/smart_diagnosis",
                "name": "Smart diagnosis",
                "payload_on": "ON",
                "payload_off": "OFF",
                "device_class": "problem",
                "icon": "mdi:stethoscope",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "remaining_time",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-remaining_time",
                "state_topic": "$this/remaining_time",
                "name": "Remaining time",
                "device_class": "duration",
                "unit_of_measurement": "min",
                "state_class": "measurement",
                "icon": "mdi:timer-sand",
            }),
        ),
        (
            "initial_time",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-initial_time",
                "state_topic": "$this/initial_time",
                "name": "Initial time",
                "device_class": "duration",
                "unit_of_measurement": "min",
                "state_class": "measurement",
                "icon": "mdi:timer-outline",
            }),
        ),
        (
            "reserve_time",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-reserve_time",
                "state_topic": "$this/reserve_time",
                "name": "Reserve time",
                "device_class": "duration",
                "unit_of_measurement": "min",
                "state_class": "measurement",
                "icon": "mdi:calendar-clock",
            }),
        ),
        (
            "energy",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-energy",
                "state_topic": "$this/energy",
                "name": "Energy",
                "device_class": "energy",
                "unit_of_measurement": "Wh",
                "state_class": "total_increasing",
                "icon": "mdi:lightning-bolt",
            }),
        ),
    ];
    comps
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{prop, DEVICE_ID};
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Real frames captured from the owner's RH16_T_KR. OFF is the current state
    // after a live reconnect; INITIAL and the EC transition are historical traffic
    // from the same appliance.
    const OFF: &str = "AA2130EB00190000000000000000000000000000000000000000000000770023BB";
    const INITIAL: &str = "AA2130EB001901000000000000000000000000000804000000000000007700D6BB";
    const DOUBLE_INITIAL: &str =
        "AA3C30EC00190100000000000000000000000000080400000000000000770000190100000000000000000000000000000400000000000000770061BB";
    // Owner toggled child lock ON then OFF on the appliance while a reserved
    // Steam Refresh run waited. rec[15] went 0x01 -> 0x11 -> 0x01, and in both
    // frames that byte was the only one in the record to change.
    const CHILD_LOCK_ON: &str =
        "AA3C30EC0019030020002001000002010D380D38010A0800050200000077000019030020002001000002010D380D38110A0800050200000077005DBB";
    const CHILD_LOCK_OFF: &str =
        "AA3C30EC0019030020002001000002010D380D38110A0800050200000077000019030020002001000002010D380D38010A0800050200000077005DBB";
    const REMOTE_START_ON: &str =
        "AA3C30EC00190100000000000000000000000000000900000000000000770000190100000000000000000000000000001900000000000000770013BB";
    const REMOTE_START_OFF: &str =
        "AA3C30EC00190100000000000000000000000000001900000000000000770000190100000000000000000000000000001800000000000000770000BB";
    const STANDARD_DETECTING: &str =
        "AA3C30EC00190100000000000000000000000000001900000000000000770000190201280128070003020000000000009B0000000100000077006DBB";
    const STANDARD_DRYING: &str =
        "AA3C30EC00190201280128070003020000000000009B000000010000007700001902010F010F070003020200000000001B0000000100000077003FBB";
    // Real captures from the 04:21 drying cycle (app daily total: 694Wh).
    // record+18 (16-bit BE) is the this-cycle Wh counter, x1.
    const ENERGY_MID_CYCLE: &str =
        "aa3c30ec00190201080114120000030300000000201b0800ad01000000770000190201070114120000030300000000201b0800b001000000770023bb";
    const ENERGY_AT_COMPLETION: &str =
        "aa3c30ec00190200010114120000030500000000001b0802b301000000770000190400010114120000030700000000081a0802b602000000770012bb";
    const STANDARD_PAUSED: &str =
        "AA3C30EC001902010F010F070003020200000000001B000000010000007700001903010F010F070003020200000000081B00000002000000770091BB";
    const STANDARD_ENERGY_DELICATE_RESERVED_19H: &str =
        "AA3C30EC001903010C010F0700030202000000000819000005020000007700001902001E001E070001010013001300019B000000030000007700D1BB";
    const STANDARD_SPEED_LOW_RESERVED_3H: &str =
        "AA3C30EC001903013701370700010102123A123A091B000000020000007700001902001E001E07000203000300030001990000000300000077001EBB";
    const ANTI_CREASE_ON: &str =
        "AA3C30EC001903011401140700040202033B033B0919000000020000007700001902001E001E0700050300030003000399000000030000007700A5BB";
    const ANTI_CREASE_OFF: &str =
        "AA3C30EC001902001E001E070005030003000300019B000000030000007700001902001E001E070005030003000300019900000003000000770051BB";
    const DUVET_RESERVED_4H: &str =
        "AA3C30EC00190202370237040000030204000400031B00000003000000770000190202370237040000030204000400031900000003000000770039BB";
    const STEAM_REFRESH_RESERVED_3H: &str =
        "AA3C30EC001903010A010A0500020202023B023B0B1900000002000000770000190200200020010000020103000300011908000003000000770002BB";
    const STEAM_REFRESH_RESUMED_11H: &str =
        "AA3C30EC0019030020002001000002010B000B0001190800010200000077000019020020002001000002010B000B00011908000103000000770073BB";
    const STEAM_REFRESH_RESERVED_14H_PARTWAY: &str =
        "AA3C30EC0019030020002001000002010D380D3801130800010200000077000019030020002001000002010D380D38011B0800010200000077005FBB";
    const CONDENSER_CARE_RUNNING: &str =
        "AA3C30EC00190301000100160000030103000300091900000002000000770000190201140114120000030100000000001908000003000000770084BB";
    const SHIRTS_LOW_AC_ON_RESERVED_3H: &str =
        "AA3C30EC001902010A010A050002020203000300031B000000030000007700001902010A010A0500020202023B023B031B0000000300000077007FBB";
    const TOWEL_RESERVED_3H: &str =
        "AA3C30EC00190301000100170000020204000400091B0000000200000077000019020128012802000002000300030001990000000300000077003EBB";
    // Owner-confirmed Big Size Item 3h reservation. All ten live download-course
    // reservations matched the execution-id/signature pair encoded in F025.
    const BIG_SIZE_ITEM_RESERVED_3H: &str =
        "AA3C30EC00190202370237040000030203000300011B000000017100007100001902023702370400000302023B023B011B000000017100007100F9BB";
    const STATUS_REQUEST_PACKET: &str = "aa0ef0ed1121010000001800b5bb";
    const POWERFUL_DRY_INSTALL: &str = "aa1df0250315000264000000000000001177000000000000000000b7bb";
    const POWERFUL_DRY_START: &str = "aa14f026110002640000000000000300770090bb";
    const STANDARD_START: &str = "aa14f0260703020000000000000001000000b4bb";

    fn meta() -> Metadata {
        Metadata::new("RH16_T_KR", "RH16_T_KR", "2.10.122")
    }

    /// The test's own course memory file: absent at first, removed when the test ends.
    struct MemoryFile(PathBuf);

    impl MemoryFile {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rethink-course-memory-rh16-{}-{}.json",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for MemoryFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A handler on `ha`: the same connection is a reconnect, a new one a restart.
    fn handler(
        ha: &Arc<MockHaConnection>,
        memory: &MemoryFile,
    ) -> (Arc<MockThinq2Device>, Arc<Device>) {
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(
            ha.clone(),
            thinq.clone(),
            meta(),
            memory.path().to_path_buf(),
        );
        (thinq, dev)
    }

    /// What a fresh install sees: a new HA connection and no memory file.
    fn make_device() -> (
        Arc<MockHaConnection>,
        Arc<MockThinq2Device>,
        Arc<Device>,
        MemoryFile,
    ) {
        let memory = MemoryFile::new();
        let ha = MockHaConnection::new();
        let (thinq, dev) = handler(&ha, &memory);
        (ha, thinq, dev, memory)
    }

    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    fn sent(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .outbox()
            .iter()
            .map(|packet| hex_encode(packet).to_ascii_lowercase())
            .collect()
    }

    fn discovered(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID).unwrap().config.unwrap().components
    }

    fn options(ha: &MockHaConnection, component: &str) -> Vec<String> {
        discovered(ha)[component]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option.as_str().unwrap().to_string())
            .collect()
    }

    fn has(options: &[String], label: &str) -> bool {
        options.iter().any(|option| option == label)
    }

    fn assert_intact(packet: &[u8]) {
        assert_eq!(usize::from(packet[1]), packet.len());
        assert_eq!(
            packet[packet.len() - 2],
            aabb_checksum(&packet[..packet.len() - 2])
        );
    }

    /// Model-declared probe derived from a real frame; not capture evidence.
    fn probe(bytes: &[(usize, u8)]) -> Vec<u8> {
        let mut packet = hex_decode(OFF);
        for &(index, value) in bytes {
            packet[index] = value;
        }
        let checksum = packet.len() - 2;
        packet[checksum] = aabb_checksum(&packet[..checksum]);
        packet
    }

    fn with_state(state: u8) -> Vec<u8> {
        probe(&[(6, state)])
    }

    fn with_error(error: u8) -> Vec<u8> {
        probe(&[(12, error)])
    }

    #[test]
    fn real_capture_fixtures_have_intact_aa_bb_envelopes() {
        for frame in [
            OFF,
            INITIAL,
            DOUBLE_INITIAL,
            CHILD_LOCK_ON,
            CHILD_LOCK_OFF,
            REMOTE_START_ON,
            REMOTE_START_OFF,
            STANDARD_DETECTING,
            STANDARD_DRYING,
            STANDARD_PAUSED,
            STANDARD_ENERGY_DELICATE_RESERVED_19H,
            STANDARD_SPEED_LOW_RESERVED_3H,
            ANTI_CREASE_ON,
            ANTI_CREASE_OFF,
            DUVET_RESERVED_4H,
            STEAM_REFRESH_RESERVED_3H,
            STEAM_REFRESH_RESUMED_11H,
            STEAM_REFRESH_RESERVED_14H_PARTWAY,
            CONDENSER_CARE_RUNNING,
            SHIRTS_LOW_AC_ON_RESERVED_3H,
            TOWEL_RESERVED_3H,
            BIG_SIZE_ITEM_RESERVED_3H,
        ] {
            assert_intact(&hex_decode(frame));
        }
    }

    #[test]
    fn exposes_the_common_read_only_status_sensors() {
        let (ha, _thinq, _dev, _memory) = make_device();
        let components = discovered(&ha);
        let mut keys: Vec<&str> = components.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "anti_crease",
                "anti_crease_select",
                "child_lock",
                "course",
                "course_select",
                "dry_level",
                "dry_level_select",
                "eco_hybrid",
                "eco_hybrid_select",
                "energy",
                "error",
                "error_message",
                "initial_time",
                "pause",
                "power",
                "power_off",
                "remaining_time",
                "remote_start",
                "reservation",
                "reserve_hours",
                "reserve_time",
                "resume",
                "smart_course",
                "smart_course_select",
                "smart_diagnosis",
                "start_course",
                "status",
                "steam",
            ]
        );
        let writable = [
            "pause",
            "power_off",
            "start_course",
            "resume",
            "course_select",
            "smart_course_select",
            "reserve_hours",
            "dry_level_select",
            "eco_hybrid_select",
            "anti_crease_select",
        ];
        for (id, component) in &components {
            if writable.contains(&id.as_str()) {
                assert_eq!(component["command_topic"], format!("$this/{id}/set"));
            } else {
                assert!(component.get("command_topic").is_none(), "{id}");
            }
        }
        assert_eq!(components["pause"]["platform"], "button");
        assert_eq!(components["pause"]["payload_press"], "");
        assert_eq!(components["power_off"]["platform"], "button");
        assert_eq!(components["power_off"]["payload_press"], "");
        assert_eq!(components["course"]["device_class"], "enum");
        assert_eq!(components["dry_level"]["device_class"], "enum");
        assert_eq!(components["eco_hybrid"]["device_class"], "enum");
        assert_eq!(components["power"]["icon"], "mdi:power");
        assert_eq!(components["status"]["icon"], "mdi:tumble-dryer");
        assert!(components["child_lock"].get("device_class").is_none());
        assert_eq!(components["child_lock"]["entity_category"], "diagnostic");
        assert_eq!(components["error"]["device_class"], "problem");
        assert_eq!(components["error_message"]["device_class"], "enum");
        assert_eq!(components["error_message"]["entity_category"], "diagnostic");
        assert_eq!(components["smart_diagnosis"]["device_class"], "problem");
        // Energy is a first-class reading like the washer's, not a
        // diagnostic: no entity_category, so it stays on the device card.
        assert!(components["energy"].get("entity_category").is_none());
    }

    #[test]
    fn decodes_the_current_real_powered_off_eb_snapshot() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Power off"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "smart_diagnosis").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
    }

    #[test]
    fn publishes_this_cycle_energy_from_the_record_18_wh_counter() {
        // Real 04:21 cycle captures: the counter rises 176 -> 694 and the
        // ThinQ app reported 694Wh for the day, so the value is Wh x1 —
        // the same convention as the F24VDD washer.
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, ENERGY_MID_CYCLE);
        assert_eq!(prop(&ha, "energy").as_deref(), Some("176"));
        emit(&thinq, ENERGY_AT_COMPLETION);
        assert_eq!(prop(&ha, "energy").as_deref(), Some("694"));
    }

    #[test]
    fn decodes_the_real_initial_eb_snapshot() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, INITIAL);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
    }

    #[test]
    fn uses_the_current_record_in_a_real_ec_frame() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, OFF);
        emit(&thinq, DOUBLE_INITIAL);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
    }

    #[test]
    fn keeps_error_separate_from_the_state_enum_and_derives_it_from_the_error_code() {
        let (ha, thinq, _dev, _memory) = make_device();
        thinq.emit_data(&with_state(5));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Error"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "smart_diagnosis").as_deref(), Some("OFF"));
        thinq.emit_data(&with_state(8));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Smart diagnosis"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "smart_diagnosis").as_deref(), Some("ON"));
    }

    #[test]
    fn maps_the_model_declared_error_byte_and_safely_handles_an_undefined_code() {
        let (ha, thinq, _dev, _memory) = make_device();
        thinq.emit_data(&with_error(1));
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("tE1"));
        thinq.emit_data(&with_error(0));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("Normal"));
        // Upstream publishes the made-up "Unsupported"; an unmapped code is HA's unknown here.
        thinq.emit_data(&with_error(3));
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("None"));
    }

    #[test]
    fn unmapped_course_dry_level_and_eco_hybrid_codes_read_as_none() {
        // rec[6], rec[8] and rec[9]: codes none of the model's tables declare.
        let (ha, thinq, _dev, _memory) = make_device();
        thinq.emit_data(&probe(&[(11, 3), (13, 6), (14, 4)]));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("None"));
    }

    #[test]
    fn uses_the_captured_process_sub_state_for_detecting_drying_then_pause() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, STANDARD_DETECTING);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Detecting"));
        emit(&thinq, STANDARD_DRYING);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        emit(&thinq, STANDARD_PAUSED);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Pause"));
    }

    #[test]
    fn reports_reserved_while_a_real_19h_or_3h_reservation_is_pending() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, STANDARD_ENERGY_DELICATE_RESERVED_19H);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Reserved"));
        emit(&thinq, STANDARD_SPEED_LOW_RESERVED_3H);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Reserved"));
    }

    #[test]
    fn reservation_flag_follows_the_same_rec15_byte_as_anti_crease_bit_0x01() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, STANDARD_ENERGY_DELICATE_RESERVED_19H);
        assert_eq!(prop(&ha, "reservation").as_deref(), Some("ON"));
        emit(&thinq, OFF);
        assert_eq!(prop(&ha, "reservation").as_deref(), Some("OFF"));
    }

    #[test]
    fn decodes_child_lock_from_the_isolated_real_on_and_off_transition() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, CHILD_LOCK_ON);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        emit(&thinq, CHILD_LOCK_OFF);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn a_frame_with_a_corrupted_checksum_is_dropped_not_decoded() {
        // The shared AABB base never verifies the checksum it writes in
        // send(); this device checks it on receive too. Flip a byte in a real
        // captured frame without recomputing the checksum, and the whole
        // update must be ignored.
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));

        let mut corrupted = hex_decode(CHILD_LOCK_ON);
        corrupted[10] ^= 0xff; // flip a mid-frame byte, checksum left untouched
        thinq.emit_data(&corrupted);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
    }

    #[test]
    fn does_not_mistake_the_unrelated_0x08_bit_for_child_lock() {
        // The first mapping used rec[15] bit 0x08, which is also set in plain
        // idle and paused captures the owner never locked. Those have to read
        // OFF or the lock is reported on for a dryer nobody locked.
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, INITIAL);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        emit(&thinq, STANDARD_PAUSED);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn keeps_child_lock_and_anti_crease_on_separate_bits_of_the_same_byte() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, ANTI_CREASE_ON);
        assert_eq!(prop(&ha, "anti_crease").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        emit(&thinq, CHILD_LOCK_ON);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "anti_crease").as_deref(), Some("OFF"));
    }

    #[test]
    fn decodes_anti_crease_from_the_single_toggle_on_and_off_pair() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, ANTI_CREASE_ON);
        assert_eq!(prop(&ha, "anti_crease").as_deref(), Some("ON"));
        emit(&thinq, ANTI_CREASE_OFF);
        assert_eq!(prop(&ha, "anti_crease").as_deref(), Some("OFF"));
    }

    #[test]
    fn decodes_remote_start_from_the_isolated_real_on_and_off_transition() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, REMOTE_START_ON);
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        emit(&thinq, REMOTE_START_OFF);
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn decodes_course_dry_level_eco_and_steam_from_real_reservation_frames() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, STANDARD_ENERGY_DELICATE_RESERVED_19H);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Standard"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Delicate"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Energy"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("30"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("30"));
        assert_eq!(prop(&ha, "reserve_time").as_deref(), Some("1140"));
        emit(&thinq, SHIRTS_LOW_AC_ON_RESERVED_3H);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Easy Care"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Light"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        emit(&thinq, DUVET_RESERVED_4H);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Bulky Item"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Speed"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        emit(&thinq, TOWEL_RESERVED_3H);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Towel"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        emit(&thinq, STEAM_REFRESH_RESERVED_3H);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Steam Refresh"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        emit(&thinq, STEAM_REFRESH_RESUMED_11H);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Steam Refresh"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Reserved"));
        emit(&thinq, CONDENSER_CARE_RUNNING);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Condenser Care"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Steam"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
    }

    #[test]
    fn start_course_replays_the_captured_template_per_course_with_reserve_folded_in() {
        for (course, reserve, expected) in [
            (
                "Steam Refresh",
                "0",
                "aa14f0260100020000000000000003080000b7bb",
            ),
            ("Towel", "0", "aa14f02602000200000000000000030000008ebb"),
            (
                "Bulky Item",
                "0",
                "aa14f0260400030000000000000203000000b5bb",
            ),
            ("Easy Care", "0", "aa14f0260503020000000000000003000000b4bb"),
            ("Standard", "0", "aa14f0260703020000000000000001000000b4bb"),
            (
                "Sports Wear",
                "0",
                "aa14f0260800010000000000000003000000b5bb",
            ),
            ("Quick Dry", "0", "aa14f0260900030000000000000203000000b0bb"),
            ("Wool", "0", "aa14f0260b00020000000000000003000000b1bb"),
            (
                "Bedding Brush",
                "0",
                "aa14f0260f00030000000000000003000000bcbb",
            ),
            (
                "Allergy Care",
                "0",
                "aa14f0261000030000000000000003080000a7bb",
            ),
            (
                "Condenser Care",
                "0",
                "aa14f0261200030000000000000003080000a1bb",
            ),
            ("Tub Clean", "0", "aa14f0261300030000000000000003080000a0bb"),
            (
                "Padding Refresh",
                "0",
                "aa14f0261400030000000000000003000000bbbb",
            ),
            ("Time Dry", "0", "aa14f0261500021e0000000000000300000059bb"),
            (
                "Outdoor Refresh",
                "0",
                "aa14f0261600033c0000000000000300000079bb",
            ),
            ("Baby Wear", "0", "aa14f0261700020000000000000003000000a5bb"),
            ("Standard", "3", "aa14f0260703020000000300000001000000b1bb"),
        ] {
            let (_ha, thinq, dev, _memory) = make_device();
            dev.set_property("course_select", course);
            dev.set_property("reserve_hours", reserve);
            dev.set_property("start_course", "");
            assert_eq!(sent(&thinq), [expected], "{course}");
        }
    }

    #[test]
    fn start_course_folds_dry_eco_and_anti_crease_in_only_where_the_model_allows() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("course_select", "Standard");
        dev.set_property("dry_level_select", "Strong");
        dev.set_property("eco_hybrid_select", "Speed");
        dev.set_property("anti_crease_select", "On");
        dev.set_property("reserve_hours", "3");
        dev.set_property("start_course", "");
        assert_eq!(sent(&thinq)[0], "aa14f0260705030000000300000201000000bcbb");
        // Easy Care allows Light/Standard only: Strong leaves the template byte.
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("course_select", "Easy Care");
        dev.set_property("dry_level_select", "Strong");
        dev.set_property("eco_hybrid_select", "Speed");
        dev.set_property("start_course", "");
        assert_eq!(sent(&thinq)[0], "aa14f0260503020000000000000003000000b4bb");
        // Condenser Care declares no anti-crease: On leaves the template byte.
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("course_select", "Condenser Care");
        dev.set_property("anti_crease_select", "On");
        dev.set_property("start_course", "");
        assert_eq!(sent(&thinq)[0], "aa14f0261200030000000000000003080000a1bb");
    }

    #[test]
    fn resume_replays_the_remembered_full_start_frame() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("resume", "");
        assert_eq!(sent(&thinq), [STANDARD_START]);
    }

    #[test]
    fn reserve_hours_1_or_2_is_refused_lg_declares_0_or_3_to_19() {
        let (ha, _thinq, dev, _memory) = make_device();
        dev.set_property("reserve_hours", "1");
        assert_eq!(prop(&ha, "reserve_hours").as_deref(), Some("0"));
        dev.set_property("reserve_hours", "2");
        assert_eq!(prop(&ha, "reserve_hours").as_deref(), Some("0"));
        // 0 and the declared 3..19 bounds remain accepted.
        dev.set_property("reserve_hours", "3");
        assert_eq!(prop(&ha, "reserve_hours").as_deref(), Some("3"));
        dev.set_property("reserve_hours", "19");
        assert_eq!(prop(&ha, "reserve_hours").as_deref(), Some("19"));
        dev.set_property("reserve_hours", "20");
        assert_eq!(prop(&ha, "reserve_hours").as_deref(), Some("19"));
    }

    #[test]
    fn selecting_a_course_resets_options_to_the_model_defaults() {
        let (ha, _thinq, dev, _memory) = make_device();
        dev.set_property("course_select", "Bulky Item");
        assert_eq!(prop(&ha, "course_select").as_deref(), Some("Bulky Item"));
        assert_eq!(prop(&ha, "anti_crease_select").as_deref(), Some("On"));
        assert_eq!(prop(&ha, "eco_hybrid_select").as_deref(), Some("Speed"));
        dev.set_property("course_select", "Sports Wear");
        assert_eq!(prop(&ha, "anti_crease_select").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "eco_hybrid_select").as_deref(), Some("Energy"));
    }

    #[test]
    fn invalid_select_values_fall_back_to_the_remembered_defaults() {
        let (ha, thinq, dev, _memory) = make_device();
        dev.set_property("course_select", "Rack Dry");
        dev.set_property("reserve_hours", "20");
        dev.set_property("dry_level_select", "Damp");
        dev.set_property("start_course", "");
        assert_eq!(prop(&ha, "course_select").as_deref(), Some("Standard"));
        assert_eq!(sent(&thinq)[0], STANDARD_START);
    }

    #[test]
    fn counts_the_reserve_minute_byte_not_just_whole_hours() {
        // A 14h reservation read 13h56m left: rec[11]=13, rec[12]=56. Reading
        // the hour alone reported 780 minutes for a 836-minute countdown.
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, STEAM_REFRESH_RESERVED_14H_PARTWAY);
        assert_eq!(prop(&ha, "reserve_time").as_deref(), Some("836"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Reserved"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Steam Refresh"));
    }

    #[test]
    fn every_status_value_it_can_publish_is_a_declared_option() {
        // device_class enum forces any value outside options to unknown, so a
        // fallback like `Code 12` would blank the entity instead of informing.
        let (ha, thinq, _dev, _memory) = make_device();
        let status_options = options(&ha, "status");
        for frame in [
            INITIAL,
            OFF,
            STANDARD_ENERGY_DELICATE_RESERVED_19H,
            STANDARD_DETECTING,
            STANDARD_DRYING,
            STANDARD_PAUSED,
            STEAM_REFRESH_RESERVED_14H_PARTWAY,
            CONDENSER_CARE_RUNNING,
            CHILD_LOCK_ON,
        ] {
            emit(&thinq, frame);
            let status = prop(&ha, "status").unwrap();
            assert!(has(&status_options, &status), "status {status}");
        }
    }

    #[test]
    fn real_captures_never_publish_the_literal_none_which_ha_reads_as_unknown() {
        // HA's MQTT sensor maps the payload 'None' to PAYLOAD_NONE and forces
        // the state to unknown, so every real capture must decode to a label;
        // only codes no capture has shown read as None.
        let (ha, thinq, _dev, _memory) = make_device();
        for frame in [
            INITIAL,
            OFF,
            STANDARD_ENERGY_DELICATE_RESERVED_19H,
            STEAM_REFRESH_RESERVED_3H,
            STEAM_REFRESH_RESUMED_11H,
            CONDENSER_CARE_RUNNING,
            SHIRTS_LOW_AC_ON_RESERVED_3H,
        ] {
            emit(&thinq, frame);
            for (key, value) in ha.device(DEVICE_ID).unwrap().properties {
                assert_ne!(
                    value.as_string(),
                    "None",
                    "{key} published the literal None"
                );
            }
        }
    }

    #[test]
    fn offers_only_startable_courses_and_no_read_only_labels_in_the_selects() {
        let (ha, _thinq, _dev, _memory) = make_device();
        let course_select = options(&ha, "course_select");
        assert!(!has(&course_select, "Unsupported"));
        assert!(!has(&options(&ha, "dry_level_select"), "Unsupported"));
        assert!(!has(&options(&ha, "eco_hybrid_select"), "Unsupported"));
        assert!(!has(&course_select, "Off"));
        // Every offered native course and Downloaded Course has a start path.
        assert_eq!(course_select.len(), 17);
        assert!(has(&course_select, "Downloaded Course"));
        // An unmapped code reads back as None, HA's unknown, so the sensors
        // declare no "Unsupported" placeholder (upstream's fallback).
        assert!(!has(&options(&ha, "course"), "Unsupported"));
        assert!(has(&options(&ha, "dry_level"), "Off"));
        assert!(has(&options(&ha, "eco_hybrid"), "Off"));
        assert!(!has(&options(&ha, "status"), "Unsupported"));
        // Only download courses with a captured install blob are offered.
        assert_eq!(
            options(&ha, "smart_course_select"),
            [
                "Powerful Dry",
                "Wrinkle Care Dry",
                "Full Size Load",
                "Refresh",
                "Small Load",
                "Gym Clothes",
                "Rainy Season",
                "Economic Dry",
                "Easy Iron",
                "Big Size Item",
            ]
        );
    }

    #[test]
    fn power_off_reproduces_the_exact_thinq_app_command_captured_by_mcp() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("power_off", "");
        assert_eq!(sent(&thinq), ["aa09f0240101009cbb"]);
    }

    #[test]
    fn pause_reproduces_the_exact_thinq_app_command_captured_by_mcp() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("pause", "");
        assert_eq!(sent(&thinq), ["aa09f02404010099bb"]);
    }

    #[test]
    fn smart_course_select_installs_and_stays_visible_while_powered_off() {
        let (ha, thinq, dev, _memory) = make_device();
        emit(&thinq, OFF);
        dev.set_property("smart_course_select", "Powerful Dry");
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(
            sent(&thinq).last().map(String::as_str),
            Some(POWERFUL_DRY_INSTALL)
        );
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("Powerful Dry"));
        assert_eq!(
            prop(&ha, "smart_course_select").as_deref(),
            Some("Powerful Dry")
        );
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Downloaded Course")
        );
    }

    #[test]
    fn restores_an_armed_smart_course_when_the_handler_reconnects() {
        let memory = MemoryFile::new();
        let ha = MockHaConnection::new();
        let (_first_thinq, first) = handler(&ha, &memory);
        first.set_property("smart_course_select", "Wrinkle Care Dry");

        let (second_thinq, _second) = handler(&ha, &memory);
        assert_eq!(
            prop(&ha, "smart_course").as_deref(),
            Some("Wrinkle Care Dry")
        );
        assert_eq!(
            prop(&ha, "smart_course_select").as_deref(),
            Some("Wrinkle Care Dry")
        );
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Downloaded Course")
        );
        assert!(second_thinq.outbox().is_empty());
    }

    #[test]
    fn installing_a_download_course_replays_the_exact_captured_app_bytes() {
        // Both install frames were captured live from the ThinQ app's own
        // toDevice traffic while the owner downloaded each course.
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Powerful Dry");
        assert_eq!(sent(&thinq), [POWERFUL_DRY_INSTALL]);
    }

    #[test]
    fn installing_the_second_download_course_replays_its_captured_bytes() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Wrinkle Care Dry");
        assert_eq!(
            sent(&thinq),
            ["aa1df025031500025a0000000002000007720000000300000000009bbb"]
        );
    }

    #[test]
    fn installing_full_size_load_replays_its_captured_bytes() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Full Size Load");
        assert_eq!(
            sent(&thinq),
            ["aa1df02503150003640000000000000007740000000500000000008ebb"]
        );
    }

    #[test]
    fn installing_refresh_replays_its_captured_bytes() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Refresh");
        assert_eq!(
            sent(&thinq),
            ["aa1df0250315000314000000000000000f6b000000000000000000d0bb"]
        );
    }

    #[test]
    fn installing_small_load_replays_its_captured_bytes() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Small Load");
        assert_eq!(
            sent(&thinq),
            ["aa1df025031500031e000000000000000e6c000000000000000000dabb"]
        );
    }

    #[test]
    fn installing_gym_clothes_replays_its_captured_bytes() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Gym Clothes");
        assert_eq!(
            sent(&thinq),
            ["aa1df025031500013d000000000000000866000000000000000000f5bb"]
        );
    }

    #[test]
    fn installing_rainy_season_replays_its_captured_bytes() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Rainy Season");
        assert_eq!(
            sent(&thinq),
            ["aa1df0250315000328000000000000000e69000000000000000000c3bb"]
        );
    }

    #[test]
    fn installing_economic_dry_replays_its_captured_bytes() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Economic Dry");
        assert_eq!(
            sent(&thinq),
            ["aa1df025031500017d000000000000000770000000030000000000b9bb"]
        );
    }

    #[test]
    fn installing_easy_iron_replays_its_captured_bytes() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Easy Iron");
        assert_eq!(
            sent(&thinq),
            ["aa1df025031500034100000000000000076e000000010000000000fbbb"]
        );
    }

    #[test]
    fn installing_big_size_item_replays_its_captured_bytes() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Big Size Item");
        assert_eq!(
            sent(&thinq),
            ["aa1df02503150003af0000000000000004710000000000000000004ebb"]
        );
    }

    #[test]
    fn refuses_a_download_course_with_no_captured_install_blob() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn identifies_a_running_download_and_reports_downloaded_course() {
        let (ha, thinq, dev, _memory) = make_device();
        dev.start();
        emit(&thinq, BIG_SIZE_ITEM_RESERVED_3H);
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("Big Size Item"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Downloaded Course"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Reserved"));
        assert!(has(&options(&ha, "course"), "Downloaded Course"));
        assert!(has(&options(&ha, "course_select"), "Downloaded Course"));
    }

    // Bridge mode tunnels an app-issued F025 straight to the physical
    // appliance, bypassing set_property entirely. HA must still learn about
    // the change, the same way it would if the install had been requested
    // from smart_course_select.
    #[test]
    fn bridge_tunnelled_app_install_updates_ha_the_same_as_a_local_select() {
        let (ha, thinq, _dev, _memory) = make_device();
        thinq.emit_send_packet(&hex_decode(POWERFUL_DRY_INSTALL));
        assert_eq!(
            prop(&ha, "smart_course_select").as_deref(),
            Some("Powerful Dry")
        );
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Downloaded Course")
        );
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("Powerful Dry"));
    }

    #[test]
    fn bridge_tunnelled_installs_are_ignored_when_the_body_matches_no_known_course() {
        let (ha, thinq, _dev, _memory) = make_device();
        thinq.emit_send_packet(&hex_decode("aa09f0240101009cbb")); // power off, not an install
        assert_eq!(prop(&ha, "smart_course_select"), None);
        assert_eq!(prop(&ha, "course_select").as_deref(), Some("Standard"));
    }

    #[test]
    fn bridge_tunnelled_install_still_lets_ha_start_the_tunnelled_course() {
        let (_ha, thinq, dev, _memory) = make_device();
        thinq.emit_send_packet(&hex_decode(POWERFUL_DRY_INSTALL));
        thinq.reset_recorder();
        dev.set_property("start_course", "");
        assert_eq!(sent(&thinq), [POWERFUL_DRY_START]);
    }

    // The armed course's signature only shows up on the wire once a run is
    // reserved or executing (see smart_course_of). An idle/powered-off status
    // frame that follows an install carries no signature at all, and must
    // not be read as "nothing armed" and flapped back to Unknown.
    #[test]
    fn an_armed_download_survives_an_idle_status_frame_with_no_signature() {
        let (ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Powerful Dry");
        emit(&thinq, OFF);
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("Powerful Dry"));
        assert_eq!(
            prop(&ha, "smart_course_select").as_deref(),
            Some("Powerful Dry")
        );
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Downloaded Course")
        );
        assert_eq!(prop(&ha, "course").as_deref(), Some("Downloaded Course"));
    }

    #[test]
    fn a_bridge_tunnelled_install_survives_a_follow_up_idle_status_frame() {
        let (ha, thinq, _dev, _memory) = make_device();
        thinq.emit_send_packet(&hex_decode(POWERFUL_DRY_INSTALL));
        emit(&thinq, OFF);
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("Powerful Dry"));
        assert_eq!(
            prop(&ha, "smart_course_select").as_deref(),
            Some("Powerful Dry")
        );
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Downloaded Course")
        );
    }

    #[test]
    fn a_real_native_course_status_frame_keeps_the_installed_download_visible() {
        // F24VDD washer convention: running a native course does not
        // uninstall the download, and smart_course keeps showing it whether
        // it runs or not. Only the native evidence itself is published.
        let (ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Powerful Dry");
        emit(&thinq, STANDARD_DETECTING);
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("Powerful Dry"));
        assert_eq!(prop(&ha, "course_select").as_deref(), Some("Standard"));
    }

    #[test]
    fn repeated_identical_status_frames_do_not_rewrite_the_memory_file() {
        // Upstream makes the file read-only and spies on console.warn. Here a
        // marker replaces the file after the first save instead: any rewrite
        // would replace the marker with this device's entry again.
        let (_ha, thinq, _dev, memory) = make_device();
        emit(&thinq, STANDARD_DETECTING);
        assert!(memory.path().exists());
        std::fs::write(memory.path(), "unchanged").unwrap();
        emit(&thinq, STANDARD_DETECTING);
        emit(&thinq, STANDARD_DETECTING);
        assert_eq!(std::fs::read_to_string(memory.path()).unwrap(), "unchanged");
    }

    #[test]
    fn a_restart_restores_the_installed_download_from_disk_memory() {
        // Arm a download, then build a fresh handler on a fresh HA
        // connection (what a real restart builds: the in-process memory
        // misses, so the handler falls back to the disk memory file).
        let (_ha, _thinq, first, memory) = make_device();
        first.set_property("smart_course_select", "Powerful Dry");
        let ha2 = MockHaConnection::new();
        let (_thinq2, _second) = handler(&ha2, &memory);
        assert_eq!(prop(&ha2, "smart_course").as_deref(), Some("Powerful Dry"));
        assert_eq!(
            prop(&ha2, "smart_course_select").as_deref(),
            Some("Powerful Dry")
        );
        assert_eq!(
            prop(&ha2, "course_select").as_deref(),
            Some("Downloaded Course")
        );
    }

    #[test]
    fn a_restart_restores_a_native_course_selection_and_its_options_from_disk() {
        let (_ha, _thinq, first, memory) = make_device();
        first.set_property("course_select", "Bulky Item");
        first.set_property("dry_level_select", "Standard+");
        first.set_property("eco_hybrid_select", "Energy");
        first.set_property("anti_crease_select", "On");
        first.set_property("reserve_hours", "5");
        let ha2 = MockHaConnection::new();
        let (_thinq2, _second) = handler(&ha2, &memory);
        assert_eq!(prop(&ha2, "course_select").as_deref(), Some("Bulky Item"));
        assert_eq!(prop(&ha2, "dry_level_select").as_deref(), Some("Standard+"));
        assert_eq!(prop(&ha2, "eco_hybrid_select").as_deref(), Some("Energy"));
        assert_eq!(prop(&ha2, "anti_crease_select").as_deref(), Some("On"));
        assert_eq!(prop(&ha2, "reserve_hours").as_deref(), Some("5"));
    }

    #[test]
    fn a_restart_shows_an_installed_but_idle_download_without_arming_it() {
        // Installed on the appliance, nothing armed (e.g. a native course
        // was running when rethink restarted): smart_course still shows the
        // download while course_select follows the restored native course.
        let memory = MemoryFile::new();
        let entry = json!({
            "downloadedCourse": "Powerful Dry",
            "useDownloadedCourse": false,
            "selectedCourse": 7,
        });
        std::fs::write(memory.path(), json!({ DEVICE_ID: entry }).to_string()).unwrap();
        let ha2 = MockHaConnection::new();
        let (_thinq2, _second) = handler(&ha2, &memory);
        assert_eq!(prop(&ha2, "smart_course").as_deref(), Some("Powerful Dry"));
        assert_eq!(
            prop(&ha2, "smart_course_select").as_deref(),
            Some("Powerful Dry")
        );
        assert_eq!(prop(&ha2, "course_select").as_deref(), Some("Standard"));
    }

    #[test]
    fn a_torn_memory_file_boots_with_defaults_instead_of_throwing() {
        // What a crash mid-write used to leave behind: truncated JSON.
        // Atomic saves make this impossible going forward, but files torn
        // by older versions must still boot cleanly with defaults.
        // (A fresh HA connection misses the in-process memory, so this
        // genuinely exercises the disk path.)
        let memory = MemoryFile::new();
        std::fs::write(
            memory.path(),
            format!(r#"{{"{DEVICE_ID}": {{"downloadedCourse": "Power"#),
        )
        .unwrap();
        let ha2 = MockHaConnection::new();
        let (_thinq2, _second) = handler(&ha2, &memory);
        assert_eq!(prop(&ha2, "course_select").as_deref(), Some("Standard"));
        assert_eq!(prop(&ha2, "smart_course"), None);
    }

    #[test]
    fn a_fresh_idle_frame_after_restart_leaves_smart_course_untouched() {
        // Post-restart the arming memory is gone while the appliance may
        // still have a download installed. With no positive evidence either
        // way, smart_course must keep HA's last displayed value instead of
        // being asserted to Unknown (the F24VDD washer convention).
        let (ha, thinq, _dev, _memory) = make_device();
        emit(&thinq, OFF);
        assert_eq!(prop(&ha, "smart_course"), None);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
    }

    #[test]
    fn starts_powerful_dry_through_downloaded_course_with_the_captured_f026_frame() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Powerful Dry");
        dev.set_property("course_select", "Downloaded Course");
        dev.set_property("start_course", "");
        assert_eq!(sent(&thinq), [POWERFUL_DRY_INSTALL, POWERFUL_DRY_START]);
    }

    #[test]
    fn starts_wrinkle_care_dry_through_downloaded_course_with_the_captured_f026_frame() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("smart_course_select", "Wrinkle Care Dry");
        dev.set_property("course_select", "Downloaded Course");
        dev.set_property("start_course", "");
        let sent = sent(&thinq);
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1], "aa14f0260703025a00000000000203007200e4bb");
    }

    #[test]
    fn builds_a_checksummed_start_frame_for_every_captured_download_definition() {
        for (course, frame) in [
            ("Powerful Dry", "aa14f026110002640000000000000300770090bb"),
            (
                "Wrinkle Care Dry",
                "aa14f0260703025a00000000000203007200e4bb",
            ),
            ("Full Size Load", "aa14f0260705036400000000000003007400ebbb"),
            ("Refresh", "aa14f0260f00031400000000000003006b003dbb"),
            ("Small Load", "aa14f0260e00031e00000000000003006c0027bb"),
            ("Gym Clothes", "aa14f0260800013d00000000000003006600d6bb"),
            ("Rainy Season", "aa14f0260e000328000000000000030069002cbb"),
            ("Economic Dry", "aa14f0260703017d000000000000030070009abb"),
            ("Easy Iron", "aa14f0260701034100000000000003006e00c4bb"),
            ("Big Size Item", "aa14f026040003af00000000000003007100abbb"),
        ] {
            let (_ha, thinq, dev, _memory) = make_device();
            dev.set_property("smart_course_select", course);
            dev.set_property("course_select", "Downloaded Course");
            dev.set_property("start_course", "");
            assert_eq!(sent(&thinq)[1], frame, "{course}");
            assert_intact(&thinq.outbox()[1]);
        }
    }

    #[test]
    fn does_not_guess_a_downloaded_start_or_resume_without_captured_state() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.set_property("course_select", "Downloaded Course");
        dev.set_property("start_course", "");
        dev.set_property("resume", "");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn asks_only_for_the_family_wide_read_only_status_snapshot_on_connect() {
        let (_ha, thinq, dev, _memory) = make_device();
        dev.start();
        assert_eq!(sent(&thinq), [STATUS_REQUEST_PACKET]);
    }

    #[test]
    fn ignores_other_device_types_and_malformed_rh16_status_shapes() {
        let (ha, thinq, _dev, _memory) = make_device();
        emit(
            &thinq,
            "AA2120EB00190000000000000000000000000000000000000000000000770033BB",
        );
        emit(&thinq, "AA0730EB00A8BB");
        assert_eq!(prop(&ha, "power"), None);
        assert_eq!(prop(&ha, "status"), None);
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("RH16_T_KR").is_some());
    }
}
