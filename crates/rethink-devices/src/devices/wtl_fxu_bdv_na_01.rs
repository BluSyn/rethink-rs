//! WTL_FXU_BDV_NA_01 WashTower (AABB).

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};

use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::sync::Arc;

const WASHER_UNIT: u8 = 0x33;
const DRYER_UNIT: u8 = 0x34;
const FIXED_HEADER_LENGTH: usize = 13;
const STATE_BLOCK_LENGTH: usize = 95;
const DOOR_OPEN: &str = "OPEN";
const DOOR_CLOSE: &str = "CLOSE";
fn washer_courses(code: u8) -> &'static str {
    match code {
        0x00 => "NOT_SELECTED",
        0x01 => "3IN2_REF",
        0x02 => "ADD_PREWASH",
        0x03 => "AIRCLEANING",
        0x04 => "ALLERGY_SPASTEAM",
        0x05 => "ALLERGYCARE",
        0x06 => "ANSIMCOLD",
        0x07 => "BABY_STEAMCARE",
        0x08 => "BABYCARE",
        0x09 => "BEDDING",
        0x0a => "BOIL",
        0x0b => "BRIGHT_WHITE",
        0x0c => "BULKY",
        0x0e => "CASUAL",
        0x0f => "COLD_CARE",
        0x10 => "COLD_CLEAN",
        0x11 => "COLDWASH",
        0x12 => "COLORCARE",
        0x13 => "COTTONECO",
        0x14 => "CUPBOARD_DRY",
        0x15 => "DARKWASH",
        0x16 => "DELICATES",
        0x17 => "DIRECTWEAR",
        0x18 => "DOUBLE_RINSE",
        0x19 => "DRAIN_SPIN",
        0x1a => "DRYONLY",
        0x1b => "DUVET",
        0x1c => "DUVETCLEANING",
        0x1d => "EASYCARE",
        0x1e => "FAVORITE",
        0x1f => "GENTLECARE",
        0x20 => "HALFLOAD",
        0x21 => "HANDWASH",
        0x22 => "HANDWASH_WOOL",
        0x23 => "HEAVYDUTY",
        0x24 => "INTENSIVE60",
        0x25 => "IRON_DRY",
        0x26 => "JEAN",
        0x27 => "KIDS_WEARS",
        0x28 => "JUMBOWASH",
        0x29 => "LINGERIE",
        0x2a => "LOWTEMP_DRY",
        0x2b => "MIX",
        0x2c => "HYGIENE_40",
        0x2d => "SANITARY_60",
        0x2e => "NORMAL",
        0x2f => "OVERNIGHT",
        0x30 => "PERM_PRESS",
        0x31 => "POWER_CLEAN",
        0x32 => "PRE_WASH",
        0x33 => "QUICK_DEO",
        0x34 => "QUICK30",
        0x35 => "QUIET",
        0x36 => "REFRESH",
        0x37 => "RINSE_SPIN",
        0x38 => "RINSEONLY",
        0x39 => "RUGGED",
        0x3a => "SAFETY",
        0x3b => "SAFETY_NORMAL",
        0x3c => "SANITARY",
        0x3d => "SANITARY_OXI",
        0x3e => "SAVING_WATER",
        0x3f => "SCHOOLING",
        0x40 => "SHOES",
        0x41 => "SILENT",
        0x42 => "SILENTWASH",
        0x43 => "SKINCARE",
        0x44 => "SMALL_LOAD",
        0x45 => "SMARTSAVE",
        0x46 => "SOAK",
        0x47 => "SPA_REF",
        0x48 => "SPEED_DRY",
        0x49 => "SPEED_TUB_CLEAN",
        0x4a => "SPEEDWASH",
        0x4b => "SPEED14",
        0x4c => "SPEEDBOIL",
        0x4d => "SPEEDWASH_DRY",
        0x4e => "SPIN_ONLY",
        0x4f => "SPORTS_WEARS",
        0x50 => "STAINCARE",
        0x51 => "STEAM_COTTON",
        0x52 => "STRONG_DRY",
        0x53 => "TIME_DRY",
        0x54 => "TOWELS",
        0x55 => "TUB_CLEAN",
        0x56 => "TUB_DRY",
        0x57 => "TURBOWASH",
        0x58 => "WASHDRY",
        0x59 => "WASHONLY",
        0x5a => "WHITE",
        0x5b => "WINDSPIN120",
        0x5c => "WINDSPIN60",
        0x5d => "WINDSPIN90",
        0x5e => "WOOL",
        0x5f => "SINGLE_SHIRTS",
        _ => "unknown",
    }
}
fn washer_temps(code: u8) -> &'static str {
    match code {
        0x00 => "NO_TEMP",
        0x01 => "TEMP_20",
        0x02 => "TEMP_30",
        0x03 => "TEMP_40",
        0x04 => "TEMP_50",
        0x05 => "TEMP_60",
        0x06 => "TEMP_95",
        0x07 => "TEMP_TAP_COLD",
        0x08 => "TEMP_COLD",
        0x09 => "TEMP_WARM",
        0x0a => "TEMP_HOT",
        0x0b => "TEMP_EXTRA_HOT",
        0x0c => "TEMP_COLD_HOT",
        0x0d => "FL27_TEMP_TAPCOLD",
        0x0e => "N/A",
        0x0f => "FL27_TEMP_ECOWARM",
        0x10 => "FL27_TEMP_WARM",
        _ => "unknown",
    }
}
fn washer_soil_wash(code: u8) -> &'static str {
    match code {
        0x00 => "NO_SOILWASH",
        0x01 => "SOILWASH_LIGHT",
        0x02 => "SOILWASH_LIGHT_NORMAL",
        0x03 => "SOILWASH_NORMAL",
        0x04 => "SOILWASH_NORMAL_HEAVY",
        0x05 => "SOILWASH_HEAVY",
        0x06 => "SOILWASH_PREWASH",
        0x07 => "SOILWASH_SOAKING",
        0x08 => "SOILWASH_TURBO_WASH",
        _ => "unknown",
    }
}
fn washer_rinse(code: u8) -> &'static str {
    match code {
        0x00 => "NO_RINSE",
        0x01 => "RINSE_1",
        0x02 => "RINSE_2",
        0x03 => "RINSE_3",
        0x04 => "RINSE_4",
        0x05 => "RINSE_5",
        0x06 => "RINSE_6",
        0x07 => "RINSE_7",
        0x08 => "RINSE_8",
        0x09 => "RINSE_1_SAFE",
        0x0a => "RINSE_2_SAFE",
        0x0b => "RINSE_3_SAFE",
        0x0c => "RINSE_4_SAFE",
        0x0d => "RINSE_5_SAFE",
        0x0e => "N/A",
        0x0f => "RINSE_PLUS",
        0x10 => "RINSE_PLUS2",
        _ => "unknown",
    }
}
fn washer_spin(code: u8) -> &'static str {
    match code {
        0x00 => "NO_SPIN",
        0x01 => "SPIN_400",
        0x02 => "SPIN_600",
        0x03 => "SPIN_700",
        0x04 => "SPIN_800",
        0x05 => "SPIN_900",
        0x06 => "SPIN_1000",
        0x07 => "SPIN_1100",
        0x08 => "SPIN_1200",
        0x09 => "SPIN_1400",
        0x0a => "SPIN_1600",
        0x0b => "SPIN_MAX",
        0x0c => "SPIN_DRAIN_ONLY",
        0x0d => "SPIN_LOW",
        0x0e => "N/A",
        0x0f => "SPIN_HIGH",
        0x10 => "SPIN_EXTRA_HIGH",
        _ => "unknown",
    }
}
fn washer_soak(code: u8) -> &'static str {
    match code {
        0x00 => "NO_SOAK",
        0x01 => "SOAK_30",
        0x02 => "SOAK_45",
        0x03 => "SOAK_60",
        0x04 => "SOAK_120",
        0x05 => "SOAK_180",
        0x06 => "SOAK_240",
        _ => "unknown",
    }
}
fn washer_water_level(code: u8) -> &'static str {
    match code {
        0x00 => "NO_WATERLEVEL",
        0x01 => "WATERLEVEL_2",
        0x02 => "WATERLEVEL_3",
        0x03 => "WATERLEVEL_4",
        0x04 => "WATERLEVEL_5",
        0x05 => "WATERLEVEL_6",
        0x06 => "WATERLEVEL_7",
        0x07 => "WATERLEVEL_8",
        0x08 => "WATERLEVEL_9",
        0x09 => "WATERLEVEL_10",
        _ => "unknown",
    }
}
fn washer_load_item(code: u8) -> &'static str {
    match code {
        0x00 => "NO_LOADITEM",
        0x01 => "LOADITEM_1",
        0x02 => "LOADITEM_2",
        0x03 => "LOADITEM_3",
        _ => "unknown",
    }
}
fn washer_load_level(code: u8) -> &'static str {
    match code {
        0x00 => "LOAD_AUTO_DETECT",
        0x01 => "LOAD_LEVEL_1",
        0x02 => "LOAD_LEVEL_2",
        0x03 => "LOAD_LEVEL_3",
        0x04 => "LOAD_LEVEL_4",
        0x05 => "LOAD_LEVEL_5",
        0x06 => "LOAD_LEVEL_6",
        0x07 => "LOAD_LEVEL_7",
        0x08 => "LOAD_LEVEL_8",
        _ => "unknown",
    }
}
fn washer_rinse_count(code: u8) -> &'static str {
    match code {
        0x00 => "NO_RINSE",
        0x01 => "RINSE_1",
        0x02 => "RINSE_2",
        0x03 => "RINSE_3",
        0x04 => "RINSE_4",
        0x05 => "RINSE_5",
        0x06 => "RINSE_6",
        0x07 => "RINSE_7",
        0x08 => "RINSE_8",
        _ => "unknown",
    }
}
fn device_buzzer(code: u8) -> &'static str {
    match code {
        0x00 => "Off",
        0x01 => "Low",
        0x02 => "Medium",
        0x03 => "High",
        0x04 => "Very High",
        _ => "unknown",
    }
}
const DEVICE_BUZZER_VALUES: &[&str] = &["Off", "Low", "Medium", "High", "Very High"];
fn dryer_temp(code: u8) -> &'static str {
    match code {
        0x00 => "NO_TEMP",
        0x01 => "TEMP_ULTRALOW",
        0x02 => "TEMP_LOW",
        0x03 => "TEMP_MEDIUM",
        0x04 => "TEMP_MEDIUMHIGH",
        0x05 => "TEMP_HIGH",
        _ => "unknown",
    }
}
fn dryer_time_dry(code: u8) -> &'static str {
    match code {
        0x00 => "NO_TIMEDRY",
        0x01 => "TIMEDRY_20",
        0x02 => "TIMEDRY_30",
        0x03 => "TIMEDRY_40",
        0x04 => "TIMEDRY_50",
        0x05 => "TIMEDRY_60",
        0x06 => "TIMEDRY_70",
        0x07 => "TIMEDRY_80",
        _ => "unknown",
    }
}
fn washer_states(code: u8) -> &'static str {
    match code {
        0x00 => "POWEROFF",
        0x01 => "INITIAL",
        0x02 => "PAUSE",
        0x03 => "DETECTING",
        0x04 => "DISPLAY_LOAD",
        0x05 => "ADD_DRAIN",
        0x06 => "DETERGENT_AMOUNT",
        0x07 => "RESERVED",
        0x08 => "SOAK",
        0x09 => "PREWASH",
        0x0b => "RUNNING",
        0x0c => "RINSING",
        0x0d => "RINSEHOLD",
        0x0e => "SPINNING",
        0x0f => "DRYING",
        0x10 => "END",
        0x11 => "COOLDOWN",
        0x12 => "COOLFAN",
        0x14 => "STEAM_SOFTENER",
        0x15 => "REFRESHING",
        0x16 => "ERROR",
        0x17 => "ERROR_AUTO_OFF",
        0x18 => "SHOES_MODULE",
        0x19 => "DOING_DIAGNOSIS",
        0x1a => "DOING_FIRM_UPDATE",
        0x1b => "FROZEN_PREVENT_INITIAL",
        0x1c => "FROZEN_PREVENT_PAUSE",
        0x1d => "FROZEN_PREVENT_RUNNING",
        0x1e => "SERVICE",
        0x1f => "TEST",
        0x20 => "AUTOTEST",
        0x21 => "FIRMWARE_UPDATE",
        0x22 => "AUDIBLE_DIAGNOSIS",
        0x23 => "AUTO_DT_OPEN_PAUSE",
        0x24 => "CONFIRM_START_FOR_CONTROL",
        0x25 => "CLOTHING_RECOGNITION",
        0x26 => "DETERGENT_INPUT",
        0x27 => "SOFTENER_INPUT",
        0x28 => "POLLUTION_DETECTING",
        0x29 => "TUB_CLEANING",
        0x2a => "END_REMOTE_MAINTAIN_ON",
        0x2b => "STEAM",
        0x2f => "LAUNDRYCARE",
        0x30 => "EZDISPENSE_CLEANING",
        0x31 => "END_WAITING",
        _ => "unknown",
    }
}
fn washer_errors(code: u8) -> &'static str {
    match code {
        0x00 => "NONE",
        0x01 => "ERROR_PUMP",
        0x02 => "ERROR_IE",
        0x03 => "ERROR_OE",
        0x04 => "ERROR_UE",
        0x05 => "ERROR_FE",
        0x06 => "ERROR_AE",
        0x07 => "ERROR_PE",
        0x08 => "ERROR_TE",
        0x09 => "ERROR_LE",
        0x0a => "ERROR_CE",
        0x0b => "ERROR_DHE",
        0x0c => "ERROR_PFE",
        0x0d => "ERROR_FF",
        0x0e => "ERROR_DCE",
        0x0f => "ERROR_EE",
        0x10 => "ERROR_LOE",
        0x11 => "ERROR_LE1",
        0x12 => "ERROR_E3",
        0x13 => "ERROR_PS",
        0x14 => "ERROR_DE1",
        _ => "unknown",
    }
}
fn dryer_errors(code: u8) -> &'static str {
    match code {
        0x00 => "NONE",
        0x01 => "ERROR_TE1",
        0x02 => "ERROR_TE2",
        0x03 => "ERROR_TE3",
        0x04 => "ERROR_TE4",
        0x05 => "ERROR_TE5",
        0x06 => "ERROR_TE6",
        0x07 => "ERROR_CE1",
        0x08 => "ERROR_CE2",
        0x09 => "ERROR_HE1",
        0x0a => "ERROR_E1",
        0x0b => "ERROR_E3",
        0x0c => "ERROR_E4",
        0x0d => "ERROR_E5",
        0x0e => "ERROR_DRAINMOTOR",
        0x0f => "ERROR_EMPTYWATER",
        0x10 => "ERROR_DOOR",
        0x11 => "ERROR_FILTERCLOGGING",
        0x12 => "ERROR_NOFILTER",
        0x13 => "ERROR_EEPROM",
        0x14 => "ERROR_F1",
        _ => "unknown",
    }
}
fn dryer_states(code: u8) -> &'static str {
    match code {
        0x00 => "POWEROFF",
        0x01 => "INITIAL",
        0x02 => "RUNNING",
        0x03 => "PAUSE",
        0x04 => "END",
        0x05 => "ERROR",
        0x06 => "AUDIBLE_DIAGNOSIS",
        0x07 => "DRYING",
        0x08 => "COOLING",
        0x09 => "WRINKLECARE",
        0x0a => "RESERVED",
        0x0b => "DELAYLOAD",
        0x0c => "SPINREERVE",
        0x0d => "AUTOTEST",
        0x0e => "DETECTING",
        0x0f => "STEAM",
        0x10 => "CLOTHING_RECOGNITION",
        0x11 => "CONDENSER_CLEAN",
        0x12 => "BEDDINGBRUSHING",
        0x13 => "DRY_REFRESHING",
        0x14 => "ALLERGYCARE",
        0x15 => "CONDENSERCARE",
        0x16 => "END_REMOTE_MAINTAIN_ON",
        0x17 => "DRYREADY",
        0x18 => "LAUNDRYCARE",
        0x19 => "DEHUMIDIFICATION",
        0x1a => "DEHUMIDIFICATION_END",
        0x1b => "END_WAITING",
        0x1c => "DRUM_CARE",
        0x1f => "AI_LOAD_CHECK",
        _ => "unknown",
    }
}
fn dryer_dry_levels(code: u8) -> &'static str {
    match code {
        0x00 => "NOT_SELECTED",
        0x01 => "DAMP",
        0x02 => "LESS",
        0x03 => "NORMAL",
        0x04 => "MORE",
        0x05 => "VERY",
        _ => "unknown",
    }
}
fn dryer_duct_clogging(code: u8) -> &'static str {
    match code {
        0x00 => "NONE",
        0x01 => "LEVEL_1",
        0x02 => "LEVEL_2",
        _ => "unknown",
    }
}
fn dryer_courses(code: u8) -> &'static str {
    match code {
        0x00 => "NOT_SELECTED",
        0x01 => "REFRESH",
        0x02 => "TOWELS",
        0x03 => "JEAN",
        0x04 => "BEDDING",
        0x05 => "EASYCARE",
        0x06 => "MIXFABRIC",
        0x07 => "NORMAL",
        0x08 => "SPORTWEAR",
        0x09 => "QUICKDRY",
        0x0a => "DELICATES",
        0x0b => "WOOL",
        0x0c => "RACKDRY",
        0x0d => "COOLAIR",
        0x0e => "WARMAIR",
        0x0f => "BEDDINGBRUSH",
        0x10 => "ALLERGYCARE",
        0x11 => "POWER",
        0x12 => "CONDENSERCARE",
        0x13 => "TUBCLEAN",
        0x14 => "PADDINGREFRESH",
        0x15 => "TIMEDRY",
        0x16 => "WATERREPELLENT",
        0x17 => "BABYWEAR",
        0x18 => "SMALLLOAD",
        0x19 => "COTTONPLUS",
        0x1a => "PERMPRESS",
        0x1b => "PET_CARE",
        0x1c => "SHIRT1EA",
        0x1d => "HEAVYDUTY",
        0x1e => "ULTRADELICATES",
        0x1f => "KIDWEAR",
        0x20 => "LOWTEMPDRY",
        0x21 => "JUMBODRY",
        0x22 => "SPEEDDRY",
        0x23 => "AIRDRY",
        0x24 => "SPOTCLEANING",
        0x25 => "STEAMFRESH",
        0x26 => "STEAMSANITARY",
        0x27 => "FRESHENUP",
        0x28 => "FTFRESH",
        0x29 => "MISTFRESH",
        0x2a => "SUPERDRY",
        0x2b => "LOWTEMPDRYPLUS",
        0x2c => "AI_COURSE",
        0x2d => "SILENT",
        0x2e => "CLOTHCARE",
        0x2f => "WRINKLEFREE",
        0x30 => "LIGHTBEDDING",
        0x31 => "GYMCLOTHES",
        0x32 => "RAINYDAY",
        0x33 => "EASYIRON",
        0x34 => "DUVET_COVER",
        0x35 => "BLANKETREFRESH",
        0x36 => "OVERNIGHTDRY",
        0x37 => "HALFLOADDRY",
        0x38 => "FULLLOADDRY",
        0x39 => "DEHUMIDIFICATION",
        0x3a => "TURBODRY",
        _ => "unknown",
    }
}
fn init_lcd_themes(code: u8) -> &'static str {
    match code {
        0x00 => "Default",
        0x01 => "Winter 1",
        0x02 => "Winter 2",
        0x03 => "Winter 3",
        0x04 => "Spring 1",
        0x05 => "Spring 2",
        0x06 => "Summer 1",
        0x07 => "Summer 2",
        0x08 => "Fall 1",
        0x09 => "Halloween",
        0x0a => "New Years",
        0x0b => "Christmas",
        0x0c => "None",
        _ => "unknown",
    }
}
const INIT_LCD_THEMES_VALUES: &[&str] = &["Default", "Winter 1", "Winter 2", "Winter 3", "Spring 1", "Spring 2", "Summer 1", "Summer 2", "Fall 1", "Halloween", "New Years", "Christmas", "None"];

