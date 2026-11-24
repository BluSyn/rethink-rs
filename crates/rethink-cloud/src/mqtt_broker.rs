//! Minimal MQTT 3.1.1 broker for ThinQ device connections (port of cloud/mqtt-broker.ts).

use bytes::{Buf, BufMut, BytesMut};
use rethink_util::sync::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct PublishPacket {
    pub topic: String,
    pub payload: Vec<u8>,
    pub retain: bool,
    pub qos: u8,
    pub dup: bool,
}

#[derive(Debug, Clone)]
pub struct Will {
    pub topic: String,
    pub payload: Vec<u8>,
}

struct Subscription {
    pattern: String,
    re_parts: Vec<String>,
}

impl Subscription {
    fn new(topic_pattern: &str) -> Self {
        Self {
            pattern: topic_pattern.to_string(),
            re_parts: topic_pattern.split('/').map(|s| s.to_string()).collect(),
        }
    }

    fn match_topic(&self, topic: &str) -> bool {
        // Port of TS: '^' + pattern.replace(/#$/, '.*').replace(/\+/g, '[^/]*') + '$'
        // Approximate with split logic for correctness on common cases.
        let pat = &self.pattern;
        if pat.ends_with('#') {
            let prefix = pat.trim_end_matches('#').trim_end_matches('/');
            if prefix.is_empty() {
                return true;
            }
            return topic == prefix || topic.starts_with(&format!("{prefix}/"));
        }
        let tparts: Vec<&str> = topic.split('/').collect();
        if tparts.len() != self.re_parts.len() {
            return false;
        }
        for (p, t) in self.re_parts.iter().zip(tparts.iter()) {
            if p == "+" {
                continue;
            }
            if p != t {
                return false;
            }
        }
        true
    }
}

type ClientId = u64;

struct ClientInner {
    #[allow(dead_code)] // retained for logging / future broker introspection
    id: ClientId,
    subscriptions: HashMap<String, Subscription>,
    will: Option<Will>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    #[allow(dead_code)] // set when ThinQ device object is bound on this client
    device_obj: bool,
}

pub type PublishHandler = Arc<dyn Fn(PublishPacket, Option<ClientId>) + Send + Sync>;
pub type ConnectHandler = Arc<dyn Fn(String, ClientId) + Send + Sync>;
pub type DisconnectHandler = Arc<dyn Fn(ClientId) + Send + Sync>;

struct BrokerState {
    clients: HashMap<ClientId, ClientInner>,
    retain_map: HashMap<String, PublishPacket>,
    next_id: ClientId,
    on_publish: Option<PublishHandler>,
    on_connect: Option<ConnectHandler>,
    on_disconnect: Option<DisconnectHandler>,
    /// Extra data for ThinQ2 acceptor: client_id → deploy JSON / device flag
    client_meta: HashMap<ClientId, ClientMeta>,
}

#[derive(Default, Clone)]
pub struct ClientMeta {
    pub deploy_msg: Option<serde_json::Value>,
    pub has_device: bool,
    pub device_id: Option<String>,
}

