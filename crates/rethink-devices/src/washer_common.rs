//! Shared washer course/state/error tables.

pub const ERRORS: &[Option<&str>] = &[
    Some("OK"),
    Some("Door lock error (DE2)"),
    Some("Door open error (DE1)"),
    Some("Water supply error (IE)"),
    Some("Water drain error (OE)"),
    Some("Out of balance error (UE)"),
    Some("Overfill error (FE)"),
    Some("Water level sensor error (PE)"),
    Some("Temperature sensor error (TE)"),
    Some("Locked motor error (LE)"),
    None,
    Some("Unknown error (dHE)"),
    Some("Power fail error (PF)"),
    Some("Unknown error (FF)"),
    Some("Unknown error (DCE)"),
    Some("Unknown error (AE)"),
    Some("EEPROM error"),
    Some("Unknown error (PS)"),
    Some("Door sensor error (DE4)"),
    Some("Vibration sensor error (VS)"),
    Some("Unknown error (LE8)"),
    Some("Unknown error (LE9)"),
    Some("Unknown error (ED1)"),
    Some("Unknown error (ED2)"),
    Some("Unknown error (ED3)"),
    Some("Unknown error (ED4)"),
    Some("Unknown error (ED5)"),
];

pub const STATES: &[Option<&str>] = &[
    Some("Off"),
    Some("Ready"),
    Some("Paused"),
    Some("Delayed"),
    Some("Measuring"),
    Some("Pre-wash"),
    Some("Washing"),
    Some("Rinsing"),
    Some("Spinning"),
    Some("Drying"),
    Some("End"),
    Some("Cooling"),
    Some("Rinse hold"),
    None,
    Some("Refreshing"),
    Some("Steam softening"),
    Some("Demo"),
    None,
    Some("Error"),
    Some("Auto DT Open Pause"),
];

pub fn course_name(code: u32) -> Option<&'static str> {
    match code {
        0x1 => Some("Cotton"),
        0x2 => Some("Ease Care"),
        0x4 => Some("Eco 40-60"),
        0x5 => Some("Duvet"),
        0x7 => Some("Mix"),
        0x8 => Some("Sports Wear"),
        0x9 => Some("Night Wash"),
        0xb => Some("Gentle Care"),
        0xc => Some("Quick 14"),
        0xd => Some("Steam refresh"),
        0xe => Some("Rinse + Spin"),
        0x12 => Some("Drum Clean"),
        0x13 => Some("Wash + Dry"),
        0x17 => Some("Spin + Drain"),
        0x18 => Some("Drying"),
        0x1b => Some("Wool"),
        0x20 => Some("Delicate"),
        0x22 => Some("Quick 30"),
        0x24 => Some("Direct Wear"),
        0x2d => Some("Allergy Care"),
        0x2c => Some("Baby Steam Care"),
        0x31 => Some("TurboWash 39"),
        0x32 => Some("TurboWash 59"),
        0x33 => Some("Baby Clothes"),
        0x34 => Some("Children Clothing"),
        0x35 => Some("School Uniform"),
        0x36 => Some("Swimwear"),
        0x37 => Some("Rainy Season Care"),
        0x38 => Some("Lightly Soiled Refresh"),
        0x39 => Some("Denim"),
        0x3a => Some("Bedding"),
        0x3b => Some("Sweat Stains"),
        0x3e => Some("Single Garments"),
        0x40 => Some("Overnight"),
        0x42 => Some("Fast Wash + Dry"),
        0x45 => Some("Turbo drying"),
        0x46 => Some("Drying shirts"),
        0x48 => Some("Sanitary"),
        0x49 => Some("Small Load"),
        0x4a => Some("Delicate Dresses"),
        0x4b => Some("Wool"),
        0x4d => Some("Cold Wash"),
        0x64 => Some("Rinse + Spin"),
        0x66 => Some("Powder Residue"),
        0x6b => Some("Cuffs + Collars"),
        0x6c => Some("Juice + Food Stains"),
        0x6e => Some("Saving Time"),
        0x6f => Some("Reducing Wrinkles"),
        _ => None,
    }
}

pub const TEMPERATURE_LABELS: &[&str] = &[
    "Not selected",
    "Cold",
    "20 °C",
    "30 °C",
    "40 °C",
    "50 °C",
    "60 °C",
    "95 °C",
];

pub const SPIN_LABELS: &[&str] = &[
    "Not selected",
    "No spin",
    "400 rpm",
    "600 rpm",
    "700 rpm",
    "800 rpm",
    "900 rpm",
    "1000 rpm",
    "1100 rpm",
    "1200 rpm",
    "1400 rpm",
    "1600 rpm",
];

pub const SOIL_LABELS: &[&str] = &["Not selected", "TurboWash", "Time save", "Normal", "Intensive"];

