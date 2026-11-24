//! Korean WashTower washer (`FAKPK21021`, deviceType 221, tag `0x33`) and dryer
//! (`BDH_D39301_KR`, deviceType 222, tag `0x34`).
//!
//! Speaks the `AA FF <tag> 0A ... <crc16> BB` extended-frame status protocol (`0x00EB` single
//! record, `0x00EC` previous+current records) and short AABB `F0 <frameId>` control frames.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, wrap_aabb, AabbDeviceCore};
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::crc16::crc16;
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const HEADER_LENGTH: usize = 15;
const TRAILER_LENGTH: usize = 3;
const POWER_ON_PENDING: Duration = Duration::from_secs(120);

const WASHER_COURSE_LABELS: &[(u16, &str)] = &[
    (0, "None"),
    (1, "Wash/Dry Refresh"),
    (2, "Add prewash"),
    (3, "Air Cleaning"),
    (4, "Allergy Spa Steam"),
    (5, "Allergy Care"),
    (6, "Safe Cold Wash"),
    (7, "Baby Steam Care"),
    (8, "Baby clothes"),
    (9, "Bedding Care"),
    (10, "Boil"),
    (11, "White Wash"),
    (12, "Large Wash"),
    (13, "Large load"),
    (14, "Everyday wear"),
    (15, "Cold Care"),
    (16, "Cold Clean"),
    (17, "Cold Wash"),
    (18, "Color Care"),
    (19, "Eco Standard"),
    (20, "Closet Dry"),
    (21, "Dark Wash"),
    (22, "Delicate Dry"),
    (23, "Ready to wear"),
    (24, "Double Rinse"),
    (25, "Drain/Spin"),
    (26, "Dry only"),
    (27, "Bedding"),
    (28, "Bedding wash"),
    (29, "Easy Care"),
    (30, "Favorite"),
    (31, "Soft Care"),
    (32, "Half load"),
    (33, "Handwash"),
    (34, "Handwash/Wool"),
    (35, "Heavy soil"),
    (36, "Focus 60min"),
    (37, "Iron Dry"),
    (38, "Jeans"),
    (39, "Kids clothes"),
    (40, "Large Wash"),
    (41, "Wool/Delicate"),
    (42, "Low-temp Dry"),
    (43, "Mixed Wash"),
    (44, "Sanitary 40°"),
    (45, "Sterilize 60°"),
    (46, "Standard"),
    (47, "Quiet"),
    (48, "Wrinkle Prevent"),
    (49, "Intensive Wash"),
    (50, "Prewash+Standard"),
    (51, "Quick Deodorize"),
    (52, "Quick 30min"),
    (53, "Quiet"),
    (54, "Refresh"),
    (55, "Rinse/Spin"),
    (56, "Rinse only"),
    (57, "Durable garments"),
    (58, "Safe"),
    (59, "Safe Standard"),
    (60, "Sterilize"),
    (61, "Oxygen Sterilize"),
    (62, "Eco Wash"),
    (63, "School uniform"),
    (64, "Shoes"),
    (65, "Quiet"),
    (66, "Quiet Wash"),
    (67, "Thorough Rinse"),
    (68, "Small Wash"),
    (69, "Smart power-save"),
    (70, "Soak"),
    (71, "Spa Refresh"),
    (72, "Quick Dry"),
    (73, "Quick Tub Clean"),
    (74, "Small Quick"),
    (75, "Quick 14min"),
    (76, "Eco Boil"),
    (77, "Quick Wash/Dry"),
    (78, "Spin only"),
    (79, "Functional garments"),
    (80, "Stain Care"),
    (81, "Steam Standard"),
    (82, "Intensive Dry"),
    (83, "Timed Dry"),
    (84, "Towels"),
    (85, "Tub Sterilize"),
    (86, "Tub Dry"),
    (87, "Turbo Wash"),
    (88, "Wash/Dry"),
    (89, "Wash only"),
    (90, "White"),
    (91, "Air Spin 120min"),
    (92, "Air Spin 60min"),
    (93, "Air Spin 90min"),
    (94, "Wool"),
    (95, "Single shirt"),
    (96, "Cotton underwear"),
    (97, "Light Boil"),
    (98, "Light soil"),
    (99, "Activewear"),
    (100, "Underwear"),
    (101, "Static prevent"),
    (102, "Fabric protect"),
    (103, "Deodorize"),
    (104, "Eco Wash"),
    (105, "Kids clothes"),
    (106, "Rainy-season Wash"),
    (107, "School uniform"),
    (108, "Shirt"),
    (109, "Single-item Wash"),
    (110, "Thorough Rinse"),
    (111, "Safe Rinse"),
    (112, "Spin only"),
    (113, "Sweat stain removal"),
    (114, "AI Wash"),
    (115, "Down jacket"),
    (116, "Eco Standard Large"),
    (117, "Eco Standard Small"),
    (118, "Standard 20°"),
    (119, "Pet Care"),
    (120, "Outdoor"),
    (121, "Water repellent"),
    (122, "Turbo 39min"),
    (123, "Turbo 59min"),
    (124, "Quick Wash/Dry"),
    (125, "Boil Wash"),
    (126, "Disinfectant Safe Wash"),
    (127, "Standard Plus"),
    (128, "Detergent tray clean"),
    (129, "Duvet cover"),
    (130, "Swimwear"),
    (131, "Dress"),
    (132, "Large"),
    (133, "Extra Large"),
    (134, "Quick Tub Rinse"),
    (135, "Quick Steam Sterilize"),
    (136, "Microplastic Care"),
    (137, "Timed Wash"),
    (138, "Timed Dry"),
    (139, "Steam Sterilize Dry"),
    (140, "Steam Refresh Dry"),
    (141, "Rainy-season Dry"),
    (142, "Padding Dry"),
    (143, "Air Dry"),
    (144, "Bedding Care Dry"),
    (145, "Soak"),
    (146, "Light soil"),
    (147, "Intensive Wash"),
    (148, "Fresh Care"),
    (149, "Towel Boil Wash"),
    (150, "New clothes"),
    (151, "Intensive Spin"),
    (152, "Custom Dry"),
    (153, "Denim"),
    (154, "Cotton underwear"),
    (155, "Soak Boil"),
    (156, "Bedding rinse/spin"),
    (157, "Heavy-soil Soak"),
    (158, "Heavy-soil Prewash"),
    (159, "Socks"),
    (160, "Double Rinse/Spin"),
    (161, "Cold Intensive Wash"),
    (162, "Baby Prewash"),
    (163, "Yoga wear"),
    (164, "Golf wear"),
    (165, "Summer bedding"),
    (166, "AI Wash/Dry"),
    (167, "Delicate Dry"),
    (168, "Wrinkle Prevent Dry"),
    (169, "Cuffs & collar"),
    (255, "Downloaded course"),
];

