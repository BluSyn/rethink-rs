//! Home Assistant MQTT discovery types and connection trait.

use crate::config::HaConfig;
use rethink_util::sync::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub identifiers: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sw_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OriginInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub support_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sw_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailabilityInfo {
    pub topic: String,
}

/// Sticky problem-style binary sensor component for device discovery.
///
/// Prefer this over MQTT device_automation triggers: it shows as a normal
/// entity on the device and automations use state ON/OFF (reliable in HA UI).
pub fn problem_binary_sensor(object_id: &str, name: &str) -> (String, Value) {
    (
        object_id.into(),
        json!({
            "platform": "binary_sensor",
            "unique_id": format!("$deviceid-{object_id}"),
            "name": name,
            "device_class": "problem",
            "payload_on": "ON",
            "payload_off": "OFF",
            "state_topic": format!("$this/{object_id}"),
        }),
    )
}

/// One-shot MQTT event entity component for device discovery.
///
/// Prefer this for moment notifications (cycle complete, …). Automate on the
/// event entity in HA; payload must be JSON `{"event_type":"..."}` (non-retained).
///
/// `device_class` must be a HA **Event** class only: `doorbell`, `button`, or
/// `motion` (or `None`). Do **not** use binary_sensor classes like `problem` —
/// MQTT discovery rejects them for event entities.
pub fn notification_event(
    object_id: &str,
    name: &str,
    event_types: &[&str],
    device_class: Option<&str>,
) -> (String, Value) {
    let mut body = json!({
        "platform": "event",
        "unique_id": format!("$deviceid-{object_id}"),
        "name": name,
        "state_topic": format!("$this/events/{object_id}"),
        "event_types": event_types,
    });
    // HA EventDeviceClass: doorbell | button | motion only.
    if let Some(dc) = device_class {
        if matches!(dc, "doorbell" | "button" | "motion") {
            body.as_object_mut()
                .unwrap()
                .insert("device_class".into(), json!(dc));
        }
    }
    (object_id.into(), body)
}

/// Device discovery document (HA MQTT discovery).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceDiscovery {
    pub device: DeviceInfo,
    pub origin: OriginInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability: Option<Vec<AvailabilityInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability_mode: Option<String>,
    pub components: HashMap<String, Value>,
}

/// Trait for publishing to HA MQTT (real connection or mock).
pub trait HaConnection: Send + Sync {
    fn publish_config(&self, id: &str, config: &DeviceDiscovery);
    fn publish_property(&self, id: &str, property: &str, value: PropertyValue);
    /// Fire a one-shot event payload. Not retained.
    fn publish_event(&self, id: &str, topic_suffix: &str, payload: &str);
    fn is_connected(&self) -> bool;

    /// Fire an MQTT event-entity payload: `events/{object_id}` ← `{"event_type":…}`.
    /// Non-retained (HA discards retained event replays).
    fn fire_notification_event(&self, id: &str, object_id: &str, event_type: &str) {
        let body = json!({ "event_type": event_type }).to_string();
        self.publish_event(id, &format!("events/{object_id}"), &body);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    Str(String),
    Num(f64),
    Int(i64),
}

impl PropertyValue {
    pub fn as_string(&self) -> String {
        match self {
            PropertyValue::Str(s) => s.clone(),
            PropertyValue::Num(n) => {
                if *n == (*n as i64) as f64 {
                    format!("{}", *n as i64)
                } else {
                    n.to_string()
                }
            }
            PropertyValue::Int(i) => i.to_string(),
        }
    }

