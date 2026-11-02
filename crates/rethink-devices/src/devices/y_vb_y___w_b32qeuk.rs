//! Y_VB_Y___W.B32QEUK washer (LG F4W1175YW), the ezDispense sibling of Y_V8.
//!
//! Status uses the Y_V8 39-byte current record. Dose levels, millilitre
//! amounts, the download slot, the drawer and the two tank-low alerts are
//! this model's fields. A programme start repeats the last reported dose
//! levels. `delay_end` is withdrawn; reserve time already carries the minutes.
//!
//! The door comes as its own 6-byte `03` record in the same `20 0a` envelope.
//! `F0 25 03 15` loads a downloadable programme into the slot. It carries only
//! the base course and settings, so it works without the cloud, and Set
//! course's Downloaded program position runs whatever the slot holds.

use crate::device_trait::DeviceHandler;
use crate::devices::y_v8_y___w_b32qeuk::{
    smart_course_name, smart_courses, Device as V8, Extension,
};
use crate::js::number;
use crate::washer_common::DOSES;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const EXTRA_COURSES: &[(u8, &str)] = &[(0x37, "Dispenser Clean")];

/// The Set course position that runs the programme in the download slot.
const DOWNLOADED_PROGRAM: &str = "Downloaded program";

/// The LG app's `F0 25` load per downloadable programme: catalogue id; course, soil, spin,
/// temp, rinse and options; then the detergent and softener levels it sends instead of the
/// reported doses.
type Download = (u8, [u8; 6], Option<u8>, Option<u8>);

const DOWNLOADS: &[Download] = &[
    (0x47, [0x01, 0x03, 0x07, 0x06, 0x02, 0x50], None, None), // Baby Care
    (0x3a, [0x05, 0x03, 0x02, 0x01, 0x01, 0x00], None, None), // Blanket
    (0x4d, [0x01, 0x03, 0xff, 0x01, 0x01, 0x00], None, None), // Cold Wash
    (0x3f, [0x20, 0x03, 0x05, 0x02, 0x01, 0x00], None, None), // Colour Protection
    (0x79, [0x17, 0x00, 0x01, 0x00, 0x00, 0x00], Some(0), Some(0)), // Drain
    (0x38, [0x08, 0x03, 0x05, 0x01, 0x01, 0x00], None, None), // Gym Clothes
    (0x48, [0x2d, 0x03, 0xff, 0x06, 0x01, 0x80], None, None), // Hygiene
    (0x39, [0x20, 0x03, 0xff, 0x02, 0x01, 0x00], None, None), // Jeans
    (0x6c, [0x01, 0x03, 0x07, 0x04, 0x01, 0x40], None, None), // Juice and Food Stains
    (0x34, [0x01, 0x03, 0x09, 0x06, 0x01, 0x40], None, None), // Kids Wear
    (0x65, [0x1b, 0x03, 0x02, 0x02, 0x01, 0x00], None, None), // Lightly Soiled Items
    (0x4a, [0x20, 0x03, 0x05, 0x02, 0x01, 0x00], None, None), // Lingerie
    (0x86, [0x3b, 0x03, 0x07, 0x04, 0x01, 0x00], None, None), // MicroPlastic Care
    (0x66, [0x01, 0x03, 0xff, 0x04, 0x02, 0x00], None, None), // Minimize Detergent Residue
    (0x6f, [0x01, 0x03, 0xff, 0x04, 0x01, 0x00], None, None), // Minimize Wrinkles
    (0x71, [0x30, 0x03, 0x02, 0x01, 0x00, 0x00], Some(0), Some(0)), // Quick Tub Clean
    (0x37, [0x01, 0x03, 0xff, 0x04, 0x01, 0x00], None, None), // Rainy Season
    (0x64, [0x0e, 0x03, 0xff, 0x00, 0x01, 0x00], Some(0), None), // Rinse + Spin
    (0x84, [0x09, 0x03, 0x07, 0x04, 0x01, 0x00], None, None), // Silent Wash
    (0x3e, [0x0c, 0x03, 0x02, 0x02, 0x01, 0x01], None, None), // Single Garment
    (0x4c, [0x01, 0x03, 0xff, 0x04, 0x02, 0x10], None, None), // Skin Care
    (0x6b, [0x01, 0x03, 0x07, 0x06, 0x01, 0x40], None, None), // Sleeve Hems and Collars
    (0x49, [0x0c, 0x03, 0x02, 0x02, 0x01, 0x01], None, None), // Small Load
    (0x7a, [0x17, 0x00, 0xff, 0x00, 0x00, 0x00], Some(0), Some(0)), // Spin
    (0x3b, [0x01, 0x03, 0xff, 0x04, 0x01, 0x00], None, None), // Sweat Stain
    (0x36, [0x1b, 0x03, 0x02, 0x02, 0x01, 0x00], None, None), // Swimming Wear
];

fn download(id: u8) -> Option<&'static Download> {
    DOWNLOADS.iter().find(|d| d.0 == id)
}

/// The programmes the slot can be loaded with, in the download catalogue's order.
fn slot_programmes() -> impl Iterator<Item = &'static (u8, &'static str)> {
    smart_courses()
        .iter()
        .filter(|(id, _)| download(*id).is_some())
}