const DRYER_COURSE_LABELS: &[(u16, &str)] = &[
    (0, "None"),
    (1, "Steam Refresh"),
    (2, "Towels"),
    (3, "Jeans"),
    (4, "Bedding"),
    (5, "Shirt"),
    (6, "Mixed garments"),
    (7, "Standard"),
    (8, "Functional garments"),
    (9, "Small Quick"),
    (10, "Delicate Dry"),
    (11, "Wool/Delicate"),
    (12, "Shelf Dry"),
    (13, "Air blow"),
    (14, "Warm air"),
    (15, "Bedding shake"),
    (16, "Steam Sterilize"),
    (17, "Intensive Dry"),
    (18, "Condenser Care"),
    (19, "Tub Sterilize"),
    (20, "Padding Refresh"),
    (21, "Timed Dry"),
    (22, "Outdoor Refresh"),
    (23, "Baby clothes"),
    (24, "Small Dry"),
    (25, "Standard Plus"),
    (26, "Wrinkle Prevent"),
    (27, "Pet Care"),
    (28, "Single shirt"),
    (29, "Heavy soil"),
    (30, "Ultra Delicate"),
    (31, "Kids clothes"),
    (32, "Low-temp Dry"),
    (33, "Large Dry"),
    (34, "Quick Dry"),
    (35, "Air Dry"),
    (36, "Spot clean"),
    (37, "Steam Refresh"),
    (38, "Steam Sterilize"),
    (39, "Refresh"),
    (40, "Garment Refresh"),
    (41, "Mist Refresh"),
    (42, "Intensive Dry"),
    (43, "Low-temp Dry Plus"),
    (44, "AI Dry"),
    (45, "Quiet"),
    (46, "Shrink ease"),
    (47, "Wrinkle Ease"),
    (48, "Thin bedding"),
    (49, "Sportswear"),
    (50, "Rainy-season Dry"),
    (51, "Iron Dry"),
    (52, "Duvet cover"),
    (53, "Blanket warm"),
    (54, "Night Dry"),
    (55, "Half-load Dry"),
    (56, "Full-load Dry"),
    (57, "Room Dehumidify"),
    (58, "Turbo Dry"),
    (114, "Wrinkle Ease Dry"),
    (255, "Downloaded course"),
];

const ON_OFF_VALUES: &[(u16, &str)] = &[(0, "Off"), (1, "On")];

const WASHER_SOIL_VALUES: &[(u16, &str)] = &[
    (0, "Off"),
    (1, "Light soil"),
    (3, "Standard"),
    (5, "Intensive"),
    (6, "Prewash"),
    (7, "Soak"),
];

const WASHER_TEMP_VALUES: &[(u16, &str)] = &[
    (0, "Off"),
    (1, "20 ℃"),
    (2, "30 ℃"),
    (3, "40 ℃"),
    (4, "50 ℃"),
    (5, "60 ℃"),
    (6, "95 ℃"),
    (8, "Cold water"),
    (20, "27 ℃"),
    (21, "35 ℃"),
    (22, "38 ℃"),
    (23, "90 ℃"),
];

const WASHER_RINSE_VALUES: &[(u16, &str)] = &[
    (0, "Off"),
    (1, "1x"),
    (2, "2x"),
    (3, "3x"),
    (4, "4x"),
    (5, "5x"),
    (6, "Rinse 6x"),
    (7, "Rinse 7x"),
    (8, "Rinse 8x"),
];

const WASHER_SPIN_VALUES: &[(u16, &str)] = &[
    (0, "Off"),
    (1, "Delicate"),
    (2, "Low"),
    (3, "700 rpm"),
    (4, "Mid"),
    (5, "900 rpm"),
    (6, "High"),
    (7, "1100 rpm"),
    (8, "Max"),
    (9, "1400 rpm"),
    (10, "1600 rpm"),
    (19, "On"),
];

const WASHER_DRY_LEVEL_VALUES: &[(u16, &str)] = &[
    (0, "Off"),
    (11, "Timed Dry 30min"),
    (12, "Timed Dry 60min"),
    (13, "Timed Dry 90min"),
    (14, "Timed Dry 120min"),
    (15, "Timed Dry 150min"),
    (19, "20min"),
    (20, "40min"),
    (21, "180min"),
    (22, "240min"),
];

const WASHER_ERROR_VALUES: &[(u16, &str)] = &[
    (0, "No error"),
    (2, "Not filling"),
    (3, "Not draining"),
    (4, "Not spinning"),
    (5, "Water level high"),
    (7, "Water level not detected"),
    (8, "Temp not detected"),
    (9, "Motor rotation fault"),
    (13, "Freeze detected"),
    (20, "Door not closed"),
    (21, "Door not locked"),
    (23, "Vibration sensor fault"),
    (33, "Off"),
    (43, "Check detergent tank"),
    (44, "Detergent not dosed"),
    (45, "Check softener tank"),
    (46, "Softener not dosed"),
    (47, "Detergent tank fault"),
    (48, "Turbidity not detected"),
];

const WASHER_STATE_VALUES: &[(u16, &str)] = &[
    (0, "Power off"),
    (1, "On"),
    (2, "Pause"),
    (3, "Detecting weight"),
    (5, "Filling"),
    (6, "Detecting weight"),
    (8, "Soaking"),
    (9, "Prewashing"),
    (11, "Washing"),
    (12, "Rinsing"),
    (13, "Awaiting rinse"),
    (14, "Spinning"),
    (15, "Drying"),
    (16, "Wash complete"),
    (21, "Wrinkle preventing"),
    (23, "Error occurred"),
    (27, "Anti-freeze standby"),
    (28, "Pause"),
    (29, "Running"),
    (35, "Pause"),
    (36, "Setting"),
    (37, "Recognizing garment"),
    (38, "Auto detergent dosing"),
    (39, "Auto softener dosing"),
    (40, "Detecting soil level"),
    (41, "Cleaning"),
    (42, "Wash complete"),
    (43, "Steaming"),
    (47, "Laundry care running"),
    (48, "Cleaning"),
    (49, "Awaiting completion"),
];

const DRYER_DRY_LEVEL_VALUES: &[(u16, &str)] = &[
    (0, "Off"),
    (1, "Delicate"),
    (2, "Low"),
    (3, "Standard"),
    (4, "Standard+"),
    (5, "Intensive"),
];

const DRYER_ECO_HYBRID_VALUES: &[(u16, &str)] =
    &[(0, "Off"), (1, "Energy"), (2, "Auto"), (3, "Speed")];

const DRYER_STATE_VALUES: &[(u16, &str)] = &[
    (0, "Power off"),
    (1, "On"),
    (2, "Drying"),
    (3, "Pause"),
    (4, "Dry complete"),
    (5, "Error occurred"),
    (6, "Smart diagnosing"),
    (7, "Drying"),
    (8, "Blowing"),
    (9, "Wrinkle preventing"),
    (11, "Detecting weight"),
    (12, "Reserved"),
    (14, "Detecting weight"),
    (15, "Steaming"),
    (16, "Recognizing garment"),
    (17, "Condenser auto-clean running"),
    (18, "Bedding shake running"),
    (19, "Refreshing"),
    (20, "Sterilizing"),
    (21, "Condenser Care running"),
    (22, "Dry complete"),
    (23, "Preparing to dry"),
    (24, "Laundry care running"),
    (25, "Dehumidifying"),
    (26, "Dry complete"),
    (27, "Awaiting completion"),
];

