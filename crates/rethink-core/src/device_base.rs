//! HA device base classes (HADevice, AABBDevice, TLVDevice).

use crate::ha::{DeviceDiscovery, DeviceInfo, HaConnection, OriginInfo, PropertyValue};
use crate::metadata::Metadata;
use crate::thinq::Thinq2Device;
use rethink_util::crc16::crc16;
use rethink_util::sync::Mutex;
use rethink_util::tlv::{self, Tlv};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// ThinQ TLV types are 10-bit (`0..=1023`). Direct slots beat HashMap on the
/// per-tag hot path (`process_key_value` / `get_raw`).
const TLV_ID_SLOTS: usize = 1024;

#[inline]
fn tlv_slot(id: u16) -> Option<usize> {
    let i = id as usize;
    (i < TLV_ID_SLOTS).then_some(i)
}

fn tlv_u32_slots() -> Vec<Option<u32>> {
    vec![None; TLV_ID_SLOTS]
}

fn tlv_field_slots() -> Vec<Option<Arc<FieldDefinition>>> {
    vec![None; TLV_ID_SLOTS]
}

/// Base HA device.
pub struct HaDeviceState {
    pub id: String,
    pub config: Option<DeviceDiscovery>,
}

pub fn default_config(meta: &Metadata, device_info: Option<Value>) -> DeviceDiscovery {
    let mut device = DeviceInfo {
        // Array form matches HA MQTT discovery examples (Zigbee2MQTT, docs)
        // and ensures device_automation triggers share the same registry identity.
        identifiers: json!(["$deviceid"]),
        manufacturer: Some("LG".into()),
        model: Some(meta.model_name.clone()),
        sw_version: meta.sw_version.clone(),
        name: None,
    };
    if let Some(Value::Object(extra)) = device_info {
        if let Some(Value::String(n)) = extra.get("name") {
            device.name = Some(n.clone());
        }
    }
    DeviceDiscovery {
        device,
        origin: OriginInfo {
            name: "rethink".into(),
            support_url: Some("https://github.com/anszom/rethink".into()),
            sw_version: None,
        },
        availability: Some(vec![
            crate::ha::AvailabilityInfo {
                topic: "$this/availability".into(),
            },
            crate::ha::AvailabilityInfo {
                topic: "$rethink/availability".into(),
            },
        ]),
        availability_mode: Some("all".into()),
        components: HashMap::new(),
    }
}

// ── AABB device ────────────────────────────────────────────────────────────

pub struct AabbDeviceCore {
    pub id: String,
    pub ha: Arc<dyn HaConnection>,
    pub thinq: Arc<dyn Thinq2Device>,
    pub publish_cache: Mutex<HashMap<String, String>>,
    pub config: Mutex<Option<DeviceDiscovery>>,
    /// Ack every frame the way the cloud does. Existing handlers leave this off.
    auto_ack: bool,
}

impl AabbDeviceCore {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>) -> Arc<Self> {
        Self::with_auto_ack(ha, thinq, false)
    }

    /// `auto_ack` should be true for new handlers. Pre-existing ones pass false so their
    /// traffic is unchanged; a bridged appliance with a handler does not also get cloud acks.
    pub fn with_auto_ack(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        auto_ack: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: thinq.id().to_string(),
            ha,
            thinq,
            publish_cache: Mutex::new(HashMap::new()),
            config: Mutex::new(None),
            auto_ack,
        })
    }

    pub fn send(&self, inner: &[u8]) {
        self.thinq.send_packet(&wrap_aabb(inner));
    }

    pub fn process_data_envelope<'a>(&self, buf: &'a [u8]) -> Option<&'a [u8]> {
        let inner = unwrap_aabb(buf)?;
        if self.auto_ack {
            if let Some(ack) = aabb_delivery_ack(inner, buf[1] == 0xff) {
                self.thinq.send_ack(&ack);
            }
        }
        Some(inner)
    }

    pub fn publish_on_off(&self, prop: &str, on: bool) {
        let s = if on { "ON" } else { "OFF" };
        if self.cache_is_current(prop, s) {
            return;
        }
        self.ha.publish_property(&self.id, prop, s.into());
    }

    pub fn publish_flag(&self, prop: &str, bits: u8, mask: u8) {
        self.publish_on_off(prop, bits & mask != 0);
    }

    /// Re-publish the last discovery document (HA config reload / reconnect).
    pub fn republish_config(&self) {
        republish_config(&*self.ha, &self.id, &self.config);
    }

    pub fn publish_property(&self, prop: &str, value: PropertyValue) {
        {
            let mut cache = self.publish_cache.lock();
            if let Some(prev) = cache.get(prop) {
                if value.eq_str(prev) {
                    return;
                }
            }
            cache.insert(prop.to_string(), value.as_string());
        }
        self.ha.publish_property(&self.id, prop, value);
    }

    /// True if `s` is already the cached value. On miss, stores `s` for next time.
    fn cache_is_current(&self, prop: &str, s: &str) -> bool {
        let mut cache = self.publish_cache.lock();
        if cache.get(prop).map(|c| c.as_str() == s).unwrap_or(false) {
            return true;
        }
        cache.insert(prop.to_string(), s.to_string());
        false
    }

    /// Publish fridge/freezer door binary state (automate on binary_sensor state).
    pub fn publish_door_with_trigger(&self, open: bool) {
        self.publish_on_off("door", open);
    }

    pub fn set_config(&self, config: DeviceDiscovery) {
        *self.config.lock() = Some(config.clone());
        self.ha
            .publish_property(&self.id, "availability", "online".into());
        self.ha.publish_config(&self.id, &config);
    }

    pub fn drop_device(&self) {
        self.ha
            .publish_property(&self.id, "availability", "offline".into());
    }
}

