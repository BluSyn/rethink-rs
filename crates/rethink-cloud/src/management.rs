//! Management HTTP + WebSocket UI (port of management/index.ts).

use crate::devmgr::{DeviceManager, Platform, SendToDevice};
use crate::ha_bridge::HaBridge;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use include_dir::{include_dir, Dir};
use rethink_util::sync::Mutex;
use rethink_bridge::Bridge;
use rethink_core::ha::HaConnection;
use rethink_util::packet_codec::{decode_packet, Decoded, Direction};
use rethink_util::tlv;
use rethink_util::aabb_analysis::{aabb_export_text, analyze_aabb_body};
use rethink_util::tlv_catalog::{classify_tlvs, llm_export_text, KNOWN_TAGS};
use rethink_util::uart_binary::{analyze_uart_binary, uart_binary_export_text};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

static HTML: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../html");

/// Per-device ring buffer of recent wire frames for the integrated monitor.
const FRAME_RING_CAP: usize = 128;

#[derive(Clone, Debug)]
pub struct FrameEvent {
    pub dir: &'static str, // "rx" | "tx"
    pub hex: String,
    pub injected: bool,
    pub ts_ms: u64,
}

impl FrameEvent {
    pub fn to_json(&self) -> Value {
        json!({
            self.dir: self.hex,
            "injected": self.injected,
            "ts": self.ts_ms,
            "history": true,
        })
    }
}

/// Shared capture of recent frames so selecting a device shows traffic immediately.
#[derive(Default)]
pub struct FrameLog {
    by_device: Mutex<HashMap<String, VecDeque<FrameEvent>>>,
}

impl FrameLog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn push(&self, device_id: &str, dir: &'static str, hex: String, injected: bool) {
        let ev = FrameEvent {
            dir,
            hex,
            injected,
            ts_ms: Self::now_ms(),
        };
        let mut map = self.by_device.lock();
        let q = map.entry(device_id.to_string()).or_default();
        if q.len() >= FRAME_RING_CAP {
            q.pop_front();
        }
        q.push_back(ev);
    }

    pub fn recent(&self, device_id: &str) -> Vec<FrameEvent> {
        self.by_device
            .lock()
            .get(device_id)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Attach capture handlers for the life of the device connection.
    pub fn attach(self: &Arc<Self>, dev: &crate::devmgr::ConnectedDevice) {
        let id = dev.id.clone();
        let log = self.clone();
        dev.add_data_handler(move |buf| {
            log.push(&id, "rx", rethink_util::hex::encode(buf), false);
        });
        let id2 = dev.id.clone();
        let log2 = self.clone();
        dev.add_send_handler(move |msg| {
            match msg {
                SendToDevice::T2Packet(b) => {
                    log2.push(&id2, "tx", rethink_util::hex::encode(b), false);
                }
                SendToDevice::T2Clip { cmd, msg_type, data } => {
                    let s = json!({"cmd": cmd, "type": msg_type, "data": data}).to_string();
                    log2.push(&id2, "tx", s, false);
                }
                SendToDevice::T1Json(v) => {
                    log2.push(&id2, "tx", v.to_string(), false);
                }
            }
        });
    }
}

#[derive(Clone)]
pub struct MgmtState {
    pub ha: Arc<dyn HaConnection>,
    pub ha_bridge: Arc<HaBridge>,
    pub manager: Arc<DeviceManager>,
    pub bridge: Option<Arc<Bridge>>,
    pub subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<String>>>>,
    pub frame_log: Arc<FrameLog>,
}

pub fn router(state: MgmtState) -> Router {
    Router::new()
        .route("/ws", get(ws_status))
        .route("/device", get(ws_device))
        .route("/thinq_login", get(thinq_login))
        .route("/thinq_login_accept", post(thinq_login_accept))
        .route("/thinq_logout", post(thinq_logout))
        .route("/bridge/{device_id}/enable", post(bridge_enable))
        .route("/bridge/{device_id}/disable", post(bridge_disable))
        // RE / device-detail APIs (used by modern management UI)
        .route("/api/tlv/catalog", get(api_tlv_catalog))
        .route("/api/decode", post(api_decode))
        .route("/api/re/export", post(api_re_export))
        .route("/api/devices/{device_id}", get(api_device_detail))
        .route("/api/devices/{device_id}/frames", get(api_device_frames))
        .fallback(static_file)
        .with_state(state)
}

