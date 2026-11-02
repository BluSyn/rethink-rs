//! Device handlers and modelId registry for rethink.

pub mod ac_tables;
pub mod device_trait;
pub mod devices;
pub mod fridge_common;
pub mod js;
pub mod registry;
pub mod tl_us_washer_courses;
pub mod washer_common;

#[cfg(test)]
pub mod test_support;

pub use device_trait::DeviceHandler;
pub use registry::{all_t1_model_ids, all_t2_model_ids, t1_factory, t2_factory};