// ── TLV device ─────────────────────────────────────────────────────────────

pub type ReadXform = Box<dyn Fn(u32) -> Option<PropertyValue> + Send + Sync>;
pub type WriteXform = Box<dyn Fn(&str) -> Option<PropertyValue> + Send + Sync>;
pub type WriteAttach = Box<dyn Fn(u32) -> Vec<u16> + Send + Sync>;
pub type ReadCallback = Box<dyn Fn(&PropertyValue) -> bool + Send + Sync>;
pub type WriteCallback = Box<dyn Fn(u32) -> bool + Send + Sync>;

pub struct FieldDefinition {
    pub id: Option<u16>,
    pub name: String,
    pub comp: String,
    /// `{comp}-{name}` HA property key, computed once at construction.
    pub ha_key: String,
    pub state_topic: Option<String>,
    pub readable: bool,
    pub writable: bool,
    pub write_xform: Option<WriteXform>,
    pub write_attach: Option<WriteAttach>,
    pub write_attach_static: Option<Vec<u16>>,
    pub read_xform: Option<ReadXform>,
    pub read_callback: Option<ReadCallback>,
    pub write_callback: Option<WriteCallback>,
}

impl FieldDefinition {
    pub fn new(comp: &str, name: &str) -> Self {
        Self {
            id: None,
            name: name.into(),
            comp: comp.into(),
            ha_key: format!("{comp}-{name}"),
            state_topic: None,
            readable: true,
            writable: true,
            write_xform: None,
            write_attach: None,
            write_attach_static: None,
            read_xform: None,
            read_callback: None,
            write_callback: None,
        }
    }

    pub fn with_id(mut self, id: u16) -> Self {
        self.id = Some(id);
        self
    }

    pub fn read_only(mut self) -> Self {
        self.writable = false;
        self
    }

    pub fn write_only(mut self) -> Self {
        self.readable = false;
        self
    }
}

pub struct TlvDeviceCore {
    pub id: String,
    pub ha: Arc<dyn HaConnection>,
    pub thinq: Arc<dyn Thinq2Device>,
    pub config: Mutex<Option<DeviceDiscovery>>,
    pub fields_by_id: Mutex<Vec<Option<Arc<FieldDefinition>>>>,
    pub fields_by_ha: Mutex<HashMap<String, Arc<FieldDefinition>>>,
    pub raw_clip_state: Mutex<Vec<Option<u32>>>,
    /// Last published HA string per field key (skip identical MQTT retains).
    pub publish_cache: Mutex<HashMap<String, String>>,
    /// true while waiting for caps
    pub waiting_caps: AtomicBool,
    /// true while waiting for initial values
    pub waiting_values: AtomicBool,
    /// Hooks for subclasses
    pub on_caps: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pub on_values: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pub is_caps_response: Mutex<Option<Box<dyn Fn(&[Tlv]) -> bool + Send + Sync>>>,
    pub is_values_response: Mutex<Option<Box<dyn Fn(&[Tlv]) -> bool + Send + Sync>>>,
    pub on_priv_data: Mutex<Option<Box<dyn Fn(u8, u8, &[u8]) + Send + Sync>>>,
    pub on_priv_cmd_resp: Mutex<Option<Box<dyn Fn(bool, u8, u8, &[u8]) + Send + Sync>>>,
    pub on_key_value_extra: Mutex<Option<Box<dyn Fn(u16, u32) + Send + Sync>>>,
    /// If set and returns true, skip default process_key_value handling (including storage).
    pub key_value_intercept: Mutex<Option<Box<dyn Fn(u16, u32) -> bool + Send + Sync>>>,
}

