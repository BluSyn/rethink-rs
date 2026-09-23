//! Core rethink types: config, HA MQTT, ThinQ1/2 device plumbing, device bases.

pub mod config;
pub mod hassio;
pub mod device_base;
pub mod ha;
pub mod logging;
pub mod metadata;
pub mod thinq;

pub use ha::{
    notification_event, problem_binary_sensor, DeviceDiscovery, HaConnection, MockHaConnection,
    PropertyValue,
};
pub use metadata::Metadata;
pub use device_base::{unwrap_aabb, wrap_aabb};
pub use thinq::{
    hex_decode, hex_encode, MockThinq1Device, MockThinq2Device, Thinq1Device, Thinq2Device,
};
