//! Device handler trait and helpers for modelId registration.

use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::{Thinq1Device, Thinq2Device};
use std::any::Any;
use std::sync::Arc;

/// Platform a device runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Thinq1,
    Thinq2,
}

/// Trait implemented by every HA device handler. `Any` lets `take_over` recognise its own type.
pub trait DeviceHandler: Any + Send + Sync {
    fn id(&self) -> &str;
    fn start(&self);
    /// Stop background work without touching HA availability: the handler is being replaced by a
    /// new session, or its offline publish is deferred. `drop_device` must still work afterwards.
    fn retire(&self) {}
    /// Called before `start` when this handler replaces `previous` after a reconnect, to carry
    /// over state that only lives in rethink (choices made in HA but not sent yet).
    fn take_over(&self, _previous: &dyn DeviceHandler) {}
    fn drop_device(&self);
    fn set_property(&self, prop: &str, value: &str);
    fn publish_config(&self);
}

/// Factory for ThinQ2 devices.
pub type T2Factory = fn(Arc<dyn HaConnection>, Arc<dyn Thinq2Device>, Metadata) -> Arc<dyn DeviceHandler>;

/// Factory for ThinQ1 devices.
pub type T1Factory = fn(Arc<dyn HaConnection>, Arc<dyn Thinq1Device>, Metadata) -> Arc<dyn DeviceHandler>;

/// Helper: property equality for publish cache (string form).
pub fn prop_str(v: &PropertyValue) -> String {
    v.as_string()
}

/// Merge components into a base discovery config.
pub fn with_components(mut base: DeviceDiscovery, components: serde_json::Map<String, serde_json::Value>) -> DeviceDiscovery {
    for (k, v) in components {
        base.components.insert(k, v);
    }
    base
}