fn enum_devices(state: &MgmtState) -> Value {
    let mut all = serde_json::Map::new();
    for (id, dev) in state.manager.all() {
        let (enabled, up) = state
            .bridge
            .as_ref()
            .map(|b| b.bridge_flags(&id))
            .unwrap_or((false, false));
        let bridge_state = device_bridge_state(state.bridge.is_some(), enabled, up);
        all.insert(
            id.clone(),
            json!({
                "model": dev.meta.model_id,
                "deviceType": dev.meta.device_type,
                "platform": match dev.platform {
                    Platform::Thinq1 => "thinq1",
                    Platform::Thinq2 => "thinq2",
                },
                "mapped": state.ha_bridge.has_device(&id),
                "bridgeState": bridge_state,
            }),
        );
    }
    Value::Object(all)
}

/// Bridge object on the management websocket. No bridge configured is not an error.
pub fn bridge_status_value(configured: bool, logged_in: bool) -> Value {
    if !configured {
        json!({ "disabled": true })
    } else {
        json!({ "loggedIn": logged_in })
    }
}

/// Per-device link: `disabled` when bridge mode is off, otherwise `online` / `offline`.
pub fn device_bridge_state(configured: bool, enabled: bool, link_up: bool) -> &'static str {
    if !configured || !enabled {
        "disabled"
    } else if link_up {
        "online"
    } else {
        "offline"
    }
}

fn bridge_status(state: &MgmtState) -> Value {
    match &state.bridge {
        Some(b) => bridge_status_value(true, b.is_logged_in()),
        None => bridge_status_value(false, false),
    }
}

async fn ws_status(ws: WebSocketUpgrade, State(state): State<MgmtState>) -> Response {
    ws.on_upgrade(move |socket| handle_status_ws(socket, state))
}