impl TlvDeviceCore {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>) -> Arc<Self> {
        let core = Arc::new(Self {
            id: thinq.id().to_string(),
            ha,
            thinq: thinq.clone(),
            config: Mutex::new(None),
            fields_by_id: Mutex::new(tlv_field_slots()),
            fields_by_ha: Mutex::new(HashMap::new()),
            raw_clip_state: Mutex::new(tlv_u32_slots()),
            publish_cache: Mutex::new(HashMap::new()),
            waiting_caps: AtomicBool::new(true),
            waiting_values: AtomicBool::new(false),
            on_caps: Mutex::new(None),
            on_values: Mutex::new(None),
            is_caps_response: Mutex::new(None),
            is_values_response: Mutex::new(None),
            on_priv_data: Mutex::new(None),
            on_priv_cmd_resp: Mutex::new(None),
            on_key_value_extra: Mutex::new(None),
            key_value_intercept: Mutex::new(None),
        });
        let c = core.clone();
        thinq.on_data(Box::new(move |data| c.process_data(data)));
        // initial caps query
        core.query_caps();
        core
    }

    pub fn set_config(&self, config: DeviceDiscovery) {
        *self.config.lock() = Some(config.clone());
        self.ha
            .publish_property(&self.id, "availability", "online".into());
        self.ha.publish_config(&self.id, &config);
    }

    pub fn add_field(&self, config: &mut DeviceDiscovery, options: FieldDefinition, autoreg: bool) {
        let full_name = options.ha_key.clone();
        let options = Arc::new(options);
        if let Some(id) = options.id {
            if let Some(i) = tlv_slot(id) {
                self.fields_by_id.lock()[i] = Some(options.clone());
            }
        }
        self.fields_by_ha
            .lock()
            .insert(full_name.clone(), options.clone());

        if autoreg {
            let topic_prefix = if options.name.is_empty() {
                String::new()
            } else {
                format!("{}_", options.name)
            };
            let entry = config
                .components
                .entry(options.comp.clone())
                .or_insert_with(|| json!({}));
            if let Value::Object(map) = entry {
                if options.readable {
                    let state_topic = options.state_topic.as_deref().unwrap_or("state_topic");
                    map.insert(
                        format!("{topic_prefix}{state_topic}"),
                        json!(format!("$this/{full_name}")),
                    );
                }
                if options.writable {
                    map.insert(
                        format!("{topic_prefix}command_topic"),
                        json!(format!("$this/{full_name}/set")),
                    );
                }
            }
        }
    }

    pub fn query_caps(&self) {
        self.send(&[1, 1, 2, 2, 1], &[Tlv::new(0x1f5, 1)]);
    }

    pub fn query(&self) {
        self.send(&[1, 1, 2, 2, 1], &[Tlv::new(0x1f5, 2)]);
    }

    pub fn send(&self, header: &[u8], tlv_els: &[Tlv]) {
        let b0 = header[0];
        let b1 = header[1];
        let b2 = header.get(2).copied().unwrap_or(2);
        let b3 = header.get(3).copied().unwrap_or(2);
        let b4 = header.get(4).copied().unwrap_or(1);
        let tlv_array = tlv::build(tlv_els);
        let mut out = Vec::with_capacity(2 + 9 + tlv_array.len() + 2);
        out.extend_from_slice(&[
            b0,
            b1,
            0x04,
            0x00,
            0x00,
            0x00,
            0x65,
            b2,
            b3,
            b4,
            tlv_array.len() as u8,
        ]);
        out.extend_from_slice(&tlv_array);
        let result = crc16(&out[2..]);
        out.push((result >> 8) as u8);
        out.push((result & 0xff) as u8);
        self.thinq.send_packet(&out);
    }

    pub fn send_priv_command(&self, cmd: u8, cmd_sub: u8, data: &[u8]) {
        let cmd_data_len = data.len() + 1;
        let mut buf = vec![
            0x00,
            0xff,
            0x04,
            0x00,
            0x00,
            0x00,
            0x65,
            0xfd,
            cmd_sub,
            (cmd_data_len >> 8) as u8,
            (cmd_data_len & 0xff) as u8,
            cmd,
        ];
        buf.extend_from_slice(data);
        let crc = crc16(&buf[2..]);
        buf.push((crc >> 8) as u8);
        buf.push((crc & 0xff) as u8);
        self.thinq.send_packet(&buf);
    }

    pub fn process_data(&self, buf: &[u8]) {
        if buf.len() < 13 {
            return;
        }
        // Standard TLV fromDevice
        if buf[2] == 0x04
            && buf[3] == 0x00
            && buf[4] == 0x00
            && buf[5] == 0x00
            && (buf[6] == 0x87 || buf[6] == 0xa7)
            && buf[7] == 0x02
            && (buf[8] == 0x01 || buf[8] == 0x04)
            && buf[10] as usize == buf.len() - 13
        {
            let tlv = tlv::parse(&buf[11..buf.len() - 2]);
            self.process_tlv(&tlv);
            return;
        }
        // priv data
        if buf[1] == 0xff
            && buf[2] == 0x04
            && buf[3] == 0x00
            && buf[4] == 0x00
            && buf[5] == 0x00
            && buf[6] == 0x87
            && buf[7] == 0xfd
            && buf[8] == 0x03
            && buf[10] as usize == buf.len() - 13
        {
            if let Some(h) = self.on_priv_data.lock().as_ref() {
                h(buf[0], buf[9], &buf[11..buf.len() - 2]);
            }
            return;
        }
        // priv cmd response
        if (buf[0] == 0x02 || buf[0] == 0x03)
            && buf[2] == 0x04
            && buf[3] == 0x00
            && buf[4] == 0x00
            && buf[5] == 0x00
            && buf[6] == 0x87
            && buf[7] == 0xfd
            && buf[8] == 0x10
            && buf[9] == 0x00
            && buf[10] == 0x05
            && buf[11] == 0xfe
            && buf.len() > 12
        {
            if let Some(h) = self.on_priv_cmd_resp.lock().as_ref() {
                h(
                    buf[0] == 0x02,
                    buf[1],
                    buf[12],
                    &buf[13..buf.len().saturating_sub(2)],
                );
            }
        }
    }

    pub fn process_tlv(&self, tlv_array: &[Tlv]) {
        for el in tlv_array {
            self.process_key_value(el.t, el.v);
        }

        let mut waiting_caps = self.waiting_caps.load(Ordering::Relaxed);
        let is_caps = waiting_caps
            && self
                .is_caps_response
                .lock()
                .as_ref()
                .map(|f| f(tlv_array))
                .unwrap_or(false);
        let is_vals = self
            .is_values_response
            .lock()
            .as_ref()
            .map(|f| f(tlv_array))
            .unwrap_or(false);

        if waiting_caps && is_caps {
            self.waiting_caps.store(false, Ordering::Relaxed);
            waiting_caps = false;
            if let Some(h) = self.on_caps.lock().as_ref() {
                h();
            }
            self.query();
            self.waiting_values.store(true, Ordering::Relaxed);
        }

        if !waiting_caps && is_vals {
            self.waiting_values.store(false, Ordering::Relaxed);
            if let Some(h) = self.on_values.lock().as_ref() {
                h();
            }
        }
    }

    pub fn process_key_value(&self, k: u16, v: u32) {
        if let Some(h) = self.key_value_intercept.lock().as_ref() {
            if h(k, v) {
                return;
            }
        }
        if let Some(i) = tlv_slot(k) {
            self.raw_clip_state.lock()[i] = Some(v);
        }
        if let Some(h) = self.on_key_value_extra.lock().as_ref() {
            h(k, v);
        }
        let def = tlv_slot(k).and_then(|i| self.fields_by_id.lock()[i].clone());
        let Some(def) = def else { return };

        let mut processed = PropertyValue::Int(v as i64);
        if let Some(ref xform) = def.read_xform {
            match xform(v) {
                Some(p) => processed = p,
                None => return,
            }
        }
        let do_read = def
            .read_callback
            .as_ref()
            .map(|cb| cb(&processed))
            .unwrap_or(true);
        if do_read && def.readable {
            {
                let mut cache = self.publish_cache.lock();
                if let Some(prev) = cache.get(&def.ha_key) {
                    if processed.eq_str(prev) {
                        return;
                    }
                }
                cache.insert(def.ha_key.clone(), processed.as_string());
            }
            self.ha.publish_property(&self.id, &def.ha_key, processed);
        }
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        let def = self.fields_by_ha.lock().get(prop).cloned();
        let Some(def) = def else {
            eprintln!("Attempting to set property {prop} which is not writable");
            return;
        };
        if !def.writable {
            eprintln!("Attempting to set property {prop} which is not writable");
            return;
        }

        let value = if let Some(ref xform) = def.write_xform {
            match xform(mqtt_value) {
                Some(v) => v,
                None => return,
            }
        } else {
            PropertyValue::Str(mqtt_value.into())
        };

        let num = match &value {
            PropertyValue::Int(i) => *i as u32,
            PropertyValue::Num(n) => *n as u32,
            PropertyValue::Str(s) => match s.parse::<u32>() {
                Ok(n) => n,
                Err(_) => return,
            },
        };

        let do_write = def
            .write_callback
            .as_ref()
            .map(|cb| cb(num))
            .unwrap_or(true);
        if do_write {
            if let Some(id) = def.id {
                if let Some(i) = tlv_slot(id) {
                    self.raw_clip_state.lock()[i] = Some(num);
                }
                let mut attach = Vec::new();
                if let Some(ref a) = def.write_attach_static {
                    attach = a.clone();
                }
                if let Some(ref a) = def.write_attach {
                    attach = a(num);
                }
                let mut write_fields = vec![id];
                write_fields.extend(attach);
                let state = self.raw_clip_state.lock();
                let tlv_array: Vec<Tlv> = write_fields
                    .iter()
                    .map(|&fid| {
                        let v = tlv_slot(fid).and_then(|i| state[i]).unwrap_or(0);
                        Tlv::new(fid, v)
                    })
                    .collect();
                drop(state);
                self.send(&[1, 1, 2, 1, 1], &tlv_array);
            }
        }
    }

    pub fn drop_device(&self) {
        self.ha
            .publish_property(&self.id, "availability", "offline".into());
    }

    pub fn republish_config(&self) {
        republish_config(&*self.ha, &self.id, &self.config);
    }

    pub fn get_raw(&self, id: u16) -> Option<u32> {
        tlv_slot(id).and_then(|i| self.raw_clip_state.lock()[i])
    }

    pub fn set_raw(&self, id: u16, v: u32) {
        if let Some(i) = tlv_slot(id) {
            self.raw_clip_state.lock()[i] = Some(v);
        }
    }
}