pub fn drying_mode(code: u32) -> Option<&'static str> {
    match code {
        0x0 => Some("Off"),
        0x2 => Some("Auto"),
        0x3 => Some("00:30"),
        0x4 => Some("01:00"),
        0x5 => Some("01:30"),
        0x6 => Some("02:00"),
        0x7 => Some("02:30"),
        0xa => Some("Iron"),
        0xb => Some("Delicate"),
        0xc => Some("Eco"),
        _ => None,
    }
}

pub const DOSES: &[&str] = &["Off", "Low", "Medium", "High"];

pub fn error_message(code: usize) -> &'static str {
    ERRORS.get(code).and_then(|x| *x).unwrap_or("unknown")
}

pub fn state_name(code: usize) -> &'static str {
    STATES.get(code).and_then(|x| *x).unwrap_or("unknown")
}

pub fn temperature_value(code: usize) -> Option<&'static str> {
    TEMPERATURE_LABELS.get(code).copied()
}

pub fn spin_value(code: usize) -> Option<&'static str> {
    if code == 255 {
        return Some("Max");
    }
    SPIN_LABELS.get(code).copied()
}

pub fn soil_level(code: usize) -> Option<&'static str> {
    SOIL_LABELS.get(code).copied()
}

pub fn spin_options() -> Vec<&'static str> {
    let mut options = SPIN_LABELS.to_vec();
    options.push("Max");
    options
}

/// Non-None STATE labels for HA enum options.
pub fn state_options() -> Vec<&'static str> {
    STATES.iter().filter_map(|s| *s).collect()
}

/// Non-None ERROR labels for HA enum options.
pub fn error_options() -> Vec<&'static str> {
    ERRORS.iter().filter_map(|s| *s).collect()
}

/// HA discovery shared by F_/Y_ AABB washers (power/start/pause + status/error + course/temp/spin + times).
/// Callers insert extras (energy, drying_mode, option bits) on top.
pub fn fy_base_components() -> Vec<(String, serde_json::Value)> {
    use serde_json::json;
    vec![
        ("power".into(), json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"","icon":"mdi:washing-machine"})),
        ("start".into(), json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
        ("pause".into(), json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
        ("status".into(), json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":state_options()})),
        ("error".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-error","state_topic":"$this/error","name":"Error","icon":"mdi:check-circle","device_class":"problem","entity_category":"diagnostic"})),
        ("error_message".into(), json!({"platform":"sensor","unique_id":"$deviceid-error-message","state_topic":"$this/error_message","name":"Error message","icon":"mdi:alert-circle-outline","device_class":"enum","entity_category":"diagnostic","options":error_options()})),
        ("course".into(), json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
        ("temp".into(), json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","device_class":"enum","options":TEMPERATURE_LABELS})),
        ("spin".into(), json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","device_class":"enum","options":spin_options()})),
        ("remote_start".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:play-circle-outline"})),
        ("door_lock".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","device_class":"lock"})),
        ("initial_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","device_class":"duration","unit_of_measurement":"min","name":"Initial time"})),
        ("remaining_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","device_class":"duration","unit_of_measurement":"min","name":"Remaining time"})),
    ]
}

/// Merge component pairs into a discovery document.
pub fn install_components(
    base: &mut rethink_core::DeviceDiscovery,
    items: impl IntoIterator<Item = (impl Into<String>, serde_json::Value)>,
) {
    for (k, v) in items {
        base.components.insert(k.into(), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_and_error_lookups() {
        assert_eq!(state_name(0), "Off");
        assert_eq!(state_name(1), "Ready");
        assert_eq!(state_name(99), "unknown");
        assert_eq!(error_message(0), "OK");
        assert_eq!(error_message(1), "Door lock error (DE2)");
        assert_eq!(error_message(999), "unknown");
    }

    #[test]
    fn temp_and_spin_tables() {
        assert_eq!(temperature_value(4), Some("40 °C"));
        assert_eq!(temperature_value(1), Some("Cold"));
        assert_eq!(temperature_value(99), None);
        assert_eq!(spin_value(10), Some("1400 rpm"));
        assert_eq!(spin_value(3), Some("600 rpm"));
        assert_eq!(spin_value(255), Some("Max"));
        assert_eq!(soil_level(4), Some("Intensive"));
        assert_eq!(course_name(0x1), Some("Cotton"));
        assert_eq!(course_name(0xffff), None);
    }

    #[test]
    fn fy_base_has_control_and_status() {
        let keys: Vec<_> = fy_base_components().into_iter().map(|(k, _)| k).collect();
        for k in [
            "power",
            "start",
            "pause",
            "status",
            "error",
            "course",
            "temp",
            "spin",
            "remaining_time",
        ] {
            assert!(keys.contains(&k.to_string()), "missing {k}");
        }
        assert_eq!(state_options().first().copied(), Some("Off"));
        assert!(!error_options().is_empty());
    }
}
