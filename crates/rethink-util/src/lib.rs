//! Pure protocol utilities for rethink — LG ThinQ de-cloud codecs.
//!
//! These modules have no I/O dependencies and are safe to unit-test in isolation.

pub mod aabb_analysis;
pub mod crc16;
pub mod hex;
pub mod json_splitter;
pub mod length_prefixed_frame;
pub mod mtosp;
pub mod packet_codec;
pub mod sync;
pub mod tlv;
pub mod tlv_catalog;
pub mod uart_binary;
pub mod wire;

pub use aabb_analysis::{aabb_export_text, analyze_aabb_body, AabbAnalysis};
pub use crc16::crc16;
pub use packet_codec::{decode_packet, encode_packet, Decoded, Direction, EncodeInput, Protocol};
pub use sync::{Mutex, RwLock};
pub use tlv::{build as tlv_build, parse as tlv_parse, Tlv};
pub use uart_binary::{analyze_uart_binary, uart_binary_export_text, UartBinaryAnalysis};
