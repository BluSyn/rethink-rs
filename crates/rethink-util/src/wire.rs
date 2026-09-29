//! Monitor / capture classification of device wire messages.
//!
//! Older rethink servers omit `type`; everything they sent was a packet.
//! `ack` is the cloud's delivery ack forwarded to a ThinQ2 appliance.

use serde_json::{json, Value};

/// `ack` when the monitor message says so, otherwise `packet`.
pub fn monitor_wire_type(explicit: Option<&str>) -> &'static str {
    if explicit == Some("ack") {
        "ack"
    } else {
        "packet"
    }
}

/// Turn one management-monitor JSON message into a capture event, if it is wire traffic.
///
/// Non-hex `tx` values (ThinQ1 objects) are stored under `raw` with the same type.
pub fn capture_from_monitor_message(msg: &Value) -> Option<Value> {
    let ty = monitor_wire_type(msg.get("type").and_then(|v| v.as_str()));
    let injected = msg.get("injected").and_then(|v| v.as_bool()).unwrap_or(false);
    if let Some(hex) = msg.get("rx").and_then(|v| v.as_str()) {
        return Some(json!({
            "k": "rx",
            "hex": hex,
            "type": ty,
            "injected": injected,
        }));
    }
    if msg.get("tx").is_some() {
        let tx = &msg["tx"];
        if let Some(hex) = tx.as_str() {
            return Some(json!({
                "k": "tx",
                "hex": hex,
                "type": ty,
                "injected": injected,
            }));
        }
        return Some(json!({
            "k": "tx",
            "raw": tx.to_string(),
            "type": ty,
            "injected": injected,
        }));
    }
    None
}

/// Type filter for capture files. Missing `type` counts as `packet`.
/// Non-wire events are excluded when a type is requested (same rule as upstream `read_capture`).
pub fn capture_event_matches_type(event: &Value, want: Option<&str>) -> bool {
    let Some(want) = want else {
        return true;
    };
    let is_wire = matches!(
        event.get("k").and_then(|v| v.as_str()),
        Some("wire" | "rx" | "tx")
    );
    is_wire && monitor_wire_type(event.get("type").and_then(|v| v.as_str())) == want
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_messages_stay_acks_and_older_messages_stay_packets() {
        let ack = json!({"tx": "aa08f0004d04a6bb", "type": "ack", "injected": false});
        let ev = capture_from_monitor_message(&ack).unwrap();
        assert_eq!(ev["k"], "tx");
        assert_eq!(ev["type"], "ack");
        assert_eq!(ev["hex"], "aa08f0004d04a6bb");

        let legacy = json!({"rx": "aa12f0", "injected": true});
        let ev = capture_from_monitor_message(&legacy).unwrap();
        assert_eq!(ev["type"], "packet");
        assert_eq!(ev["k"], "rx");
        assert_eq!(ev["injected"], true);

        let obj = json!({"tx": {"cmd": "packet", "data": "AA"}, "type": "packet"});
        let ev = capture_from_monitor_message(&obj).unwrap();
        assert_eq!(ev["type"], "packet");
        assert!(ev.get("raw").is_some());

        assert!(capture_from_monitor_message(&json!({"status": "online"})).is_none());
        assert_eq!(monitor_wire_type(None), "packet");
        assert_eq!(monitor_wire_type(Some("nope")), "packet");
    }

    #[test]
    fn type_filter_keeps_only_matching_wire_events() {
        let ack = json!({"k": "tx", "type": "ack", "hex": "aa"});
        let packet = json!({"k": "rx", "hex": "bb"});
        let note = json!({"k": "note", "text": "hi"});
        assert!(capture_event_matches_type(&ack, Some("ack")));
        assert!(!capture_event_matches_type(&packet, Some("ack")));
        assert!(capture_event_matches_type(&packet, Some("packet")));
        assert!(!capture_event_matches_type(&note, Some("packet")));
        assert!(capture_event_matches_type(&note, None));
    }
}
