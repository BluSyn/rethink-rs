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
    fn send_json_to_local(&self, body: serde_json::Value) {
        (self.0.send_to_device)(SendToDevice::T1Json(body));
    }
    fn deploy_payload(&self) -> Option<serde_json::Value> {
        self.0.deploy_payload()
    }
}