const DRYER_ERROR_VALUES: &[(u16, &str)] = &[
    (0, "No error"),
    (1, "Temp sensor fault 1"),
    (2, "Temp sensor fault 2"),
    (4, "Temp sensor fault 4"),
    (5, "Temp sensor fault 5"),
    (6, "Temp sensor fault 6"),
    (10, "Steam not operating"),
    (11, "Steam water refill"),
    (12, "Steam not operating"),
    (14, "Not draining"),
    (15, "Drain check notice"),
    (16, "Door not closed"),
    (18, "Filter installed"),
    (20, "Drum temp fault"),
    (21, "Compressor fault 1"),
    (22, "Compressor fault 2"),
    (31, "Drum motor fault"),
    (38, "Door not closed"),
    (40, "Fan motor fault"),
    (43, "Door not locked"),
    (49, "IR sensor fault"),
    (50, "Internal fan fault"),
    (52, "Door closed"),
    (53, "Dehumidify kit not detected"),
    (54, "Low-temp detected"),
];

#[derive(Clone, Copy)]
struct FieldSpec {
    key: &'static str,
    entity_key: &'static str,
    offset: usize,
    length: usize,
    bit: Option<u8>,
    label: &'static str,
    values: Option<&'static [(u16, &'static str)]>,
    public: bool,
}

#[derive(Clone, Copy)]
struct OptionRule {
    key: &'static str,
    default_value: u16,
    selectable: Option<&'static [u16]>,
}

#[derive(Clone, Copy)]
struct CourseRule {
    course: u16,
    options: &'static [OptionRule],
}

#[derive(Clone, Copy)]
pub(crate) struct WashTowerModel {
    tag: u8,
    record_length: usize,
    live_record_length: usize,
    device_name: &'static str,
    default_course: u16,
    fields: &'static [FieldSpec],
    setting_keys: &'static [&'static str],
    course_rules: &'static [CourseRule],
}

const WASHER_FIELDS: &[FieldSpec] = &[
    FieldSpec { key: "soilWash", entity_key: "soil_wash", offset: 1, length: 1, bit: None, label: "Wash strength", values: Some(WASHER_SOIL_VALUES), public: false },
    FieldSpec { key: "temp", entity_key: "temp", offset: 2, length: 1, bit: None, label: "Temperature", values: Some(WASHER_TEMP_VALUES), public: false },
    FieldSpec { key: "rinse", entity_key: "rinse", offset: 3, length: 1, bit: None, label: "Rinse", values: Some(WASHER_RINSE_VALUES), public: false },
    FieldSpec { key: "spin", entity_key: "spin", offset: 4, length: 1, bit: None, label: "Spin strength", values: Some(WASHER_SPIN_VALUES), public: false },
    FieldSpec { key: "course", entity_key: "course", offset: 5, length: 1, bit: None, label: "Course", values: Some(WASHER_COURSE_LABELS), public: true },
    FieldSpec { key: "dryLevel", entity_key: "dry_level", offset: 6, length: 1, bit: None, label: "Dryness", values: Some(WASHER_DRY_LEVEL_VALUES), public: false },
    FieldSpec { key: "reserveTimeMinute", entity_key: "reserve_time", offset: 11, length: 2, bit: None, label: "Reserved time", values: None, public: true },
    FieldSpec { key: "remainTimeMinute", entity_key: "remaining_time", offset: 13, length: 2, bit: None, label: "Time remaining", values: None, public: true },
    FieldSpec { key: "initialTimeMinute", entity_key: "initial_time", offset: 15, length: 2, bit: None, label: "Total time", values: None, public: true },
    FieldSpec { key: "error", entity_key: "error", offset: 19, length: 1, bit: None, label: "Error", values: Some(WASHER_ERROR_VALUES), public: true },
    FieldSpec { key: "baseDownloadCourseData", entity_key: "base_download_course_data", offset: 20, length: 1, bit: None, label: "Downloaded course basis", values: None, public: false },
    FieldSpec { key: "state", entity_key: "state", offset: 21, length: 1, bit: None, label: "Current state", values: Some(WASHER_STATE_VALUES), public: true },
    FieldSpec { key: "downloadCourse", entity_key: "download_course", offset: 23, length: 1, bit: None, label: "Downloaded course", values: None, public: false },
    FieldSpec { key: "turboWash", entity_key: "turbo_wash", offset: 34, length: 1, bit: Some(5), label: "Turbo Wash", values: Some(ON_OFF_VALUES), public: false },
    FieldSpec { key: "steam", entity_key: "steam", offset: 35, length: 1, bit: Some(4), label: "Steam", values: Some(ON_OFF_VALUES), public: false },
    FieldSpec { key: "freshCare", entity_key: "fresh_care", offset: 36, length: 1, bit: Some(6), label: "Status info 052", values: Some(ON_OFF_VALUES), public: false },
    FieldSpec { key: "doorClose", entity_key: "door", offset: 37, length: 1, bit: Some(0), label: "Door", values: Some(ON_OFF_VALUES), public: true },
    FieldSpec { key: "remoteStart", entity_key: "remote_start", offset: 37, length: 1, bit: Some(4), label: "Remote start", values: Some(ON_OFF_VALUES), public: true },
    FieldSpec { key: "childLock", entity_key: "child_lock", offset: 37, length: 1, bit: Some(5), label: "Button lock", values: Some(ON_OFF_VALUES), public: true },
    FieldSpec { key: "doorLock", entity_key: "door_lock", offset: 38, length: 1, bit: Some(0), label: "Door lock", values: Some(ON_OFF_VALUES), public: true },
    FieldSpec { key: "remoteMaintain", entity_key: "remote_maintain", offset: 40, length: 1, bit: Some(2), label: "Remote control lock", values: Some(ON_OFF_VALUES), public: false },
];

const DRYER_FIELDS: &[FieldSpec] = &[
    FieldSpec { key: "dryLevel", entity_key: "dry_level", offset: 1, length: 1, bit: None, label: "Dryness", values: Some(DRYER_DRY_LEVEL_VALUES), public: false },
    FieldSpec { key: "ecoHybrid", entity_key: "eco_hybrid", offset: 2, length: 1, bit: None, label: "Eco mode", values: Some(DRYER_ECO_HYBRID_VALUES), public: false },
    FieldSpec { key: "course", entity_key: "course", offset: 5, length: 1, bit: None, label: "Course", values: Some(DRYER_COURSE_LABELS), public: true },
    FieldSpec { key: "reserveTimeMinute", entity_key: "reserve_time", offset: 7, length: 2, bit: None, label: "Reserved time", values: None, public: true },
    FieldSpec { key: "remainTimeMinute", entity_key: "remaining_time", offset: 9, length: 2, bit: None, label: "Time remaining", values: None, public: true },
    FieldSpec { key: "initialTimeMinute", entity_key: "initial_time", offset: 11, length: 2, bit: None, label: "Total time", values: None, public: true },
    FieldSpec { key: "state", entity_key: "state", offset: 13, length: 1, bit: None, label: "Current state", values: Some(DRYER_STATE_VALUES), public: true },
    FieldSpec { key: "error", entity_key: "error", offset: 15, length: 1, bit: None, label: "Error", values: Some(DRYER_ERROR_VALUES), public: true },
    FieldSpec { key: "baseDownloadCourseData", entity_key: "base_download_course_data", offset: 20, length: 1, bit: None, label: "Downloaded course basis", values: None, public: false },
    FieldSpec { key: "downloadCourse", entity_key: "download_course", offset: 23, length: 1, bit: None, label: "Downloaded course", values: None, public: false },
    FieldSpec { key: "wrinkleCare", entity_key: "wrinkle_care", offset: 24, length: 1, bit: Some(3), label: "Wrinkle Prevent", values: Some(ON_OFF_VALUES), public: false },
    FieldSpec { key: "steam", entity_key: "steam", offset: 25, length: 1, bit: Some(2), label: "Steam", values: Some(ON_OFF_VALUES), public: false },
    FieldSpec { key: "remoteStart", entity_key: "remote_start", offset: 26, length: 1, bit: Some(6), label: "Remote start", values: Some(ON_OFF_VALUES), public: true },
    FieldSpec { key: "childLock", entity_key: "child_lock", offset: 26, length: 1, bit: Some(4), label: "Button lock", values: Some(ON_OFF_VALUES), public: true },
    FieldSpec { key: "doorLock", entity_key: "door_lock", offset: 26, length: 1, bit: Some(0), label: "Door lock", values: Some(ON_OFF_VALUES), public: true },
    FieldSpec { key: "remoteMaintain", entity_key: "remote_maintain", offset: 27, length: 1, bit: Some(1), label: "Remote control lock", values: Some(ON_OFF_VALUES), public: false },
    FieldSpec { key: "moreLessTime", entity_key: "more_less_time", offset: 39, length: 2, bit: None, label: "Dry time adjust", values: None, public: false },
];

