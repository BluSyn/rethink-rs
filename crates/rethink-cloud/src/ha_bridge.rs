//! Wire connected ThinQ devices to HA device handlers via the registry.

use crate::devmgr::{ConnectedDevice, Platform};
use rethink_util::sync::Mutex;
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::thinq::{Thinq1Device, Thinq2Device};
use rethink_devices::device_trait::DeviceHandler;
use rethink_devices::registry::{lookup_t1, lookup_t2};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How long a closed device stays available in HA. A ThinQ2 module often opens a new MQTT session
/// before the old socket has closed; publishing `offline` for the dying session makes Home Assistant
/// flip on/unavailable at the same timestamp (e.g. a washer every couple of minutes mid-cycle).
pub const AVAILABILITY_GRACE: Duration = Duration::from_secs(5);

/// Adds the Wi-Fi signal and network sensors (the official app shows both on the device info page)
/// to a ThinQ2 handler's discovery document, so the device handlers need no changes.
struct WifiDiagnostics {
    inner: Arc<dyn HaConnection>,
    ssid: Option<String>,
}

impl HaConnection for WifiDiagnostics {
    fn publish_config(&self, id: &str, config: &DeviceDiscovery) {
        let mut config = config.clone();
        config.components.insert(
            "wifirssi".into(),
            serde_json::json!({
                "platform": "sensor",
                "unique_id": "$deviceid-wifirssi",
                "state_topic": "$this/wifirssi",
                "name": "Wi-Fi signal",
                "device_class": "signal_strength",
                "unit_of_measurement": "dBm",
                "state_class": "measurement",
                "entity_category": "diagnostic",
            }),
        );
        if self.ssid.is_some() {
            config.components.insert(
                "wifissid".into(),
                serde_json::json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-wifissid",
                    "state_topic": "$this/wifissid",
                    "name": "Wi-Fi network",
                    "icon": "mdi:wifi",
                    "entity_category": "diagnostic",
                }),
            );
        }
        self.inner.publish_config(id, &config);
        if let Some(ssid) = &self.ssid {
            self.inner.publish_property(id, "wifissid", ssid.as_str().into());
        }
    }
    fn publish_property(&self, id: &str, property: &str, value: PropertyValue) {
        self.inner.publish_property(id, property, value);
    }
    fn publish_event(&self, id: &str, topic_suffix: &str, payload: &str) {
        self.inner.publish_event(id, topic_suffix, payload);
    }
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
}

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
    fn on_send_packet(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>) {
        self.dev.add_send_handler(move |msg| {
            if let crate::devmgr::SendToDevice::T2Packet(buf) = msg {
                handler(&buf);
            }
        });
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
    /// Bumped per device id on every registration and close; a deferred offline only fires if its
    /// epoch is still current.
    offline_epochs: Mutex<HashMap<String, u64>>,
    availability_grace: Duration,
}

impl HaBridge {
    pub fn new(ha: Arc<dyn HaConnection>) -> Arc<Self> {
        Self::with_availability_grace(ha, AVAILABILITY_GRACE)
    }

    pub fn with_availability_grace(ha: Arc<dyn HaConnection>, grace: Duration) -> Arc<Self> {
        Arc::new(Self {
            ha: ha.clone(),
            ha_devices: Mutex::new(HashMap::new()),
            t2_adapters: Mutex::new(HashMap::new()),
            t1_adapters: Mutex::new(HashMap::new()),
            offline_epochs: Mutex::new(HashMap::new()),
            availability_grace: grace,
        })
    }

    /// Invalidate any pending offline for `id` and return the new epoch.
    fn bump_offline_epoch(&self, id: &str) -> u64 {
        let mut epochs = self.offline_epochs.lock();
        let epoch = epochs.entry(id.to_string()).or_insert(0);
        *epoch += 1;
        *epoch
    }