// Frame types the cloud does not ack: the appliance's own acks, the c3 heartbeat, and eb/ec
// status records outside an envelope.
const UNACKED_TYPES: [u8; 4] = [0x00, 0xc3, 0xeb, 0xec];
const FRAME_TYPE_ENVELOPE: u8 = 0x0a;
const ENVELOPE_SEQ_OFFSET: usize = 5;
const ENVELOPE_MIN_LEN: usize = 13;

/// `f0 00 <type> 04 [<seq16>]` wrapped as an AABB frame, or `None` when the cloud would not ack.
///
/// `long_frame` is the `AA FF` form, whose 16-bit checksum leaves one extra byte on `inner`.
pub fn aabb_delivery_ack(inner: &[u8], long_frame: bool) -> Option<Vec<u8>> {
    if inner.len() < 2 {
        return None;
    }
    let ty = inner[1];
    if UNACKED_TYPES.contains(&ty) {
        return None;
    }
    let mut body = vec![0xf0, 0x00, ty, 0x04];
    if ty == FRAME_TYPE_ENVELOPE {
        let header_len = inner.len() - usize::from(long_frame);
        if header_len < ENVELOPE_MIN_LEN || inner.len() < ENVELOPE_SEQ_OFFSET + 2 {
            return None;
        }
        body.extend_from_slice(&inner[ENVELOPE_SEQ_OFFSET..ENVELOPE_SEQ_OFFSET + 2]);
    }
    Some(wrap_aabb(&body))
}