const WASHER_SETTING_KEYS: &[&str] = &[
    "course", "soilWash", "spin", "temp", "rinse", "freshCare", "steam", "turboWash",
];

const DRYER_SETTING_KEYS: &[&str] = &["course", "ecoHybrid", "wrinkleCare", "steam", "dryLevel"];

const WASHER_COURSE_RULES: &[CourseRule] = &[
    CourseRule {
        course: 27,
        options: &[
            OptionRule { key: "soilWash", default_value: 3, selectable: Some(&[0, 3]) },
            OptionRule { key: "rinse", default_value: 3, selectable: Some(&[0, 1, 2, 3, 4, 5]) },
            OptionRule { key: "spin", default_value: 4, selectable: Some(&[0, 1, 2, 4]) },
            OptionRule { key: "temp", default_value: 8, selectable: Some(&[8, 2, 3]) },
            OptionRule { key: "turboWash", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "freshCare", default_value: 0, selectable: Some(&[0, 1]) },
        ],
    },
    CourseRule {
        course: 46,
        options: &[
            OptionRule { key: "soilWash", default_value: 3, selectable: Some(&[0, 3, 5, 6]) },
            OptionRule { key: "rinse", default_value: 2, selectable: Some(&[0, 1, 2, 3, 4, 5]) },
            OptionRule { key: "spin", default_value: 6, selectable: Some(&[0, 1, 2, 4, 6, 8]) },
            OptionRule { key: "temp", default_value: 3, selectable: Some(&[8, 2, 3, 5]) },
            OptionRule { key: "turboWash", default_value: 1, selectable: Some(&[0, 1]) },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "freshCare", default_value: 0, selectable: Some(&[0, 1]) },
        ],
    },
    CourseRule {
        course: 74,
        options: &[
            OptionRule { key: "soilWash", default_value: 3, selectable: Some(&[0, 3]) },
            OptionRule { key: "rinse", default_value: 1, selectable: Some(&[0, 1, 2, 3, 4, 5]) },
            OptionRule { key: "spin", default_value: 4, selectable: Some(&[0, 1, 2, 4, 6, 8]) },
            OptionRule { key: "temp", default_value: 3, selectable: Some(&[8, 2, 3]) },
            OptionRule { key: "turboWash", default_value: 1, selectable: Some(&[0, 1]) },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "freshCare", default_value: 0, selectable: Some(&[0, 1]) },
        ],
    },
    CourseRule {
        course: 76,
        options: &[
            OptionRule { key: "soilWash", default_value: 3, selectable: Some(&[0, 3]) },
            OptionRule { key: "rinse", default_value: 3, selectable: Some(&[0, 1, 2, 3, 4, 5]) },
            OptionRule { key: "spin", default_value: 4, selectable: Some(&[0, 1, 2, 4, 6, 8]) },
            OptionRule { key: "temp", default_value: 6, selectable: None },
            OptionRule { key: "turboWash", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "freshCare", default_value: 0, selectable: Some(&[0, 1]) },
        ],
    },
    CourseRule {
        course: 85,
        options: &[
            OptionRule { key: "soilWash", default_value: 3, selectable: None },
            OptionRule { key: "rinse", default_value: 2, selectable: None },
            OptionRule { key: "spin", default_value: 4, selectable: None },
            OptionRule { key: "temp", default_value: 5, selectable: None },
            OptionRule { key: "turboWash", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "freshCare", default_value: 0, selectable: Some(&[0, 1]) },
        ],
    },
    CourseRule {
        course: 114,
        options: &[
            OptionRule { key: "soilWash", default_value: 3, selectable: None },
            OptionRule { key: "rinse", default_value: 2, selectable: Some(&[1, 2, 3, 4, 5]) },
            OptionRule { key: "spin", default_value: 6, selectable: Some(&[1, 2, 4, 6, 8]) },
            OptionRule { key: "temp", default_value: 3, selectable: Some(&[8, 2, 3, 5]) },
            OptionRule { key: "turboWash", default_value: 1, selectable: Some(&[0, 1]) },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "freshCare", default_value: 0, selectable: Some(&[0, 1]) },
        ],
    },
];

const DRYER_COURSE_RULES: &[CourseRule] = &[
    CourseRule {
        course: 1,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 3, selectable: None },
            OptionRule { key: "dryLevel", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 1, selectable: None },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "moreLessTime", default_value: 0, selectable: None },
        ],
    },
    CourseRule {
        course: 4,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 2, selectable: None },
            OptionRule { key: "dryLevel", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "moreLessTime", default_value: 0, selectable: None },
        ],
    },
    CourseRule {
        course: 7,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 2, selectable: Some(&[1, 2, 3]) },
            OptionRule { key: "dryLevel", default_value: 3, selectable: Some(&[1, 2, 3, 4, 5]) },
            OptionRule { key: "steam", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "moreLessTime", default_value: 0, selectable: None },
        ],
    },
    CourseRule {
        course: 9,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 3, selectable: None },
            OptionRule { key: "dryLevel", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "moreLessTime", default_value: 0, selectable: None },
        ],
    },
    CourseRule {
        course: 15,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 2, selectable: None },
            OptionRule { key: "dryLevel", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "moreLessTime", default_value: 0, selectable: None },
        ],
    },
    CourseRule {
        course: 16,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 3, selectable: None },
            OptionRule { key: "dryLevel", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 1, selectable: Some(&[0, 1]) },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "moreLessTime", default_value: 0, selectable: None },
        ],
    },
    CourseRule {
        course: 18,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 3, selectable: None },
            OptionRule { key: "dryLevel", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: None },
            OptionRule { key: "moreLessTime", default_value: 0, selectable: None },
        ],
    },
    CourseRule {
        course: 19,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 3, selectable: None },
            OptionRule { key: "dryLevel", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 1, selectable: None },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: None },
            OptionRule { key: "moreLessTime", default_value: 0, selectable: None },
        ],
    },
    CourseRule {
        course: 21,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 2, selectable: None },
            OptionRule { key: "dryLevel", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "moreLessTime", default_value: 30, selectable: None },
        ],
    },
    CourseRule {
        course: 44,
        options: &[
            OptionRule { key: "ecoHybrid", default_value: 2, selectable: None },
            OptionRule { key: "dryLevel", default_value: 0, selectable: None },
            OptionRule { key: "steam", default_value: 0, selectable: None },
            OptionRule { key: "wrinkleCare", default_value: 0, selectable: Some(&[0, 1]) },
            OptionRule { key: "moreLessTime", default_value: 0, selectable: None },
        ],
    },
];

