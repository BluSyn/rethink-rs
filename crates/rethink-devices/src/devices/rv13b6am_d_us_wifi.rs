//! RV13B6AM_D_US_WIFI — LG DLE3500W electric dryer (AABB, read-only).
//!
//! Port of upstream PR #336. Extends `rv13b6es_d_us_wifi` with this model's
//! course table, downloaded cycle naming (`rec[21]`), Time Dry setting
//! (`rec[10]`), and Flow Sense (`rec[22]`), while omitting panel options this
//! model lacks (`reduce_static`, `damp_dry_signal`, `turbo_steam`, `load_item`).

use crate::device_trait::DeviceHandler;
use crate::devices::rv13b6es_d_us_wifi::{self, VariantHooks};
use rethink_core::device_base::AabbDeviceCore;
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const COURSE_OFFSET: usize = 6;
const TIME_DRY_SETTING_OFFSET: usize = 10;
const DOWNLOADED_ID_OFFSET: usize = 21;
const FLOW_SENSE_OFFSET: usize = 22;
const COURSE_TIME_DRY: u8 = 0x12;

const OMITTED: &[&str] = &["reduce_static", "damp_dry_signal", "turbo_steam", "load_item"];

const COURSE: &[(u8, &str)] = &[
    (0x01, "Heavy Duty"),
    (0x02, "Towels"),
    (0x03, "Normal"),
    (0x04, "Perm Press"),
    (0x05, "Delicates"),
    (0x07, "Bedding"),
    (0x09, "Small Load"),
    (0x10, "Speed Dry"),
    (0x11, "Air Dry"),
    (0x12, "Time Dry"),
    (0x13, "Rack Dry"),
];

const DOWNLOADED: &[(u8, &str)] = &[(100, "Super Dry"), (107, "Gym Clothes")];

fn named(table: &[(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

fn extra_components(comps: &mut HashMap<String, Value>) {
    comps.insert(
        "time_dry_setting".into(),
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-time_dry_setting",
            "state_topic": "$this/time_dry_setting",
            "name": "Time Dry setting",
            "icon": "mdi:timer-cog-outline",
            "unit_of_measurement": "min",
            "entity_category": "diagnostic",
        }),
    );
    comps.insert(
        "flow_sense".into(),
        json!({
            "platform": "binary_sensor",
            "unique_id": "$deviceid-flow_sense",
            "state_topic": "$this/flow_sense",
            "name": "Flow Sense",
            "icon": "mdi:air-filter",
            "device_class": "problem",
        }),
    );
}

fn course(rec: &[u8]) -> Option<String> {
    let downloaded = rec[DOWNLOADED_ID_OFFSET];
    if downloaded == 0 {
        return named(COURSE, rec[COURSE_OFFSET]).map(str::to_string);
    }
    Some(match named(DOWNLOADED, downloaded) {
        Some(name) => format!("Downloaded ({name})"),
        None => "Downloaded".to_string(),
    })
}

fn on_record(core: &AabbDeviceCore, rec: &[u8]) {
    let time_dry = rec[TIME_DRY_SETTING_OFFSET];
    let mins = if rec[COURSE_OFFSET] == COURSE_TIME_DRY && (1..=5).contains(&time_dry) {
        i64::from(time_dry + 1) * 10
    } else {
        0
    };
    core.publish_property("time_dry_setting", PropertyValue::Int(mins));
    core.publish_on_off("flow_sense", rec[FLOW_SENSE_OFFSET] != 0);
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    rv13b6es_d_us_wifi::Device::with_hooks(
        ha,
        thinq,
        meta,
        Some(VariantHooks {
            extra_components,
            course_fn: course,
            omitted: OMITTED,
            on_record,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const DOWNLOADED_GYM_CLOTHES: &str =
        "aa4030ec001b01003b003b1a000505000400000040a80000010064000064000000001b01001b001b0b000305000400000040a8000001006b00006b000000ccbb";
    const RACK_DRY: &str =
        "aa4030ec001b010029002903000303000400000040a80000000000000064000000001b010032003213000000000400000040a80000000000000064000000e5bb";
    const SMALL_LOAD: &str =
        "aa4030ec001b010037003702000303000400000040a80000000000000064000000001b01001e001e09000305000400000040a80000000000000064000000f4bb";
    const TIME_DRY_40: &str =
        "aa4030ec001b010028002812000005030400000040a80000000000000064000000001b320028002812000005030400000000a80000000100000064000000f1bb";
    const WRINKLE_CARE: &str =
        "aa4030ec001b040001000b120000050304e2000050a8000087330000006b000000001b3800010001120000050304e2000050a8000087040000006b000000c4bb";
    const FLOW_SENSE_SET: &str =
        "aa4030ec001b32002d010012000005050400000000a90003020100000064000000001b32002d010012000005050400000000a900030a01000100640000003ebb";
    const FLOW_SENSE_CLEARED: &str =
        "aa4030ec001b000001000100000000000400000040a80008330400010064000000001b010001000100000000000400000040a8000833000000006400000009bb";

    fn feed(frames: &[&str]) -> Arc<MockHaConnection> {
        let meta = Metadata::new("RV13B6AM_D_US_WIFI", "RV13B6AM_D_US_WIFI", "0.0.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let _dev = create(ha.clone(), thinq.clone(), meta);
        for f in frames {
            thinq.emit_data(&hex_decode(f));
        }
        ha
    }

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_courses_downloaded_time_dry_wrinkle_and_flow_sense() {
        let ha = feed(&[DOWNLOADED_GYM_CLOTHES]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Downloaded (Gym Clothes)"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("High"));
        for omitted in OMITTED {
            assert!(prop(&ha, omitted).is_none(), "{omitted} should not publish");
        }

        assert_eq!(prop(&feed(&[RACK_DRY]), "course").as_deref(), Some("Rack Dry"));
        assert_eq!(prop(&feed(&[SMALL_LOAD]), "course").as_deref(), Some("Small Load"));

        let ha = feed(&[TIME_DRY_40]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Time Dry"));
        assert_eq!(prop(&ha, "time_dry_setting").as_deref(), Some("40"));
        assert_eq!(prop(&feed(&[RACK_DRY]), "time_dry_setting").as_deref(), Some("0"));

        let ha = feed(&[WRINKLE_CARE]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Wrinkle care"));
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("ON"));

        assert_eq!(prop(&feed(&[FLOW_SENSE_SET]), "flow_sense").as_deref(), Some("ON"));
        assert_eq!(
            prop(&feed(&[FLOW_SENSE_SET, FLOW_SENSE_CLEARED]), "flow_sense").as_deref(),
            Some("OFF")
        );
    }
}