    pub fn from_num(n: impl Into<f64>) -> Self {
        let v = n.into();
        if v == (v as i64) as f64 && v.abs() < 1e15 {
            PropertyValue::Int(v as i64)
        } else {
            PropertyValue::Num(v)
        }
    }
}

impl From<&str> for PropertyValue {
    fn from(s: &str) -> Self {
        PropertyValue::Str(s.into())
    }
}

impl From<String> for PropertyValue {
    fn from(s: String) -> Self {
        PropertyValue::Str(s)
    }
}

impl From<i32> for PropertyValue {
    fn from(n: i32) -> Self {
        PropertyValue::Int(n as i64)
    }
}

impl From<i64> for PropertyValue {
    fn from(n: i64) -> Self {
        PropertyValue::Int(n)
    }
}

impl From<u32> for PropertyValue {
    fn from(n: u32) -> Self {
        PropertyValue::Int(n as i64)
    }
}

impl From<f64> for PropertyValue {
    fn from(n: f64) -> Self {
        PropertyValue::from_num(n)
    }
}

/// Mock HA connection for unit tests.
#[derive(Default)]
pub struct MockHaConnection {
    inner: Mutex<MockHaInner>,
    set_handlers: Mutex<Vec<Box<dyn Fn(&str, &str, &str) + Send + Sync>>>,
}

#[derive(Default)]
struct MockHaInner {
    devices: HashMap<String, MockDeviceInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct MockDeviceInfo {
    pub config: Option<DeviceDiscovery>,
    pub availability: Option<String>,
    pub properties: HashMap<String, PropertyValue>,
    /// (topic_suffix, payload) non-retained events
    pub events: Vec<(String, String)>,
}

impl MockHaConnection {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn on_set_property<F>(&self, f: F)
    where
        F: Fn(&str, &str, &str) + Send + Sync + 'static,
    {
        self.set_handlers.lock().push(Box::new(f));
    }

    pub fn devices(&self) -> HashMap<String, MockDeviceInfo> {
        self.inner.lock().devices.clone()
    }

    pub fn device(&self, id: &str) -> Option<MockDeviceInfo> {
        self.inner.lock().devices.get(id).cloned()
    }

    pub fn get_property(&self, id: &str, component: &str, topic_id: &str) -> Option<PropertyValue> {
        let dev = self.device(id)?;
        let topic = self.lookup_topic(&dev, component, topic_id)?;
        dev.properties.get(&topic).cloned()
    }

    pub fn lookup_topic(&self, dev: &MockDeviceInfo, component: &str, topic_id: &str) -> Option<String> {
        let comp = dev.config.as_ref()?.components.get(component)?;
        let key = format!("{topic_id}_topic");
        let topic = comp.get(&key)?.as_str()?;
        Some(topic.strip_prefix("$this/").unwrap_or(topic).to_string())
    }

    pub fn set_property(&self, id: &str, component: &str, topic_id: &str, value: &str) {
        let dev = match self.device(id) {
            Some(d) => d,
            None => return,
        };
        let topic = match self.lookup_topic(&dev, component, topic_id) {
            Some(t) => t,
            None => return,
        };
        let prop = topic.strip_suffix("/set").unwrap_or(&topic);
        for h in self.set_handlers.lock().iter() {
            h(id, prop, value);
        }
    }

    pub fn emit_set_property(&self, id: &str, prop: &str, value: &str) {
        for h in self.set_handlers.lock().iter() {
            h(id, prop, value);
        }
    }
}

impl HaConnection for MockHaConnection {
    fn publish_config(&self, id: &str, config: &DeviceDiscovery) {
        let mut inner = self.inner.lock();
        let entry = inner.devices.entry(id.to_string()).or_default();
        entry.config = Some(config.clone());
    }

    fn publish_property(&self, id: &str, property: &str, value: PropertyValue) {
        let mut inner = self.inner.lock();
        let entry = inner.devices.entry(id.to_string()).or_default();
        if property == "availability" {
            entry.availability = Some(value.as_string());
        } else {
            entry.properties.insert(property.to_string(), value);
        }
    }

    fn publish_event(&self, id: &str, topic_suffix: &str, payload: &str) {
        let mut inner = self.inner.lock();
        let entry = inner.devices.entry(id.to_string()).or_default();
        entry
            .events
            .push((topic_suffix.to_string(), payload.to_string()));
    }