pub(crate) const WASHER_MODEL: WashTowerModel = WashTowerModel {
    tag: 0x33,
    record_length: 72,
    live_record_length: 67,
    device_name: "LG WashTower Washer",
    default_course: 46,
    fields: WASHER_FIELDS,
    setting_keys: WASHER_SETTING_KEYS,
    course_rules: WASHER_COURSE_RULES,
};

pub(crate) const DRYER_MODEL: WashTowerModel = WashTowerModel {
    tag: 0x34,
    record_length: 51,
    live_record_length: 50,
    device_name: "LG WashTower Dryer",
    default_course: 7,
    fields: DRYER_FIELDS,
    setting_keys: DRYER_SETTING_KEYS,
    course_rules: DRYER_COURSE_RULES,
};

const CTRL_WAKE: (u8, &[u8]) = (0x2a, &[0x01, 0x00]);
const CTRL_POWER_ON: (u8, &[u8]) = (0xe5, &[0x00, 0x02, 0x01, 0xff, 0x01, 0x02, 0x01]);
const CTRL_STATUS_QUERY: (u8, &[u8]) = (
    0xed,
    &[0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x04, 0x11, 0x12, 0x00, 0x00],
);
const CTRL_OFF: (u8, &[u8]) = (0x24, &[0x01, 0x01, 0x00]);
const CTRL_STOP: (u8, &[u8]) = (0x24, &[0x04, 0x01, 0x00]);
const CTRL_REMOTE_MAINTAIN_ON: (u8, &[u8]) = (0x24, &[0x10, 0x01, 0x01]);
const CTRL_REMOTE_MAINTAIN_OFF: (u8, &[u8]) = (0x24, &[0x10, 0x01, 0x00]);

fn lookup_label(values: &[(u16, &'static str)], raw: u16) -> &'static str {
    values
        .iter()
        .find(|&&(k, _)| k == raw)
        .map(|&(_, v)| v)
        .unwrap_or("unknown")
}

fn unique_labels<'a>(iter: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for s in iter {
        if seen.insert(s) {
            out.push(s);
        }
    }
    out
}

fn find_rule(model: &WashTowerModel, course: u16) -> Option<&'static CourseRule> {
    model.course_rules.iter().find(|r| r.course == course)
}

fn find_field(model: &WashTowerModel, key: &str) -> Option<&'static FieldSpec> {
    model.fields.iter().find(|f| f.key == key)
}

fn select_current_record<'a>(frame: &'a [u8], model: &WashTowerModel) -> Option<&'a [u8]> {
    if frame.len() < HEADER_LENGTH + model.live_record_length + TRAILER_LENGTH {
        return None;
    }
    if frame[0] != 0xaa
        || frame[1] != 0xff
        || frame[2] != model.tag
        || frame[frame.len() - 1] != 0xbb
    {
        return None;
    }
    if usize::from(u16::from_le_bytes([frame[5], frame[6]])) != frame.len() {
        return None;
    }
    let fid = u16::from_be_bytes([frame[11], frame[12]]);
    if fid != 0x00eb && fid != 0x00ec {
        return None;
    }
    let n = usize::from(u16::from_be_bytes([frame[13], frame[14]]));
    if n != frame.len() - HEADER_LENGTH - TRAILER_LENGTH {
        return None;
    }
    let expected_crc = u16::from_be_bytes([
        frame[frame.len() - TRAILER_LENGTH],
        frame[frame.len() - TRAILER_LENGTH + 1],
    ]);
    if crc16(&frame[..frame.len() - TRAILER_LENGTH]) != expected_crc {
        return None;
    }
    if fid == 0x00eb {
        if n != model.live_record_length && n != model.record_length {
            return None;
        }
        return Some(&frame[HEADER_LENGTH..HEADER_LENGTH + n]);
    }
    let rl = if n == model.live_record_length * 2 {
        model.live_record_length
    } else if n == model.record_length * 2 {
        model.record_length
    } else {
        return None;
    };
    Some(&frame[HEADER_LENGTH + rl..HEADER_LENGTH + rl * 2])
}

fn is_power_on_success(frame: &[u8], model: &WashTowerModel) -> bool {
    let len = frame.len();
    if len != 13 && len != 13 + model.live_record_length && len != 13 + model.record_length {
        return false;
    }
    if frame[0] != 0xaa
        || usize::from(frame[1]) != len
        || frame[2] != model.tag
        || frame[3] != 0xe6
        || frame[len - 1] != 0xbb
    {
        return false;
    }
    let checksum = frame[..len - 2]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b))
        ^ 0x55;
    frame[len - 2] == checksum
        && frame[4..10] == [0x00, 0x02, 0x01, 0xff, 0x01, 0x02]
        && frame[10] == 0
}

fn build_control_frame(frame_id: u8, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(2 + payload.len());
    inner.push(0xf0);
    inner.push(frame_id);
    inner.extend_from_slice(payload);
    wrap_aabb(&inner)
}

fn option_byte(model: &WashTowerModel, draft: &HashMap<&'static str, u16>, offset: usize) -> u8 {
    let mut value = 0u8;
    for field in model.fields {
        if field.offset != offset
            || field.bit.is_none()
            || !model.setting_keys.contains(&field.key)
        {
            continue;
        }
        let raw = draft.get(field.key).copied().unwrap_or(0);
        if raw & 1 != 0 {
            value |= 1 << field.bit.unwrap();
        }
    }
    value
}

fn build_start_payload(model: &WashTowerModel, draft: &HashMap<&'static str, u16>) -> Vec<u8> {
    let get = |k: &'static str| draft.get(k).copied().unwrap_or(0);
    if model.tag == 0x33 {
        let mut p = vec![0u8; 21];
        let course = get("course") as u8;
        p[0] = course;
        p[1] = get("soilWash") as u8;
        p[2] = get("spin") as u8;
        p[3] = get("temp") as u8;
        p[4] = get("rinse") as u8;
        p[7] = option_byte(model, draft, 34);
        p[8] = option_byte(model, draft, 35);
        p[9] = option_byte(model, draft, 36);
        p[10] = course;
        p[11] = get("downloadCourse") as u8;
        p[15] = get("dryLevel") as u8;
        p
    } else {
        let mut p = vec![0u8; 14];
        p[0] = get("course") as u8;
        p[1] = get("dryLevel") as u8;
        p[2] = get("ecoHybrid") as u8;
        p[3] = get("moreLessTime") as u8;
        let res = get("reserveTimeMinute").to_be_bytes();
        p[6] = res[0];
        p[7] = res[1];
        p[9] = option_byte(model, draft, 24);
        p[10] = option_byte(model, draft, 25);
        p[11] = option_byte(model, draft, 26);
        p[12] = get("downloadCourse") as u8;
        p
    }
}