fn slot_name(id: u8) -> &'static str {
    slot_programmes()
        .find(|(c, _)| *c == id)
        .map_or("None", |(_, n)| *n)
}

fn dose_name(code: u8) -> &'static str {
    DOSES.get(code as usize).copied().unwrap_or("None")
}

/// `Number()` then `Number.isInteger()`, as upstream. A blank payload is 0, out of range.
fn parse_ez(value: &str) -> Option<u8> {
    let ml = number(value)?;
    (ml.fract() == 0.0 && (9.0..=150.0).contains(&ml)).then_some(ml as u8)
}

#[derive(Clone, Copy, Default)]
struct State {
    /// The dispenser settings last reported, which a programme start has to repeat.
    detergent: u8,
    softener: u8,
    dispense_type: u8,
    /// The programme in the download slot, and whether Set course points at it.
    slot: u8,
    run_slot: bool,
}

#[derive(Default)]
struct Ezdispense {
    state: Mutex<State>,
}

impl Ezdispense {
    fn state(&self) -> State {
        *self.state.lock()
    }

    /// `F0 25 03 15`, then the `F0 26` body with its flags byte left at 00.
    fn download_command(&self, dl: &Download) -> [u8; 20] {
        let (id, [course, soil, spin, temp, rinse, options], detergent, softener) = *dl;
        let state = self.state();
        [
            0xf0,
            0x25,
            0x03,
            0x15,
            course,
            soil,
            spin,
            temp,
            rinse,
            0,
            0,
            0,
            0,
            options,
            0,
            0,
            id,
            detergent.unwrap_or(state.detergent),
            softener.unwrap_or(state.softener),
            0,
        ]
    }
}

