//! Adapt ConnectedDevice to rethink_bridge::LocalDevice.

use crate::devmgr::{ConnectedDevice, Platform, SendToDevice};
use rethink_bridge::LocalDevice;
use std::sync::Arc;

pub struct ConnectedAsLocal(pub Arc<ConnectedDevice>);

impl LocalDevice for ConnectedAsLocal {
    fn id(&self) -> &str {
        &self.0.id
    }
    fn platform(&self) -> &str {
        match self.0.platform {
            Platform::Thinq1 => "thinq1",
            Platform::Thinq2 => "thinq2",
        }
    }
    fn model_id(&self) -> &str {
        &self.0.meta.model_id
    }
    fn model_name(&self) -> &str {
        &self.0.meta.model_name
    }
    fn device_type(&self) -> Option<&str> {
        self.0.meta.device_type.as_deref()
    }
    fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>) {
        self.0.add_data_handler(handler);
    }
    fn on_close(&self, handler: Box<dyn Fn() + Send + Sync>) {
        self.0.add_close_handler(handler);
    }
    fn send_to_local(&self, buf: &[u8]) {
        (self.0.send_to_device)(SendToDevice::T2Packet(buf.to_vec()));
    }
    fn send_ack_to_local(&self, buf: &[u8]) {
        (self.0.send_to_device)(SendToDevice::T2Ack(buf.to_vec()));
    }
    fn is_managed(&self) -> bool {
        self.0.is_managed()
    }
    fn send_json_to_local(&self, body: serde_json::Value) {
        (self.0.send_to_device)(SendToDevice::T1Json(body));
    }
    fn deploy_payload(&self) -> Option<serde_json::Value> {
        self.0.deploy_payload()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::metadata::Metadata;
    use rethink_util::sync::Mutex;

    #[test]
    fn local_ack_is_a_t2_ack_and_managed_is_visible() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let send = Arc::new(move |msg: SendToDevice| {
            record.lock().push(msg);
        });
        let emit = Arc::new(|_buf: Vec<u8>| {});
        let dev = ConnectedDevice::new(
            "id".into(),
            Platform::Thinq2,
            Metadata::new("M", "M", "1"),
            emit,
            send,
        );
        let local = ConnectedAsLocal(dev.clone());
        assert!(!local.is_managed());
        local.send_ack_to_local(&[0xaa, 0x08, 0xf0, 0x00, 0x4d, 0x04, 0xa6, 0xbb]);
        local.send_to_local(&[0xaa, 0x12]);
        dev.set_managed(true);
        assert!(local.is_managed());
        let got = seen.lock().clone();
        assert!(matches!(&got[0], SendToDevice::T2Ack(b) if b == &[0xaa, 0x08, 0xf0, 0x00, 0x4d, 0x04, 0xa6, 0xbb]));
        assert!(matches!(&got[1], SendToDevice::T2Packet(b) if b == &[0xaa, 0x12]));
    }
}
