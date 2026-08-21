//! Known ThinQ TLV tag catalog for RE tooling (unknown highlighting).

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct KnownTag {
    pub id: u16,
    pub name: &'static str,
    pub family: &'static str,
}

/// Tags observed across RAC/WIN/POT/DHUM/HUM and wiki annotations.
pub static KNOWN_TAGS: &[KnownTag] = &[
    KnownTag { id: 0x1f5, name: "query_type", family: "clip" },
    KnownTag { id: 0x1f7, name: "power", family: "climate" },
    KnownTag { id: 0x1f9, name: "op_mode", family: "climate" },
    KnownTag { id: 0x1fa, name: "fan_strength", family: "climate" },
    KnownTag { id: 0x1fc, name: "temp_related_1fc", family: "climate" },
    KnownTag { id: 0x1fd, name: "current_temp_half_c", family: "climate" },
    KnownTag { id: 0x1fe, name: "setpoint_temp_half_c", family: "climate" },
    KnownTag { id: 0x253, name: "target_humidity", family: "humidity" },
    // RAC/CST: often RH×10 (900 → 90%). DHUM: RH percent (60 → 60%).
    // Panel hint: wire ≥200 → treat as ×10; else percent.
    KnownTag { id: 0x336, name: "current_humidity", family: "humidity" },
    KnownTag { id: 0x2da, name: "eeprom_checksum", family: "caps" },
    KnownTag { id: 0x2cc, name: "feature_bits_2cc", family: "caps" },
    KnownTag { id: 0x2cd, name: "feature_bits_2cd", family: "caps" },
    KnownTag { id: 0x2d3, name: "feature_bits_2d3", family: "caps" },
    KnownTag { id: 0x2d5, name: "cap_tag_2d5", family: "caps" },
    KnownTag { id: 0x2d6, name: "cap_tag_2d6", family: "caps" },
    // Capability bitmaps (ac_common TAG_CAPS_MODES/FANS; POT/PAC/CST comments; wiki RAC v=1 dump)
    KnownTag { id: 0x2c1, name: "caps_modes", family: "caps" },
    KnownTag { id: 0x2c2, name: "caps_fans", family: "caps" },
    // CST: feature bitmap on 0x2cb when 0x2cc is absent (PR #122 CST_570004_WW)
    KnownTag { id: 0x2cb, name: "feature_bits_2cb", family: "caps" },
    // Cooling setpoint range, half-°C (32/60 → 16/30). ac_common temperatureRange()
    KnownTag { id: 0x2e1, name: "caps_temp_min_half_c", family: "caps" },
    KnownTag { id: 0x2e2, name: "caps_temp_max_half_c", family: "caps" },
    // Other-mode setpoint ranges (half-°C). ac_common: "0x2e3–0x2ec carry the ranges of the other modes"
    KnownTag { id: 0x2e3, name: "caps_temp_range_2e3", family: "caps" },
    KnownTag { id: 0x2e4, name: "caps_temp_range_2e4", family: "caps" },
    KnownTag { id: 0x2eb, name: "caps_temp_range_2eb", family: "caps" },
    KnownTag { id: 0x2ec, name: "caps_temp_range_2ec", family: "caps" },
    // DHUM CAPS_ONLY_TAGS + HA min_humidity/max_humidity 30/70 matching wire
    KnownTag { id: 0x2e5, name: "caps_humidity_min", family: "caps" },
    KnownTag { id: 0x2e6, name: "caps_humidity_max", family: "caps" },
    // RAC/CST: bit0 or bit 0x200 → skip basic-filter priv-command ("no filter"/mFilter)
    KnownTag { id: 0x2f1, name: "filter_capability", family: "caps" },
    KnownTag { id: 0x205, name: "wind_or_vane_205", family: "climate" },
    KnownTag { id: 0x206, name: "wind_or_vane_206", family: "climate" },
    KnownTag { id: 0x20d, name: "energy_save", family: "climate" },
    KnownTag { id: 0x20e, name: "autodry", family: "climate" },
    KnownTag { id: 0x20f, name: "airclean", family: "climate" },
    KnownTag { id: 0x21a, name: "sleep_timer", family: "timer" },
    KnownTag { id: 0x23f, name: "aq_or_comfort_flag", family: "aq" },
    KnownTag { id: 0x271, name: "power_or_limit_flag", family: "climate" },
    // RAC vertical swing (not "filter"); 0=off, 1–6 steps, 100=on/auto
    KnownTag { id: 0x321, name: "swing_vertical", family: "climate" },
    KnownTag { id: 0x322, name: "swing_horizontal", family: "climate" },
    // RAC/CST jet cool/heat (handler jet field); 0=off in command polls
    KnownTag { id: 0x323, name: "jet", family: "climate" },
    KnownTag { id: 0x325, name: "humidity_ctrl_flag", family: "humidity" },
    // Near filter pair 0x355/0x356 on CST values dumps
    KnownTag { id: 0x2f2, name: "filter_status_code", family: "diag" },
    KnownTag { id: 0x353, name: "pm_or_aq_flag_353", family: "aq" },
    KnownTag { id: 0x354, name: "pm_or_aq_flag_354", family: "aq" },
    KnownTag { id: 0x357, name: "filter_param_357", family: "diag" },
    KnownTag { id: 0x358, name: "filter_param_358", family: "diag" },
    // DHUM bucket event; on AC often 0 — event channel
    KnownTag { id: 0x2b1, name: "event_channel_2b1", family: "diag" },
    KnownTag { id: 0x3a7, name: "feature_bit_3a7", family: "climate" },
    KnownTag { id: 0x109, name: "night_mode", family: "humidifier" },
    KnownTag { id: 0x1e3, name: "product_status", family: "humidifier" },
    KnownTag { id: 0x1e4, name: "sleep_mode", family: "humidifier" },
    KnownTag { id: 0x1e6, name: "auto_operation", family: "humidifier" },
    KnownTag { id: 0x1e7, name: "humidify_switch", family: "humidifier" },
    KnownTag { id: 0x1e9, name: "hygiene_dry", family: "humidifier" },
    KnownTag { id: 0x117, name: "over_prevention", family: "humidifier" },
    KnownTag { id: 0x161, name: "anti_glare", family: "humidifier" },
    KnownTag { id: 0x164, name: "standby_sterilize", family: "humidifier" },
    KnownTag { id: 0x1b8, name: "mood_light_power", family: "humidifier" },
    KnownTag { id: 0x1ed, name: "water_filter_level", family: "humidifier" },
    KnownTag { id: 0x1ee, name: "watertank_remain", family: "humidifier" },
    KnownTag { id: 0x21b, name: "off_timer", family: "timer" },
    // RAC/DHUM: turn-on timer (paired with 0x21b); hours, 0=off
    KnownTag { id: 0x21c, name: "on_timer", family: "timer" },
    KnownTag { id: 0x21e, name: "tank_or_bucket_light", family: "humidity" },
    KnownTag { id: 0x21f, name: "display_brightness", family: "humidifier" },
    KnownTag { id: 0x221, name: "error_code", family: "diag" },
    KnownTag { id: 0x225, name: "auto_dry_remain", family: "humidifier" },
    // DHUM values: often 0; likely reserved / feature flag group with 0x21b–0x226
    KnownTag { id: 0x226, name: "timer_or_sched_flag", family: "timer" },
    KnownTag { id: 0x240, name: "air_quality", family: "aq" },
    // DHUM: cumulative counter (runtime/energy-ish); units unconfirmed — seen ~7k
    KnownTag { id: 0x232, name: "usage_counter", family: "diag" },
    // DHUM: small status/aux reading (seen 26); not half-°C ambient (that's 0x1fd)
    KnownTag { id: 0x233, name: "aux_reading", family: "diag" },
    KnownTag { id: 0x2a2, name: "uv_nano", family: "humidity" },
    // DHUM: often 0 with values; filter/sensor related family near 0x2a2
    KnownTag { id: 0x2ac, name: "filter_or_sensor_flag", family: "diag" },
    KnownTag { id: 0x2ad, name: "watertank_time", family: "humidifier" },
    KnownTag { id: 0x2b2, name: "bucket_full", family: "humidity" },
    // DHUM fan-per-mode memory table (triple stream, not stored as HA state)
    KnownTag { id: 0x2d7, name: "fan_table_mode", family: "climate" },
    KnownTag { id: 0x2d8, name: "fan_table_pad", family: "climate" },
    KnownTag { id: 0x2d9, name: "fan_table_fan", family: "climate" },
    // DHUM: often 0 in values; likely capability/echo bit near humidity block
    KnownTag { id: 0x324, name: "humidity_feature_flag", family: "humidity" },
    KnownTag { id: 0x333, name: "pm1", family: "aq" },
    KnownTag { id: 0x334, name: "pm25", family: "aq" },
    KnownTag { id: 0x335, name: "pm10", family: "aq" },
    KnownTag { id: 0x337, name: "sensor_mon", family: "humidifier" },
    KnownTag { id: 0x33a, name: "humidity_aux_flag", family: "humidity" },
    KnownTag { id: 0x355, name: "filter_used", family: "diag" },
    KnownTag { id: 0x356, name: "filter_max", family: "diag" },
    KnownTag { id: 0x35a, name: "start_time", family: "timer" },
    KnownTag { id: 0x35b, name: "stop_time", family: "timer" },
    KnownTag { id: 0x360, name: "ionizer", family: "humidity" },
    KnownTag { id: 0x3a0, name: "bell_sound", family: "humidifier" },
    KnownTag { id: 0x3e0, name: "mood_color", family: "humidifier" },
    KnownTag { id: 0x3e8, name: "caps_marker_hum", family: "caps" },
];