/// Wrap an AABB inner body as `AA <len> <inner> <csum> BB`.
pub fn wrap_aabb(inner: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(inner.len() + 4);
    packet.push(0xaa);
    packet.push((inner.len() + 4) as u8);
    packet.extend_from_slice(inner);
    let mut sum = 0u8;
    for &b in &packet {
        sum = sum.wrapping_add(b);
    }
    packet.push(sum ^ 0x55);
    packet.push(0xbb);
    packet
}

/// Strip `AA … BB` framing. Checksum is not validated (devices send it; tests often omit it).
///
/// Returns a borrow of `buf` (no copy). The slice excludes the leading `AA <len>`
/// and the trailing `<csum> BB`.
pub fn unwrap_aabb(buf: &[u8]) -> Option<&[u8]> {
    if buf.len() >= 4 && buf[0] == 0xaa && buf[buf.len() - 1] == 0xbb {
        Some(&buf[2..buf.len() - 2])
    } else {
        None
    }
}

/// `XX EB` is one status block. `XX EC` is previous then current; the current block is returned.
///
/// `status_length` is the size of one record. Returns the record the handler should read.
pub fn process_common_status(buf: &[u8], class_byte: u8, status_length: usize) -> Option<&[u8]> {
    if buf.len() < 2 || buf[0] != class_byte {
        return None;
    }
    if buf[1] == 0xeb && buf.len() == 2 + status_length {
        return Some(&buf[2..]);
    }
    if buf[1] == 0xec && buf.len() == 2 + status_length * 2 {
        return Some(&buf[2 + status_length..]);
    }
    None
}