fn buzzer_index(value: &str) -> Option<u8> {
    DEVICE_BUZZER_VALUES.iter().position(|v| *v == value).map(|i| i as u8)
}
fn init_lcd_index(value: &str) -> Option<u8> {
    INIT_LCD_THEMES_VALUES.iter().position(|v| *v == value).map(|i| i as u8)
}

fn be_u16(block: &[u8], off: usize) -> i64 {
    ((block[off] as i64) << 8) | (block[off + 1] as i64)
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });
        let mut base = default_config(&meta, Some(json!({"name": "LG WashTower"})));
        let mut components = Map::new();
        let comps: &[(&str, serde_json::Value)] = &[
            ("washer_state", json!({"platform":"sensor","unique_id":"$deviceid-washer-state","state_topic":"$this/washer/state","name":"Washer state","icon":"mdi:washing-machine"})),
            ("washer_soil_wash", json!({"platform":"sensor","unique_id":"$deviceid-washer-soil-wash","state_topic":"$this/washer/soil_wash","name":"Washer soil wash","icon":"mdi:spray"})),
            ("washer_rinse", json!({"platform":"sensor","unique_id":"$deviceid-washer-rinse","state_topic":"$this/washer/rinse","name":"Washer rinse","icon":"mdi:water"})),
            ("washer_spin", json!({"platform":"sensor","unique_id":"$deviceid-washer-spin","state_topic":"$this/washer/spin","name":"Washer spin","icon":"mdi:rotate-right"})),
            ("washer_soak", json!({"platform":"sensor","unique_id":"$deviceid-washer-soak","state_topic":"$this/washer/soak","name":"Washer soak","icon":"mdi:water"})),
            ("washer_water_level", json!({"platform":"sensor","unique_id":"$deviceid-washer-water-level","state_topic":"$this/washer/water_level","name":"Washer water level","icon":"mdi:water-check"})),
            ("washer_load_item", json!({"platform":"sensor","unique_id":"$deviceid-washer-load-item","state_topic":"$this/washer/load_item","name":"Washer load item","icon":"mdi:tshirt-crew"})),
            ("washer_load_level", json!({"platform":"sensor","unique_id":"$deviceid-washer-load-level","state_topic":"$this/washer/load_level","name":"Washer load level","icon":"mdi:weight"})),
            ("washer_rinse_count", json!({"platform":"sensor","unique_id":"$deviceid-washer-rinse-count","state_topic":"$this/washer/rinse_count","name":"Washer rinse count","icon":"mdi:counter"})),
            ("washer_laundry_texture", json!({"platform":"sensor","unique_id":"$deviceid-washer-laundry-texture","state_topic":"$this/washer/laundry_texture","name":"Washer laundry texture","icon":"mdi:texture"})),
            ("washer_reserve_time", json!({"platform":"sensor","unique_id":"$deviceid-washer-reserve-time","state_topic":"$this/washer/reserve_time","device_class":"duration","unit_of_measurement":"min","name":"Washer reserve time"})),
            ("washer_remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-washer-remaining-time","state_topic":"$this/washer/remaining_time","device_class":"duration","unit_of_measurement":"min","name":"Washer remaining time"})),
            ("washer_initial_time", json!({"platform":"sensor","unique_id":"$deviceid-washer-initial-time","state_topic":"$this/washer/initial_time","device_class":"duration","unit_of_measurement":"min","name":"Washer initial time"})),
            ("washer_temp", json!({"platform":"sensor","unique_id":"$deviceid-washer-temp","state_topic":"$this/washer/temp","name":"Washer temperature","icon":"mdi:thermometer"})),
            ("washer_course", json!({"platform":"sensor","unique_id":"$deviceid-washer-course","state_topic":"$this/washer/course","name":"Washer course","icon":"mdi:washing-machine"})),
            ("washer_energy", json!({"platform":"sensor","unique_id":"$deviceid-washer-energy","state_topic":"$this/washer/energy","name":"Washer energy","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh","icon":"mdi:lightning-bolt"})),
            ("dryer_reserve_time", json!({"platform":"sensor","unique_id":"$deviceid-dryer-reserve-time","state_topic":"$this/dryer/reserve_time","device_class":"duration","unit_of_measurement":"min","name":"Dryer reserve time"})),
            ("dryer_remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-dryer-remaining-time","state_topic":"$this/dryer/remaining_time","device_class":"duration","unit_of_measurement":"min","name":"Dryer remaining time"})),
            ("dryer_initial_time", json!({"platform":"sensor","unique_id":"$deviceid-dryer-initial-time","state_topic":"$this/dryer/initial_time","device_class":"duration","unit_of_measurement":"min","name":"Dryer initial time"})),
            ("dryer_course", json!({"platform":"sensor","unique_id":"$deviceid-dryer-course","state_topic":"$this/dryer/course","name":"Dryer course","icon":"mdi:tumble-dryer"})),
            ("dryer_state", json!({"platform":"sensor","unique_id":"$deviceid-dryer-state","state_topic":"$this/dryer/state","name":"Dryer state","icon":"mdi:tumble-dryer"})),
            ("dryer_dry_level", json!({"platform":"sensor","unique_id":"$deviceid-dryer-dry-level","state_topic":"$this/dryer/dry_level","name":"Dryer dry level","icon":"mdi:water-percent"})),
            ("dryer_temp", json!({"platform":"sensor","unique_id":"$deviceid-dryer-temp","state_topic":"$this/dryer/temp","name":"Dryer temperature","icon":"mdi:thermometer"})),
            ("dryer_time_dry", json!({"platform":"sensor","unique_id":"$deviceid-dryer-time-dry","state_topic":"$this/dryer/time_dry","name":"Dryer time dry","icon":"mdi:timer"})),
            ("washer_buzzer", json!({"platform":"select","unique_id":"$deviceid-washer-buzzer","state_topic":"$this/washer/buzzer","command_topic":"$this/washer/buzzer/set","options":DEVICE_BUZZER_VALUES,"optimistic":true,"name":"Washer buzzer","icon":"mdi:volume-high","availability":[{"topic":"$this/washer/power","payload_available":"ON","payload_not_available":"OFF"}]})),
            ("washer_error", json!({"platform":"sensor","unique_id":"$deviceid-washer-error","state_topic":"$this/washer/error","name":"Washer error","icon":"mdi:alert-circle"})),
            ("dryer_buzzer", json!({"platform":"select","unique_id":"$deviceid-dryer-buzzer","state_topic":"$this/dryer/buzzer","command_topic":"$this/dryer/buzzer/set","options":DEVICE_BUZZER_VALUES,"optimistic":true,"name":"Dryer buzzer","icon":"mdi:volume-high","availability":[{"topic":"$this/dryer/power","payload_available":"ON","payload_not_available":"OFF"}]})),
            ("dryer_error", json!({"platform":"sensor","unique_id":"$deviceid-dryer-error","state_topic":"$this/dryer/error","name":"Dryer error","icon":"mdi:alert-circle"})),
            ("washer_door", json!({"platform":"binary_sensor","unique_id":"$deviceid-washer-door","state_topic":"$this/washer/door","device_class":"door","payload_on":DOOR_OPEN,"payload_off":DOOR_CLOSE,"name":"Washer door"})),
            ("washer_door_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-washer-door-lock","state_topic":"$this/washer/door_lock","payload_on":"ON","payload_off":"OFF","name":"Washer door lock","icon":"mdi:lock"})),
            ("washer_add_garment", json!({"platform":"binary_sensor","unique_id":"$deviceid-washer-add-garment","state_topic":"$this/washer/add_garment","payload_on":"ON","payload_off":"OFF","name":"Washer add garment","icon":"mdi:tshirt-crew"})),
            ("washer_child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-washer-child-lock","state_topic":"$this/washer/child_lock","payload_on":"ON","payload_off":"OFF","name":"Washer child lock","icon":"mdi:lock-outline"})),
            ("washer_remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-washer-remote-start","state_topic":"$this/washer/remote_start","payload_on":"ON","payload_off":"OFF","name":"Washer remote start","icon":"mdi:remote"})),
            ("washer_remote_maintain", json!({"platform":"switch","unique_id":"$deviceid-washer-remote-maintain","state_topic":"$this/washer/remote_maintain","command_topic":"$this/washer/remote_maintain/set","payload_on":"ON","payload_off":"OFF","name":"Washer keep remote start","icon":"mdi:remote","availability":[{"topic":"$this/washer/power","payload_available":"ON","payload_not_available":"OFF"}]})),
            ("dryer_door", json!({"platform":"binary_sensor","unique_id":"$deviceid-dryer-door","state_topic":"$this/dryer/door","device_class":"door","payload_on":DOOR_OPEN,"payload_off":DOOR_CLOSE,"name":"Dryer door"})),
            ("dryer_child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-dryer-child-lock","state_topic":"$this/dryer/child_lock","payload_on":"ON","payload_off":"OFF","name":"Dryer child lock","icon":"mdi:lock-outline"})),
            ("dryer_remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-dryer-remote-start","state_topic":"$this/dryer/remote_start","payload_on":"ON","payload_off":"OFF","name":"Dryer remote start","icon":"mdi:remote"})),
            ("dryer_remote_maintain", json!({"platform":"switch","unique_id":"$deviceid-dryer-remote-maintain","state_topic":"$this/dryer/remote_maintain","command_topic":"$this/dryer/remote_maintain/set","payload_on":"ON","payload_off":"OFF","name":"Dryer keep remote start","icon":"mdi:remote","availability":[{"topic":"$this/dryer/power","payload_available":"ON","payload_not_available":"OFF"}]})),
            ("dryer_duct_clogging", json!({"platform":"sensor","unique_id":"$deviceid-dryer-duct-clogging","state_topic":"$this/dryer/duct_clogging","name":"Dryer duct clogging","icon":"mdi:pipe-wrench"})),
            ("washer_power", json!({"platform":"switch","unique_id":"$deviceid-washer-power","state_topic":"$this/washer/power","command_topic":"$this/washer/power/set","payload_on":"ON","payload_off":"OFF","optimistic":true,"name":"Washer power","icon":"mdi:washing-machine"})),
            ("dryer_power", json!({"platform":"switch","unique_id":"$deviceid-dryer-power","state_topic":"$this/dryer/power","command_topic":"$this/dryer/power/set","payload_on":"ON","payload_off":"OFF","optimistic":true,"name":"Dryer power","icon":"mdi:tumble-dryer"})),
            ("init_lcd", json!({"platform":"select","unique_id":"$deviceid-init-lcd","state_topic":"$this/shared/init_lcd","command_topic":"$this/shared/init_lcd/set","options":INIT_LCD_THEMES_VALUES,"optimistic":true,"name":"Init LCD","icon":"mdi:image","availability":[{"topic":"$this/washer/power","payload_available":"ON","payload_not_available":"OFF"}]})),
        ];
        for (k, v) in comps { components.insert((*k).into(), v.clone()); }
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

    fn process_state_block(&self, block: &[u8]) {
        if block.len() != STATE_BLOCK_LENGTH { return; }
        self.core.publish_property("washer/soil_wash", washer_soil_wash(block[3]).into());
        self.core.publish_property("washer/temp", washer_temps(block[4]).into());
        self.core.publish_property("washer/rinse", washer_rinse(block[5]).into());
        self.core.publish_property("washer/spin", washer_spin(block[6]).into());
        self.core.publish_property("washer/course", washer_courses(block[7]).into());
        self.core.publish_property("washer/soak", washer_soak(block[9]).into());
        self.core.publish_property("washer/water_level", washer_water_level(block[11]).into());
        self.core.publish_property("washer/load_item", washer_load_item(block[12]).into());
        self.core.publish_property("washer/reserve_time", be_u16(block, 13).into());
        self.core.publish_property("washer/remaining_time", be_u16(block, 15).into());
        self.core.publish_property("washer/initial_time", be_u16(block, 17).into());
        self.core.publish_property("washer/energy", be_u16(block, 19).into());
        self.core.publish_property("washer/load_level", washer_load_level(block[26]).into());
        self.core.publish_property("washer/rinse_count", washer_rinse_count(block[29]).into());
        self.core.publish_property("washer/laundry_texture", (block[43] as i64).into());
        self.core.publish_property("shared/init_lcd", init_lcd_themes(block[48]).into());
        let washer_state = block[23];
        self.core.publish_on_off("washer/power", washer_state != 0);
        self.core.publish_property("washer/state", washer_states(washer_state).into());
        self.core.publish_property("washer/error", washer_errors(block[21]).into());
        self.core.publish_property("washer/buzzer", device_buzzer(block[31]).into());
        self.core.publish_on_off("washer/add_garment", block[39] & 0x80 != 0);
        self.core.publish_on_off("washer/child_lock", block[39] & 0x20 != 0);
        self.core.publish_on_off("washer/remote_start", block[39] & 0x10 != 0);
        self.core.publish_on_off("washer/door_lock", block[40] & 0x01 != 0);
        self.core.publish_on_off("washer/remote_maintain", block[42] & 0x04 != 0);
        self.core.publish_property("dryer/dry_level", dryer_dry_levels(block[54]).into());
        self.core.publish_property("dryer/temp", dryer_temp(block[56]).into());
        self.core.publish_property("dryer/time_dry", dryer_time_dry(block[57]).into());
        self.core.publish_property("dryer/course", dryer_courses(block[58]).into());
        self.core.publish_property("dryer/reserve_time", be_u16(block, 60).into());
        self.core.publish_property("dryer/remaining_time", be_u16(block, 62).into());
        self.core.publish_property("dryer/initial_time", be_u16(block, 64).into());
        let dryer_state = block[66];
        self.core.publish_on_off("dryer/power", dryer_state != 0);
        self.core.publish_property("dryer/state", dryer_states(dryer_state).into());
        self.core.publish_property("dryer/error", dryer_errors(block[68]).into());
        self.core.publish_property("dryer/buzzer", device_buzzer(block[72]).into());
        self.core.publish_property("dryer/duct_clogging", dryer_duct_clogging(block[75]).into());
        self.core.publish_on_off("dryer/remote_start", block[79] & 0x40 != 0);
        self.core.publish_on_off("dryer/child_lock", block[79] & 0x10 != 0);
        self.core.publish_on_off("dryer/remote_maintain", block[80] & 0x02 != 0);
    }

    fn process_state_resync(&self, body: &[u8]) {
        if body.len() != STATE_BLOCK_LENGTH { return; }
        self.process_state_block(body);
    }
    fn process_washer_update(&self, body: &[u8]) {
        if body.len() != 48 { return; }
        self.core.publish_property("washer/door", if body[5] != 0 { DOOR_CLOSE } else { DOOR_OPEN }.into());
    }
    fn process_dryer_update(&self, body: &[u8]) {
        if body.len() != 60 { return; }
        self.core.publish_property("dryer/door", if body[16] != 0 { DOOR_OPEN } else { DOOR_CLOSE }.into());
    }
    fn process_status_update(&self, body: &[u8]) {
        if body.len() != STATE_BLOCK_LENGTH * 2 { return; }
        self.process_state_block(&body[STATE_BLOCK_LENGTH..]);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < FIXED_HEADER_LENGTH + 1 { return; }
        let body = &buf[FIXED_HEADER_LENGTH..buf.len()-1];
        match buf[3] {
            0x42 => self.process_washer_update(body),
            0x4e => self.process_dryer_update(body),
            0x71 => self.process_state_resync(body),
            0xd0 => self.process_status_update(body),
            _ => {}
        }
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        if prop == "washer/power" {
            let on = if value == "ON" { 0x01 } else { 0x00 };
            self.core.send(&[0xf0, 0xe5, 0x00, 0x02, 0x01, WASHER_UNIT, 0x01, 0x02, on]);
        } else if prop == "dryer/power" {
            let on = if value == "ON" { 0x01 } else { 0x00 };
            self.core.send(&[0xf0, 0xe5, 0x00, 0x02, 0x01, DRYER_UNIT, 0x01, 0x02, on]);
        } else if prop == "washer/buzzer" {
            if let Some(idx) = buzzer_index(value) {
                self.core.send(&[0xf0, 0xe5, 0x00, 0x02, 0x01, WASHER_UNIT, 0x01, 0x13, idx]);
            }
        } else if prop == "dryer/buzzer" {
            if let Some(idx) = buzzer_index(value) {
                self.core.send(&[0xf0, 0xe5, 0x00, 0x02, 0x01, DRYER_UNIT, 0x01, 0x13, idx]);
            }
        } else if prop == "washer/remote_maintain" {
            let on = if value == "ON" { 0x01 } else { 0x00 };
            self.core.send(&[0xf0, 0x24, 0x10, 0x01, on, WASHER_UNIT]);
        } else if prop == "dryer/remote_maintain" {
            let on = if value == "ON" { 0x01 } else { 0x00 };
            self.core.send(&[0xf0, 0x24, 0x10, 0x01, on, DRYER_UNIT]);
        } else if prop == "shared/init_lcd" {
            if let Some(idx) = init_lcd_index(value) {
                self.core.send(&[0xf0, 0xe5, 0x00, 0x02, 0x01, WASHER_UNIT, 0x01, 0x51, idx]);
            }
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str { &self.core.id }
    fn start(&self) {
        self.core.publish_property("washer/door", DOOR_CLOSE.into());
        self.core.publish_property("dryer/door", DOOR_CLOSE.into());
        crate::devices::washer_ctrl::request_status(&self.core);
    }
    fn drop_device(&self) { self.core.drop_device(); }
    fn set_property(&self, prop: &str, value: &str) { Device::set_property(self, prop, value); }
    fn publish_config(&self) {
        self.core.republish_config();
    }
}

pub fn create(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{Thinq2Device, hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Metadata};
    use crate::device_trait::DeviceHandler;

    const DEVICE_ID: &str = "test-id";
    const STATUS_WASHER_RUNNING: &str = "aad0360a00d0008542000100ec00be013200030e0e0e1600000000000000002d002d00000016010000070000020f0202022d2d00000000000c380000000000070401002a000000000000000000003c00000001000200000200000000201800810700000000070000000000000000013200030e0e0e1600000000000000002d002d000000160b0100070000020f0202022d2d00000010000c380000000000070401002a000000000000000000003c0000000100020000020000000020180081070000000007000000000000000023a9bb";
    const STATE_RESYNC: &str = "aa71360a00710085f3000100eb005f013200030e0e0e1600000000000000002a002d000200160b2600070000020f0202022d2d00000010010c380000000000060401002a000000000000000000000000000001000200000000000000000000810700000000060000000000000000d05dbb";
    const WASHER_DOOR_OPEN: &str = "aa42360a0042007d83000201030007100c010b1000330105002557544c5f4658555f4244565f4e415f30310000000102d71c0b8b010700000000000000000018babb";
    const WASHER_DOOR_CLOSE: &str = "aa42360a0042007d84000201030007100c010b1001330105002557544c5f4658555f4244565f4e415f30310000000102d51c0b8b0107000000000000000000c4cebb";
    const DRYER_DOOR_OPEN: &str = "aa4e360a004e007d850002010300130a0a01040a000021ff000000000000000105340105002557544c5f4658555f4244565f4e415f30310000000102d81c0b8b0107000000000000000000b590bb";
    const DRYER_DOOR_CLOSE: &str = "aa4e360a004e007d860002010300130a0a01040a00002200000000000000000005340105002557544c5f4658555f4244565f4e415f30310000000102d71c0b8b0107000000000000000000b490bb";

    fn meta() -> Metadata { Metadata::new("WTL_FXU_BDV_NA_01", "WKEX200HBA", "1.0") }
    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    #[test]
    fn config() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        for c in ["washer_state","washer_power","washer_door","dryer_state","dryer_power","init_lcd"] {
            assert!(comps.contains_key(c), "missing {c}");
        }
        let opts = comps["init_lcd"]["options"].as_array().unwrap();
        assert!(opts.iter().any(|v| v == "Default"));
        assert!(opts.iter().any(|v| v == "Christmas"));
    }

    #[test]
    fn status_resync_doors_writes() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(STATUS_WASHER_RUNNING));
        assert_eq!(prop(&ha, "washer/power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "washer/state").as_deref(), Some("RUNNING"));
        assert_eq!(prop(&ha, "washer/course").as_deref(), Some("DELICATES"));
        assert_eq!(prop(&ha, "washer/temp").as_deref(), Some("N/A"));
        assert_eq!(prop(&ha, "washer/remaining_time").as_deref(), Some("45"));
        assert_eq!(prop(&ha, "shared/init_lcd").as_deref(), Some("Summer 2"));

        thinq.emit_data(&hex_decode(STATE_RESYNC));
        assert_eq!(prop(&ha, "washer/power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "washer/course").as_deref(), Some("DELICATES"));

        thinq.emit_data(&hex_decode(WASHER_DOOR_OPEN));
        assert_eq!(prop(&ha, "washer/door").as_deref(), Some("OPEN"));
        thinq.emit_data(&hex_decode(WASHER_DOOR_CLOSE));
        assert_eq!(prop(&ha, "washer/door").as_deref(), Some("CLOSE"));
        thinq.emit_data(&hex_decode(DRYER_DOOR_OPEN));
        assert_eq!(prop(&ha, "dryer/door").as_deref(), Some("OPEN"));
        thinq.emit_data(&hex_decode(DRYER_DOOR_CLOSE));
        assert_eq!(prop(&ha, "dryer/door").as_deref(), Some("CLOSE"));

        thinq.reset_recorder();
        dev.start();
        assert_eq!(prop(&ha, "washer/door").as_deref(), Some("CLOSE"));
        assert_eq!(prop(&ha, "dryer/door").as_deref(), Some("CLOSE"));
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1121010000001800B5BB");

        thinq.reset_recorder();
        dev.set_property("washer/power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0DF0E50002013301020193BB");
        thinq.reset_recorder();
        dev.set_property("washer/power", "OFF");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0DF0E50002013301020090BB");
        thinq.reset_recorder();
        dev.set_property("dryer/power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0DF0E50002013401020192BB");
        thinq.reset_recorder();
        dev.set_property("shared/init_lcd", "Default");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0DF0E50002013301510041BB");
        thinq.reset_recorder();
        dev.set_property("washer/buzzer", "Low");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0DF0E50002013301130182BB");
        thinq.reset_recorder();
        dev.set_property("washer/remote_maintain", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0AF0241001013358BB");
    }
}
