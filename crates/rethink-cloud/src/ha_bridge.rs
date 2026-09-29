//! Wire connected ThinQ devices to HA device handlers via the registry.

use crate::devmgr::{ConnectedDevice, Platform};
use rethink_util::sync::Mutex;
use rethink_core::ha::HaConnection;
use rethink_core::thinq::{Thinq1Device, Thinq2Device};
use rethink_devices::device_trait::DeviceHandler;
use rethink_devices::registry::{lookup_t1, lookup_t2};
use std::collections::HashMap;
use std::sync::Arc;

/// Adapter: ConnectedDevice → Thinq2Device trait.
struct T2Adapter {
    dev: Arc<ConnectedDevice>,
    handlers: Mutex<Vec<Box<dyn Fn(&[u8]) + Send + Sync>>>,
}

impl Thinq2Device for T2Adapter {
    fn id(&self) -> &str {
        &self.dev.id
    }
    fn meta(&self) -> &rethink_core::metadata::Metadata {
        &self.dev.meta
    }
    fn send_packet(&self, buf: &[u8]) {
        self.dev
            .notify_send(crate::devmgr::SendToDevice::T2Packet(buf.to_vec()));
        (self.dev.send_to_device)(crate::devmgr::SendToDevice::T2Packet(buf.to_vec()));
    }
    fn send_ack(&self, buf: &[u8]) {
        // send_to_device already notifies the monitor. A second notify_send would log the ack twice.
        (self.dev.send_to_device)(crate::devmgr::SendToDevice::T2Ack(buf.to_vec()));
    }
    fn send(&self, cmd: &str, msg_type: i32, data: serde_json::Value) {
        let msg = crate::devmgr::SendToDevice::T2Clip {
            cmd: cmd.to_string(),
            msg_type,
            data,
        };
        self.dev.notify_send(msg.clone());
        (self.dev.send_to_device)(msg);
    }
    fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>) {
        self.handlers.lock().push(handler);
    }
    fn emit_data(&self, buf: &[u8]) {
        for h in self.handlers.lock().iter() {
            h(buf);
        }
        self.dev.notify_data(buf);
    }
}

struct T1Adapter {
    dev: Arc<ConnectedDevice>,
    handlers: Mutex<Vec<Box<dyn Fn(&[u8]) + Send + Sync>>>,
}

impl Thinq1Device for T1Adapter {
    fn id(&self) -> &str {
        &self.dev.id
    }
    fn meta(&self) -> &rethink_core::metadata::Metadata {
        &self.dev.meta
    }
    fn send(&self, body: serde_json::Value) {
        (self.dev.send_to_device)(crate::devmgr::SendToDevice::T1Json(body));
    }
    fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>) {
        self.handlers.lock().push(handler);
    }
    fn emit_data(&self, buf: &[u8]) {
        for h in self.handlers.lock().iter() {
            h(buf);
        }
        self.dev.notify_data(buf);
    }
}

pub struct HaBridge {
    ha: Arc<dyn HaConnection>,
    ha_devices: Mutex<HashMap<String, Arc<dyn DeviceHandler>>>,
    t2_adapters: Mutex<HashMap<String, Arc<T2Adapter>>>,
    t1_adapters: Mutex<HashMap<String, Arc<T1Adapter>>>,
}

impl HaBridge {
    pub fn new(ha: Arc<dyn HaConnection>) -> Arc<Self> {
        Arc::new(Self {
            ha: ha.clone(),
            ha_devices: Mutex::new(HashMap::new()),
            t2_adapters: Mutex::new(HashMap::new()),
            t1_adapters: Mutex::new(HashMap::new()),
        })
    }

    /// Wire HA MQTT setProperty / discovery onto this bridge (must share the same HaMqttSink
    /// instance as the MQTT client).
    pub fn attach_ha_mqtt_sink(self: &Arc<Self>, sink: &rethink_core::ha::HaMqttSink) {
        let b = self.clone();
        sink.on_set_property(move |id, prop, value| {
            b.set_property(id, prop, value);
        });
        let b2 = self.clone();
        sink.on_discovery(move || {
            b2.republish_all();
        });
    }

