//! VIC2_799_US_FACTORY — LG WM3500CW ThinQ2 front-load washer (AABB, read-only).
//!
//! Port of upstream PR #336. Extends `f3l7cyk5w_us_wifi` with this model's
//! course table, downloaded cycle naming (`rec[21]`), error codes (`rec[7]`),
//! and Fresh Care (`rec[16]` bit `0x01`).

use crate::device_trait::DeviceHandler;
use crate::devices::f3l7cyk5w_us_wifi::{self, VariantHooks};
use rethink_core::device_base::AabbDeviceCore;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const COURSE_OFFSET: usize = 6;
const ERROR_OFFSET: usize = 7;
const OPT2_OFFSET: usize = 16;
const OPT2_FRESH_CARE: u8 = 0x01;
const DOWNLOADED_ID_OFFSET: usize = 21;
const COURSE_DOWNLOADED: u8 = 0x0a;

const COURSE: &[(u8, &str)] = &[
    (0x01, "Tub Clean"),
    (0x02, "Bright Whites"),
    (0x03, "Bedding"),
    (0x04, "Heavy Duty"),
    (0x05, "Normal"),
    (0x06, "Perm Press"),
    (0x07, "Delicates"),
    (0x08, "Towels"),
    (0x09, "Speed Wash"),
    (COURSE_DOWNLOADED, "Downloaded"),
];

const DOWNLOADED: &[(u8, &str)] = &[(0x33, "Small Load"), (0x37, "Denim"), (0x6a, "Econo Wash")];

const ERRORS: &[(u8, &str)] = &[
    (0, "OK"),
    (1, "Door open error (dE2)"),
    (2, "Water supply error (IE)"),
    (3, "Water drain error (OE)"),
    (4, "Out of balance error (UE)"),
    (5, "Overfill error (FE)"),
    (6, "Water pressure error (PE)"),
    (7, "Thermistor error (tE)"),
    (8, "BLDC error (LE)"),
    (9, "Unknown error (CE)"),
    (10, "SVC call (dHE)"),
    (11, "Power failure error (PF)"),
    (12, "Freeze error (FF)"),
    (13, "Unknown error (dCE)"),
    (14, "Unknown error (AE)"),
    (15, "EEPROM error (EE)"),
    (16, "Unknown error (sud)"),
    (17, "Door open error (DE1)"),
    (18, "Sliding Lid Open error (LOE)"),
    (19, "Unknown error (PS)"),
];

fn named(table: &[(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

fn extra_components(comps: &mut Map<String, Value>) {
    let error_options: Vec<&str> = ERRORS.iter().map(|(_, n)| *n).collect();
    comps.insert(
        "error".into(),
        json!({
            "platform": "binary_sensor",
            "unique_id": "$deviceid-error",
            "state_topic": "$this/error",
            "name": "Error",
            "icon": "mdi:check-circle",
            "device_class": "problem",
            "entity_category": "diagnostic",
        }),
    );
    comps.insert(
        "error_message".into(),
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-error-message",
            "state_topic": "$this/error_message",
            "name": "Error message",
            "icon": "mdi:alert-circle-outline",
            "device_class": "enum",
            "entity_category": "diagnostic",
            "options": error_options,
        }),
    );
    comps.insert(
        "fresh_care".into(),
        json!({
            "platform": "binary_sensor",
            "unique_id": "$deviceid-fresh_care",
            "state_topic": "$this/fresh_care",
            "name": "Fresh Care",
            "icon": "mdi:fan",
        }),
    );
}

fn course(rec: &[u8]) -> Option<String> {
    let code = rec[COURSE_OFFSET];
    if code != COURSE_DOWNLOADED {
        return named(COURSE, code).map(str::to_string);
    }
    Some(match named(DOWNLOADED, rec[DOWNLOADED_ID_OFFSET]) {
        Some(name) => format!("Downloaded ({name})"),
        None => "Downloaded".to_string(),
    })
}

fn on_record(core: &AabbDeviceCore, rec: &[u8]) {
    let error = rec[ERROR_OFFSET];
    core.publish_property("error_message", named(ERRORS, error).unwrap_or("None").into());
    core.publish_on_off("error", error != 0);
    core.publish_on_off("fresh_care", rec[OPT2_OFFSET] & OPT2_FRESH_CARE != 0);
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    f3l7cyk5w_us_wifi::Device::with_hooks(
        ha,
        thinq,
        meta,
        Some(VariantHooks {
            extra_components,
            course_fn: course,
            on_record,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const DOOR_OPEN_ERROR: &str =
        "aa3a20ec001805012301230a00030502020000000010000000006a0b0000001805012301230a11030502020000000010000000006a0b000054bb";
    const DOWNLOADED_DENIM: &str =
        "aa3a20ec001805000000000000000000000000000000000000006a0b0000001805010301030a0003030202000000000000000000370b060056bb";
    const FRESH_CARE_ON: &str =
        "aa3a20ec00180501010101080003050403000000000000000700332c000000180501010101080003050403000000000100000700332c000078bb";
    const BRIGHT_WHITES: &str =
        "aa3a20ec001805011d011d010000030002000000040000000700332c0000001805010c010c020003050603000000000000000700332c00003cbb";

    fn feed(frame: &str) -> Arc<MockHaConnection> {
        let meta = Metadata::new("VIC2_799_US_FACTORY", "VIC2_799_US_FACTORY", "0.0.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let _dev = create(ha.clone(), thinq.clone(), meta);
        thinq.emit_data(&hex_decode(frame));
        ha
    }

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_errors_downloaded_fresh_care_and_courses() {
        let ha = feed(DOOR_OPEN_ERROR);
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("Door open error (DE1)"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Downloaded (Econo Wash)"));

        assert_eq!(prop(&feed(DOWNLOADED_DENIM), "course").as_deref(), Some("Downloaded (Denim)"));

        let ha = feed(FRESH_CARE_ON);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Towels"));
        assert_eq!(prop(&ha, "fresh_care").as_deref(), Some("ON"));

        let ha = feed(BRIGHT_WHITES);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Bright Whites"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "fresh_care").as_deref(), Some("OFF"));
    }
}