/// EB/EC records wrapped in a `<class> 0a` envelope. Same current-record rule as
/// [`process_common_status`]. A single trailing checksum byte is allowed.
pub fn process_enveloped_status(buf: &[u8], class_byte: u8, status_length: usize) -> Option<&[u8]> {
    const ENVELOPE_MIN_LEN: usize = 13;
    const INNER_TYPE_OFFSET: usize = 10;
    const INNER_LEN_OFFSET: usize = 11;
    if buf.len() < ENVELOPE_MIN_LEN || buf[0] != class_byte || buf[1] != 0x0a {
        return None;
    }
    let inner_len = u16::from_be_bytes([buf[INNER_LEN_OFFSET], buf[INNER_LEN_OFFSET + 1]]) as usize;
    let trailing = buf.len() - ENVELOPE_MIN_LEN - inner_len;
    if trailing != 0 && trailing != 1 {
        return None;
    }
    let records = &buf[ENVELOPE_MIN_LEN..ENVELOPE_MIN_LEN + inner_len];
    match buf[INNER_TYPE_OFFSET] {
        0xeb if inner_len == status_length => Some(records),
        0xec if inner_len == status_length * 2 => Some(&records[status_length..]),
        _ => None,
    }
}

fn republish_config(ha: &dyn HaConnection, id: &str, config: &Mutex<Option<DeviceDiscovery>>) {
    if let Some(cfg) = config.lock().clone() {
        ha.publish_property(id, "availability", "online".into());
        ha.publish_config(id, &cfg);
    }
}

/// Helper to build a component JSON object.
pub fn component(platform: &str, unique_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("platform".into(), json!(platform));
    m.insert("unique_id".into(), json!(unique_id));
    m
}

