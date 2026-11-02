//! RV13U6AM8W_D_US_WIFI — LG 27" dryer (AABB). The control board is shared by the DLE7300WE
//! (electric) and DLG7301WE (gas), whose frames decode identically.
//!
//! Class `0x30`. `0xEB` (one record), `0xEC` (old state, then current) and the three-record frame
//! of a mid-cycle power-off all end with the current 28-byte record, led by a `0x1b` marker, so
//! the frame tail is decoded. `0xE2` idle snapshots replay stale state and are ignored, as are the
//! `0x31` serial and `0xD8`/`0x72` heartbeats. Auto-ack is on and `start` asks for a snapshot.
//!
//! Start (`F0 26`) is rebuilt from the last record, so it runs what is set on the machine; offset 10
//! is `0x41` for a new cycle and `0x01` to resume a pause. Pause and power off are the washer's
//! `F0 24` commands. Nothing is gated on Remote Start: the dryer ignores an unarmed start itself.
//!
//! Port of upstream PR #175.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_status;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x30;
const RECORD_MARKER: u8 = 0x1b;
const RECORD_LEN: usize = 28;
const HEADER_LEN: usize = 3;
const STATUS_FRAME_TYPES: [u8; 2] = [0xeb, 0xec];

const PHASE_OFFSET: usize = 1;
const TIME_HOUR_OFFSET: usize = 2;
const TIME_MIN_OFFSET: usize = 3;
// Projected total length (remaining + elapsed), recomputed live rather than fixed at start.
const INITIAL_TIME_HOUR_OFFSET: usize = 4;
const INITIAL_TIME_MIN_OFFSET: usize = 5;
const COURSE_OFFSET: usize = 6;
const DRY_LEVEL_OFFSET: usize = 8;
const TEMP_OFFSET: usize = 9;
const TIME_DRY_OFFSET: usize = 10;
const SIGNAL_OFFSET: usize = 11;
const MORE_LESS_TIME_OFFSET: usize = 12;
const FLAGS_OFFSET: usize = 15;
const FLAG_CHILD_LOCK: u8 = 0x01;
const FLAG_DAMP_DRY_SIGNAL: u8 = 0x08;
const FLAG_WRINKLE_CARE: u8 = 0x10;
const FLAG_CUSTOM_PGM: u8 = 0x20;
// A lamp, not a cycle setting: never copied into the start command.
const FLAG_DRUM_LIGHT: u8 = 0x40;
const FLAG_ANTI_BACTERIAL: u8 = 0x80;
const OPT2_OFFSET: usize = 16;
// The remote-control session: armed at the panel, but also opened by the dryer for any run and
// dropped by a panel pause or at End.
const OPT2_REMOTE_START: u8 = 0x01;
const OPT2_ENERGY_SAVER: u8 = 0x02;
// rec[18..20] is the elapsed time in 15 s units (initial_time - remaining_time); not published.
const PRE_STATE_OFFSET: usize = 20;
const SMART_COURSE_ACTIVE_OFFSET: usize = 21;
const DOWNLOADED_COURSE_OFFSET: usize = 24;

const CMD_PAUSE: &[u8] = &[0xf0, 0x24, 0x04, 0x01, 0x00];
const CMD_POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];
const START_OPTS_BASE: u8 = 0x01;
const START_OPTS_NEW_CYCLE: u8 = 0x40;
const START_OPTS_ENERGY_SAVER: u8 = 0x02;

const PHASE_OFF: u8 = 0x00;
const PHASE_PAUSED: u8 = 0x03;
const PHASE_END: u8 = 0x04;

const STATUS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Initial"),
    (0x02, "Drying"),
    (0x32, "Drying"),
    (0x03, "Paused"),
    (0x04, "End"),
    (0x05, "Error"),
    (0x08, "Smart Diagnosis"),
    (0x33, "Cooling"),
    (0x38, "Wrinkle Care"),
];
// Dial positions plus the base cycles reachable only through a smart course.
const CYCLE: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x01, "Heavy Duty"),
    (0x03, "Normal"),
    (0x04, "Perm. Press"),
    (0x05, "Delicates"),
    (0x06, "Ultra Delicates"),
    (0x07, "Bedding"),
    (0x0a, "Khaki/Jean"),
    (0x0b, "Sportswear"),
    (0x0c, "Kids Wear"),
    (0x0d, "Low Temp Dry"),
    (0x0e, "Jumbo Dry"),
    (0x0f, "Wool"),
    (0x10, "Speed Dry"),
    (0x11, "Air Dry"),
    (0x12, "Time Dry"),
    (0x17, "Freshen Up"),
    (0x1a, "Super Dry"),
];
const SMART_COURSE: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x64, "Super Dry"),
    (0x65, "Denim"),
    (0x66, "Kids' Clothes"),
    (0x68, "Ultra Delicates"),
    (0x69, "Low Temp Dry"),
    (0x6a, "Freshen Up"),
    (0x6b, "Gym Clothes"),
    (0x6c, "Blankets"),
    (0x6d, "Blanket Refresh"),
    (0x6e, "Rainy Day"),
    (0x70, "Lingerie"),
    (0x71, "Socks"),
    (0x72, "Overnight Dry"),
    (0x73, "Bedding/Curtains"),
    (0xc8, "Kids' Gym Clothes"),
    (0xc9, "Easy Ironing"),
    (0xca, "Wrinkle Prevention"),
    (0xcb, "EconoDry Small Load"),
    (0xcd, "Half Load Dry"),
    (0xce, "Full Load Dry"),
];
const TIME_DRY: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x01, "20 min"),
    (0x02, "30 min"),
    (0x03, "40 min"),
    (0x04, "50 min"),
    (0x05, "60 min"),
];
const SIGNAL: &[(u8, &str)] = &[(0x00, "Off"), (0x01, "Low"), (0x04, "High")];
const TEMPS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Ultra Low"),
    (0x02, "Low"),
    (0x03, "Medium"),
    (0x04, "Med High"),
    (0x05, "High"),
];
const DRY_LEVELS: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x01, "Damp"),
    (0x02, "Less"),
    (0x03, "Normal"),
    (0x04, "More"),
    (0x05, "Very"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
        .unwrap_or("None")
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (_, name) in table {
        if !out.contains(name) {
            out.push(*name);
        }
    }
    out
}