    /// Publish `offline` for a closed device after the grace period, unless it re-registered.
    fn schedule_offline(self: &Arc<Self>, id: String, handler: Arc<dyn DeviceHandler>) {
        let epoch = self.bump_offline_epoch(&id);
        let bridge = self.clone();
        let fire = move || {
            // Held across the publish so a racing new_device() orders its `online` after this.
            let epochs = bridge.offline_epochs.lock();
            if epochs.get(&id) == Some(&epoch) {
                handler.drop_device();
            }
        };
        let grace = self.availability_grace;
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    tokio::time::sleep(grace).await;
                    fire();
                });
            }
            Err(_) => {
                std::thread::spawn(move || {
                    std::thread::sleep(grace);
                    fire();
                });
            }
        }
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
        let old = self.ha_devices.lock().remove(&thinqdev.id);
        self.bump_offline_epoch(&thinqdev.id);

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
                    let ha: Arc<dyn HaConnection> = Arc::new(WifiDiagnostics {
                        inner: self.ha.clone(),
                        ssid: thinqdev.home_ap_ssid(),
                    });
                    let (id, rssi_ha) = (thinqdev.id.clone(), ha.clone());
                    let last_rssi = Mutex::new(None::<f64>);
                    thinqdev.add_rssi_handler(move |rssi| {
                        if last_rssi.lock().replace(rssi) == Some(rssi) {
                            return;
                        }
                        let value = if rssi.fract() == 0.0 {
                            PropertyValue::Int(rssi as i64)
                        } else {
                            PropertyValue::Num(rssi)
                        };
                        rssi_ha.publish_property(&id, "wifirssi", value);
                    });
                    Some(factory(
                        ha,
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
            if let Some(old) = old {
                old.drop_device();
            }
            return;
        };
        // The new session is already here: retire the old handler without publishing offline.
        if let Some(old) = old {
            old.retire();
        }

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
            let removed = bridge.ha_devices.lock().remove(&id);
            bridge.t2_adapters.lock().remove(&id);
            bridge.t1_adapters.lock().remove(&id);
            if let Some(ha) = removed {
                ha.retire();
                bridge.schedule_offline(id.clone(), ha);
            }
        });
        hadevice.start();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devmgr::SendToDevice;
    use rethink_core::ha::{DeviceDiscovery, PropertyValue};
    use rethink_core::metadata::Metadata;

    /// Records every availability payload, in order.
    #[derive(Default)]
    struct AvailabilityLog(Mutex<Vec<String>>);

    impl AvailabilityLog {
        fn entries(&self) -> Vec<String> {
            self.0.lock().clone()
        }
    }

    impl HaConnection for AvailabilityLog {
        fn publish_config(&self, _id: &str, _config: &DeviceDiscovery) {}
        fn publish_property(&self, _id: &str, property: &str, value: PropertyValue) {
            if property == "availability" {
                self.0.lock().push(value.as_string());
            }
        }
        fn publish_event(&self, _id: &str, _topic_suffix: &str, _payload: &str) {}
        fn is_connected(&self) -> bool {
            true
        }
    }

    const GRACE: Duration = Duration::from_millis(50);
    const PAST_GRACE: Duration = Duration::from_millis(250);

    fn session(id: &str, model_id: &str) -> Arc<ConnectedDevice> {
        ConnectedDevice::new(
            id.into(),
            Platform::Thinq2,
            Metadata::new(model_id, model_id, "1.0"),
            Arc::new(|_: Vec<u8>| {}),
            Arc::new(|_: SendToDevice| {}),
        )
    }

    fn washer(id: &str) -> Arc<ConnectedDevice> {
        session(id, "F3L2CYU__")
    }

    fn bridge() -> (Arc<AvailabilityLog>, Arc<HaBridge>) {
        let log = Arc::new(AvailabilityLog::default());
        let bridge = HaBridge::with_availability_grace(log.clone(), GRACE);
        (log, bridge)
    }

    #[test]
    fn a_replaced_session_never_publishes_offline() {
        let (log, bridge) = bridge();
        let first = washer("w");
        bridge.new_device(first.clone());
        bridge.new_device(washer("w"));
        // The dying socket closes after its replacement registered.
        first.notify_close();
        std::thread::sleep(PAST_GRACE);
        assert_eq!(log.entries(), ["online", "online"]);
        assert!(bridge.has_device("w"));
    }

    #[test]
    fn a_closed_device_goes_offline_only_after_the_grace_period() {
        let (log, bridge) = bridge();
        let dev = washer("w");
        bridge.new_device(dev.clone());
        dev.notify_close();
        assert!(!bridge.has_device("w"));
        assert_eq!(log.entries(), ["online"]);
        std::thread::sleep(PAST_GRACE);
        assert_eq!(log.entries(), ["online", "offline"]);
    }

    #[test]
    fn a_reconnect_within_the_grace_period_stays_online() {
        let (log, bridge) = bridge();
        let dev = washer("w");
        bridge.new_device(dev.clone());
        dev.notify_close();
        bridge.new_device(washer("w"));
        std::thread::sleep(PAST_GRACE);
        assert_eq!(log.entries(), ["online", "online"]);
        assert!(bridge.has_device("w"));
    }

    #[test]
    fn a_second_close_after_a_reconnect_still_goes_offline_once() {
        let (log, bridge) = bridge();
        let first = washer("w");
        bridge.new_device(first.clone());
        first.notify_close();
        let second = washer("w");
        bridge.new_device(second.clone());
        second.notify_close();
        std::thread::sleep(PAST_GRACE);
        assert_eq!(log.entries(), ["online", "online", "offline"]);
    }

    #[test]
    fn a_replacement_without_a_handler_takes_the_old_one_offline() {
        let (log, bridge) = bridge();
        bridge.new_device(washer("w"));
        bridge.new_device(session("w", "NOT_A_KNOWN_MODEL"));
        assert_eq!(log.entries(), ["online", "offline"]);
        assert!(!bridge.has_device("w"));
    }

    #[tokio::test]
    async fn the_grace_timer_runs_on_the_tokio_runtime() {
        let (log, bridge) = bridge();
        let dev = washer("w");
        bridge.new_device(dev.clone());
        dev.notify_close();
        assert_eq!(log.entries(), ["online"]);
        tokio::time::sleep(PAST_GRACE).await;
        assert_eq!(log.entries(), ["online", "offline"]);
    }

    fn wifi_components(ha: &rethink_core::MockHaConnection) -> Vec<String> {
        let mut keys: Vec<String> = ha
            .device("w")
            .and_then(|d| d.config)
            .map(|c| c.components.into_keys().filter(|k| k.starts_with("wifi")).collect())
            .unwrap_or_default();
        keys.sort();
        keys
    }

    #[test]
    fn thinq2_handlers_get_wifi_signal_and_network_sensors() {
        let ha = rethink_core::MockHaConnection::new();
        let bridge = HaBridge::new(ha.clone());
        let dev = washer("w");
        dev.set_deploy(serde_json::json!({"data": {"appInfo": {"HomeApSsid": "HomeNet"}}}));
        bridge.new_device(dev.clone());
        assert_eq!(wifi_components(&ha), ["wifirssi", "wifissid"]);
        let config = ha.device("w").unwrap().config.unwrap();
        assert_eq!(config.components["wifirssi"]["device_class"], "signal_strength");
        assert_eq!(config.components["wifirssi"]["unit_of_measurement"], "dBm");
        assert_eq!(config.components["wifissid"]["entity_category"], "diagnostic");
        let prop = |name: &str| ha.device("w").unwrap().properties.get(name).cloned();
        assert_eq!(prop("wifissid"), Some("HomeNet".into()));
        dev.notify_rssi(-60.0);
        assert_eq!(prop("wifirssi"), Some(PropertyValue::Int(-60)));
        // A republish keeps the sensors; repeated readings are not re-sent.
        bridge.republish_all();
        assert_eq!(wifi_components(&ha), ["wifirssi", "wifissid"]);
    }

    #[test]
    fn without_deploy_info_only_the_signal_sensor_is_added() {
        let ha = rethink_core::MockHaConnection::new();
        let bridge = HaBridge::new(ha.clone());
        bridge.new_device(washer("w"));
        assert_eq!(wifi_components(&ha), ["wifirssi"]);
        assert!(ha.device("w").unwrap().properties.get("wifissid").is_none());
    }

    #[test]
    fn repeated_rssi_readings_are_published_once() {
        let log = Arc::new(RssiLog::default());
        let bridge = HaBridge::new(log.clone());
        let dev = washer("w");
        bridge.new_device(dev.clone());
        for rssi in [-60.0, -60.0, -61.5, -61.5, -60.0] {
            dev.notify_rssi(rssi);
        }
        assert_eq!(*log.0.lock(), ["-60", "-61.5", "-60"]);
    }

    /// Records every wifirssi payload, in order.
    #[derive(Default)]
    struct RssiLog(Mutex<Vec<String>>);

    impl HaConnection for RssiLog {
        fn publish_config(&self, _id: &str, _config: &DeviceDiscovery) {}
        fn publish_property(&self, _id: &str, property: &str, value: PropertyValue) {
            if property == "wifirssi" {
                self.0.lock().push(value.as_string());
            }
        }
        fn publish_event(&self, _id: &str, _topic_suffix: &str, _payload: &str) {}
        fn is_connected(&self) -> bool {
            true
        }
    }

    #[test]
    fn t2_send_packet_listeners_see_packets_only() {
        let dev = ConnectedDevice::new(
            "id".into(),
            Platform::Thinq2,
            Metadata::new("M", "M", "1"),
            Arc::new(|_buf: Vec<u8>| {}),
            Arc::new(|_msg: SendToDevice| {}),
        );
        let adapter = T2Adapter {
            dev: dev.clone(),
            handlers: Mutex::new(Vec::new()),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        adapter.on_send_packet(Box::new(move |buf| record.lock().push(buf.to_vec())));
        dev.notify_send(SendToDevice::T2Packet(vec![0xaa, 0x01]));
        dev.notify_send(SendToDevice::T2Ack(vec![0xaa, 0x02]));
        dev.notify_send(SendToDevice::T2Clip {
            cmd: "x".into(),
            msg_type: 1,
            data: serde_json::Value::Null,
        });
        assert_eq!(*seen.lock(), vec![vec![0xaa, 0x01]]);
    }
}