pub fn known_tag_name(id: u16) -> Option<&'static str> {
    KNOWN_TAGS.iter().find(|t| t.id == id).map(|t| t.name)
}

pub fn is_known_tag(id: u16) -> bool {
    known_tag_name(id).is_some()
}

/// Tags that appear in public capability dumps but have no evidenced meaning yet.
///
/// Kept UNKNOWN in classify/export; the note is the residual record (family + why
/// it cannot be named from public/in-tree sources). Do not invent HA semantics.
#[derive(Debug, Clone, Serialize)]
pub struct ResidualUnknown {
    pub id: u16,
    pub family: &'static str,
    pub reason: &'static str,
}

pub static RESIDUAL_UNKNOWNS: &[ResidualUnknown] = &[
    ResidualUnknown {
        id: 0x2c0,
        family: "caps",
        reason: "First tag in wiki RAC v=1 caps dump (values 1/10/13); no handler reads it",
    },
    ResidualUnknown {
        id: 0x2c3,
        family: "caps",
        reason: "Listed next to 0x2c1/0x2c2 in wiki RAC caps dump; no semantic decode",
    },
    ResidualUnknown {
        id: 0x2c4,
        family: "caps",
        reason: "Listed next to 0x2c1/0x2c2 in wiki RAC caps dump; no semantic decode",
    },
    ResidualUnknown {
        id: 0x2c7,
        family: "caps",
        reason: "Seen in RAC/CST live caps; not in wiki RAC v=1 table and unused by handlers",
    },
    ResidualUnknown {
        id: 0x2ce,
        family: "caps",
        reason: "CST caps only; no public field map (adjacent to 0x2cd jet/swing bitmap)",
    },
    ResidualUnknown {
        id: 0x2cf,
        family: "caps",
        reason: "DHUM caps only; no handler/public note beyond presence",
    },
    ResidualUnknown {
        id: 0x2db,
        family: "caps",
        reason: "Wiki RAC caps dump lists it (6882313 matches this RAC dump); packed id/build, no decode",
    },
    ResidualUnknown {
        id: 0x2dc,
        family: "caps",
        reason: "Wiki RAC v=1 lists 0x2dc; values differ across units (1 vs 254 vs 512); unused",
    },
    ResidualUnknown {
        id: 0x2dd,
        family: "caps",
        reason: "CST/DHUM caps; not in wiki RAC table; no handler use",
    },
    ResidualUnknown {
        id: 0x2ee,
        family: "caps",
        reason: "CST caps only; no public note",
    },
    ResidualUnknown {
        id: 0x2ef,
        family: "caps",
        reason: "CST caps only; no public note",
    },
    ResidualUnknown {
        id: 0x2f3,
        family: "diag",
        reason: "CST caps near 0x2f1/0x2f2 filter cluster; no confirmed meaning",
    },
    ResidualUnknown {
        id: 0x2f4,
        family: "caps",
        reason: "DHUM/CST caps (large values); no handler/public map",
    },
    ResidualUnknown {
        id: 0x2f5,
        family: "caps",
        reason: "Wiki RAC v=1 lists 0x2f5=7; unused by handlers",
    },
    ResidualUnknown {
        id: 0x2f6,
        family: "caps",
        reason: "DHUM caps only; no public map",
    },
    ResidualUnknown {
        id: 0x350,
        family: "caps",
        reason: "DHUM/CST caps (0/1); no public map",
    },
    ResidualUnknown {
        id: 0x374,
        family: "caps",
        reason: "RAC/CST=40, DHUM=4 in caps; no public map",
    },
    ResidualUnknown {
        id: 0x397,
        family: "caps",
        reason: "CST caps (tag repeats); no public map",
    },
];