fn build_components(model: &WashTowerModel) -> HashMap<String, Value> {
    let mut components = HashMap::new();
    for field in model.fields {
        if !field.public || field.offset + field.length > model.record_length {
            continue;
        }
        let key = field.entity_key;
        let comp = if field.key.ends_with("TimeMinute") {
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{key}"),
                "state_topic": format!("$this/{key}"),
                "name": field.label,
                "device_class": "duration",
                "unit_of_measurement": "min",
            })
        } else if matches!(field.key, "state" | "course" | "error") {
            let mut opts = unique_labels(
                field
                    .values
                    .unwrap_or(&[])
                    .iter()
                    .map(|&(_, s)| s),
            );
            if !opts.contains(&"unknown") {
                opts.push("unknown");
            }
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{key}"),
                "state_topic": format!("$this/{key}"),
                "name": field.label,
                "device_class": "enum",
                "options": opts,
            })
        } else if field.key == "doorClose" {
            json!({
                "platform": "binary_sensor",
                "unique_id": format!("$deviceid-{key}"),
                "state_topic": format!("$this/{key}"),
                "name": "Door",
                "device_class": "door",
                "payload_on": "Off",
                "payload_off": "On",
            })
        } else if matches!(field.key, "childLock" | "doorLock" | "remoteStart") {
            let icon = if field.key == "remoteStart" {
                "mdi:remote"
            } else {
                "mdi:lock-outline"
            };
            json!({
                "platform": "binary_sensor",
                "unique_id": format!("$deviceid-{key}"),
                "state_topic": format!("$this/{key}"),
                "name": field.label,
                "payload_on": "On",
                "payload_off": "Off",
                "icon": icon,
            })
        } else {
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{key}"),
                "state_topic": format!("$this/{key}"),
                "name": field.label,
            })
        };
        components.insert(key.to_string(), comp);
    }

    for &setting_key in model.setting_keys {
        let Some(field) = find_field(model, setting_key) else {
            continue;
        };
        let Some(values) = field.values else {
            continue;
        };
        let key = field.entity_key;
        let opts: Vec<&str> = if setting_key == "course" {
            unique_labels(
                values
                    .iter()
                    .filter(|&&(raw, _)| find_rule(model, raw).is_some())
                    .map(|&(_, s)| s),
            )
        } else {
            unique_labels(values.iter().map(|&(_, s)| s))
        };
        components.insert(
            format!("setting_{key}"),
            json!({
                "platform": "select",
                "unique_id": format!("$deviceid-setting-{key}"),
                "state_topic": format!("$this/setting_{key}"),
                "command_topic": format!("$this/setting_{key}/set"),
                "name": format!("{} Setting", field.label),
                "options": opts,
            }),
        );
    }

    components.insert(
        "wake".into(),
        json!({"platform":"button","unique_id":"$deviceid-wake","command_topic":"$this/wake/set","name":"Exit power-save"}),
    );
    components.insert(
        "power".into(),
        json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"Power","payload_on":"ON","payload_off":"OFF"}),
    );
    components.insert(
        "power_transition".into(),
        json!({"platform":"sensor","unique_id":"$deviceid-power-transition","state_topic":"$this/power_transition","name":"Power transition state","entity_category":"diagnostic"}),
    );
    components.insert(
        "off".into(),
        json!({"platform":"button","unique_id":"$deviceid-off","command_topic":"$this/off/set","name":"Power off"}),
    );
    components.insert(
        "stop".into(),
        json!({"platform":"button","unique_id":"$deviceid-stop","command_topic":"$this/stop/set","name":"Pause"}),
    );
    components.insert(
        "start_course".into(),
        json!({"platform":"button","unique_id":"$deviceid-start-course","command_topic":"$this/start_course/set","name":"Start course"}),
    );
    components.insert(
        "remote_maintain".into(),
        json!({"platform":"switch","unique_id":"$deviceid-remote_maintain","state_topic":"$this/remote_maintain","command_topic":"$this/remote_maintain/set","name":"Keep remote start","payload_on":"On","payload_off":"Off","icon":"mdi:remote"}),
    );
    components
}

#[derive(Default)]
struct State {
    observed: HashMap<&'static str, u16>,
    draft: HashMap<&'static str, u16>,
    dirty_draft_keys: HashSet<&'static str>,
    current_state: Option<u16>,
    remote_start_allowed: bool,
    remote_allowed: bool,
    pending_power_on_at: Option<Instant>,
    power_on_refresh_sent: bool,
    power_transition_published: bool,
    draft_reconciled: bool,
    stopped: bool,
    timer: Option<JoinHandle<()>>,
}

struct Shared {
    core: Arc<AabbDeviceCore>,
    model: WashTowerModel,
    state: Mutex<State>,
    timer_wake: Condvar,
}

impl Shared {
    fn send_ctrl(&self, frame_id: u8, payload: &[u8]) {
        self.core
            .thinq
            .send_packet(&build_control_frame(frame_id, payload));
    }

    fn publish_draft_settings(&self, st: &State) {
        for &key in self.model.setting_keys {
            if let (Some(&raw), Some(field)) = (st.draft.get(key), find_field(&self.model, key)) {
                if let Some(values) = field.values {
                    if let Some(&(_, label)) = values.iter().find(|&&(k, _)| k == raw) {
                        self.core
                            .publish_property(&format!("setting_{}", field.entity_key), label.into());
                    }
                }
            }
        }
    }

    fn apply_course_defaults(&self, st: &mut State, course: u16, preserve_dirty: bool) -> bool {
        let Some(rule) = find_rule(&self.model, course) else {
            return false;
        };
        let mut updates: Vec<(&'static str, u16)> = vec![
            ("course", course),
            ("baseDownloadCourseData", 0),
            ("downloadCourse", 0),
        ];
        for opt in rule.options {
            updates.push((opt.key, opt.default_value));
        }
        for (key, value) in updates {
            if preserve_dirty && st.dirty_draft_keys.contains(key) {
                continue;
            }
            st.draft.insert(key, value);
            st.dirty_draft_keys.insert(key);
        }
        self.publish_draft_settings(st);
        true
    }

    fn valid_draft(&self, st: &State) -> bool {
        let Some(course) = st.draft.get("course").copied() else {
            return false;
        };
        let Some(rule) = find_rule(&self.model, course) else {
            return false;
        };
        for opt in rule.options {
            let Some(&val) = st.draft.get(opt.key) else {
                return false;
            };
            match opt.selectable {
                Some(allowed) => {
                    if !allowed.contains(&val) {
                        return false;
                    }
                }
                None => {
                    if val != opt.default_value {
                        return false;
                    }
                }
            }
        }
        let get = |k: &'static str| st.draft.get(k).copied().unwrap_or(0);
        if self.model.tag == 0x33 {
            let no_wash = get("soilWash") == 0;
            let no_spin = get("spin") == 0;
            let no_rinse = get("rinse") == 0;
            if (no_wash && get("temp") != 0)
                || ((no_wash || no_spin || no_rinse) && get("turboWash") != 0)
            {
                return false;
            }
        } else if get("steam") == 1
            && (get("dryLevel") == 1 || (get("dryLevel") == 3 && get("ecoHybrid") == 2))
        {
            return false;
        }
        true
    }

    fn expire_pending_locked(&self, st: &mut State, now: Instant) -> bool {
        let Some(at) = st.pending_power_on_at else {
            return false;
        };
        if now.duration_since(at) < POWER_ON_PENDING {
            return false;
        }
        st.pending_power_on_at = None;
        st.power_on_refresh_sent = false;
        self.timer_wake.notify_all();
        st.power_transition_published = true;
        self.core.publish_property("power_transition", "Timeout".into());
        true
    }