async fn handle_status_ws(mut socket: WebSocket, state: MgmtState) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    state.subscribers.lock().push(tx);

    let hello = json!({
        "ha": state.ha.is_connected(),
        "bridge": bridge_status(&state),
        "devices": enum_devices(&state),
    });
    let _ = socket
        .send(Message::Text(hello.to_string().into()))
        .await;

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(s) => {
                        if socket.send(Message::Text(s.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
    // cleanup dead senders later on next broadcast
}

pub fn broadcast(state: &MgmtState, message: &Value) {
    let str = message.to_string();
    let mut subs = state.subscribers.lock();
    subs.retain(|tx| tx.send(str.clone()).is_ok());
}

#[derive(Deserialize)]
struct DeviceQuery {
    id: Option<String>,
}

async fn ws_device(
    ws: WebSocketUpgrade,
    Query(q): Query<DeviceQuery>,
    State(state): State<MgmtState>,
) -> Response {
    let Some(id) = q.id else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    ws.on_upgrade(move |socket| handle_device_ws(socket, state, id))
}

async fn handle_device_ws(mut socket: WebSocket, state: MgmtState, id: String) {
    // Periodic presence check + packet monitor
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut online = false;
    let (data_tx, mut data_rx) = mpsc::unbounded_channel::<String>();

    // Register data listeners when device appears
    let mut hooked: HashSet<String> = HashSet::new();
    let mut history_sent = false;

    // Replay recent frames immediately so the UI is not empty until the next packet.
    let history = state.frame_log.recent(&id);
    if !history.is_empty() {
        let _ = socket
            .send(Message::Text(
                json!({
                    "history": history.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
                    "count": history.len(),
                })
                .to_string()
                .into(),
            ))
            .await;
        history_sent = true;
    }

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let dev = state.manager.get(&id);
                let is_on = dev.is_some();
                if is_on != online {
                    online = is_on;
                    if let Some(ref d) = dev {
                        let _ = socket.send(Message::Text(json!({
                            "status": "online",
                            "meta": d.meta,
                        }).to_string().into())).await;
                        // History may have grown while waiting offline → online; re-send once.
                        if !history_sent {
                            let hist = state.frame_log.recent(&id);
                            if !hist.is_empty() {
                                let _ = socket.send(Message::Text(json!({
                                    "history": hist.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
                                    "count": hist.len(),
                                }).to_string().into())).await;
                            }
                            history_sent = true;
                        }
                        if !hooked.contains(&id) {
                            hooked.insert(id.clone());
                            // Live fan-out only — ring buffer is filled by FrameLog::attach in main.
                            let tx = data_tx.clone();
                            d.add_data_handler(move |buf| {
                                let _ = tx.send(json!({
                                    "rx": rethink_util::hex::encode(buf),
                                    "injected": false,
                                    "ts": FrameLog::now_ms(),
                                }).to_string());
                            });
                            let tx2 = data_tx.clone();
                            d.add_send_handler(move |msg| {
                                let tx_val = match msg {
                                    SendToDevice::T2Packet(b) => json!({
                                        "tx": rethink_util::hex::encode(b),
                                        "injected": false,
                                        "ts": FrameLog::now_ms(),
                                    }),
                                    SendToDevice::T2Clip { cmd, msg_type, data } => json!({
                                        "tx": { "cmd": cmd, "type": msg_type, "data": data },
                                        "injected": false,
                                        "ts": FrameLog::now_ms(),
                                    }),
                                    SendToDevice::T1Json(v) => json!({
                                        "tx": v,
                                        "injected": false,
                                        "ts": FrameLog::now_ms(),
                                    }),
                                };
                                let _ = tx2.send(tx_val.to_string());
                            });
                        }
                    } else {
                        let _ = socket.send(Message::Text(
                            json!({"status":"offline"}).to_string().into()
                        )).await;
                    }
                }
            }
            msg = data_rx.recv() => {
                if let Some(s) = msg {
                    if socket.send(Message::Text(s.into())).await.is_err() {
                        break;
                    }
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if let Some(dev) = state.manager.get(&id) {
                                if let Some(s) = v.get("sendToDevice").and_then(|x| x.as_str()) {
                                    if let Ok(buf) = rethink_util::hex::decode(s) {
                                        // Mark inject in the ring buffer (send handlers also log a live copy).
                                        state.frame_log.push(&id, "tx", s.to_string(), true);
                                        (dev.send_to_device)(SendToDevice::T2Packet(buf));
                                    }
                                }
                                if let Some(obj) = v.get("sendToDevice").filter(|x| x.is_object()) {
                                    (dev.send_to_device)(SendToDevice::T1Json(obj.clone()));
                                }
                                if let Some(s) = v.get("sendFromDevice").and_then(|x| x.as_str()) {
                                    if let Ok(buf) = rethink_util::hex::decode(s) {
                                        state.frame_log.push(&id, "rx", s.to_string(), true);
                                        (dev.emit_data)(buf);
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct LoginQuery {
    #[serde(rename = "countryCode")]
    country_code: Option<String>,
}

async fn thinq_login(
    State(state): State<MgmtState>,
    Query(q): Query<LoginQuery>,
) -> Response {
    let Some(bridge) = &state.bridge else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cc = q.country_code.unwrap_or_else(|| "US".into());
    match bridge.begin_login(&cc).await {
        Ok(url) => Redirect::temporary(&url).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct LoginAccept {
    url: String,
    #[serde(rename = "countryCode")]
    country_code: String,
}

async fn thinq_login_accept(
    State(state): State<MgmtState>,
    Json(body): Json<LoginAccept>,
) -> StatusCode {
    let Some(bridge) = &state.bridge else {
        return StatusCode::NOT_FOUND;
    };
    match bridge.complete_login(&body.country_code, &body.url).await {
        Ok(true) => {
            broadcast(&state, &json!({ "bridge": bridge_status(&state) }));
            StatusCode::OK
        }
        _ => StatusCode::BAD_REQUEST,
    }
}

async fn thinq_logout(State(state): State<MgmtState>) -> StatusCode {
    if let Some(bridge) = &state.bridge {
        let _ = bridge.logout().await;
        broadcast(&state, &json!({ "bridge": bridge_status(&state) }));
    }
    StatusCode::OK
}

#[derive(Deserialize)]
struct EnableBody {
    #[serde(rename = "deviceType")]
    device_type: Option<String>,
}

async fn bridge_enable(
    State(state): State<MgmtState>,
    Path(device_id): Path<String>,
    body: Result<Json<EnableBody>, axum::extract::rejection::JsonRejection>,
) -> StatusCode {
    let Some(bridge) = state.bridge.clone() else {
        return StatusCode::NOT_FOUND;
    };
    let dt = body.ok().and_then(|b| b.0.device_type);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    let mgr = state.manager.clone();
    let device = match mgr.get(&device_id) {
        Some(d) => Arc::new(crate::bridge_adapter::ConnectedAsLocal(d))
            as Arc<dyn rethink_bridge::LocalDevice>,
        None => {
            broadcast(&state, &json!({ "status": "device not connected" }));
            return StatusCode::BAD_REQUEST;
        }
    };
    let report: Box<dyn FnMut(&str) + Send> = Box::new(move |s: &str| {
        let _ = status_tx.send(s.to_string());
    });
    let result = bridge.enable(device, dt.as_deref(), Some(report)).await;
    while let Ok(s) = status_rx.try_recv() {
        broadcast(&state, &json!({ "status": s }));
    }
    match result {
        Ok(true) => {
            broadcast(&state, &json!({ "devices": enum_devices(&state) }));
            StatusCode::NO_CONTENT
        }
        Ok(false) => StatusCode::BAD_REQUEST,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn bridge_disable(
    State(state): State<MgmtState>,
    Path(device_id): Path<String>,
) -> StatusCode {
    if let Some(bridge) = &state.bridge {
        let _ = bridge.disable(&device_id).await;
        broadcast(&state, &json!({ "devices": enum_devices(&state) }));
    }
    StatusCode::NO_CONTENT
}

fn mime_for_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ico" => "image/x-icon",
        "txt" | "md" => "text/plain; charset=utf-8",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Short git SHA from build.rs (`git rev-parse` or `RETHINK_GIT_SHA`), else `dev`.
const GIT_SHA: &str = env!("RETHINK_GIT_SHA");

async fn static_file(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    // try with .html extension
    let candidates = [path, &format!("{path}.html")];
    for c in candidates {
        if let Some(file) = HTML.get_file(c) {
            let mime = mime_for_path(c);
            let mut body = file.contents().to_vec();
            // Inject build identity into the nav badge (placeholder in index.html).
            if c == "index.html" || c.ends_with("/index.html") {
                if let Ok(s) = std::str::from_utf8(&body) {
                    body = s
                        .replace("__RETHINK_GIT_SHA__", GIT_SHA)
                        .into_bytes();
                }
            }
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime)],
                body,
            )
                .into_response();
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

// ── Reverse-engineering / device detail APIs ───────────────────────────────

async fn api_tlv_catalog() -> Json<Value> {
    let tags: Vec<Value> = KNOWN_TAGS
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "hex": format!("0x{:03x}", t.id),
                "name": t.name,
                "family": t.family,
            })
        })
        .collect();
    Json(json!({ "tags": tags, "count": tags.len() }))
}

#[derive(Deserialize)]
struct DecodeBody {
    hex: String,
    /// Optional: "fromDevice" | "toDevice" (default fromDevice).
    direction: Option<String>,
    model_id: Option<String>,
}

fn strip_hex(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn decode_hex_payload(hex_in: &str, direction: Option<&str>) -> Result<Value, String> {
    let hex = strip_hex(hex_in);
    if hex.is_empty() {
        return Err("empty hex".into());
    }
    let bytes = rethink_util::hex::decode(&hex).map_err(|e| format!("hex decode: {e}"))?;
    let requested_dir = match direction.unwrap_or("fromDevice") {
        "toDevice" | "to" | "tx" => "toDevice",
        _ => "fromDevice",
    };

    let mut protocol = String::from("Unknown");
    // (t, v, byte_start_in_packet, byte_end_in_packet)
    let mut tlv_spans: Vec<(u16, u32, usize, usize)> = Vec::new();
    let mut aabb_body: Option<String> = None;
    let mut notes = Vec::new();
    let mut dir_out = requested_dir.to_string();
    let mut crc_ok: Option<bool> = None;
    let mut binary_analysis: Option<serde_json::Value> = None;

    match decode_packet(&hex) {
        Decoded::Tlv(t) => {
            protocol = "Tlv".into();
            dir_out = match t.direction {
                Direction::FromDevice => "fromDevice".into(),
                Direction::ToDevice => "toDevice".into(),
            };
            crc_ok = Some(t.crc_ok);
            // Full UART frame: TLV body starts at byte 11 (after 2 reliability + 9 header).
            let tlv_base = 11usize;
            let len = t.frame.len as usize;
            if bytes.len() >= tlv_base + len {
                let spans = tlv::parse_with_spans(&bytes[tlv_base..tlv_base + len]);
                tlv_spans = spans
                    .into_iter()
                    .map(|s| {
                        (
                            s.tlv.t,
                            s.tlv.v,
                            tlv_base + s.byte_start,
                            tlv_base + s.byte_end,
                        )
                    })
                    .collect();
            } else {
                tlv_spans = t
                    .tlv
                    .iter()
                    .map(|e| (e.t, e.v, 0usize, 0usize))
                    .collect();
            }
            notes.push(format!(
                "kind=0x{:02x} b5=0x{:02x} b6=0x{:02x} b7=0x{:02x} len={}",
                t.frame.kind, t.frame.byte5, t.frame.byte6, t.frame.byte7, t.frame.len
            ));
            if t.frame.len == 0 {
                notes.push("empty body (ACK/keepalive-style)".into());
            }
        }
        Decoded::Aabb(a) => {
            protocol = "Aabb".into();
            aabb_body = Some(a.body.clone());
            crc_ok = Some(a.checksum_ok);
            let body_bytes = rethink_util::hex::decode(&a.body).unwrap_or_default();
            let analysis =
                analyze_aabb_body(&body_bytes, bytes.len(), a.length, Some(a.checksum_ok));
            if let Some(k) = analysis.kind {
                notes.push(format!(
                    "kind=0x{k:02x} type={} body_len={}",
                    analysis
                        .frame_type
                        .map(|t| format!("0x{t:02x}"))
                        .unwrap_or_else(|| "—".into()),
                    analysis.body_len
                ));
            } else {
                notes.push(format!("AABB body_len={}", analysis.body_len));
            }
            if let Some(phase) = analysis.fields.iter().find(|f| f.name == "phase") {
                notes.push(format!("phase={}", phase.interpretation));
            }
            binary_analysis = Some(serde_json::to_value(&analysis).unwrap_or(json!({})));
        }
        Decoded::Unknown(u) => {
            notes.push(format!("packet_codec: {}", u.reason));
            // Non-TLV UART envelope: analyze body with heuristics (do NOT invent TLV tags).
            if u.reason.starts_with("uart_binary") {
                protocol = "UartBinary".into();
                if bytes.len() >= 13 {
                    let kind = bytes[6];
                    let b5 = bytes[7];
                    let b6 = bytes[8];
                    let b7 = bytes[9];
                    let len = bytes[10] as usize;
                    let start = 11usize;
                    let end = (start + len).min(bytes.len().saturating_sub(2));
                    if end > start {
                        let body = &bytes[start..end];
                        aabb_body = Some(rethink_util::hex::encode(body));
                        let crc = if u.reason.contains("crc_ok=true") {
                            Some(true)
                        } else if u.reason.contains("crc_ok=false") {
                            Some(false)
                        } else {
                            None
                        };
                        let analysis = analyze_uart_binary(kind, b5, b6, b7, body, crc);
                        notes.push(format!(
                            "binary body {} bytes — heuristic hits={}",
                            body.len(),
                            analysis.heuristics.len()
                        ));
                        binary_analysis = Some(serde_json::to_value(&analysis).unwrap_or(json!({})));
                    }
                }
            } else {
                // Raw TLV body (no UART envelope) — try whole buffer, then after 2-byte prefix
                let candidates: &[(usize, &[u8])] = if bytes.len() > 2 {
                    &[(0, &bytes[..]), (2, &bytes[2..])]
                } else {
                    &[(0, &bytes[..])]
                };
                let mut parsed = false;
                for &(base, slice) in candidates {
                    let spans = tlv::parse_with_spans(slice);
                    if !spans.is_empty() {
                        tlv_spans = spans
                            .into_iter()
                            .map(|s| {
                                (
                                    s.tlv.t,
                                    s.tlv.v,
                                    base + s.byte_start,
                                    base + s.byte_end,
                                )
                            })
                            .collect();
                        protocol = "TlvRaw".into();
                        parsed = true;
                        break;
                    }
                }
                if !parsed && bytes.len() >= 2 {
                    aabb_body = Some(hex.clone());
                    protocol = "Raw".into();
                }
            }
        }
    }

    let tlv_items: Vec<(u16, u32)> = tlv_spans.iter().map(|(t, v, _, _)| (*t, *v)).collect();
    let classified = classify_tlvs(&tlv_items);
    let unknowns: Vec<Value> = classified
        .iter()
        .filter(|c| !c.known)
        .map(|c| json!({"t": c.t, "hex": format!("0x{:03x}", c.t), "v": c.v}))
        .collect();
    let elements: Vec<Value> = classified
        .iter()
        .zip(tlv_spans.iter())
        .map(|(c, (_, _, b0, b1))| {
            json!({
                "t": c.t,
                "hex": format!("0x{:03x}", c.t),
                "v": c.v,
                "known": c.known,
                "name": c.name,
                // Byte offsets in the full packet; hex char offsets are 2× (no separators).
                "byteStart": b0,
                "byteEnd": b1,
                "hexStart": b0 * 2,
                "hexEnd": b1 * 2,
            })
        })
        .collect();

    Ok(json!({
        "ok": true,
        "protocol": protocol,
        "direction": dir_out,
        "crcOk": crc_ok,
        "elements": elements,
        "unknowns": unknowns,
        "unknownCount": unknowns.len(),
        "aabbBody": aabb_body,
        "binaryAnalysis": binary_analysis,
        "notes": notes,
        "hex": hex,
    }))
}

async fn api_decode(Json(body): Json<DecodeBody>) -> Response {
    match decode_hex_payload(&body.hex, body.direction.as_deref()) {
        Ok(mut v) => {
            if let Some(m) = body.model_id {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("modelId".into(), json!(m));
                }
            }
            Json(v).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct ExportBody {
    hex: String,
    direction: Option<String>,
    model_id: Option<String>,
}

async fn api_re_export(Json(body): Json<ExportBody>) -> Response {
    let decoded = match decode_hex_payload(&body.hex, body.direction.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": e}))).into_response();
        }
    };
    let hex = decoded
        .get("hex")
        .and_then(|h| h.as_str())
        .unwrap_or("");
    let protocol = decoded
        .get("protocol")
        .and_then(|p| p.as_str())
        .unwrap_or("");

    // AABB fixed-layout frames (dryer/washer/fridge) — not TLV.
    if protocol == "Aabb" {
        let text = if let Some(ba) = decoded
            .get("binaryAnalysis")
            .filter(|v| !v.is_null())
        {
            // Re-analyze from body for stable text (same pattern as UartBinary)
            if let Some(body_hex) = ba.get("body_hex").and_then(|v| v.as_str()) {
                let raw = rethink_util::hex::decode(body_hex).unwrap_or_default();
                let len_byte = ba
                    .get("length_byte")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u8;
                let packet_len = ba
                    .get("packet_len")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(hex.len() as u64 / 2) as usize;
                let csum = ba.get("checksum_ok").and_then(|v| v.as_bool());
                let analysis = analyze_aabb_body(&raw, packet_len, len_byte, csum);
                aabb_export_text(
                    body.model_id.as_deref(),
                    body.direction.as_deref(),
                    hex,
                    &analysis,
                )
            } else if let Some(body_h) = decoded.get("aabbBody").and_then(|v| v.as_str()) {
                let raw = rethink_util::hex::decode(body_h).unwrap_or_default();
                let analysis = analyze_aabb_body(&raw, hex.len() / 2, 0, None);
                aabb_export_text(
                    body.model_id.as_deref(),
                    body.direction.as_deref(),
                    hex,
                    &analysis,
                )
            } else {
                format!("# ThinQ AABB frame\nhex: {hex}\n")
            }
        } else if let Some(body_h) = decoded.get("aabbBody").and_then(|v| v.as_str()) {
            let raw = rethink_util::hex::decode(body_h).unwrap_or_default();
            let analysis = analyze_aabb_body(&raw, hex.len() / 2, 0, None);
            aabb_export_text(
                body.model_id.as_deref(),
                body.direction.as_deref(),
                hex,
                &analysis,
            )
        } else {
            format!("# ThinQ AABB frame\nhex: {hex}\n")
        };
        return Json(json!({
            "ok": true,
            "text": text,
            "unknownCount": 0,
            "decode": decoded,
        }))
        .into_response();
    }

    // Binary UART: prefer structured heuristic export over empty TLV export.
    // Note: JSON null for binaryAnalysis is still Some(Value::Null) — filter it out.
    if protocol == "UartBinary" {
        let text = if let Some(ba) = decoded
            .get("binaryAnalysis")
            .filter(|v| !v.is_null())
        {
            if let (Some(kind), Some(b5), Some(b6), Some(b7), Some(body_hex)) = (
                ba.pointer("/envelope/kind").and_then(|v| v.as_u64()),
                ba.pointer("/envelope/byte5").and_then(|v| v.as_u64()),
                ba.pointer("/envelope/byte6").and_then(|v| v.as_u64()),
                ba.pointer("/envelope/byte7").and_then(|v| v.as_u64()),
                ba.get("body_hex").and_then(|v| v.as_str()),
            ) {
                let raw_body = rethink_util::hex::decode(body_hex).unwrap_or_default();
                let crc = ba.pointer("/envelope/crc_ok").and_then(|v| v.as_bool());
                let analysis = analyze_uart_binary(
                    kind as u8,
                    b5 as u8,
                    b6 as u8,
                    b7 as u8,
                    &raw_body,
                    crc,
                );
                uart_binary_export_text(
                    body.model_id.as_deref(),
                    body.direction.as_deref().or(Some("fromDevice")),
                    hex,
                    &analysis,
                )
            } else {
                // Envelope present but incomplete — compact fallback, never dump JSON "null"
                let notes = decoded
                    .get("notes")
                    .and_then(|n| n.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default();
                let mut out = String::from("# ThinQ UART binary\n");
                if let Some(m) = body.model_id.as_deref() {
                    out.push_str(&format!("modelId: {m}\n"));
                }
                if !notes.is_empty() {
                    out.push_str(&format!("{notes}\n"));
                }
                out.push_str(&format!("hex: {hex}\n"));
                if let Some(body_h) = decoded.get("aabbBody").and_then(|v| v.as_str()) {
                    out.push_str(&format!("body: {body_h}\n"));
                }
                out
            }
        } else {
            // Empty body / no analysis — envelope only from notes
            let notes = decoded
                .get("notes")
                .and_then(|n| n.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default();
            let mut out = String::from("# ThinQ UART binary\n");
            if let Some(m) = body.model_id.as_deref() {
                out.push_str(&format!("modelId: {m}\n"));
            }
            if !notes.is_empty() {
                out.push_str(&format!("{notes}\n"));
            }
            out.push_str(&format!("hex: {hex}\n"));
            out.push_str("body: (empty)\n");
            out
        };
        return Json(json!({
            "ok": true,
            "text": text,
            "unknownCount": 0,
            "decode": decoded,
        }))
        .into_response();
    }

    let items: Vec<(u16, u32)> = decoded
        .get("elements")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|el| {
                    let t = el.get("t")?.as_u64()? as u16;
                    let v = el.get("v")?.as_u64()? as u32;
                    Some((t, v))
                })
                .collect()
        })
        .unwrap_or_default();
    let classified = classify_tlvs(&items);
    let text = llm_export_text(
        body.model_id.as_deref(),
        body.direction.as_deref().or(Some("fromDevice")),
        hex,
        &classified,
    );
    Json(json!({
        "ok": true,
        "text": text,
        "unknownCount": classified.iter().filter(|c| !c.known).count(),
        "decode": decoded,
    }))
    .into_response()
}

async fn api_device_frames(
    State(state): State<MgmtState>,
    Path(device_id): Path<String>,
) -> Json<Value> {
    let frames = state.frame_log.recent(&device_id);
    Json(json!({
        "ok": true,
        "deviceId": device_id,
        "count": frames.len(),
        "frames": frames.iter().map(|e| {
            json!({
                "dir": e.dir,
                "hex": e.hex,
                "injected": e.injected,
                "ts": e.ts_ms,
            })
        }).collect::<Vec<_>>(),
    }))
}

async fn api_device_detail(
    State(state): State<MgmtState>,
    Path(device_id): Path<String>,
) -> Response {
    let Some(dev) = state.manager.get(&device_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "device not connected"})),
        )
            .into_response();
    };
    let mapped = state.ha_bridge.has_device(&device_id);
    let bridged = state
        .bridge
        .as_ref()
        .map(|b| b.status_for(&device_id))
        .unwrap_or(false);

    Json(json!({
        "ok": true,
        "id": dev.id,
        "platform": match dev.platform {
            Platform::Thinq1 => "thinq1",
            Platform::Thinq2 => "thinq2",
        },
        "modelId": dev.meta.model_id,
        "modelName": dev.meta.model_name,
        "deviceType": dev.meta.device_type,
        "swVersion": dev.meta.sw_version,
        "mapped": mapped,
        "bridged": bridged,
        "haConnected": state.ha.is_connected(),
        // Live property stream is via monitor WebSocket (/device?id=…);
        // detail endpoint focuses on identity + bridge/HA mapping status.
        "monitorUrl": format!("monitor?id={device_id}"),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_status_distinguishes_disabled_offline_and_online() {
        assert_eq!(
            bridge_status_value(false, false),
            serde_json::json!({ "disabled": true })
        );
        assert_eq!(bridge_status_value(false, false).get("loggedIn"), None);
        assert_eq!(
            bridge_status_value(true, false),
            serde_json::json!({ "loggedIn": false })
        );
        assert_eq!(
            bridge_status_value(true, true),
            serde_json::json!({ "loggedIn": true })
        );
        assert_eq!(device_bridge_state(false, false, false), "disabled");
        assert_eq!(device_bridge_state(true, false, false), "disabled");
        assert_eq!(device_bridge_state(true, true, false), "offline");
        assert_eq!(device_bridge_state(true, true, true), "online");

        let panel = include_str!("../../../html/panel.js");
        assert!(
            panel.contains("Disabled by configuration"),
            "management page must show bridge-not-configured as disabled, not an error"
        );
        assert!(panel.contains("json.bridge.disabled"));
        assert!(
            !panel.contains("status_bridge_text').innerText = 'Not configured'"),
            "the logged-out error wording must not be used for a disabled bridge"
        );
    }

    #[test]
    fn frame_log_ring_keeps_recent() {
        let log = FrameLog::new();
        for i in 0..150 {
            log.push("dev1", "rx", format!("{i:04x}"), false);
        }
        let recent = log.recent("dev1");
        assert_eq!(recent.len(), FRAME_RING_CAP);
        assert!(recent.first().unwrap().hex.contains(&format!("{:04x}", 150 - FRAME_RING_CAP)));
        assert_eq!(recent.last().unwrap().hex, format!("{:04x}", 149));
        assert!(log.recent("other").is_empty());
    }

    fn re_decode_for_test(hex: &str, direction: Option<&str>) -> Result<Value, String> {
        decode_hex_payload(hex, direction)
    }

    fn re_export_for_test(hex: &str, model_id: Option<&str>) -> Result<String, String> {
        let decoded = decode_hex_payload(hex, Some("fromDevice"))?;
        let items: Vec<(u16, u32)> = decoded
            .get("elements")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|el| {
                        let t = el.get("t")?.as_u64()? as u16;
                        let v = el.get("v")?.as_u64()? as u32;
                        Some((t, v))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let classified = classify_tlvs(&items);
        let h = decoded.get("hex").and_then(|x| x.as_str()).unwrap_or(hex);
        Ok(llm_export_text(model_id, Some("fromDevice"), h, &classified))
    }

    #[test]
    fn decode_known_tlv_query_style() {
        // Tags are 10-bit; use 0x3fe as a deliberately uncatalogued tag.
        let tlv_bytes = tlv::build(&[tlv::Tlv::new(0x1f7, 1), tlv::Tlv::new(0x3fe, 42)]);
        let hex = rethink_util::hex::encode(&tlv_bytes);
        let v = re_decode_for_test(&hex, Some("fromDevice")).unwrap();
        assert_eq!(v["ok"], true);
        let els = v["elements"].as_array().unwrap();
        assert!(els.iter().any(|e| e["t"] == 0x1f7 && e["known"] == true));
        assert!(els.iter().any(|e| e["t"] == 0x3fe && e["known"] == false));
        assert!(v["unknownCount"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn export_includes_llm_section() {
        let tlv_bytes = tlv::build(&[tlv::Tlv::new(0x1f7, 1), tlv::Tlv::new(0x3fe, 9)]);
        let text = re_export_for_test(&rethink_util::hex::encode(&tlv_bytes), Some("HUM_056905_WW")).unwrap();
        assert!(text.contains("UNKNOWN") || text.contains("Unknown"));
        assert!(text.contains("HUM_056905_WW"));
        assert!(text.contains("0x3fe") || text.contains("3fe"));
    }

    #[test]
    fn catalog_nonempty() {
        assert!(!KNOWN_TAGS.is_empty());
    }
}