pub fn residual_note(id: u16) -> Option<&'static ResidualUnknown> {
    RESIDUAL_UNKNOWNS.iter().find(|r| r.id == id)
}

#[derive(Debug, Clone, Serialize)]
pub struct ClassifiedTlv {
    pub t: u16,
    pub v: u32,
    pub known: bool,
    pub name: Option<&'static str>,
}

/// Classify a TLV list against the known catalog.
pub fn classify_tlvs(items: &[(u16, u32)]) -> Vec<ClassifiedTlv> {
    items
        .iter()
        .map(|(t, v)| {
            let name = known_tag_name(*t);
            ClassifiedTlv {
                t: *t,
                v: *v,
                known: name.is_some(),
                name,
            }
        })
        .collect()
}

/// Compact single-frame breakdown for humans and analysis tools.
pub fn llm_export_text(
    model_id: Option<&str>,
    direction: Option<&str>,
    hex_packet: &str,
    classified: &[ClassifiedTlv],
) -> String {
    let mut out = String::new();
    out.push_str("# ThinQ TLV frame\n");
    if let Some(m) = model_id {
        out.push_str(&format!("modelId: {m}\n"));
    }
    if let Some(d) = direction {
        out.push_str(&format!("direction: {d}\n"));
    }
    out.push_str(&format!("hex: {hex_packet}\n"));
    out.push_str("tags:\n");
    let mut unknowns = Vec::new();
    for c in classified {
        if c.known {
            out.push_str(&format!(
                "  0x{:03x} {} = {}\n",
                c.t,
                c.name.unwrap_or("?"),
                c.v
            ));
        } else {
            out.push_str(&format!("  0x{:03x} UNKNOWN = {}\n", c.t, c.v));
            unknowns.push(c);
        }
    }
    if !unknowns.is_empty() {
        out.push_str("unknown:\n");
        for c in &unknowns {
            out.push_str(&format!("  0x{:03x} = {}\n", c.t, c.v));
        }
        let mut residual_lines = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for c in &unknowns {
            if !seen.insert(c.t) {
                continue;
            }
            if let Some(r) = residual_note(c.t) {
                residual_lines.push(format!("  0x{:03x} [{}] {}\n", r.id, r.family, r.reason));
            }
        }
        if !residual_lines.is_empty() {
            out.push_str("residual:\n");
            for line in residual_lines {
                out.push_str(&line);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet_codec::{decode_packet, Decoded};

    #[test]
    fn classifies_known_and_unknown() {
        let c = classify_tlvs(&[(0x1f7, 1), (0x999, 42)]);
        assert!(c[0].known);
        assert_eq!(c[0].name, Some("power"));
        assert!(!c[1].known);
    }

    #[test]
    fn llm_export_includes_unknown_section() {
        let c = classify_tlvs(&[(0x1f7, 1), (0xabc, 9)]);
        let text = llm_export_text(Some("RAC_056905_WW"), Some("fromDevice"), "deadbeef", &c);
        assert!(text.contains("UNKNOWN"));
        assert!(text.contains("0xabc"));
        assert!(text.contains("RAC_056905_WW"));
        assert!(text.contains("unknown:"));
        assert!(!text.to_lowercase().contains("copy into an llm"));
    }

    /// User-supplied fromDevice capability dumps (MQTT 0000 + UART 0xa7).
    const DUMP_DHUM: &str = "000004000000a702011d68b00db0707e0000b0b0011044b3200800b340b4e00100b5a00402b541b6a00a7cb6e09439b7200200b750a0bc40b3c0bd30010001bd501fbd85b9501eb99046d401dd04fa01b5d011b600b642b5d012b600b642b5d013b600b642b5d014b600b642b5d015b600b642828e";
    const DUMP_CST: &str = "000004000000a70201007fb00ab05057b0a001d5b0c1b103b4d011b710febb81bbc0bc41bc9089bcc1b35055b384b540b2e01004b75020b85020b8903cb8d020b9103cbad024bb103cb1c1bd41b5c0b648b6102ab5c1b648b6102ab5c2b648b6102ab5c6b648b6102ab5c4b648b6102ab69037b6f0570004e5e0ad52e5e034f3d400bd30800000dd10286018";
    const DUMP_RAC: &str = "000004000000a70201fa65b001b05077b0a00154b0c1b103b305b345b540b6a03d26b6f0690409b710feb4c7bc600201dd1028bc9081b1c1b85020b8903cb8d020b9103cbd47b5c0b61032b648b5c1b61030b648b5c2b61030b648b5c4b6103cb648b5c5b61030b648b5c6b6102cb648232b";

    fn export_dump(model: &str, hex: &str) -> (Vec<ClassifiedTlv>, String) {
        let Decoded::Tlv(frame) = decode_packet(hex) else {
            panic!("{model}: expected shipped TLV decode");
        };
        let items: Vec<(u16, u32)> = frame.tlv.iter().map(|e| (e.t, e.v)).collect();
        let classified = classify_tlvs(&items);
        let text = llm_export_text(Some(model), Some("fromDevice"), hex, &classified);
        (classified, text)
    }

    fn named<'a>(c: &'a [ClassifiedTlv], id: u16) -> &'a str {
        let hit = c
            .iter()
            .find(|x| x.t == id)
            .unwrap_or_else(|| panic!("dump missing tag 0x{id:03x}"));
        assert!(hit.known, "0x{id:03x} still UNKNOWN (v={})", hit.v);
        hit.name.unwrap()
    }

    #[test]
    fn dhum_caps_dump_names_evidenced_tags() {
        let (c, text) = export_dump("DHUM_056905_WW", DUMP_DHUM);
        assert_eq!(named(&c, 0x2c1), "caps_modes");
        assert_eq!(named(&c, 0x2c2), "caps_fans");
        assert_eq!(named(&c, 0x2e5), "caps_humidity_min");
        assert_eq!(named(&c, 0x2e6), "caps_humidity_max");
        assert_eq!(named(&c, 0x2f1), "filter_capability");
        assert_eq!(named(&c, 0x2cc), "feature_bits_2cc");
        assert_eq!(named(&c, 0x2d7), "fan_table_mode");
        assert_eq!(named(&c, 0x3e8), "caps_marker_hum");
        assert!(text.contains("caps_humidity_min"));
        assert!(!text.contains("0x2e5 UNKNOWN"));
        let unknown: Vec<u16> = c.iter().filter(|x| !x.known).map(|x| x.t).collect();
        assert_eq!(
            unknown,
            vec![0x2c0, 0x2db, 0x2dc, 0x2dd, 0x2cf, 0x2f4, 0x2f5, 0x2f6, 0x350, 0x374]
        );
        for id in &unknown {
            assert!(residual_note(*id).is_some(), "DHUM unknown 0x{id:03x} has no residual note");
        }
    }

    #[test]
    fn cst_caps_dump_names_evidenced_tags() {
        let (c, text) = export_dump("CST_570004_WW", DUMP_CST);
        assert_eq!(named(&c, 0x2c1), "caps_modes");
        assert_eq!(named(&c, 0x2c2), "caps_fans");
        assert_eq!(named(&c, 0x2cb), "feature_bits_2cb");
        assert_eq!(named(&c, 0x2e1), "caps_temp_min_half_c");
        assert_eq!(named(&c, 0x2e2), "caps_temp_max_half_c");
        assert_eq!(named(&c, 0x2e3), "caps_temp_range_2e3");
        assert_eq!(named(&c, 0x2e4), "caps_temp_range_2e4");
        assert_eq!(named(&c, 0x2eb), "caps_temp_range_2eb");
        assert_eq!(named(&c, 0x2ec), "caps_temp_range_2ec");
        assert_eq!(named(&c, 0x2f1), "filter_capability");
        assert_eq!(named(&c, 0x2f2), "filter_status_code");
        assert!(text.contains("caps_temp_min_half_c"));
        assert!(!text.contains("0x2c1 UNKNOWN"));
        assert!(!text.contains("0x2cb UNKNOWN"));
        let unknown: Vec<u16> = c.iter().filter(|x| !x.known).map(|x| x.t).collect();
        assert_eq!(
            unknown,
            vec![
                0x2c0, 0x2c3, 0x2c4, 0x2dc, 0x2ee, 0x2ef, 0x2f3, 0x2ce, 0x2dd, 0x2c7, 0x2f5, 0x2db,
                0x397, 0x397, 0x350, 0x2f4, 0x374
            ]
        );
        for id in &unknown {
            assert!(residual_note(*id).is_some(), "CST unknown 0x{id:03x} has no residual note");
        }
        assert!(text.contains("residual:"));
    }

    #[test]
    fn rac_caps_dump_names_evidenced_tags() {
        let (c, text) = export_dump("RAC_056905_WW", DUMP_RAC);
        assert_eq!(named(&c, 0x2c1), "caps_modes");
        assert_eq!(named(&c, 0x2c2), "caps_fans");
        assert_eq!(named(&c, 0x2e1), "caps_temp_min_half_c");
        assert_eq!(named(&c, 0x2e2), "caps_temp_max_half_c");
        assert_eq!(named(&c, 0x2e3), "caps_temp_range_2e3");
        assert_eq!(named(&c, 0x2e4), "caps_temp_range_2e4");
        assert_eq!(named(&c, 0x2f1), "filter_capability");
        assert_eq!(named(&c, 0x2cc), "feature_bits_2cc");
        assert_eq!(named(&c, 0x2cd), "feature_bits_2cd");
        assert!(text.contains("filter_capability"));
        assert!(!text.contains("0x2e1 UNKNOWN"));
        let unknown: Vec<u16> = c.iter().filter(|x| !x.known).map(|x| x.t).collect();
        assert_eq!(
            unknown,
            vec![0x2c0, 0x2c3, 0x2c4, 0x2db, 0x2dc, 0x374, 0x2c7, 0x2f5]
        );
        for id in &unknown {
            assert!(residual_note(*id).is_some(), "RAC unknown 0x{id:03x} has no residual note");
        }
    }

    #[test]
    fn residual_notes_do_not_invent_known_names() {
        for r in RESIDUAL_UNKNOWNS {
            assert!(
                known_tag_name(r.id).is_none(),
                "residual 0x{:03x} is also in KNOWN_TAGS",
                r.id
            );
        }
    }

    /// `TLV_DUMP_OUTDIR=<dir> cargo test -p rethink-util --lib tlv_catalog`
    /// writes shipped classify/export text for the three user dumps.
    #[test]
    fn write_dump_exports_if_outdir_set() {
        let Ok(dir) = std::env::var("TLV_DUMP_OUTDIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        for (model, hex, file) in [
            ("DHUM_056905_WW", DUMP_DHUM, "dump-dhum.txt"),
            ("CST_570004_WW", DUMP_CST, "dump-cst.txt"),
            ("RAC_056905_WW", DUMP_RAC, "dump-rac.txt"),
        ] {
            let (_c, text) = export_dump(model, hex);
            std::fs::write(dir.join(file), &text).unwrap_or_else(|e| panic!("write {file}: {e}"));
        }
    }
}