    fn start_power_on_timer(self: &Arc<Self>, st: &mut State) {
        if st.timer.is_some() {
            self.timer_wake.notify_all();
            return;
        }
        let shared = self.clone();
        st.timer = Some(thread::spawn(move || {
            let mut st = shared.state.lock();
            loop {
                if st.stopped {
                    return;
                }
                let Some(at) = st.pending_power_on_at else {
                    st.timer = None;
                    return;
                };
                let now = Instant::now();
                if shared.expire_pending_locked(&mut st, now) {
                    st.timer = None;
                    return;
                }
                let remaining = POWER_ON_PENDING.saturating_sub(now.duration_since(at));
                st = shared
                    .timer_wake
                    .wait_timeout(st, remaining)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }));
    }

    fn stop_timer(&self) {
        let handle = {
            let mut st = self.state.lock();
            st.stopped = true;
            st.pending_power_on_at = None;
            st.power_on_refresh_sent = false;
            self.timer_wake.notify_all();
            st.timer.take()
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
    }

    fn handle_power_on_response(&self, frame: &[u8]) {
        let send_refresh = {
            let mut st = self.state.lock();
            if st.pending_power_on_at.is_none() || self.expire_pending_locked(&mut st, Instant::now())
            {
                return;
            }
            if st.power_on_refresh_sent || !is_power_on_success(frame, &self.model) {
                return;
            }
            st.power_on_refresh_sent = true;
            st.power_transition_published = true;
            self.core
                .publish_property("power_transition", "Command accepted".into());
            true
        };
        if send_refresh {
            self.send_ctrl(CTRL_STATUS_QUERY.0, CTRL_STATUS_QUERY.1);
        }
    }

    fn process_data(&self, frame: &[u8]) {
        self.handle_power_on_response(frame);
        let Some(record) = select_current_record(frame, &self.model) else {
            return;
        };

        let mut to_publish: Vec<(&'static str, PropertyValue)> = Vec::new();
        {
            let mut st = self.state.lock();
            st.remote_start_allowed = false;
            st.remote_allowed = false;

            for field in self.model.fields {
                if field.offset + field.length > record.len() {
                    continue;
                }
                let mut raw = if field.length == 2 {
                    u16::from_be_bytes([record[field.offset], record[field.offset + 1]])
                } else {
                    u16::from(record[field.offset])
                };
                if let Some(bit) = field.bit {
                    raw = (raw >> bit) & 1;
                }
                if field.public || field.key == "remoteMaintain" {
                    let val: PropertyValue = match field.values {
                        Some(table) => lookup_label(table, raw).into(),
                        None => i64::from(raw).into(),
                    };
                    to_publish.push((field.entity_key, val));
                }
                st.observed.insert(field.key, raw);
                if !st.dirty_draft_keys.contains(field.key) {
                    st.draft.insert(field.key, raw);
                }
                if field.key == "state" {
                    st.current_state = Some(raw);
                }
                if field.key == "remoteStart" && raw == 1 {
                    st.remote_start_allowed = true;
                }
                if (field.key == "remoteStart" || field.key == "remoteMaintain") && raw == 1 {
                    st.remote_allowed = true;
                }
            }

            if !st.draft_reconciled {
                st.draft_reconciled = true;
                if !st.dirty_draft_keys.contains("course") {
                    let course = st
                        .observed
                        .get("course")
                        .copied()
                        .filter(|&c| find_rule(&self.model, c).is_some())
                        .unwrap_or(self.model.default_course);
                    self.apply_course_defaults(&mut st, course, true);
                }
            }

            let is_off = st.current_state == Some(0);
            to_publish.push(("power", if is_off { "OFF" } else { "ON" }.into()));

            if !is_off && st.pending_power_on_at.is_some() {
                st.pending_power_on_at = None;
                st.power_on_refresh_sent = false;
                self.timer_wake.notify_all();
                st.power_transition_published = true;
                to_publish.push(("power_transition", "Complete".into()));
            } else {
                self.expire_pending_locked(&mut st, Instant::now());
            }
            if !st.power_transition_published {
                st.power_transition_published = true;
                to_publish.push(("power_transition", "Standby".into()));
            }
        }

        for (k, v) in to_publish {
            self.core.publish_property(k, v);
        }
    }
}

pub struct Device {
    shared: Arc<Shared>,
}

impl Device {
    pub(crate) fn with_model(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        model: WashTowerModel,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let shared = Arc::new(Shared {
            core: core.clone(),
            model,
            state: Mutex::default(),
            timer_wake: Condvar::new(),
        });
        let mut config = default_config(&meta, Some(json!({"name": model.device_name})));
        config.components = build_components(&model);
        core.set_config(config);

        let s = shared.clone();
        thinq.on_data(Box::new(move |data| s.process_data(data)));
        Arc::new(Self { shared })
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.shared.core.id
    }

    fn start(&self) {}

    fn retire(&self) {
        self.shared.stop_timer();
    }

    fn drop_device(&self) {
        self.shared.stop_timer();
        self.shared.core.drop_device();
    }

    fn publish_config(&self) {
        self.shared.core.republish_config();
    }

    fn set_property(&self, prop: &str, value: &str) {
        let model = &self.shared.model;
        if let Some(setting_entity) = prop.strip_prefix("setting_") {
            let Some(field) = model
                .fields
                .iter()
                .find(|f| f.entity_key == setting_entity && model.setting_keys.contains(&f.key))
            else {
                return;
            };
            let Some(values) = field.values else {
                return;
            };
            let Some(&(raw, _)) = values.iter().find(|&&(_, label)| label == value) else {
                return;
            };
            {
                let mut st = self.shared.state.lock();
                if field.key == "course" {
                    if !self.shared.apply_course_defaults(&mut st, raw, false) {
                        return;
                    }
                } else {
                    st.draft.insert(field.key, raw);
                    st.dirty_draft_keys.insert(field.key);
                }
            }
            self.shared.core.publish_property(prop, value.into());
            return;
        }
        if prop == "send_course" {
            return;
        }

        let mut st = self.shared.state.lock();
        let Some(current_state) = st.current_state else {
            return;
        };

        match prop {
            "wake" => {
                if current_state != 0 {
                    return;
                }
                drop(st);
                self.shared.send_ctrl(CTRL_WAKE.0, CTRL_WAKE.1);
            }
            "power" => match value {
                "ON" => {
                    if current_state != 0 {
                        return;
                    }
                    self.shared.expire_pending_locked(&mut st, Instant::now());
                    if st.pending_power_on_at.is_some() {
                        return;
                    }
                    st.pending_power_on_at = Some(Instant::now());
                    st.power_on_refresh_sent = false;
                    st.power_transition_published = true;
                    self.shared.start_power_on_timer(&mut st);
                    drop(st);
                    self.shared.send_ctrl(CTRL_POWER_ON.0, CTRL_POWER_ON.1);
                    self.shared
                        .core
                        .publish_property("power_transition", "Turning on".into());
                }
                "OFF" => {
                    if current_state == 0 || !st.remote_allowed {
                        return;
                    }
                    drop(st);
                    self.shared.send_ctrl(CTRL_OFF.0, CTRL_OFF.1);
                }
                _ => {}
            },
            "remote_maintain" => {
                let enable = matches!(value, "ON" | "On");
                let disable = matches!(value, "OFF" | "Off");
                if enable && !matches!(current_state, 0 | 1) {
                    return;
                }
                if disable && current_state != 1 {
                    return;
                }
                let cmd = if enable {
                    CTRL_REMOTE_MAINTAIN_ON
                } else if disable {
                    CTRL_REMOTE_MAINTAIN_OFF
                } else {
                    return;
                };
                drop(st);
                self.shared.send_ctrl(cmd.0, cmd.1);
            }
            "start_course" => {
                if current_state != 1 || !st.remote_start_allowed {
                    return;
                }
                if model.tag == 0x33 && st.observed.get("reserveTimeMinute").copied().unwrap_or(0) != 0
                {
                    return;
                }
                if !self.shared.valid_draft(&st) {
                    return;
                }
                let payload = build_start_payload(model, &st.draft);
                drop(st);
                self.shared.send_ctrl(0x26, &payload);
            }
            "off" | "stop" => {
                if current_state == 0 || !st.remote_allowed {
                    return;
                }
                let cmd = if prop == "off" { CTRL_OFF } else { CTRL_STOP };
                drop(st);
                self.shared.send_ctrl(cmd.0, cmd.1);
            }
            _ => {}
        }
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::with_model(ha, thinq, meta, WASHER_MODEL)
}

pub(crate) fn create_dryer(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::with_model(ha, thinq, meta, DRYER_MODEL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::ha::MockHaConnection;
    use rethink_core::thinq::{hex_decode, hex_encode, MockThinq2Device};

    const ID: &str = "test-device";

    // Captured real frames from Korean WashTower (FAKPK21021 + BDH_D39301_KR).
    const WASHER_EB: &str = "aaff330a005500bc84000100eb004300000000000000000000000000000000000002005e01009a000000001a0400000000000000410000300000000000000400006000000000000000f00000000408000000fc1cbb";
    const DRYER_EB: &str = "aaff340a0044006320000100eb003200000000000000000000000000000100000000040000008f20000000070000870100000000000000000000000000000000008343bb";
    const DRYER_EC: &str = "aaff340a0076008217000100ec006400000000000000000000000000010000000000040000008f200000800700008701000000000000000000000000000000000000000000000000000000000000010000000000040000008f20000082070000870100000000000000000000000000000000008320bb";
    const DRYER_POWER_ON_E6: &str = "aa3f34e6000201ff01020000000000000000000000000000000100000000040000008f00000000070000870100000000000000000000000000000000007ebb";

    #[test]
    fn decodes_captured_washer_and_dryer_frames_and_power_on_flow() {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("FAKPK21021", "FAKPK21021", "test");
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let washer = create(ha.clone(), thinq.clone(), meta);

        thinq.emit_data(&hex_decode(WASHER_EB));
        let p = ha.device(ID).unwrap().properties;
        assert_eq!(p.get("state").map(|v| v.as_string()).as_deref(), Some("On"));
        assert_eq!(p.get("power").map(|v| v.as_string()).as_deref(), Some("ON"));
        assert_eq!(p.get("error").map(|v| v.as_string()).as_deref(), Some("No error"));
        assert_eq!(
            p.get("setting_course").map(|v| v.as_string()).as_deref(),
            Some("Standard")
        );
        washer.drop_device();

        let ha_d = MockHaConnection::new();
        let meta_d = Metadata::new("BDH_D39301_KR", "BDH_D39301_KR", "test");
        let thinq_d = MockThinq2Device::new(ID, meta_d.clone());
        let dryer = create_dryer(ha_d.clone(), thinq_d.clone(), meta_d);

        thinq_d.emit_data(&hex_decode(DRYER_EB));
        let pd = ha_d.device(ID).unwrap().properties;
        assert_eq!(
            pd.get("state").map(|v| v.as_string()).as_deref(),
            Some("Power off")
        );
        assert_eq!(pd.get("power").map(|v| v.as_string()).as_deref(), Some("OFF"));
        assert_eq!(
            pd.get("power_transition").map(|v| v.as_string()).as_deref(),
            Some("Standby")
        );

        // Power ON from off sends vtCtrl power and triggers status query on E6 success.
        dryer.set_property("power", "ON");
        assert_eq!(
            thinq_d.outbox().iter().map(|b| hex_encode(b)).collect::<Vec<_>>(),
            vec!["AA0DF0E5000201FF010201C7BB"]
        );
        assert_eq!(
            ha_d.device(ID).unwrap().properties.get("power_transition").map(|v| v.as_string()).as_deref(),
            Some("Turning on")
        );
        thinq_d.emit_data(&hex_decode(DRYER_POWER_ON_E6));
        assert_eq!(
            thinq_d.outbox().iter().map(|b| hex_encode(b)).collect::<Vec<_>>(),
            vec![
                "AA0DF0E5000201FF010201C7BB",
                "AA12F0ED1121010000001804111200005EBB",
            ]
        );
        assert_eq!(
            ha_d.device(ID).unwrap().properties.get("power_transition").map(|v| v.as_string()).as_deref(),
            Some("Command accepted")
        );

        // Captured 0xEC selects current record and completes power transition.
        thinq_d.emit_data(&hex_decode(DRYER_EC));
        let pd = ha_d.device(ID).unwrap().properties;
        assert_eq!(pd.get("state").map(|v| v.as_string()).as_deref(), Some("On"));
        assert_eq!(pd.get("power").map(|v| v.as_string()).as_deref(), Some("ON"));
        assert_eq!(
            pd.get("remote_maintain").map(|v| v.as_string()).as_deref(),
            Some("On")
        );
        assert_eq!(
            pd.get("power_transition").map(|v| v.as_string()).as_deref(),
            Some("Complete")
        );

        // Power OFF allowed because remoteMaintain is On.
        thinq_d.reset_recorder();
        dryer.set_property("power", "OFF");
        assert_eq!(
            thinq_d.outbox().iter().map(|b| hex_encode(b)).collect::<Vec<_>>(),
            vec!["AA09F0240101009CBB"]
        );
        dryer.drop_device();
    }

    #[test]
    fn start_payloads_match_official_bit_packing() {
        let mut washer = HashMap::new();
        washer.insert("course", 46);
        washer.insert("soilWash", 3);
        washer.insert("spin", 6);
        washer.insert("temp", 3);
        washer.insert("rinse", 2);
        washer.insert("turboWash", 1);
        washer.insert("freshCare", 1);
        washer.insert("downloadCourse", 9);
        washer.insert("dryLevel", 4);
        assert_eq!(
            hex_encode(&build_start_payload(&WASHER_MODEL, &washer)),
            "2E0306030200002000402E09000000040000000000"
        );

        let mut dryer = HashMap::new();
        dryer.insert("course", 7);
        dryer.insert("dryLevel", 4);
        dryer.insert("ecoHybrid", 2);
        dryer.insert("moreLessTime", 5);
        dryer.insert("reserveTimeMinute", 30);
        dryer.insert("wrinkleCare", 1);
        dryer.insert("downloadCourse", 9);
        assert_eq!(
            hex_encode(&build_start_payload(&DRYER_MODEL, &dryer)),
            "070402050000001E000800000900"
        );
    }
}