pub fn insert_component(config: &mut DeviceDiscovery, key: &str, obj: Map<String, Value>) {
    config.components.insert(key.into(), Value::Object(obj));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_roundtrip() {
        let inner = vec![0x20, 0xde, 0x01, 0x02];
        let pkt = wrap_aabb(&inner);
        assert_eq!(pkt[0], 0xaa);
        assert_eq!(*pkt.last().unwrap(), 0xbb);
        assert_eq!(pkt[1] as usize, inner.len() + 4);
        assert_eq!(unwrap_aabb(&pkt), Some(inner.as_slice()));
        // Zero-copy: returned slice is a window into `pkt`.
        let inner_ref = unwrap_aabb(&pkt).unwrap();
        assert_eq!(inner_ref.as_ptr(), pkt[2..].as_ptr());
    }

    #[test]
    fn unwrap_rejects_short_or_unframed() {
        assert!(unwrap_aabb(&[]).is_none());
        assert!(unwrap_aabb(&[0xaa, 0x04, 0xbb]).is_none()); // len 3
        assert!(unwrap_aabb(&[0x00, 0x04, 0x00, 0xbb]).is_none());
        assert!(unwrap_aabb(&[0xaa, 0x04, 0x00, 0x00]).is_none());
    }

    #[test]
    fn laundry_status_query_frame_matches_known_hex() {
        let inner = [0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x00];
        let pkt = wrap_aabb(&inner);
        assert_eq!(
            crate::hex_encode(&pkt).to_ascii_uppercase(),
            "AA0EF0ED1121010000001800B5BB"
        );
    }

    #[test]
    fn wrap_aabb_checksum_matches_packet_codec() {
        let inner = [0x40, 0xeb, 0x00, 0x01, 0x02];
        let pkt = wrap_aabb(&inner);
        let without_csum = &pkt[..pkt.len() - 2];
        assert_eq!(
            pkt[pkt.len() - 2],
            rethink_util::packet_codec::aabb_checksum(without_csum)
        );
    }

    struct CountingHa {
        pubs: Mutex<Vec<(String, String)>>,
    }
    impl HaConnection for CountingHa {
        fn publish_config(&self, _id: &str, _config: &DeviceDiscovery) {}
        fn publish_property(&self, _id: &str, property: &str, value: PropertyValue) {
            self.pubs
                .lock()
                .push((property.to_string(), value.as_string()));
        }
        fn publish_event(&self, _id: &str, _topic_suffix: &str, _payload: &str) {}
        fn is_connected(&self) -> bool {
            true
        }
    }

    #[test]
    fn aabb_publish_on_off_skips_identical() {
        let ha = Arc::new(CountingHa {
            pubs: Mutex::new(Vec::new()),
        });
        let thinq = crate::MockThinq2Device::new("id", crate::Metadata::new("M", "M", "1"));
        let core = AabbDeviceCore::new(ha.clone(), thinq);
        core.publish_on_off("door", true);
        core.publish_on_off("door", true);
        core.publish_on_off("door", false);
        core.publish_property("fridge_setpoint", 3i64.into());
        core.publish_property("fridge_setpoint", 3i64.into());
        core.publish_property("fridge_setpoint", 4i64.into());
        let pubs = ha.pubs.lock().clone();
        assert_eq!(
            pubs,
            vec![
                ("door".into(), "ON".into()),
                ("door".into(), "OFF".into()),
                ("fridge_setpoint".into(), "3".into()),
                ("fridge_setpoint".into(), "4".into()),
            ]
        );
    }

    #[test]
    fn tlv_process_key_value_skips_identical_republish() {
        let ha = Arc::new(CountingHa {
            pubs: Mutex::new(Vec::new()),
        });
        let thinq = crate::MockThinq2Device::new("id", crate::Metadata::new("M", "M", "1"));
        let core = TlvDeviceCore::new(ha.clone(), thinq);
        let mut cfg = default_config(&crate::Metadata::new("M", "M", "1"), None);
        let mut field = FieldDefinition::new("power", "").with_id(0x1f7);
        field.read_xform = Some(Box::new(|raw| {
            Some(if raw != 0 { "ON".into() } else { "OFF".into() })
        }));
        core.add_field(&mut cfg, field, true);
        core.process_key_value(0x1f7, 1);
        core.process_key_value(0x1f7, 1);
        core.process_key_value(0x1f7, 0);
        let pubs = ha.pubs.lock().clone();
        let power_pubs: Vec<_> = pubs
            .into_iter()
            .filter(|(k, _)| k == "power-")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(power_pubs, vec!["ON".to_string(), "OFF".to_string()]);
    }

    #[test]
    fn field_ha_key_is_comp_dash_name() {
        let empty_name = FieldDefinition::new("climate", "");
        assert_eq!(empty_name.ha_key, "climate-");
        let named = FieldDefinition::new("climate", "mode");
        assert_eq!(named.ha_key, "climate-mode");
    }

    #[test]
    fn tlv_raw_slots_cover_10bit_ids_only() {
        let ha = Arc::new(CountingHa {
            pubs: Mutex::new(Vec::new()),
        });
        let thinq = crate::MockThinq2Device::new("id", crate::Metadata::new("M", "M", "1"));
        let core = TlvDeviceCore::new(ha, thinq);
        assert_eq!(core.get_raw(0x1f7), None);
        core.set_raw(0x1f7, 42);
        assert_eq!(core.get_raw(0x1f7), Some(42));
        core.set_raw(0x3ff, 7);
        assert_eq!(core.get_raw(0x3ff), Some(7));
        // 10-bit type space; wider ids are not stored.
        core.set_raw(0x400, 1);
        assert_eq!(core.get_raw(0x400), None);
        assert_eq!(core.get_raw(0xffff), None);
    }

    /// Drive the same path a handler uses: `on_data` → `process_data_envelope`.
    struct AckProbe {
        thinq: Arc<crate::MockThinq2Device>,
        processed: Arc<Mutex<Vec<String>>>,
    }

    fn ack_probe(auto_ack: bool) -> AckProbe {
        let ha = Arc::new(CountingHa {
            pubs: Mutex::new(Vec::new()),
        });
        let thinq =
            crate::MockThinq2Device::new("test-id", crate::Metadata::new("AABB", "AABB", "0.0.0"));
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), auto_ack);
        let processed = Arc::new(Mutex::new(Vec::new()));
        let seen = processed.clone();
        let core2 = core.clone();
        thinq.on_data(Box::new(move |buf| {
            if let Some(inner) = core2.process_data_envelope(buf) {
                seen.lock().push(crate::hex_encode(inner));
            }
        }));
        AckProbe { thinq, processed }
    }

    fn ack_hexes(thinq: &crate::MockThinq2Device) -> Vec<String> {
        thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn auto_ack_matches_cloud_delivery_acks() {
        // Captured cloud ack of a dryer's `30 4d 01` course-list request.
        let probe = ack_probe(true);
        probe.thinq.emit_data(&wrap_aabb(&[0x30, 0x4d, 0x01]));
        assert_eq!(ack_hexes(&probe.thinq), vec!["AA08F0004D04A6BB".to_string()]);
        assert!(
            probe.thinq.outbox().is_empty(),
            "acks do not go out as packets"
        );
        assert_eq!(probe.processed.lock().clone(), vec!["304D01".to_string()]);

        // Status envelopes, as captured: ack is the sequence at inner[5..7].
        let probe = ack_probe(true);
        probe.thinq.emit_data(&crate::hex_decode(
            "aaff310a00230053ed000101030011100b0306100000008a00000000000000059317bb",
        ));
        probe.thinq.emit_data(&crate::hex_decode(
            "aaff200a00a2001002000100ec00900aff000e00000000bb",
        ));
        assert_eq!(
            ack_hexes(&probe.thinq),
            vec![
                crate::hex_encode(&wrap_aabb(&[0xf0, 0x00, 0x0a, 0x04, 0x53, 0xed])),
                crate::hex_encode(&wrap_aabb(&[0xf0, 0x00, 0x0a, 0x04, 0x10, 0x02])),
            ]
        );

        // Truncated envelope is processed and not acked.
        let probe = ack_probe(true);
        probe
            .thinq
            .emit_data(&crate::hex_decode("AA08200A01020304BB"));
        assert!(ack_hexes(&probe.thinq).is_empty());
        assert_eq!(probe.processed.lock().clone(), vec!["200A010203".to_string()]);

        // 13-byte envelope header is required, in either framing. Expected bytes are the cloud's replies.
        let probe = ack_probe(true);
        probe.thinq.emit_data(&crate::hex_decode(
            "aaff200a00120010130001007f0000e91dbb",
        ));
        probe.thinq.emit_data(&crate::hex_decode(
            "aaff200a00110010120001007f00decabb",
        ));
        probe.thinq.emit_data(&crate::hex_decode(
            "aa11200a00110010b00001007f000063bb",
        ));
        probe
            .thinq
            .emit_data(&crate::hex_decode("aa10200a00100010c00001007f0011bb"));
        assert_eq!(
            ack_hexes(&probe.thinq),
            vec![
                "AA0AF0000A04101380BB".to_string(),
                "AA0AF0000A0410B027BB".to_string(),
            ]
        );

        let probe = ack_probe(true);
        probe
            .thinq
            .emit_data(&crate::hex_decode("aa0720d801ffbb"));
        probe
            .thinq
            .emit_data(&crate::hex_decode("aa0720720111bb"));
        probe
            .thinq
            .emit_data(&crate::hex_decode("aa0720e901eebb"));
        assert_eq!(
            ack_hexes(&probe.thinq),
            vec![
                "AA08F000D8042BBB".to_string(),
                "AA08F00072044DBB".to_string(),
                "AA08F000E904DABB".to_string(),
            ]
        );

        // Appliance acks, c3 heartbeat, and bare eb/ec records are not acked.
        let probe = ack_probe(true);
        for frame in [
            "AA084000430060BB",
            "aa0731c302f2bb",
            "aa0720eb01e8bb",
            "aa0720ec01ebbb",
            "aa5420ec002501000000000000000000000000000000000000000001006400000000000000000000000000002501033a033a0100030b04010000000000010002000001006400000400000000000000000000e0bb",
        ] {
            probe.thinq.emit_data(&crate::hex_decode(frame));
        }
        assert!(ack_hexes(&probe.thinq).is_empty());
        assert_eq!(probe.processed.lock().len(), 5);

        let probe = ack_probe(true);
        probe.thinq.emit_data(&[0x01, 0x02, 0x03, 0x04, 0x05]);
        assert!(ack_hexes(&probe.thinq).is_empty());
        assert!(probe.processed.lock().is_empty());

        let probe = ack_probe(false);
        probe.thinq.emit_data(&wrap_aabb(&[0x30, 0x4d, 0x01]));
        probe.thinq.emit_data(&crate::hex_decode(
            "aaff200a00a2001002000100ec00900aff000e00000000bb",
        ));
        assert!(ack_hexes(&probe.thinq).is_empty());
        assert_eq!(probe.processed.lock().len(), 2);
    }
}
