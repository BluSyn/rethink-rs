//! ThinQ device abstractions (platform-agnostic + mock devices for tests).

use crate::metadata::Metadata;
use rethink_util::sync::Mutex;
use std::sync::Arc;

/// Outbound ThinQ2 message recorded by mocks.
#[derive(Debug, Clone)]
pub struct SentMessage {
    pub cmd: String,
    pub msg_type: i32,
    pub data: serde_json::Value,
}

/// Trait for a ThinQ2-connected appliance (send packets / receive data).
pub trait Thinq2Device: Send + Sync {
    fn id(&self) -> &str;
    fn meta(&self) -> &Metadata;
    fn send_packet(&self, buf: &[u8]);
    fn send(&self, cmd: &str, msg_type: i32, data: serde_json::Value);
    fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>);
    /// Inject a data packet as if received from the appliance (tests + broker).
    fn emit_data(&self, buf: &[u8]);
}

/// Trait for ThinQ1 devices (JSON control body; binary status reports).
pub trait Thinq1Device: Send + Sync {
    fn id(&self) -> &str;
    fn meta(&self) -> &Metadata;
    fn send(&self, body: serde_json::Value);
    /// Status reports are binary frames (e.g. 28-byte washer state).
    fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>);
    fn emit_data(&self, buf: &[u8]);
    /// Optional HTTP diagmon reports (energyMonInfo, …). Default is a no-op so
    /// existing adapters compile; mocks and live wiring can override.
    fn on_diagmon(&self, handler: Box<dyn Fn(&str, &serde_json::Value) + Send + Sync>) {
        let _ = handler;
    }
    fn emit_diagmon(&self, diag_mon_type: &str, decoded: serde_json::Value) {
        let _ = (diag_mon_type, decoded);
    }
}

/// Mock ThinQ2 device for unit tests.
pub struct MockThinq2Device {
    id: String,
    meta: Metadata,
    outbox: Mutex<Vec<Vec<u8>>>,
    sent: Mutex<Vec<SentMessage>>,
    handlers: Mutex<Vec<Box<dyn Fn(&[u8]) + Send + Sync>>>,
}

impl MockThinq2Device {
    pub fn new(id: impl Into<String>, meta: Metadata) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            meta,
            outbox: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
            handlers: Mutex::new(Vec::new()),
        })
    }

    pub fn outbox(&self) -> Vec<Vec<u8>> {
        self.outbox.lock().clone()
    }

    pub fn sent(&self) -> Vec<SentMessage> {
        self.sent.lock().clone()
    }

    pub fn reset_recorder(&self) {
        self.outbox.lock().clear();
        self.sent.lock().clear();
    }
}

impl Thinq2Device for MockThinq2Device {
    fn id(&self) -> &str {
        &self.id
    }

    fn meta(&self) -> &Metadata {
        &self.meta
    }

    fn send_packet(&self, buf: &[u8]) {
        self.outbox.lock().push(buf.to_vec());
        // also notify any send-data listeners if needed
    }

    fn send(&self, cmd: &str, msg_type: i32, data: serde_json::Value) {
        self.sent.lock().push(SentMessage {
            cmd: cmd.into(),
            msg_type,
            data,
        });
    }

    fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>) {
        self.handlers.lock().push(handler);
    }

    fn emit_data(&self, buf: &[u8]) {
        let guard = self.handlers.lock();
        for h in guard.iter() {
            h(buf);
        }
    }
}

/// Mock ThinQ1 device for unit tests.
pub struct MockThinq1Device {
    id: String,
    meta: Metadata,
    sent: Mutex<Vec<serde_json::Value>>,
    handlers: Mutex<Vec<Box<dyn Fn(&[u8]) + Send + Sync>>>,
    diagmon_handlers: Mutex<Vec<Box<dyn Fn(&str, &serde_json::Value) + Send + Sync>>>,
}

impl MockThinq1Device {
    pub fn new(id: impl Into<String>, meta: Metadata) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            meta,
            sent: Mutex::new(Vec::new()),
            handlers: Mutex::new(Vec::new()),
            diagmon_handlers: Mutex::new(Vec::new()),
        })
    }

    pub fn sent(&self) -> Vec<serde_json::Value> {
        self.sent.lock().clone()
    }

    pub fn reset_recorder(&self) {
        self.sent.lock().clear();
    }
}

impl Thinq1Device for MockThinq1Device {
    fn id(&self) -> &str {
        &self.id
    }

    fn meta(&self) -> &Metadata {
        &self.meta
    }

    fn send(&self, body: serde_json::Value) {
        self.sent.lock().push(body);
    }

    fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>) {
        self.handlers.lock().push(handler);
    }

    fn emit_data(&self, buf: &[u8]) {
        let guard = self.handlers.lock();
        for h in guard.iter() {
            h(buf);
        }
    }

    fn on_diagmon(&self, handler: Box<dyn Fn(&str, &serde_json::Value) + Send + Sync>) {
        self.diagmon_handlers.lock().push(handler);
    }

    fn emit_diagmon(&self, diag_mon_type: &str, decoded: serde_json::Value) {
        let guard = self.diagmon_handlers.lock();
        for h in guard.iter() {
            h(diag_mon_type, &decoded);
        }
    }
}

/// Hex helpers used by tests.
pub fn hex_encode(b: &[u8]) -> String {
    rethink_util::hex::encode_upper(b)
}

pub fn hex_decode(s: &str) -> Vec<u8> {
    rethink_util::hex::decode_ignore_ws(s).unwrap_or_default()
}

#[cfg(test)]
mod hex_helper_tests {
    use super::*;

    #[test]
    fn hex_decode_strips_whitespace_in_one_pass() {
        assert_eq!(
            hex_decode("AA 0E\nF0ED"),
            vec![0xaa, 0x0e, 0xf0, 0xed]
        );
        assert_eq!(hex_decode("not-hex"), Vec::<u8>::new());
        assert_eq!(hex_encode(&[0xaa, 0xbb]), "AABB");
    }
}