    fn is_connected(&self) -> bool {
        true
    }
}

/// Shared sink for a real MQTT-backed HA connection (filled by rethink-cloud).
pub struct HaMqttSink {
    pub config: HaConfig,
    pub published_availability: Mutex<HashSet<String>>,
    /// Callback to publish raw MQTT: (topic, payload, retain)
    pub publish_fn: Mutex<Option<Box<dyn Fn(&str, &[u8], bool) + Send + Sync>>>,
    pub set_handlers: Mutex<Vec<Box<dyn Fn(&str, &str, &str) + Send + Sync>>>,
    pub discovery_handlers: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
    pub connected: Mutex<bool>,
}

impl HaMqttSink {
    pub fn new(config: HaConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            published_availability: Mutex::new(HashSet::new()),
            publish_fn: Mutex::new(None),
            set_handlers: Mutex::new(Vec::new()),
            discovery_handlers: Mutex::new(Vec::new()),
            connected: Mutex::new(false),
        })
    }

    pub fn set_publish_fn<F>(&self, f: F)
    where
        F: Fn(&str, &[u8], bool) + Send + Sync + 'static,
    {
        *self.publish_fn.lock() = Some(Box::new(f));
    }

    fn do_publish(&self, topic: &str, payload: &[u8], retain: bool) {
        if let Some(f) = self.publish_fn.lock().as_ref() {
            f(topic, payload, retain);
        } else {
            // Once is enough — startup race if devices connect before HA client installs publish_fn.
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    target: "rethink_ha",
                    %topic,
                    retain,
                    "HA MQTT publish_fn not set; dropping publish (will not warn again)"
                );
            }
        }
    }

    pub fn on_set_property<F>(&self, f: F)
    where
        F: Fn(&str, &str, &str) + Send + Sync + 'static,
    {
        self.set_handlers.lock().push(Box::new(f));
    }

    pub fn on_discovery<F>(&self, f: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.discovery_handlers.lock().push(Box::new(f));
    }

    pub fn emit_discovery(&self) {
        for h in self.discovery_handlers.lock().iter() {
            h();
        }
    }

    pub fn handle_message(&self, topic: &str, message: &[u8], retain: bool) {
        let msg = String::from_utf8_lossy(message);
        if msg == "online"
            && topic
                .strip_prefix(self.config.discovery_prefix.as_str())
                .is_some_and(|rest| rest == "/status")
        {
            self.emit_discovery();
        }
        let Some(rest) = topic
            .strip_prefix(self.config.rethink_prefix.as_str())
            .and_then(|r| r.strip_prefix('/'))
        else {
            return;
        };
        if let Some(without_set) = rest.strip_suffix("/set") {
            if let Some((id, prop)) = without_set.split_once('/') {
                for h in self.set_handlers.lock().iter() {
                    h(id, prop, &msg);
                }
            }
        }
        if retain && msg == "online" {
            if let Some(id) = rest.strip_suffix("/availability") {
                if !id.is_empty() && !id.contains('/') && !self.published_availability.lock().contains(id)
                {
                    self.do_publish(topic, b"offline", true);
                }
            }
        }
    }
}

#[cfg(test)]
mod ha_mqtt_sink_tests {
    use super::*;
    use crate::config::HaConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_cfg() -> HaConfig {
        HaConfig {
            mqtt_url: "mqtt://127.0.0.1:1883".into(),
            discovery_prefix: "homeassistant".into(),
            rethink_prefix: "rethink".into(),
            mqtt_user: String::new(),
            mqtt_pass: String::new(),
        }
    }