    pub fn has_device(&self, id: &str) -> bool {
        self.ha_devices.lock().contains_key(id)
    }

    #[allow(dead_code)] // handy for diagnostics / management extensions
    pub fn devices_snapshot(&self) -> Vec<String> {
        self.ha_devices.lock().keys().cloned().collect()
    }

    pub fn republish_all(&self) {
        // Clone Arcs out of the lock so publish_config (and nested MQTT work)
        // does not block set_property / new_device.
        let devices: Vec<_> = self.ha_devices.lock().values().cloned().collect();
        for d in devices {
            d.publish_config();
        }
    }

    pub fn set_property(&self, id: &str, prop: &str, value: &str) {
        if let Some(d) = self.ha_devices.lock().get(id) {
            d.set_property(prop, value);
        }
    }

    pub fn new_device(self: &Arc<Self>, thinqdev: Arc<ConnectedDevice>) {
        if let Some(old) = self.ha_devices.lock().remove(&thinqdev.id) {
            old.drop_device();
        }

        let hadevice: Option<Arc<dyn DeviceHandler>> = match thinqdev.platform {
            Platform::Thinq1 => {
                if let Some(factory) = lookup_t1(&thinqdev.meta.model_id) {
                    let adapter = Arc::new(T1Adapter {
                        dev: thinqdev.clone(),
                        handlers: Mutex::new(Vec::new()),
                    });
                    let ad = adapter.clone();
                    thinqdev.add_data_handler(move |buf| {
                        for h in ad.handlers.lock().iter() {
                            h(buf);
                        }
                    });
                    self.t1_adapters
                        .lock()
                        .insert(thinqdev.id.clone(), adapter.clone());
                    Some(factory(
                        self.ha.clone(),
                        adapter as Arc<dyn Thinq1Device>,
                        thinqdev.meta.clone(),
                    ))
                } else {
                    None
                }
            }
            Platform::Thinq2 => {
                if let Some(factory) = lookup_t2(&thinqdev.meta.model_id) {
                    let adapter = Arc::new(T2Adapter {
                        dev: thinqdev.clone(),
                        handlers: Mutex::new(Vec::new()),
                    });
                    let ad = adapter.clone();
                    thinqdev.add_data_handler(move |buf| {
                        for h in ad.handlers.lock().iter() {
                            h(buf);
                        }
                    });
                    self.t2_adapters
                        .lock()
                        .insert(thinqdev.id.clone(), adapter.clone());
                    Some(factory(
                        self.ha.clone(),
                        adapter as Arc<dyn Thinq2Device>,
                        thinqdev.meta.clone(),
                    ))
                } else {
                    None
                }
            }
        };

        let Some(hadevice) = hadevice else {
            eprintln!(
                "{:?} device type {} unknown",
                thinqdev.platform, thinqdev.meta.model_id
            );
            return;
        };

        let id = thinqdev.id.clone();
        self.ha_devices.lock().insert(id.clone(), hadevice.clone());
        // Not cleared on drop: this connection is not reused outside the handler.
        thinqdev.set_managed(true);
        let bridge = self.clone();
        // Only tear down HA when *this* ConnectedDevice closes — not when a
        // superseded reconnect's stale close handler runs after re-accept.
        let thinq_for_close = thinqdev.clone();
        thinqdev.add_close_handler(move || {
            let still_ours = {
                let t2 = bridge.t2_adapters.lock();
                if let Some(a) = t2.get(&id) {
                    Arc::ptr_eq(&a.dev, &thinq_for_close)
                } else {
                    drop(t2);
                    bridge
                        .t1_adapters
                        .lock()
                        .get(&id)
                        .map(|a| Arc::ptr_eq(&a.dev, &thinq_for_close))
                        .unwrap_or(false)
                }
            };
            if !still_ours {
                return;
            }
            if let Some(ha) = bridge.ha_devices.lock().remove(&id) {
                ha.drop_device();
            }
            bridge.t2_adapters.lock().remove(&id);
            bridge.t1_adapters.lock().remove(&id);
        });
        hadevice.start();
    }
}