impl Extension for Ezdispense {
    fn extra_courses(&self) -> &'static [(u8, &'static str)] {
        EXTRA_COURSES
    }

    fn components(&self, components: &mut Map<String, Value>) {
        if let Some(Value::Array(options)) = components
            .get_mut("set_course")
            .and_then(|c| c.get_mut("options"))
        {
            options.push(DOWNLOADED_PROGRAM.into());
        }
        let slot: Vec<&str> = slot_programmes().map(|(_, n)| *n).collect();
        for (key, value) in [
            (
                "downloaded_program",
                json!({"platform":"sensor","unique_id":"$deviceid-downloaded_program","state_topic":"$this/downloaded_program","name":"Downloaded program","icon":"mdi:download"}),
            ),
            (
                "set_downloaded_program",
                json!({"platform":"select","unique_id":"$deviceid-set_downloaded_program","state_topic":"$this/set_downloaded_program","command_topic":"$this/set_downloaded_program/set","name":"Set downloaded program","icon":"mdi:download","options":slot,"entity_category":"config"}),
            ),
            (
                "detergent",
                json!({"platform":"sensor","unique_id":"$deviceid-detergent","state_topic":"$this/detergent","name":"Detergent dose","icon":"mdi:cup","device_class":"enum","options":DOSES}),
            ),
            (
                "softener",
                json!({"platform":"sensor","unique_id":"$deviceid-softener","state_topic":"$this/softener","name":"Softener dose","icon":"mdi:cup-outline","device_class":"enum","options":DOSES}),
            ),
            (
                "ezdispense_detergent",
                json!({"platform":"number","unique_id":"$deviceid-ezdispense_detergent","state_topic":"$this/ezdispense_detergent","command_topic":"$this/ezdispense_detergent/set","name":"ezDispense detergent amount","icon":"mdi:cup","unit_of_measurement":"mL","min":9,"max":150,"step":1,"mode":"box","entity_category":"config"}),
            ),
            (
                "ezdispense_softener",
                json!({"platform":"number","unique_id":"$deviceid-ezdispense_softener","state_topic":"$this/ezdispense_softener","command_topic":"$this/ezdispense_softener/set","name":"ezDispense softener amount","icon":"mdi:cup-outline","unit_of_measurement":"mL","min":9,"max":150,"step":1,"mode":"box","entity_category":"config"}),
            ),
            (
                "door",
                json!({"platform":"binary_sensor","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","device_class":"door"}),
            ),
            (
                "dispenser_drawer",
                json!({"platform":"binary_sensor","unique_id":"$deviceid-dispenser_drawer","state_topic":"$this/dispenser_drawer","name":"Dispenser drawer","device_class":"opening"}),
            ),
            (
                "detergent_refill",
                json!({"platform":"binary_sensor","unique_id":"$deviceid-detergent_refill","state_topic":"$this/detergent_refill","name":"Detergent refill","icon":"mdi:cup","device_class":"problem"}),
            ),
            (
                "softener_refill",
                json!({"platform":"binary_sensor","unique_id":"$deviceid-softener_refill","state_topic":"$this/softener_refill","name":"Softener refill","icon":"mdi:cup-outline","device_class":"problem"}),
            ),
            ("delay_end", json!({"platform":"sensor"})),
        ] {
            components.insert(key.into(), value);
        }
    }

    fn process_aabb(&self, dev: &V8, inner: &[u8]) {
        // 20 0a [len16] [seq16] .. .. .. 03 00 06 [6-byte record], on every door open or close.
        // Its last byte is 00 open, 01 closed; 04 shows up once after power-on, meaning unknown.
        if inner.len() < 19
            || inner[0] != 0x20
            || inner[1] != 0x0a
            || inner[10] != 0x03
            || inner[11..13] != [0x00, 0x06]
        {
            return;
        }
        match inner[18] {
            0x00 => dev.core().publish_on_off("door", true),
            0x01 => dev.core().publish_on_off("door", false),
            _ => {}
        }
    }

    fn process_state(&self, dev: &V8, s: &[u8]) {
        let core = dev.core();
        core.publish_property("downloaded_program", smart_course_name(s[25]).into());
        core.publish_property("set_downloaded_program", slot_name(s[25]).into());
        core.publish_property("detergent", dose_name(s[32]).into());
        core.publish_property("softener", dose_name(s[33]).into());
        core.publish_property("ezdispense_detergent", i64::from(s[34]).into());
        core.publish_property("ezdispense_softener", i64::from(s[35]).into());
        // [37]: bit 2 drawer open; bits 0 and 1 detergent and softener tank low, the
        // softener one reading clear while the drawer is open. Bits 6 and 7 pulse, unknown.
        core.publish_on_off("dispenser_drawer", s[37] & 0x04 != 0);
        core.publish_on_off("detergent_refill", s[37] & 0x01 != 0);
        core.publish_on_off("softener_refill", s[37] & 0x02 != 0);
        let mut state = self.state.lock();
        state.detergent = s[32];
        state.softener = s[33];
        state.dispense_type = s[36];
        state.slot = s[25];
    }

    fn course_setting(&self, dev: &V8) -> Option<&'static str> {
        // also while the washer reports a downloaded programme selected at its own dial
        let selected = dev.pending().course.is_none() && dev.reported_smart_course() != 0;
        (self.state().run_slot || selected).then_some(DOWNLOADED_PROGRAM)
    }

    fn programme_command(&self, dev: &V8, cmd: &mut [u8; 18]) {
        let state = self.state();
        cmd[15] = state.detergent;
        cmd[16] = state.softener;
        cmd[17] = state.dispense_type;
        // The slot's programme runs on the settings it was loaded with, unless changed since.
        let Some(&(_, [course, soil, spin, temp, rinse, options], detergent, softener)) =
            download(state.slot).filter(|_| state.run_slot)
        else {
            return;
        };
        let pending = dev.pending();
        cmd[2] = course;
        cmd[3] = pending.soil.unwrap_or(soil);
        cmd[4] = pending.spin.unwrap_or(spin);
        cmd[5] = pending.temp.unwrap_or(temp);
        cmd[6] = pending.rinse.unwrap_or(rinse);
        cmd[11] = pending.options.unwrap_or(options);
        cmd[14] = state.slot;
        cmd[15] = detergent.unwrap_or(cmd[15]);
        cmd[16] = softener.unwrap_or(cmd[16]);
    }

    fn set_property(&self, dev: &V8, prop: &str, value: &str) -> bool {
        match prop {
            "set_downloaded_program" => {
                let programme = slot_programmes()
                    .find(|(_, n)| *n == value)
                    .and_then(|(id, _)| download(*id));
                if let Some(dl) = programme {
                    dev.core().send(&self.download_command(dl));
                    // loading a programme is picking it to run, as the LG app does
                    self.state.lock().run_slot = true;
                    dev.drop_pending_course();
                    dev.publish_settings();
                }
                true
            }
            // F0 24 <parameter> 01 <value>, in ml per 5 kg of laundry
            "ezdispense_detergent" | "ezdispense_softener" => {
                let parameter = if prop == "ezdispense_detergent" {
                    0x0d
                } else {
                    0x0e
                };
                if let Some(ml) = parse_ez(value) {
                    dev.core().send(&[0xf0, 0x24, parameter, 0x01, ml]);
                }
                true
            }
            "set_course" => {
                let run_slot = value == DOWNLOADED_PROGRAM;
                self.state.lock().run_slot = run_slot;
                if run_slot {
                    dev.drop_pending_course();
                    dev.publish_settings();
                }
                run_slot
            }
            "run_programme" => {
                let state = self.state();
                // Downloaded program with nothing known in the slot runs nothing.
                if !state.run_slot || download(state.slot).is_some() {
                    dev.run_programme();
                    self.state.lock().run_slot = false;
                    dev.publish_settings();
                }
                true
            }
            _ => false,
        }
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    V8::with_extension(ha, thinq, meta, Ezdispense::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{
        hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device, Thinq2Device,
    };
    use rethink_util::crc16::crc16;

    const ID: &str = "test-id";
    // A neutral start with no programme selected, then the same washer powered off.
    const INITIAL: &str = "aaff200a006000f52c000100ec004e00000100000000000000000000000000000000000000001f006400000000000002022a1e00800100000100000000000000000000000000000000000000001f006400000000000002022a1e0000010ee4bb";
    const POWER_OFF: &str = "aaff200a006000f531000100ec004e00000100000000000000000000000000000000000000001f006400000000000002022a1e00000100000000000000000000000000000000000000000001001f006400000000000002022a1e000001605cbb";
    const MEASURING: &str = "aaff200a00600008b5000100ec004e000001000000000000000000000000000000000000000020006400000000012402022a1e004001000004010c010c1200030106010000000042000001010020006400000200000000002a1e0000019db0bb";
    const RUNNING: &str = "aaff200a006000f98f000100ec004e0000060219021b0400030a0401000000004220000104001f006400000200000103012a1e0040010000060219021b0400030a0401000000004220000104001f006400000200000103012a1e00000164afbb";
    const RINSING: &str = "aaff200a00600002da000100ec004e0000070022021b0400030a0001000000004200000106001f00640000020000b903012a1e0000010000070022021b0400030a0001000000004200000106001f00640000020000ba03012a1e000001ce60bb";
    const SPINNING: &str = "aaff200a006000cc44000100ec004e000007001d021b0400030a0001000000004200000106001100640000020000c102002a1e004001000008001c021b0400000a0000000000004200000107001100640000020000c202002a1e000001dc6cbb";
    // A single 0xeb record, laid out like the replacing record of the pairs above.
    const SINGLE: &str = "aaff200a003900c2a6000100eb00270000060216021b0400030a04010000000042200001040011006400000200000602002a1e00400189a4bb";
    // Well formed and correctly checksummed, but message type 0x0a instead of 0x00.
    const UNKNOWN: &str = "aaff200a003900ce8600010ae20027000004032603260400030a04010000000002200001010011006400000200011602002a1e000001dbabbb";
    // The selector turned from Drum Clean to the downloaded Blanket on its Duvet base (0x05):
    // Blanket's catalogue id 0x3a is at [22] (selected download) and [25] (download slot).
    const DUVET: &str = "aaff200a006000e811000100ec004e000001010c010c1200030106010000000000000003000005003a00000200000000002a1e000001000001011c011c0500030201010000000000000001003a05003a00000300000002022a1e000001990cbb";
    // ezDispense amounts at [34]/[35]: 42/30, then 43/30 and 43/31 as the app set each one.
    const EZ_42_30: &str = "aaff200a0060000400000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e004001000001000000000000000000000000000000000000000005003400000000000002022a1e00800151adbb";
    const EZ_43_30: &str = "aaff200a0060000440000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e000001000001000000000000000000000000000000000000000005003400000000000002022b1e0000016e6bbb";
    const EZ_43_31: &str = "aaff200a006000045a000100ec004e000001000000000000000000000000000000000000000005003400000000000002022b1e000001000001000000000000000000000000000000000000000005003400000000000002022b1f000001fda5bb";
    // The drawer opened and shut: [37] bit 2 holds while it is open, bit 6 pulses meanwhile.
    const DRAWER_OPEN: &str = "aaff200a0060004cd1000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e000001000001000000000000000000000000000000000000000005003400000000000002022a1e0004014a1dbb";
    const DRAWER_PULSE_ON: &str = "aaff200a0060004cd4000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e000401000001000000000000000000000000000000000000000005003400000000000002022a1e004401bd20bb";
    const DRAWER_PULSE_OFF: &str = "aaff200a0060004cff000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e004401000001000000000000000000000000000000000000000005003400000000000002022a1e00040183e8bb";
    const DRAWER_CLOSED: &str = "aaff200a0060004d02000100ec004e000001000000000000000000000000000000000000000005003400000000000002022a1e000401000001000000000000000000000000000000000000000005003400000000000002022a1e000001c8b2bb";
    // The softener tank low, in the order the washer sent them: [37] bit 1 set throughout,
    // while bit 6 pulses on and then off.
    const SOFTENER_LOW_PULSE_ON: &str = "aaff200a006000da2d000100ec004e00000100000000000000000000000000000000000000000f003400000000000002022a1e00020100000100000000000000000000000000000000000000000f003400000000000002022a1e0042011011bb";
    const SOFTENER_LOW_PULSE_OFF: &str = "aaff200a006000dab9000100ec004e00000100000000000000000000000000000000000000000f003400000000000002022a1e00420100000100000000000000000000000000000000000000000f003400000000000002022a1e000201e3fbbb";
    // The detergent tank low and the softener tank not: [37] bit 0 set, bit 6 pulsing on.
    const DETERGENT_LOW_PULSE_ON: &str = "aaff200a006000dec6000100ec004e000001032603260000000000000000000000000003000010003400000000000002022a1e000101000001032603260000000000000000000000000003000010003400000000000002022a1e00410121b4bb";
    // Not a capture: an idle Cotton record with detergent MORE ([32] = 3) and softener LESS
    // ([33] = 1), reported from a simulated washer so the LG app's start carried the doses.
    const COTTON_DOSED: &str = "aaff200a0039040081000100eb0027000001032603260100000000000000000002000003000011007100000000000003010000000000557abb";

    const WRITE_INIT: &str = "AA0EF0ED1121010000001800B5BB";
    const WRITE_POWER_ON: &str = "AA08F02A010098BB";
    const WRITE_POWER_OFF: &str = "AA09F0240101009CBB";
    const WRITE_PAUSE: &str = "AA09F02404010099BB";
    const WRITE_START: &str = "AA09F02405010098BB";
    // The LG app setting the detergent amount to 43 ml and the softener amount to 31 ml.
    const WRITE_EZDISPENSE_DETERGENT_43: &str = "AA09F0240D012B55BB";
    const WRITE_EZDISPENSE_SOFTENER_31: &str = "AA09F0240E011FA0BB";
    // The LG app starting Cotton at 40 °C with Rinse+ on COTTON_DOSED, through the bridge.
    const APP_START_COTTON: &str = "AA16F0260103FF04020000000000030000030100B3BB";
    // The LG app starting the downloaded Blanket selected in DUVET.
    const APP_START_BLANKET: &str = "f0260503020101000000000003003a020200";
    // The LG app starting INITIAL's download slot, Rinse + Spin, with nothing selected.
    const APP_START_SLOT_RINSE_SPIN: &str = "f0260e03ff00010000000000030064000200";
    // The F0 25 loads the LG app sent for each downloadable programme, the washer
    // reporting INITIAL's dose levels.
    const APP_DOWNLOADS: &[(&str, &str)] = &[
        ("Baby Care", "f025031501030706020000000050000047020200"),
        ("Blanket", "f02503150503020101000000000000003a020200"),
        ("Cold Wash", "f02503150103ff0101000000000000004d020200"),
        (
            "Colour Protection",
            "f02503152003050201000000000000003f020200",
        ),
        ("Drain", "f025031517000100000000000000000079000000"),
        ("Gym Clothes", "f025031508030501010000000000000038020200"),
        ("Hygiene", "f02503152d03ff06010000000080000048020200"),
        ("Jeans", "f02503152003ff02010000000000000039020200"),
        (
            "Juice and Food Stains",
            "f02503150103070401000000004000006c020200",
        ),
        ("Kids Wear", "f025031501030906010000000040000034020200"),
        (
            "Lightly Soiled Items",
            "f02503151b030202010000000000000065020200",
        ),
        ("Lingerie", "f02503152003050201000000000000004a020200"),
        (
            "MicroPlastic Care",
            "f02503153b030704010000000000000086020200",
        ),
        (
            "Minimize Detergent Residue",
            "f02503150103ff04020000000000000066020200",
        ),
        (
            "Minimize Wrinkles",
            "f02503150103ff0401000000000000006f020200",
        ),
        (
            "Quick Tub Clean",
            "f025031530030201000000000000000071000000",
        ),
        ("Rainy Season", "f02503150103ff04010000000000000037020200"),
        ("Rinse + Spin", "f02503150e03ff00010000000000000064000200"),
        ("Silent Wash", "f025031509030704010000000000000084020200"),
        ("Single Garment", "f02503150c03020201000000000100003e020200"),
        ("Skin Care", "f02503150103ff0402000000001000004c020200"),
        (
            "Sleeve Hems and Collars",
            "f02503150103070601000000004000006b020200",
        ),
        ("Small Load", "f02503150c030202010000000001000049020200"),
        ("Spin", "f02503151700ff0000000000000000007a000000"),
        ("Sweat Stain", "f02503150103ff0401000000000000003b020200"),
        ("Swimming Wear", "f02503151b030202010000000000000036020200"),
    ];
    // Door pushes from a Y_VB_Y___W.B32QEUK-FL: closed, open, and the 04 seen after power-on.
    const DOOR_CLOSED: &str = "AAFF200A00180044B4000101030006100A01021001723CBB";
    const DOOR_OPEN: &str = "AAFF200A00180044B6000101030006100A01021000E8DBBB";
    const DOOR_UNKNOWN: &str = "AAFF200A0018004311000101030006110A01021104FC08BB";

    fn meta() -> Metadata {
        Metadata::new("Y_VB_Y___W.B32QEUK", "Y_VB_Y___W.B32QEUK", "2.11.207")
    }

    fn make() -> (
        Arc<MockHaConnection>,
        Arc<MockThinq2Device>,
        Arc<dyn DeviceHandler>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = create(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn feed(
        frames: &[&str],
    ) -> (
        Arc<MockHaConnection>,
        Arc<MockThinq2Device>,
        Arc<dyn DeviceHandler>,
    ) {
        let (ha, thinq, dev) = make();
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    fn outbox_hex(thinq: &MockThinq2Device) -> Vec<String> {
        thinq.outbox().iter().map(|p| hex_encode(p)).collect()
    }

    fn inner_hex(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .outbox()
            .iter()
            .map(|p| hex_encode(&p[2..p.len() - 2]).to_lowercase())
            .collect()
    }

    /// Rewrites one byte of a single-record (0xeb) status frame, fixing up its CRC16.
    fn with_record_byte(frame: &str, offset: usize, value: u8) -> Vec<u8> {
        let mut copy = hex_decode(frame);
        copy[2 + 13 + offset] = value;
        let crc_at = copy.len() - 3;
        let crc = crc16(&copy[..crc_at]);
        copy[crc_at..crc_at + 2].copy_from_slice(&crc.to_be_bytes());
        copy
    }

    #[test]
    fn initial_state_neutral_start_no_program_selected() {
        let (ha, _, _) = feed(&[INITIAL]);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Standby");
        expect(&ha, "error", "OFF");
        expect(&ha, "error_message", "OK");
        expect(&ha, "course", "Not selected");
        expect(&ha, "spin", "Not selected");
        expect(&ha, "temp", "Not selected");
        expect(&ha, "rinse", "Not selected");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "initial_time", "0");
        expect(&ha, "cycles", "31");
        expect(&ha, "energy", "0");
        expect(&ha, "reserve_time", "0");
        // Lock byte 0x00: door and child lock ON, which is unlocked for HA's lock class.
        expect(&ha, "remote_start", "OFF");
        expect(&ha, "door_lock", "ON");
        expect(&ha, "child_lock", "ON");
        expect(&ha, "detergent", "Medium");
        expect(&ha, "softener", "Medium");
        expect(&ha, "turbowash", "OFF");
        expect(&ha, "prewash", "OFF");
        expect(&ha, "steam", "OFF");
        // The download slot's factory content.
        expect(&ha, "downloaded_program", "Rinse + Spin");
    }

    #[test]
    fn delay_end_is_withdrawn_for_the_reserve_time() {
        let (ha, _, _) = make();
        let config = ha.device(ID).unwrap().config.unwrap();
        assert_eq!(
            config.components["delay_end"],
            json!({"platform": "sensor"})
        );
        assert!(config.components.contains_key("reserve_time"));
    }

    #[test]
    fn selecting_a_downloaded_program_course_shows_its_base_smart_course_its_catalog_name() {
        let (ha, _, _) = feed(&[DUVET]);
        expect(&ha, "status", "Standby");
        expect(&ha, "course", "Duvet");
        // Download 0x3a is the cloud's BLANKET, not this dial's AI Wash position.
        expect(&ha, "smart_course", "Blanket");
        expect(&ha, "downloaded_program", "Blanket");
        expect(&ha, "spin", "400 rpm");
        expect(&ha, "initial_time", "88");
        expect(&ha, "detergent", "Medium");
        expect(&ha, "softener", "Medium");
    }

    #[test]
    fn a_downloaded_microplastic_care_is_named() {
        let mut frame = hex_decode(DUVET);
        frame[76] = 0x86;
        frame[79] = 0x86;
        let (ha, thinq, _) = make();
        thinq.emit_data(&frame);
        expect(&ha, "smart_course", "MicroPlastic Care");
        expect(&ha, "downloaded_program", "MicroPlastic Care");
    }

    #[test]
    fn status_follows_the_washer_through_a_cycle() {
        let (ha, thinq, _) = make();
        for (frame, status) in [
            (MEASURING, "Detecting"),
            (RUNNING, "Washing"),
            (RINSING, "Rinsing"),
            (SPINNING, "Spinning"),
            (POWER_OFF, "Off"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            expect(&ha, "status", status);
        }
    }

    #[test]
    fn dispenser_drawer_follows_bit_2_of_37_not_the_bit_6_pulse() {
        let (ha, thinq, _) = make();
        for (frame, drawer) in [
            (INITIAL, "OFF"),
            (DRAWER_OPEN, "ON"),
            (DRAWER_PULSE_ON, "ON"),
            (DRAWER_PULSE_OFF, "ON"),
            (DRAWER_CLOSED, "OFF"),
            // bit 6 alone, mid-cycle with the drawer shut
            (SINGLE, "OFF"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            expect(&ha, "dispenser_drawer", drawer);
        }
    }

    #[test]
    fn softener_alert_follows_bit_1_of_37_not_the_bit_6_pulse() {
        let (ha, thinq, _) = make();
        for (frame, low) in [
            (INITIAL, "OFF"),
            (SOFTENER_LOW_PULSE_ON, "ON"),
            (SOFTENER_LOW_PULSE_OFF, "ON"),
            (DRAWER_PULSE_ON, "OFF"),
            (SINGLE, "OFF"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            expect(&ha, "softener_refill", low);
            expect(&ha, "detergent_refill", "OFF");
        }
    }

    #[test]
    fn detergent_alert_follows_bit_0_of_37() {
        let (ha, _, _) = feed(&[DETERGENT_LOW_PULSE_ON]);
        expect(&ha, "detergent_refill", "ON");
        expect(&ha, "softener_refill", "OFF");
    }

    #[test]
    fn power_off_transition() {
        let (ha, _, _) = feed(&[POWER_OFF]);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "error", "OFF");
        expect(&ha, "error_message", "OK");
    }

    #[test]
    fn measuring_state_tub_clean_door_locked_remote_start_active() {
        let (ha, _, _) = feed(&[MEASURING]);
        expect(&ha, "status", "Detecting");
        expect(&ha, "course", "Tub Clean");
        expect(&ha, "spin", "No spin");
        expect(&ha, "temp", "60 °C");
        expect(&ha, "initial_time", "72");
        expect(&ha, "remaining_time", "72");
        expect(&ha, "cycles", "32");
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "OFF");
        // nothing dosed yet while measuring
        expect(&ha, "detergent", "Off");
        expect(&ha, "softener", "Off");
    }

    #[test]
    fn running_state_eco_40_60_door_locked_remote_start_active() {
        let (ha, _, _) = feed(&[RUNNING]);
        expect(&ha, "status", "Washing");
        expect(&ha, "course", "Eco 40-60");
        expect(&ha, "spin", "1400 rpm");
        expect(&ha, "temp", "40 °C");
        expect(&ha, "rinse", "Normal");
        expect(&ha, "initial_time", "147");
        expect(&ha, "remaining_time", "145");
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "OFF");
        expect(&ha, "detergent", "High");
        expect(&ha, "softener", "Low");
    }

    #[test]
    fn rinsing_state() {
        let (ha, _, _) = feed(&[RINSING]);
        expect(&ha, "status", "Rinsing");
        expect(&ha, "course", "Eco 40-60");
        // temp isn't reported during the rinse phase
        expect(&ha, "temp", "Not selected");
        expect(&ha, "remaining_time", "34");
        expect(&ha, "energy", "186");
        expect(&ha, "initial_time", "147");
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "OFF");
    }

    #[test]
    fn spinning_state_eco_40_60_door_locked_remote_start_active() {
        let (ha, _, _) = feed(&[SPINNING]);
        expect(&ha, "status", "Spinning");
        expect(&ha, "course", "Eco 40-60");
        expect(&ha, "spin", "1400 rpm");
        // temp isn't reported during the spin phase
        expect(&ha, "temp", "Not selected");
        expect(&ha, "initial_time", "147");
        expect(&ha, "remaining_time", "28");
        expect(&ha, "energy", "194");
        expect(&ha, "cycles", "17");
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "OFF");
        expect(&ha, "detergent", "Medium");
        expect(&ha, "softener", "Off");
    }

    #[test]
    fn single_block_frame_still_decodes_via_payload_a() {
        let (ha, _, _) = feed(&[SINGLE]);
        expect(&ha, "status", "Washing");
        expect(&ha, "course", "Eco 40-60");
        expect(&ha, "spin", "1400 rpm");
        expect(&ha, "temp", "40 °C");
        expect(&ha, "initial_time", "147");
        expect(&ha, "remaining_time", "142");
        expect(&ha, "energy", "6");
        expect(&ha, "cycles", "17");
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "OFF");
        expect(&ha, "detergent", "Medium");
        expect(&ha, "softener", "Off");
    }

    #[test]
    fn a_well_formed_frame_with_an_unrecognized_message_type_is_ignored() {
        let (ha, _, _) = feed(&[UNKNOWN]);
        assert!(prop(&ha, "power").is_none());
    }

    #[test]
    fn frames_not_matching_the_aa_bb_envelope_are_ignored() {
        let (ha, thinq, _) = make();
        let before = prop(&ha, "power");
        thinq.emit_data(&hex_decode("001122"));
        assert_eq!(prop(&ha, "power"), before);
    }

    #[test]
    fn start_sends_the_f0ed_initialisation_packet() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(outbox_hex(&thinq), [WRITE_INIT]);
    }

    #[test]
    fn ha_write_power_on() {
        let (_, thinq, dev) = make();
        dev.set_property("power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_ON);
    }

    #[test]
    fn ha_write_power_off() {
        let (_, thinq, dev) = make();
        dev.set_property("power", "OFF");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_OFF);
    }

    #[test]
    fn ha_write_pause_button() {
        let (_, thinq, dev) = make();
        dev.set_property("pause", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_PAUSE);
    }

    #[test]
    fn ha_write_start_button() {
        let (_, thinq, dev) = make();
        dev.set_property("start", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_START);
    }

    #[test]
    fn ezdispense_amounts_decode_from_the_status_push() {
        let (ha, thinq, _) = make();
        for (frame, detergent, softener) in [
            (EZ_42_30, "42", "30"),
            (EZ_43_30, "43", "30"),
            (EZ_43_31, "43", "31"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            expect(&ha, "ezdispense_detergent", detergent);
            expect(&ha, "ezdispense_softener", softener);
            // separate from the dose levels, which stay Medium throughout
            expect(&ha, "detergent", "Medium");
            expect(&ha, "softener", "Medium");
        }
    }

    #[test]
    fn ha_write_ezdispense_amounts_sends_the_same_packets_as_the_lg_app() {
        let (_, thinq, dev) = make();
        dev.set_property("ezdispense_detergent", "43");
        dev.set_property("ezdispense_softener", "31");
        assert_eq!(
            outbox_hex(&thinq),
            [WRITE_EZDISPENSE_DETERGENT_43, WRITE_EZDISPENSE_SOFTENER_31]
        );
    }

    #[test]
    fn ha_write_ezdispense_amounts_accepts_the_edges_of_the_app_range_9_and_150_ml() {
        let (_, thinq, dev) = make();
        dev.set_property("ezdispense_detergent", "9");
        dev.set_property("ezdispense_softener", "150");
        let inners: Vec<Vec<u8>> = thinq
            .outbox()
            .iter()
            .map(|p| p[2..p.len() - 2].to_vec())
            .collect();
        assert_eq!(
            inners,
            [
                vec![0xf0, 0x24, 0x0d, 0x01, 9],
                vec![0xf0, 0x24, 0x0e, 0x01, 150]
            ]
        );
    }

    #[test]
    fn ha_write_ezdispense_amounts_rejects_values_outside_the_app_range() {
        let (_, thinq, dev) = make();
        for value in ["", " ", "0", "8", "151", "256", "42.5", "-1", "abc"] {
            dev.set_property("ezdispense_detergent", value);
            dev.set_property("ezdispense_softener", value);
        }
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn ha_write_ezdispense_amounts_parse_like_javascript_number() {
        for value in ["42.0", "4.2e1", "0x2a", " 42 "] {
            let (_, thinq, dev) = make();
            dev.set_property("ezdispense_detergent", value);
            dev.set_property("ezdispense_softener", value);
            assert_eq!(
                outbox_hex(&thinq),
                [
                    hex_encode(&wrap_aabb(&[0xf0, 0x24, 0x0d, 0x01, 42])),
                    hex_encode(&wrap_aabb(&[0xf0, 0x24, 0x0e, 0x01, 42]))
                ],
                "{value:?}"
            );
        }
    }

    #[test]
    fn the_dispenser_nozzle_clean_course_decodes() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&with_record_byte(COTTON_DOSED, 7, 0x37));
        expect(&ha, "course", "Dispenser Clean");
    }

    #[test]
    fn programme_the_assembled_command_matches_the_one_the_lg_app_sends_dose_levels_included() {
        let (ha, thinq, dev) = feed(&[COTTON_DOSED]);
        expect(&ha, "detergent", "High");
        expect(&ha, "softener", "Low");
        thinq.reset_recorder();

        dev.set_property("set_course", "Cotton");
        dev.set_property("set_soil", "Normal");
        dev.set_property("set_spin", "Max");
        dev.set_property("set_temp", "40 °C");
        dev.set_property("set_rinse", "Rinse+");
        dev.set_property("run_programme", "");

        assert_eq!(hex_encode(thinq.outbox().last().unwrap()), APP_START_COTTON);
    }

    #[test]
    fn programme_an_untouched_start_runs_the_selected_downloaded_course() {
        let (_, thinq, dev) = feed(&[DUVET]);
        thinq.reset_recorder();

        dev.set_property("run_programme", "");

        assert_eq!(inner_hex(&thinq)[0], APP_START_BLANKET);
    }

    #[test]
    fn programme_downloaded_program_runs_the_programme_in_the_slot() {
        for (frame, command) in [
            (INITIAL, APP_START_SLOT_RINSE_SPIN),
            (DUVET, APP_START_BLANKET),
        ] {
            let (_, thinq, dev) = feed(&[frame]);
            thinq.reset_recorder();

            dev.set_property("set_course", "Downloaded program");
            dev.set_property("run_programme", "");

            assert_eq!(inner_hex(&thinq)[0], command);
        }
    }

    #[test]
    fn programme_set_course_shows_downloaded_program_while_the_washer_has_it_selected() {
        let (ha, _, _) = feed(&[DUVET]);
        expect(&ha, "set_course", "Downloaded program");
    }

    #[test]
    fn programme_picking_a_course_drops_the_downloaded_course() {
        let (_, thinq, dev) = feed(&[DUVET]);
        thinq.reset_recorder();

        dev.set_property("set_course", "Duvet");
        dev.set_property("run_programme", "");

        assert_eq!(thinq.outbox()[0][2 + 14], 0);
    }

    #[test]
    fn programme_the_dispense_type_is_repeated_as_reported() {
        let (_, thinq, dev) = make();
        // EZD_DETERGENT_MAIN
        thinq.emit_data(&with_record_byte(COTTON_DOSED, 36, 2));
        thinq.reset_recorder();

        dev.set_property("run_programme", "");

        let packet = thinq.outbox().last().unwrap().clone();
        let inner = &packet[2..];
        assert_eq!(inner[15], 3, "detergent level");
        assert_eq!(inner[16], 1, "softener level");
        assert_eq!(inner[17], 2, "dispense type");
    }

    #[test]
    fn ha_write_to_unknown_property_emits_no_packet() {
        let (_, thinq, dev) = make();
        dev.set_property("does-not-exist", "whatever");
        assert!(thinq.outbox().is_empty());
    }

    fn load(programme: &str) -> Vec<String> {
        let (_, thinq, dev) = feed(&[INITIAL]);
        thinq.reset_recorder();
        dev.set_property("set_downloaded_program", programme);
        inner_hex(&thinq)
    }

    #[test]
    fn download_slot_loading_each_programme_sends_what_the_lg_app_sends() {
        for (programme, command) in APP_DOWNLOADS {
            assert_eq!(load(programme), [*command], "loading {programme}");
        }
    }

    #[test]
    fn download_slot_loading_a_programme_sets_set_course_to_downloaded_program() {
        let (ha, _, dev) = feed(&[INITIAL]);
        dev.set_property("set_course", "Cotton");
        dev.set_property("set_downloaded_program", "Blanket");
        expect(&ha, "set_course", "Downloaded program");
    }

    #[test]
    fn download_slot_a_programme_without_a_captured_load_command_sends_nothing() {
        assert!(load("School Uniform").is_empty());
    }

    #[test]
    fn door_record_publishes_open_closed_from_the_03_record() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(DOOR_CLOSED));
        expect(&ha, "door", "OFF");
        thinq.emit_data(&hex_decode(DOOR_OPEN));
        expect(&ha, "door", "ON");
        thinq.emit_data(&hex_decode(DOOR_UNKNOWN)); // ignored
        expect(&ha, "door", "ON");
        thinq.emit_data(&hex_decode(DOOR_CLOSED));
        expect(&ha, "door", "OFF");
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("Y_VB_Y___W.B32QEUK").is_some());
    }
}