    #[test]
    fn set_property_handlers_fire_on_handle_message() {
        let sink = HaMqttSink::new(test_cfg());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        sink.on_set_property(move |id, prop, val| {
            assert_eq!(id, "dev1");
            assert_eq!(prop, "climate-mode");
            assert_eq!(val, "heat");
            h.fetch_add(1, Ordering::SeqCst);
        });
        sink.handle_message("rethink/dev1/climate-mode/set", b"heat", false);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn set_property_joins_nested_path_without_vec_split() {
        let sink = HaMqttSink::new(test_cfg());
        let got = Arc::new(Mutex::new(None));
        let g = got.clone();
        sink.on_set_property(move |id, prop, val| {
            *g.lock() = Some((id.to_string(), prop.to_string(), val.to_string()));
        });
        sink.handle_message("rethink/dev1/upper/remote_temperature/set", b"350", false);
        assert_eq!(
            got.lock().clone(),
            Some(("dev1".into(), "upper/remote_temperature".into(), "350".into()))
        );
    }

    #[test]
    fn stale_availability_only_for_device_root_topic() {
        let sink = HaMqttSink::new(test_cfg());
        let pubs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let p = pubs.clone();
        sink.set_publish_fn(move |topic, _, _| {
            p.lock().push(topic.to_string());
        });
        // Nested availability is not the device root; must not force offline.
        sink.handle_message("rethink/dev1/extra/availability", b"online", true);
        assert!(pubs.lock().is_empty());
        sink.handle_message("rethink/dev1/availability", b"online", true);
        assert_eq!(pubs.lock().as_slice(), ["rethink/dev1/availability"]);
    }

    #[test]
    fn discovery_status_online_fires_without_format_alloc_on_mismatch() {
        let sink = HaMqttSink::new(test_cfg());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        sink.on_discovery(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
        sink.handle_message("homeassistant/status", b"online", false);
        sink.handle_message("homeassistant/status", b"offline", false);
        sink.handle_message("other/status", b"online", false);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn publish_fn_receives_publish_property() {
        let sink = HaMqttSink::new(test_cfg());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        sink.set_publish_fn(move |topic, payload, retain| {
            assert!(topic.contains("rethink/dev1/power"));
            assert_eq!(payload, b"ON");
            assert!(retain);
            h.fetch_add(1, Ordering::SeqCst);
        });
        use crate::ha::HaConnection;
        sink.publish_property("dev1", "power", "ON".into());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn notification_entities_and_events_publish() {
        use crate::ha::{
            notification_event, problem_binary_sensor, DeviceDiscovery, DeviceInfo, HaConnection,
            OriginInfo,
        };
        use std::sync::Mutex as StdMutex;

        let sink = HaMqttSink::new(test_cfg());
        let pubs: Arc<StdMutex<Vec<(String, String, bool)>>> = Arc::new(StdMutex::new(Vec::new()));
        let p = pubs.clone();
        sink.set_publish_fn(move |topic, payload, retain| {
            p.lock().unwrap().push((
                topic.to_string(),
                String::from_utf8_lossy(payload).into_owned(),
                retain,
            ));
        });

        let mut components = HashMap::new();
        let (k, v) = problem_binary_sensor("bucket_full", "Bucket full");
        components.insert(k, v);
        let (k, v) = notification_event("cycle_complete", "Cycle complete", &["cycle_complete"], None);
        components.insert(k, v);

        let config = DeviceDiscovery {
            device: DeviceInfo {
                identifiers: json!(["$deviceid"]),
                manufacturer: Some("LG".into()),
                model: Some("DHUM".into()),
                sw_version: None,
                name: Some("Dehumidifier".into()),
            },
            origin: OriginInfo {
                name: "rethink".into(),
                support_url: Some("https://example.invalid/rethink".into()),
                sw_version: None,
            },
            availability: None,
            availability_mode: None,
            components,
        };
        sink.publish_config("dev-xyz", &config);

        let logged = pubs.lock().unwrap().clone();
        assert!(
            logged.iter().any(|(t, body, retain)| {
                t == "homeassistant/device/rethink/dev-xyz/config"
                    && *retain
                    && body.contains("\"identifiers\":[\"dev-xyz\"]")
                    && body.contains("\"platform\":\"binary_sensor\"")
                    && body.contains("bucket_full")
                    && body.contains("\"platform\":\"event\"")
                    && body.contains("cycle_complete")
                    && !body.contains("device_automation")
            }),
            "entity-based notifications missing from device discovery: {logged:?}"
        );

        pubs.lock().unwrap().clear();
        sink.fire_notification_event("dev-xyz", "cycle_complete", "cycle_complete");
        let logged = pubs.lock().unwrap().clone();
        assert!(
            logged.iter().any(|(t, body, retain)| {
                t == "rethink/dev-xyz/events/cycle_complete"
                    && body.contains("\"event_type\":\"cycle_complete\"")
                    && !*retain
            }),
            "non-retained notification event missing: {logged:?}"
        );
    }
}

impl HaConnection for HaMqttSink {
    fn publish_config(&self, id: &str, config: &DeviceDiscovery) {
        let discovery_topic = format!(
            "{}/device/rethink/{}/config",
            self.config.discovery_prefix, id
        );
        let device_topic = format!("{}/{}", self.config.rethink_prefix, id);
        let replacements = [
            ("$this", device_topic.as_str()),
            ("$rethink", self.config.rethink_prefix.as_str()),
            ("$deviceid", id),
        ];
        let mut payload = recursive_replace(
            &serde_json::to_value(config).unwrap_or(json!({})),
            &replacements,
        );
        normalize_device_identifiers(&mut payload);

        // Drop leftover nested device_automation trigger_* keys from older builds
        // so HA does not re-register broken device triggers from retained history.
        if let Some(comps) = payload
            .as_object_mut()
            .and_then(|o| o.get_mut("components"))
            .and_then(|c| c.as_object_mut())
        {
            comps.retain(|k, v| {
                !(k.starts_with("trigger_")
                    || v.get("platform").and_then(|p| p.as_str()) == Some("device_automation"))
            });
        }

        let body = serde_json::to_vec(&payload).unwrap_or_default();
        // Retain so HA recovers after broker/HA restart without waiting for birth.
        self.do_publish(&discovery_topic, &body, true);
    }

    fn publish_property(&self, id: &str, property: &str, value: PropertyValue) {
        if property == "availability" {
            self.published_availability.lock().insert(id.to_string());
        }
        let topic = format!("{}/{}/{}", self.config.rethink_prefix, id, property);
        let payload = value.as_string();
        self.do_publish(&topic, payload.as_bytes(), true);
    }

    fn publish_event(&self, id: &str, topic_suffix: &str, payload: &str) {
        let topic = format!("{}/{}/{}", self.config.rethink_prefix, id, topic_suffix);
        self.do_publish(&topic, payload.as_bytes(), false);
    }

    fn is_connected(&self) -> bool {
        *self.connected.lock()
    }
}

fn recursive_replace(val: &Value, replacements: &[(&str, &str)]) -> Value {
    match val {
        Value::Array(a) => Value::Array(a.iter().map(|v| recursive_replace(v, replacements)).collect()),
        Value::Object(o) => {
            let mut m = serde_json::Map::new();
            for (k, v) in o {
                m.insert(k.clone(), recursive_replace(v, replacements));
            }
            Value::Object(m)
        }
        Value::String(s) => {
            let mut out = s.clone();
            for (pat, rep) in replacements {
                out = out.replace(pat, rep);
            }
            Value::String(out)
        }
        other => other.clone(),
    }
}

/// HA accepts string or list for identifiers; always emit a list so triggers and
/// entities share a stable multi-value identity in the device registry.
fn normalize_device_identifiers(payload: &mut Value) {
    if let Some(dev) = payload.get_mut("device") {
        normalize_device_identifiers_obj(dev);
    }
}

fn normalize_device_identifiers_obj(device: &mut Value) {
    let Some(obj) = device.as_object_mut() else {
        return;
    };
    match obj.get("identifiers").cloned() {
        Some(Value::String(s)) => {
            obj.insert("identifiers".into(), json!([s]));
        }
        Some(Value::Array(_)) => {}
        Some(other) => {
            obj.insert("identifiers".into(), json!([other]));
        }
        None => {}
    }
}