#[derive(Clone)]
pub struct Broker {
    state: Arc<Mutex<BrokerState>>,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl Broker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(BrokerState {
                clients: HashMap::new(),
                retain_map: HashMap::new(),
                next_id: 1,
                on_publish: None,
                on_connect: None,
                on_disconnect: None,
                client_meta: HashMap::new(),
            })),
        }
    }

    pub fn on_publish(&self, h: PublishHandler) {
        self.state.lock().on_publish = Some(h);
    }

    #[allow(dead_code)] // optional hook for ThinQ acceptor wiring
    pub fn on_connect(&self, h: ConnectHandler) {
        self.state.lock().on_connect = Some(h);
    }

    pub fn on_disconnect(&self, h: DisconnectHandler) {
        self.state.lock().on_disconnect = Some(h);
    }

    pub fn client_meta(&self, id: ClientId) -> ClientMeta {
        self.state
            .lock()
            .client_meta
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_client_meta(&self, id: ClientId, meta: ClientMeta) {
        self.state.lock().client_meta.insert(id, meta);
    }

    pub fn destroy_client(&self, id: ClientId) {
        let mut st = self.state.lock();
        if let Some(c) = st.clients.remove(&id) {
            let _ = c.tx.send(Vec::new()); // empty = close signal
        }
        st.client_meta.remove(&id);
    }

    pub fn publish(&self, packet: PublishPacket, from: Option<ClientId>) {
        let (handler, clients, retain_update) = {
            let mut st = self.state.lock();
            if packet.retain {
                if packet.payload.is_empty() {
                    st.retain_map.remove(&packet.topic);
                } else {
                    st.retain_map.insert(packet.topic.clone(), packet.clone());
                }
            }
            let handler = st.on_publish.clone();
            let mut targets = Vec::new();
            for c in st.clients.values() {
                for sub in c.subscriptions.values() {
                    if sub.match_topic(&packet.topic) {
                        targets.push(c.tx.clone());
                        break;
                    }
                }
            }
            (handler, targets, ())
        };
        let _ = retain_update;
        if let Some(h) = handler {
            h(packet.clone(), from);
        }
        let wire = encode_publish(&packet);
        for tx in clients {
            let _ = tx.send(wire.clone());
        }
    }

    /// Accept a plain TCP stream as an MQTT client.
    pub async fn accept_tcp(self: &Arc<Self>, stream: TcpStream) {
        self.handle_connection(stream).await;
    }

    /// Accept any TLS-wrapped stream (rustls or OpenSSL) as an MQTT client.
    pub async fn accept_tls<S>(self: &Arc<Self>, stream: S)
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'static,
    {
        self.handle_connection(stream).await;
    }

    pub async fn handle_connection<S>(self: &Arc<Self>, stream: S)
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let client_id = {
            let mut st = self.state.lock();
            let id = st.next_id;
            st.next_id += 1;
            st.clients.insert(
                id,
                ClientInner {
                    id,
                    subscriptions: HashMap::new(),
                    will: None,
                    tx: tx.clone(),
                    device_obj: false,
                },
            );
            st.client_meta.insert(id, ClientMeta::default());
            id
        };

        let write_task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if msg.is_empty() {
                    break;
                }
                if writer.write_all(&msg).await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        });

        let mut buf = BytesMut::with_capacity(4096);
        let mut tmp = [0u8; 4096];
        let mut idle = tokio::time::interval(std::time::Duration::from_secs(60 * 5));
        idle.reset();

        loop {
            tokio::select! {
                _ = idle.tick() => {
                    break;
                }
                n = reader.read(&mut tmp) => {
                    match n {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            idle.reset();
                            buf.extend_from_slice(&tmp[..n]);
                            while let Some(packet) = try_decode_packet(&mut buf) {
                                if !self.dispatch(client_id, packet, &tx).await {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        // LWT + cleanup
        let (will, disc) = {
            let mut st = self.state.lock();
            let will = st.clients.get(&client_id).and_then(|c| c.will.clone());
            st.clients.remove(&client_id);
            let disc = st.on_disconnect.clone();
            st.client_meta.remove(&client_id);
            (will, disc)
        };
        if let Some(w) = will {
            self.publish(
                PublishPacket {
                    topic: w.topic,
                    payload: w.payload,
                    retain: false,
                    qos: 0,
                    dup: false,
                },
                Some(client_id),
            );
        }
        if let Some(h) = disc {
            h(client_id);
        }
        let _ = tx.send(Vec::new());
        let _ = write_task.await;
    }

    async fn dispatch(
        &self,
        client_id: ClientId,
        packet: MqttPacket,
        tx: &mpsc::UnboundedSender<Vec<u8>>,
    ) -> bool {
        match packet {
            MqttPacket::Connect { client_name, will } => {
                {
                    let mut st = self.state.lock();
                    if let Some(c) = st.clients.get_mut(&client_id) {
                        c.will = will;
                    }
                    if let Some(h) = st.on_connect.clone() {
                        h(client_name, client_id);
                    }
                }
                let _ = tx.send(vec![0x20, 0x02, 0x00, 0x00]); // CONNACK success
                true
            }
            MqttPacket::Publish(p) => {
                if p.qos > 0 {
                    // PUBACK
                    let mut pkt = vec![0x40, 0x02];
                    pkt.put_u16(p.message_id.unwrap_or(0));
                    let _ = tx.send(pkt);
                }
                self.publish(
                    PublishPacket {
                        topic: p.topic,
                        payload: p.payload,
                        retain: p.retain,
                        qos: p.qos,
                        dup: p.dup,
                    },
                    Some(client_id),
                );
                true
            }
            MqttPacket::Subscribe { message_id, topics } => {
                let mut granted = Vec::new();
                let new_subs: Vec<Subscription> = topics
                    .iter()
                    .map(|t| {
                        granted.push(0u8); // QoS 0
                        Subscription::new(t)
                    })
                    .collect();

                let retained_to_send = {
                    let mut st = self.state.lock();
                    let retain_map = st.retain_map.clone();
                    let client = st.clients.get_mut(&client_id);
                    let mut unseen: HashSet<String> = retain_map.keys().cloned().collect();
                    if let Some(c) = client.as_ref() {
                        for t in retain_map.keys() {
                            for s in c.subscriptions.values() {
                                if s.match_topic(t) {
                                    unseen.remove(t);
                                    break;
                                }
                            }
                        }
                    }
                    if let Some(c) = st.clients.get_mut(&client_id) {
                        for (i, t) in topics.iter().enumerate() {
                            c.subscriptions
                                .insert(t.clone(), new_subs[i].clone_sub());
                        }
                    }
                    let mut out = Vec::new();
                    for t in unseen {
                        for s in &new_subs {
                            if s.match_topic(&t) {
                                if let Some(p) = retain_map.get(&t) {
                                    out.push(p.clone());
                                }
                                break;
                            }
                        }
                    }
                    out
                };

                let mut suback = vec![0x90]; // SUBACK
                let mut body = BytesMut::new();
                body.put_u16(message_id);
                body.extend_from_slice(&granted);
                encode_remaining_length(&mut suback, body.len());
                suback.extend_from_slice(&body);
                let _ = tx.send(suback);

                for p in retained_to_send {
                    let _ = tx.send(encode_publish(&p));
                }
                true
            }
            MqttPacket::Unsubscribe {
                message_id,
                topics,
            } => {
                {
                    let mut st = self.state.lock();
                    if let Some(c) = st.clients.get_mut(&client_id) {
                        for t in topics {
                            c.subscriptions.remove(&t);
                        }
                    }
                }
                let mut unsuback = vec![0xb0, 0x02];
                unsuback.put_u16(message_id);
                let _ = tx.send(unsuback);
                true
            }
            MqttPacket::PingReq => {
                let _ = tx.send(vec![0xd0, 0x00]);
                true
            }
            MqttPacket::Disconnect => false,
            MqttPacket::Unknown => true,
        }
    }
}

impl Subscription {
    fn clone_sub(&self) -> Self {
        Self {
            pattern: self.pattern.clone(),
            re_parts: self.re_parts.clone(),
        }
    }
}

enum MqttPacket {
    Connect {
        client_name: String,
        will: Option<Will>,
    },
    Publish(PublishIn),
    Subscribe {
        message_id: u16,
        topics: Vec<String>,
    },
    Unsubscribe {
        message_id: u16,
        topics: Vec<String>,
    },
    PingReq,
    Disconnect,
    Unknown,
}

struct PublishIn {
    topic: String,
    payload: Vec<u8>,
    retain: bool,
    qos: u8,
    dup: bool,
    message_id: Option<u16>,
}

fn try_decode_packet(buf: &mut BytesMut) -> Option<MqttPacket> {
    if buf.len() < 2 {
        return None;
    }
    let first = buf[0];
    let (rem_len, rl_bytes) = decode_remaining_length(&buf[1..])?;
    let header_len = 1 + rl_bytes;
    if buf.len() < header_len + rem_len {
        return None;
    }
    let _ = buf.split_to(header_len);
    let mut body = buf.split_to(rem_len);
    let packet_type = first >> 4;
    let flags = first & 0x0f;

    Some(match packet_type {
        1 => decode_connect(&mut body),
        3 => {
            let dup = flags & 0x08 != 0;
            let qos = (flags >> 1) & 0x03;
            let retain = flags & 0x01 != 0;
            if body.remaining() < 2 {
                return Some(MqttPacket::Unknown);
            }
            let tlen = body.get_u16() as usize;
            if body.remaining() < tlen {
                return Some(MqttPacket::Unknown);
            }
            let topic = String::from_utf8_lossy(&body.copy_to_bytes(tlen)).into_owned();
            let message_id = if qos > 0 {
                if body.remaining() < 2 {
                    return Some(MqttPacket::Unknown);
                }
                Some(body.get_u16())
            } else {
                None
            };
            let payload = body.to_vec();
            MqttPacket::Publish(PublishIn {
                topic,
                payload,
                retain,
                qos,
                dup,
                message_id,
            })
        }
        8 => {
            if body.remaining() < 2 {
                return Some(MqttPacket::Unknown);
            }
            let message_id = body.get_u16();
            let mut topics = Vec::new();
            while body.remaining() >= 2 {
                let tlen = body.get_u16() as usize;
                if body.remaining() < tlen + 1 {
                    break;
                }
                let topic = String::from_utf8_lossy(&body.copy_to_bytes(tlen)).into_owned();
                let _qos = body.get_u8();
                topics.push(topic);
            }
            MqttPacket::Subscribe { message_id, topics }
        }
        10 => {
            if body.remaining() < 2 {
                return Some(MqttPacket::Unknown);
            }
            let message_id = body.get_u16();
            let mut topics = Vec::new();
            while body.remaining() >= 2 {
                let tlen = body.get_u16() as usize;
                if body.remaining() < tlen {
                    break;
                }
                topics.push(String::from_utf8_lossy(&body.copy_to_bytes(tlen)).into_owned());
            }
            MqttPacket::Unsubscribe { message_id, topics }
        }
        12 => MqttPacket::PingReq,
        14 => MqttPacket::Disconnect,
        _ => MqttPacket::Unknown,
    })
}

fn decode_connect(body: &mut BytesMut) -> MqttPacket {
    // protocol name
    if body.remaining() < 2 {
        return MqttPacket::Unknown;
    }
    let nlen = body.get_u16() as usize;
    if body.remaining() < nlen + 4 {
        return MqttPacket::Unknown;
    }
    let _ = body.copy_to_bytes(nlen); // MQTT
    let _proto_level = body.get_u8();
    let connect_flags = body.get_u8();
    let _keepalive = body.get_u16();
    // client id
    if body.remaining() < 2 {
        return MqttPacket::Unknown;
    }
    let cid_len = body.get_u16() as usize;
    if body.remaining() < cid_len {
        return MqttPacket::Unknown;
    }
    let client_name = String::from_utf8_lossy(&body.copy_to_bytes(cid_len)).into_owned();

    let will = if connect_flags & 0x04 != 0 {
        // will topic + payload
        if body.remaining() < 2 {
            return MqttPacket::Connect {
                client_name,
                will: None,
            };
        }
        let wt_len = body.get_u16() as usize;
        if body.remaining() < wt_len + 2 {
            return MqttPacket::Connect {
                client_name,
                will: None,
            };
        }
        let topic = String::from_utf8_lossy(&body.copy_to_bytes(wt_len)).into_owned();
        let wp_len = body.get_u16() as usize;
        if body.remaining() < wp_len {
            return MqttPacket::Connect {
                client_name,
                will: None,
            };
        }
        let payload = body.copy_to_bytes(wp_len).to_vec();
        Some(Will { topic, payload })
    } else {
        None
    };

    // skip username/password if present
    let _ = connect_flags;

    MqttPacket::Connect { client_name, will }
}

fn decode_remaining_length(data: &[u8]) -> Option<(usize, usize)> {
    let mut multiplier = 1usize;
    let mut value = 0usize;
    for (i, &b) in data.iter().enumerate() {
        value += (b as usize & 127) * multiplier;
        multiplier *= 128;
        if b & 128 == 0 {
            return Some((value, i + 1));
        }
        if i >= 3 {
            return None;
        }
    }
    None
}

fn encode_remaining_length(out: &mut Vec<u8>, mut len: usize) {
    loop {
        let mut b = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            b |= 128;
        }
        out.push(b);
        if len == 0 {
            break;
        }
    }
}

fn encode_publish(p: &PublishPacket) -> Vec<u8> {
    let mut body = BytesMut::new();
    body.put_u16(p.topic.len() as u16);
    body.extend_from_slice(p.topic.as_bytes());
    // qos 0 — no message id
    body.extend_from_slice(&p.payload);
    let mut flags = 0u8;
    if p.retain {
        flags |= 0x01;
    }
    if p.dup {
        flags |= 0x08;
    }
    flags |= (p.qos & 0x03) << 1;
    let mut out = vec![0x30 | flags];
    encode_remaining_length(&mut out, body.len());
    out.extend_from_slice(&body);
    out
}