fn components() -> HashMap<String, Value> {
    [
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine"})),
        ("pre_state", json!({"platform":"sensor","unique_id":"$deviceid-pre_state","state_topic":"$this/pre_state","name":"Previous state","icon":"mdi:history","entity_category":"diagnostic","device_class":"enum","options":labels(STATUS)})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time estimate","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
        ("power", json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:tumble-dryer"})),
        ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Control lock","icon":"mdi:lock-alert","entity_category":"diagnostic"})),
        ("wrinkle_care", json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Wrinkle Care","icon":"mdi:tshirt-crew-outline"})),
        ("anti_bacterial", json!({"platform":"binary_sensor","unique_id":"$deviceid-anti_bacterial","state_topic":"$this/anti_bacterial","name":"Anti-Bacterial","icon":"mdi:bacteria-outline"})),
        ("damp_dry_signal", json!({"platform":"binary_sensor","unique_id":"$deviceid-damp_dry_signal","state_topic":"$this/damp_dry_signal","name":"Damp Dry Signal","icon":"mdi:water-alert-outline"})),
        ("energy_saver", json!({"platform":"binary_sensor","unique_id":"$deviceid-energy_saver","state_topic":"$this/energy_saver","name":"Energy Saver","icon":"mdi:leaf"})),
        ("drum_light", json!({"platform":"binary_sensor","unique_id":"$deviceid-drum_light","state_topic":"$this/drum_light","name":"Drum light","icon":"mdi:lightbulb-on-outline"})),
        ("start", json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","name":"Start","icon":"mdi:play"})),
        ("pause", json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","name":"Pause","icon":"mdi:pause"})),
        ("power_off", json!({"platform":"button","unique_id":"$deviceid-power_off","command_topic":"$this/power_off/set","name":"Power off","icon":"mdi:power"})),
        ("custom_pgm", json!({"platform":"binary_sensor","unique_id":"$deviceid-custom_pgm","state_topic":"$this/custom_pgm","name":"Custom program","icon":"mdi:content-save-cog-outline"})),
        ("signal", json!({"platform":"sensor","unique_id":"$deviceid-signal","state_topic":"$this/signal","name":"Signal","icon":"mdi:bell-outline","device_class":"enum","options":labels(SIGNAL)})),
        ("smart_course", json!({"platform":"sensor","unique_id":"$deviceid-smart_course","state_topic":"$this/smart_course","name":"Smart course","icon":"mdi:cloud-download-outline","device_class":"enum","options":labels(SMART_COURSE)})),
        ("downloaded_course", json!({"platform":"sensor","unique_id":"$deviceid-downloaded_course","state_topic":"$this/downloaded_course","name":"Downloaded cycle","icon":"mdi:tray-arrow-down","entity_category":"diagnostic","device_class":"enum","options":labels(SMART_COURSE)})),
        ("time_dry", json!({"platform":"sensor","unique_id":"$deviceid-time_dry","state_topic":"$this/time_dry","name":"Time Dry","icon":"mdi:timer-cog-outline","device_class":"enum","options":labels(TIME_DRY)})),
        ("more_less_time", json!({"platform":"sensor","unique_id":"$deviceid-more_less_time","state_topic":"$this/more_less_time","name":"More/Less time","icon":"mdi:plus-minus-variant","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
        ("cycle", json!({"platform":"sensor","unique_id":"$deviceid-cycle","state_topic":"$this/cycle","name":"Cycle","icon":"mdi:tumble-dryer","device_class":"enum","options":labels(CYCLE)})),
        ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer","device_class":"enum","options":labels(TEMPS)})),
        ("dry_level", json!({"platform":"sensor","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","name":"Dry level","icon":"mdi:water-percent","device_class":"enum","options":labels(DRY_LEVELS)})),
        // Tombstone: the bit published as drum_running is remote_start.
        ("drum_running", json!({"platform":"binary_sensor"})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// Start and resume are one packet; only the 0x40 bit at offset 10 tells them apart. The course is
/// carried twice, at offsets 2 and 12.
fn start_command(rec: &[u8]) -> Vec<u8> {
    let mut opts = START_OPTS_BASE;
    if rec[PHASE_OFFSET] != PHASE_PAUSED {
        opts |= START_OPTS_NEW_CYCLE;
    }
    if rec[OPT2_OFFSET] & OPT2_ENERGY_SAVER != 0 {
        opts |= START_OPTS_ENERGY_SAVER;
    }
    vec![
        0xf0,
        0x26,
        rec[COURSE_OFFSET],
        0,
        0,
        rec[TEMP_OFFSET],
        0,
        0,
        rec[TIME_DRY_OFFSET],
        rec[FLAGS_OFFSET] & !FLAG_DRUM_LIGHT,
        opts,
        0,
        rec[COURSE_OFFSET],
        0,
        0,
        0,
        0,
        rec[DRY_LEVEL_OFFSET],
        0,
        rec[MORE_LESS_TIME_OFFSET],
        0,
        0,
        0,
    ]
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    /// Marker-relative record (`rec[0] == 0x1b`) the start command is built from.
    last_record: Mutex<Option<Vec<u8>>>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            last_record: Mutex::new(None),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < HEADER_LEN + RECORD_LEN
            || buf[0] != CLASS_BYTE
            || !STATUS_FRAME_TYPES.contains(&buf[1])
        {
            return;
        }
        let rec = &buf[buf.len() - RECORD_LEN..];
        if rec[0] == RECORD_MARKER {
            self.process_record(rec);
        }
    }

    fn process_record(&self, rec: &[u8]) {
        *self.last_record.lock() = Some(rec.to_vec());
        let phase = rec[PHASE_OFFSET];
        let is_off = phase == PHASE_OFF;
        let minutes = |hour: usize, min: usize| i64::from(rec[hour]) * 60 + i64::from(rec[min]);

        self.core
            .publish_property("status", named(STATUS, phase).into());
        self.core
            .publish_property("pre_state", named(STATUS, rec[PRE_STATE_OFFSET]).into());
        // Off and End both keep the last countdown minute.
        let remaining = if is_off || phase == PHASE_END {
            0
        } else {
            minutes(TIME_HOUR_OFFSET, TIME_MIN_OFFSET)
        };
        let initial = if is_off {
            0
        } else {
            minutes(INITIAL_TIME_HOUR_OFFSET, INITIAL_TIME_MIN_OFFSET)
        };
        self.core
            .publish_property("remaining_time", PropertyValue::Int(remaining));
        self.core
            .publish_property("initial_time", PropertyValue::Int(initial));
        self.core.publish_on_off("power", !is_off);

        let flags = rec[FLAGS_OFFSET];
        let opt2 = rec[OPT2_OFFSET];
        self.core
            .publish_on_off("remote_start", opt2 & OPT2_REMOTE_START != 0);
        self.core
            .publish_on_off("energy_saver", opt2 & OPT2_ENERGY_SAVER != 0);
        self.core
            .publish_on_off("child_lock", flags & FLAG_CHILD_LOCK != 0);
        self.core
            .publish_on_off("damp_dry_signal", flags & FLAG_DAMP_DRY_SIGNAL != 0);
        self.core
            .publish_on_off("wrinkle_care", flags & FLAG_WRINKLE_CARE != 0);
        self.core
            .publish_on_off("drum_light", flags & FLAG_DRUM_LIGHT != 0);
        self.core
            .publish_on_off("custom_pgm", flags & FLAG_CUSTOM_PGM != 0);
        self.core
            .publish_on_off("anti_bacterial", flags & FLAG_ANTI_BACTERIAL != 0);
        self.core
            .publish_property("signal", named(SIGNAL, rec[SIGNAL_OFFSET]).into());
        self.core
            .publish_property("time_dry", named(TIME_DRY, rec[TIME_DRY_OFFSET]).into());
        // signed: negative trims the course default, positive extends it
        let more_less = if is_off {
            0
        } else {
            i64::from(rec[MORE_LESS_TIME_OFFSET] as i8)
        };
        self.core
            .publish_property("more_less_time", PropertyValue::Int(more_less));
        self.core
            .publish_property("cycle", named(CYCLE, rec[COURSE_OFFSET]).into());
        self.core.publish_property(
            "smart_course",
            named(SMART_COURSE, rec[SMART_COURSE_ACTIVE_OFFSET]).into(),
        );
        self.core.publish_property(
            "downloaded_course",
            named(SMART_COURSE, rec[DOWNLOADED_COURSE_OFFSET]).into(),
        );
        self.core
            .publish_property("temp", named(TEMPS, rec[TEMP_OFFSET]).into());
        self.core
            .publish_property("dry_level", named(DRY_LEVELS, rec[DRY_LEVEL_OFFSET]).into());
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        request_status(&self.core);
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, _value: &str) {
        let Some(rec) = self.last_record.lock().clone() else {
            return;
        };
        match prop {
            "pause" => self.core.send(CMD_PAUSE),
            "power_off" => self.core.send(CMD_POWER_OFF),
            "start" => self.core.send(&start_command(&rec)),
            _ => {}
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const EB_RECONNECT: &str =
        "AA2330EB001B010001000100000000000100000000A8000000000000006400000046BB";
    const EC_HEAVY_DUTY: &str =
        "AA4030EC001B320036003601000305000100000000A90000000100000064000000001B320035003601000305000100000000A90000530100000064000000AFBB";
    const EC_TIME_DRY_SETUP: &str =
        "AA4030EC001B010014001412000005010100000040A80000000000000064000000001B010014001412000001010100000040A8000000000000006400000001BB";
    const EC_RESUMED: &str =
        "AA4030EC001B030012001412000002010100000040A80000743200000064000000001B320012001412000002010100000000A900007403000000640000000ABB";
    const LIVE_APP_PAUSE: &str =
        "AA4030EC001B32001D010012000005050100000010A900007903000000CA000000001B03001D010012000005050100000050A900007D32000000CA0000000DBB";
    const LIVE_PANEL_START: &str =
        "AA4030EC001B010100010007000403000100000040A800000000000000CA000000001B320100010007000403000100000000A800000001000000CA000000E3BB";
    const LIVE_PANEL_START_ARMED: &str =
        "AA4030EC001B320100010007000403000100000000A800000001000000CA000000001B320100010007000403000100000000A900000001000000CA000000FCBB";
    const LIVE_PANEL_PAUSE: &str =
        "AA4030EC001B320013010012000005050100000010A90000A203000000CA000000001B030013010012000005050100000050A80000A532000000CA000000C1BB";
    const LIVE_COOLING: &str =
        "AA4030EC001B330005010012000005050100000010A90000DB32000000CA000000001B330004010012000005050100000010A90000DF32000000CA0000005EBB";
    const LIVE_END: &str =
        "AA4030EC001B330001010012000005050100000010A90000EB32000000CA000000001B040001010112000005050100000050A80000EF33000000CA00000063BB";
    const LIVE_WRINKLE_CARE: &str =
        "AA4030EC001B040001010112000005050100000050A80000EF33000000CA000000001B380001000112000005050100000050A90000EF04000000CA00000004BB";
    const LIVE_DOOR_OPENED: &str =
        "AA4030EC001B000001000100000000000100000040A80000F038000000CA000000001B000001000100000000000100000000A80000F038000000CA000000E3BB";
    const EC_POWER_OFF: &str =
        "AA4030EC001B320012001412000002010100000000A900007403000000640000000F001412000002010100000000A900009F0300000064000000001B00000F000F00000000000100000040A80000A13200000064000000C1BB";
    const EC_COOLING_TRUNCATED: &str =
        "AA4030EC001B330001010301000305000100000040A8000BFD33000000640000000BBB";
    const LIVE_54_53: &str =
        "AA4030EC001B320036010012000005050100000000A900001701000000CA000000001B320035010012000005050100000000A900001B01000000CA00000034BB";
    const LIVE_53_52: &str =
        "AA4030EC001B320035010012000005050100000000A900001B01000000CA000000001B320034010012000005050100000000A900001F01000000CA00000032BB";
    const IDENTITY: &str =
        "AA3730310201534141333839363434323600002DF000008000000000000253414133383936343331300000AEAD000040000000000020BB";
    const ANTI_BAC: &str =
        "AA4030EC001B010029002903000304000100000040A800000000000000CA000000001B01010A010A030005050001000000C0A800000000000000CA000000F6BB";
    const WRINKLE: &str =
        "AA4030EC001B01010A010A03000505000100000080A800000000000000CA000000001B01010A010A03000505000100000090A800000000000000CA0000002FBB";
    const DRUM_LIGHT: &str =
        "AA4030EC001B01010A010A03000505000100000090A800000000000000CA000000001B01010A010A030005050001000000D0A800000000000000CA0000009FBB";
    const CHILD_LOCK: &str =
        "AA4030EC001B010029002903000304000100000018A800000000000000CA000000001B010029002903000304000100000019A800000000000000CA00000058BB";
    const DAMP_DRY: &str =
        "AA4030EC001B010029002903000304000100000010A800000000000000CA000000001B010029002903000304000100000018A800000000000000CA00000051BB";
    const SIG_HIGH: &str =
        "AA4030EC001B010029002903000304000100000018A800000000000000CA000000001B010029002903000304000400000018A800000000000000CA0000005ABB";
    const ML_MINUS5: &str =
        "AA4030EC001B010019001910000005000000000000A800000000000000CA000000001B0100140014100000050000FB000000A800000000000000CA000000F4BB";
    const ML_PLUS5: &str =
        "AA4030EC001B010019001910000005000000000000A800000000000000CA000000001B01001E001E10000005000005000000A800000000000000CA000000EABB";
    const TIMEDRY_60: &str =
        "AA4030EC001B010032003212000005040000000000A800000000000000CA000000001B010100010012000005050000000000A800000000000000CA000000EABB";
    const REMOTE_ON: &str =
        "AA4030EC001B010028002812000005030000000000A800000000000000CA000000001B010028002812000005030000000000A900000000000000CA000000A2BB";
    const DL_BEDDING_CURTAINS: &str =
        "AA4030EC001B010029002903000303000100000010A800000000CA0000CA000000001B010105010507000503000100000000A80000000073000073000000C5BB";
    const DL_BLANKETS: &str =
        "AA4030EC001B010014001412000001010100000000A8000000006D00006D000000001B01012301230E000503000100000000A8000000006C00006C00000089BB";
    const DL_ECONODRY: &str =
        "AA4030EC001B01001E001E03000103000100000000A800000000C90000C9000000001B010103010303000303000100000000AA00000000CB0000CB0000005BBB";
    const DL_SOCKS: &str =
        "AA4030EC001B01003200320D000302000100000000A8000000006E00006E000000001B010023002312000005020105000000A800000000710000710000007DBB";
    const DL_SUPER_DRY: &str =
        "AA4030EC001B010023002312000005020105000000A80000000071000071000000001B01003B003B1A000505000100000000A800000000640000640000006DBB";
    const DL_ULTRA_DELICATES: &str =
        "AA4030EC001B01003B003B1A000505000100000000A80000000064000064000000001B01001E001E06000301000100000000A800000000680000680000005DBB";
    const CUSTOM_PGM: &str =
        "AA4030EC001B010103010303000304000100000000AA00000000000000CA000000001B010103010303000304000100000020AA00000000000000CA00000039BB";
    const ARMED_NORMAL: &str =
        "AA4030EC001B010103010303000304000100000040AA00000000000000CA000000001B010103010303000304000100000040AB00000000000000CA00000098BB";
    const ARMED_TIME_DRY: &str =
        "AA4030EC001B010100010012000005050100000040A800000000000000CA000000001B010100010012000005050100000040A900000000000000CA000000B4BB";
    const ARMED_WRINKLE: &str =
        "AA4030EC001B010103010303000304000100000050AA00001300000000CA000000001B010103010303000304000100000050AB00001300000000CA00000046BB";
    const EB_PAUSED: &str =
        "AA2330EB001B030012001412000002010100000040A8000074320000006400000061BB";
    const LIVE_EB: &str = "AA2330EB001B32001E002603000304000400000000AB0002090100000064000000F7BB";
    const LIVE_EC: [&str; 3] = [
        "AA4030EC001B32001E002603000304000400000000AB0002090100000064000000001B32001E002603000304000400000000AB00020A01000000640000002EBB",
        "AA4030EC001B32001E002603000304000400000000AB00020A0100000064000000001B32001D002603000304000400000000AB00020A01000000640000002EBB",
        "AA4030EC001B32001D002603000304000400000000AB00020A0100000064000000001B32001D002603000304000400000000AB00020B01000000640000002EBB",
    ];
    const LIVE_CYCLE_END: [&str; 2] = [
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

    fn feed(frames: &[&str]) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        for frame in frames {
            emit(&thinq, frame);
        }
        ha
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    fn values(ha: &MockHaConnection, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|n| prop(ha, n).unwrap_or_default())
            .collect()
    }

    fn components_of(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID).unwrap().config.unwrap().components
    }

    /// The AABB body: `AA <len>` and `<csum> BB` stripped.
    fn inner(hex: &str) -> Vec<u8> {
        let b = hex_decode(hex);
        b[2..b.len() - 2].to_vec()
    }

    fn sent(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .outbox()
            .iter()
            .map(|p| hex_encode(p).to_ascii_lowercase())
            .collect()
    }

    fn press(frame: &str, prop: &str) -> Vec<String> {
        let (_, thinq, dev) = make();
        emit(&thinq, frame);
        dev.set_property(prop, "PRESS");
        sent(&thinq)
    }

    #[test]
    fn config_exposes_expected_components() {
        let comps = components_of(&feed(&[]));
        for c in [
            "status",
            "pre_state",
            "remaining_time",
            "initial_time",
            "power",
            "remote_start",
            "child_lock",
            "wrinkle_care",
            "anti_bacterial",
            "damp_dry_signal",
            "energy_saver",
            "drum_light",
            "signal",
            "time_dry",
            "more_less_time",
            "cycle",
            "temp",
            "dry_level",
        ] {
            assert!(comps.contains_key(c), "component {c} present");
        }
        assert!(comps["status"].get("device_class").is_none());
        assert!(comps["status"].get("options").is_none());
    }

    #[test]
    fn start_requests_a_status_snapshot() {
        let (_, thinq, dev) = make();
        dev.start();
        assert_eq!(sent(&thinq), ["aa0ef0ed1121010000001800b5bb"]);
    }

    #[test]
    fn consecutive_live_frames_decode_as_a_one_minute_countdown() {
        expect(&feed(&[LIVE_54_53]), "remaining_time", "53");
        expect(&feed(&[LIVE_53_52]), "remaining_time", "52");
    }

    #[test]
    fn each_frames_first_record_repeats_the_previous_frames_second() {
        let first = inner(LIVE_54_53);
        let second = inner(LIVE_53_52);
        assert_eq!(first[32..], second[3..31]);
    }

    #[test]
    fn cycle_ending_into_wrinkle_care_walks_cooling_end_wrinkle_care() {
        let end = feed(&[LIVE_END]);
        expect(&end, "status", "End");
        expect(&end, "pre_state", "Cooling");
        expect(&end, "remote_start", "OFF");

        let wc = feed(&[LIVE_WRINKLE_CARE]);
        expect(&wc, "status", "Wrinkle Care");
        expect(&wc, "pre_state", "End");
        expect(&wc, "wrinkle_care", "ON");
        expect(&wc, "remote_start", "ON");

        let door = feed(&[LIVE_DOOR_OPENED]);
        expect(&door, "status", "Off");
        expect(&door, "pre_state", "Wrinkle Care");
        expect(&door, "power", "OFF");
    }

    #[test]
    fn resuming_from_pause_reports_drying() {
        let p = feed(&[EC_RESUMED]);
        expect(&p, "status", "Drying");
        expect(&p, "remaining_time", "18");
    }

    #[test]
    fn power_off_mid_cycle_is_decoded_instead_of_dropped() {
        let p = feed(&[LIVE_54_53, EC_POWER_OFF]);
        expect(&p, "status", "Off");
        expect(&p, "power", "OFF");
        expect(&p, "remaining_time", "0");
        expect(&p, "initial_time", "0");
    }

    #[test]
    fn truncated_cooling_frame_still_decodes() {
        expect(&feed(&[EC_COOLING_TRUNCATED]), "status", "Cooling");
    }

    #[test]
    fn pre_state_names_the_phase_the_dryer_just_left() {
        expect(&feed(&[EC_RESUMED]), "pre_state", "Paused");
        expect(&feed(&[LIVE_54_53]), "pre_state", "Initial");
    }

    #[test]
    fn load_switched_off_mid_cycle_still_shows_what_it_was_doing() {
        let p = feed(&[EC_POWER_OFF]);
        expect(&p, "status", "Off");
        expect(&p, "pre_state", "Drying");
    }

    #[test]
    fn initial_time_estimate_reads_the_hour_byte() {
        expect(&feed(&[LIVE_54_53]), "initial_time", "60");
    }

    #[test]
    fn initial_time_pins_while_remaining_time_counts_down() {
        let a = feed(&[LIVE_54_53]);
        let b = feed(&[LIVE_53_52]);
        assert_eq!(prop(&a, "initial_time"), prop(&b, "initial_time"));
        let remaining =
            |ha: &MockHaConnection| -> i64 { prop(ha, "remaining_time").unwrap().parse().unwrap() };
        assert_eq!(remaining(&a) - remaining(&b), 1);
    }

    #[test]
    fn powered_off_dryer_reports_no_cycle_rather_than_an_unknown_one() {
        expect(&feed(&[EC_POWER_OFF]), "cycle", "Not selected");
    }

    #[test]
    fn heavy_duty_capture_decodes_its_own_declared_defaults() {
        let p = feed(&[EC_HEAVY_DUTY]);
        expect(&p, "cycle", "Heavy Duty");
        expect(&p, "dry_level", "Normal");
        expect(&p, "temp", "High");
    }

    #[test]
    fn time_dry_has_no_dry_level_and_carries_the_newly_pressed_temperature() {
        let p = feed(&[EC_TIME_DRY_SETUP]);
        expect(&p, "cycle", "Time Dry");
        expect(&p, "dry_level", "Not selected");
        expect(&p, "temp", "Ultra Low");
    }

    #[test]
    fn panel_start_arms_itself_one_frame_into_drying() {
        let starting = feed(&[LIVE_PANEL_START]);
        expect(&starting, "status", "Drying");
        expect(&starting, "remote_start", "OFF");
        expect(&starting, "cycle", "Bedding");

        let armed = feed(&[LIVE_PANEL_START_ARMED]);
        expect(&armed, "status", "Drying");
        expect(&armed, "remote_start", "ON");
    }

    #[test]
    fn remote_start_is_read_from_rec16_bit0() {
        expect(&feed(&[EC_HEAVY_DUTY]), "remote_start", "ON");
        expect(&feed(&[LIVE_54_53]), "remote_start", "ON");
        expect(&feed(&[EB_RECONNECT]), "remote_start", "OFF");
        expect(&feed(&[EC_POWER_OFF]), "remote_start", "OFF");
        expect(&feed(&[EC_COOLING_TRUNCATED]), "remote_start", "OFF");
        expect(&feed(&[LIVE_COOLING]), "remote_start", "ON");
    }

    #[test]
    fn non_status_washer_and_unmarked_frames_are_ignored() {
        for junk in [
            IDENTITY,
            "aa0730d82b91bb",
            "aa09307200c80048bb",
            "AA2120EB00190000010023000000000000000000400000000208000000670091BB",
            "AA2330EB0000010001000100000000000100000000A8000000000000006400000046BB",
            "AA2330E2031B320100010012000005050100000010A80000F001000000CA00000094BB",
        ] {
            assert!(prop(&feed(&[junk]), "power").is_none(), "{junk}");
        }
    }

    #[test]
    fn unknown_phase_code_publishes_the_unknown_fallback() {
        let p = feed(&["AA2330EB001BFF0001000100000000000100000000A8000000000000006400000046BB"]);
        expect(&p, "status", "None");
    }

    #[test]
    fn no_published_value_uses_the_reserved_none_payload() {
        for f in [EB_RECONNECT, EC_POWER_OFF, EC_HEAVY_DUTY, EC_TIME_DRY_SETUP] {
            for (name, value) in feed(&[f]).device(DEVICE_ID).unwrap().properties {
                assert_ne!(value.as_string(), "None", "{name} on {f}");
            }
        }
    }

    #[test]
    fn option_bits_decode_to_what_the_panel_showed() {
        expect(&feed(&[ANTI_BAC]), "anti_bacterial", "ON");
        expect(&feed(&[WRINKLE]), "wrinkle_care", "ON");
        expect(&feed(&[DRUM_LIGHT]), "drum_light", "ON");
        expect(&feed(&[CHILD_LOCK]), "child_lock", "ON");
        expect(&feed(&[DAMP_DRY]), "damp_dry_signal", "ON");
        expect(&feed(&[REMOTE_ON]), "remote_start", "ON");
    }

    #[test]
    fn wrinkle_care_is_read_from_rec15() {
        let p = feed(&[WRINKLE]);
        expect(&p, "wrinkle_care", "ON");
        expect(&p, "anti_bacterial", "ON");
    }

    #[test]
    fn drum_light_is_a_real_toggle() {
        expect(&feed(&[DRUM_LIGHT]), "drum_light", "ON");
        expect(&feed(&[WRINKLE]), "drum_light", "OFF");
    }

    #[test]
    fn signal_volume() {
        expect(&feed(&[SIG_HIGH]), "signal", "High");
        expect(&feed(&[CHILD_LOCK]), "signal", "Low");
        expect(&feed(&[ML_PLUS5]), "signal", "Off");
    }

    #[test]
    fn more_less_time_is_signed() {
        expect(&feed(&[ML_MINUS5]), "more_less_time", "-5");
        expect(&feed(&[ML_PLUS5]), "more_less_time", "5");
    }

    #[test]
    fn time_dry_duration() {
        let p = feed(&[TIMEDRY_60]);
        expect(&p, "time_dry", "60 min");
        expect(&p, "cycle", "Time Dry");
        expect(&p, "remaining_time", "60");
    }

    #[test]
    fn smart_course_codes_decode_to_the_names_the_app_showed() {
        expect(
            &feed(&[DL_BEDDING_CURTAINS]),
            "smart_course",
            "Bedding/Curtains",
        );
        expect(&feed(&[DL_BLANKETS]), "smart_course", "Blankets");
        expect(&feed(&[DL_ECONODRY]), "smart_course", "EconoDry Small Load");
        expect(&feed(&[DL_SOCKS]), "smart_course", "Socks");
        expect(&feed(&[DL_SUPER_DRY]), "smart_course", "Super Dry");
        expect(
            &feed(&[DL_ULTRA_DELICATES]),
            "smart_course",
            "Ultra Delicates",
        );
    }

    #[test]
    fn smart_courses_expose_base_cycles_that_are_not_on_the_dial() {
        expect(&feed(&[DL_BLANKETS]), "cycle", "Jumbo Dry");
        expect(&feed(&[DL_SUPER_DRY]), "cycle", "Super Dry");
        expect(&feed(&[DL_ULTRA_DELICATES]), "cycle", "Ultra Delicates");
    }

    #[test]
    fn each_smart_course_carries_its_declared_settings() {
        let socks = feed(&[DL_SOCKS]);
        expect(&socks, "cycle", "Time Dry");
        expect(&socks, "time_dry", "30 min");
        expect(&socks, "more_less_time", "5");
        expect(&socks, "temp", "High");
        expect(&feed(&[DL_ECONODRY]), "energy_saver", "ON");
        expect(&feed(&[DL_SOCKS]), "energy_saver", "OFF");
    }

    #[test]
    fn downloaded_slot_is_reported_separately_from_the_active_smart_course() {
        expect(&feed(&[DL_SUPER_DRY]), "downloaded_course", "Super Dry");
    }

    #[test]
    fn no_smart_course_engaged_reports_not_selected() {
        expect(&feed(&[EC_HEAVY_DUTY]), "smart_course", "Not selected");
    }

    #[test]
    fn custom_pgm_is_its_own_bit() {
        expect(&feed(&[CUSTOM_PGM]), "custom_pgm", "ON");
        expect(&feed(&[WRINKLE]), "custom_pgm", "OFF");
    }

    #[test]
    fn start_reproduces_the_app_command_byte_for_byte() {
        assert_eq!(
            press(ARMED_NORMAL, "start"),
            ["aa1bf0260300000400000000430003000000000300000000007ebb"]
        );
    }

    #[test]
    fn start_on_a_second_cycle_also_reproduces_the_app_command() {
        assert_eq!(
            press(ARMED_TIME_DRY, "start"),
            ["aa1bf0261200000500000500410012000000000000000000001fbb"]
        );
    }

    #[test]
    fn start_carries_the_option_bits_without_the_drum_light() {
        assert_eq!(
            press(ARMED_WRINKLE, "start"),
            ["aa1bf0260300000400000010430003000000000300000000006ebb"]
        );
    }

    #[test]
    fn start_while_paused_reproduces_the_app_resume() {
        assert_eq!(
            press(LIVE_APP_PAUSE, "start"),
            ["aa1bf0261200000500000510010012000000000000000000004fbb"]
        );
    }

    #[test]
    fn panel_pause_still_resumes_though_remote_start_reads_off() {
        expect(&feed(&[LIVE_APP_PAUSE]), "remote_start", "ON");
        expect(&feed(&[LIVE_PANEL_PAUSE]), "remote_start", "OFF");
        expect(&feed(&[LIVE_PANEL_PAUSE]), "status", "Paused");

        let (_, thinq, dev) = make();
        emit(&thinq, LIVE_PANEL_PAUSE);
        dev.set_property("start", "PRESS");
        let pkt = &thinq.outbox()[0];
        assert_eq!(pkt[2 + 10], 0x01, "resume, not a fresh start");
    }

    #[test]
    fn same_state_not_paused_starts_instead_of_resuming() {
        let (_, thinq, dev) = make();
        emit(&thinq, ARMED_TIME_DRY);
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox()[0][2 + 10], 0x41);
    }

    #[test]
    fn pause_and_power_off_are_the_washer_commands() {
        let (_, thinq, dev) = make();
        emit(&thinq, ARMED_NORMAL);
        dev.set_property("pause", "PRESS");
        dev.set_property("power_off", "PRESS");
        assert_eq!(sent(&thinq), ["aa09f02404010099bb", "aa09f0240101009cbb"]);
    }

    #[test]
    fn start_is_not_gated_in_software() {
        assert_eq!(press(EB_RECONNECT, "start").len(), 1);
    }

    #[test]
    fn power_off_is_not_gated_on_remote_start() {
        assert_eq!(press(EB_RECONNECT, "power_off").len(), 1);
    }

    // Upstream master's cases that still hold, with the PR's state and setting labels.

    #[test]
    fn real_post_reconnect_eb_publishes_initial_1_min() {
        assert_eq!(
            values(
                &feed(&[EB_RECONNECT]),
                &["status", "remaining_time", "power"]
            ),
            ["Initial", "1", "ON"]
        );
    }

    #[test]
    fn real_heavy_duty_ec_publishes_its_current_record() {
        assert_eq!(
            values(
                &feed(&[EC_HEAVY_DUTY]),
                &["status", "remaining_time", "power"]
            ),
            ["Drying", "53", "ON"]
        );
    }

    #[test]
    fn real_time_dry_setup_ec_publishes_initial_20_min() {
        assert_eq!(
            values(
                &feed(&[EC_TIME_DRY_SETUP]),
                &["status", "remaining_time", "power"]
            ),
            ["Initial", "20", "ON"]
        );
    }

    #[test]
    fn real_resume_ec_publishes_its_current_record() {
        assert_eq!(
            values(&feed(&[EC_RESUMED]), &["status", "remaining_time", "power"]),
            ["Drying", "18", "ON"]
        );
    }

    #[test]
    fn paused_record_publishes_paused_18_min_power_on() {
        assert_eq!(
            values(&feed(&[EB_PAUSED]), &["status", "remaining_time", "power"]),
            ["Paused", "18", "ON"]
        );
    }

    #[test]
    fn time_dry_reports_no_dry_level_and_the_selected_heat() {
        assert_eq!(
            values(&feed(&[EC_TIME_DRY_SETUP]), &["cycle", "dry_level", "temp"]),
            ["Time Dry", "Not selected", "Ultra Low"]
        );
        assert_eq!(
            values(&feed(&[EC_RESUMED]), &["cycle", "dry_level", "temp"]),
            ["Time Dry", "Not selected", "Low"]
        );
    }

    #[test]
    fn acks_identity_and_heartbeat_but_not_status_records() {
        let (_, thinq, _) = make();
        emit(&thinq, IDENTITY);
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
        let inners: Vec<Vec<u8>> = LIVE_EC.iter().map(|f| inner(f)).collect();
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
                    "remote_start"
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
    fn end_of_cycle_publishes_end_then_off_with_0_min() {
        let (ha, thinq, _) = make();
        emit(&thinq, LIVE_EC[2]);
        emit(&thinq, LIVE_CYCLE_END[0]);
        assert_eq!(
            values(&ha, &["status", "remaining_time", "remote_start"]),
            ["End", "0", "OFF"]
        );
        emit(&thinq, LIVE_CYCLE_END[1]);
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
            ["Off", "0", "OFF", "Not selected", "Off", "Not selected"]
        );
    }

    #[test]
    fn reconnect_snapshot_while_off_publishes_0_min() {
        assert_eq!(
            values(
                &feed(&[LIVE_OFF_RECONNECT]),
                &["status", "remaining_time", "cycle", "dry_level"]
            ),
            ["Off", "0", "Not selected", "Not selected"]
        );
    }

    #[test]
    fn no_live_value_is_none() {
        let (ha, thinq, _) = make();
        let frames = [LIVE_EB]
            .into_iter()
            .chain(LIVE_EC)
            .chain(LIVE_CYCLE_END)
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
        let ha = feed(&["AA2320EB000000000000000000000000000000000000000000000000000000000000BB"]);
        assert!(prop(&ha, "power").is_none());
    }

    #[test]
    fn frames_that_are_too_short_are_ignored() {
        assert!(prop(&feed(&["AA0430EB00BB"]), "power").is_none());
    }

    // Rust-only checks.

    #[test]
    fn enum_options_list_each_label_once_in_table_order() {
        let comps = components_of(&feed(&[]));
        assert_eq!(
            comps["pre_state"]["options"],
            json!([
                "Off",
                "Initial",
                "Drying",
                "Paused",
                "End",
                "Error",
                "Smart Diagnosis",
                "Cooling",
                "Wrinkle Care"
            ])
        );
        for (key, first, len) in [
            ("cycle", "Not selected", 18),
            ("smart_course", "Not selected", 21),
            ("downloaded_course", "Not selected", 21),
            ("time_dry", "Not selected", 6),
            ("signal", "Off", 3),
            ("temp", "Off", 6),
            ("dry_level", "Not selected", 6),
        ] {
            let options = comps[key]["options"].as_array().unwrap();
            assert_eq!(options[0], first, "{key}");
            assert_eq!(options.len(), len, "{key}");
        }
    }

    #[test]
    fn drum_running_is_withdrawn_with_a_tombstone() {
        let ha = feed(&[EC_HEAVY_DUTY]);
        assert_eq!(
            components_of(&ha)["drum_running"],
            json!({"platform": "binary_sensor"})
        );
        assert!(prop(&ha, "drum_running").is_none());
    }

    #[test]
    fn commands_need_a_status_record_first() {
        let (_, thinq, dev) = make();
        for prop in ["start", "pause", "power_off"] {
            dev.set_property(prop, "PRESS");
        }
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("RV13U6AM8W_D_US_WIFI").is_some());
    }
}
