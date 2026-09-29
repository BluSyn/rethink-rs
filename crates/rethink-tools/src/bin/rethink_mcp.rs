//! Minimal MCP server (stdio, newline-delimited JSON-RPC 2.0) for RE workflows.
//! Replaces tools/mcp-server.ts — uses management HTTP APIs + pure codecs.
//!
//! Tools: set_mgmt_host, list_devices, encode_packet, decode_packet, read_capture,
//!        inject (gated), health.
//!
//! Run: rethink-mcp
//! Env: RETHINK_MGMT=host:port (default localhost:44401)

use anyhow::{anyhow, Context, Result};
use rethink_util::packet_codec::{
    decode_packet, encode_packet, AabbEncodeInput, Decoded, Direction, EncodeInput, TlvEncodeInput,
};
use rethink_util::tlv::Tlv;
use serde_json::{json, Value};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;

static MGMT: Mutex<String> = Mutex::new(String::new());

fn mgmt_host() -> String {
    MGMT.lock()
        .unwrap()
        .clone()
        .if_empty(|| env::var("RETHINK_MGMT").unwrap_or_else(|_| "localhost:44401".into()))
}

trait IfEmpty {
    fn if_empty(self, f: impl FnOnce() -> String) -> String;
}
impl IfEmpty for String {
    fn if_empty(self, f: impl FnOnce() -> String) -> String {
        if self.is_empty() {
            f()
        } else {
            self
        }
    }
}

fn http_get(path: &str) -> Result<Value> {
    let host = mgmt_host();
    let url = format!("http://{host}{path}");
    let body = ureq::get(&url)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?
        .into_string()?;
    Ok(serde_json::from_str(&body).unwrap_or(json!({"raw": body})))
}

fn http_post_json(path: &str, body: &Value) -> Result<Value> {
    let host = mgmt_host();
    let url = format!("http://{host}{path}");
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| anyhow!("POST {url}: {e}"))?
        .into_string()?;
    Ok(serde_json::from_str(&resp).unwrap_or(json!({"raw": resp})))
}

fn tool_list() -> Value {
    json!([
        {
            "name": "set_mgmt_host",
            "description": "Set management host[:port] for subsequent device tools",
            "inputSchema": {
                "type": "object",
                "properties": { "host": { "type": "string" } },
                "required": ["host"]
            }
        },
        {
            "name": "list_devices",
            "description": "List devices connected to rethink-cloud",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "health",
            "description": "GET /api/health on management UI",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "decode_packet",
            "description": "Decode ThinQ UART/AABB hex via rethink-util + optional catalog classify via management API",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hex": { "type": "string" },
                    "direction": { "type": "string" },
                    "model_id": { "type": "string" },
                    "via_mgmt": { "type": "boolean", "description": "If true, POST /api/decode on rethink-cloud" }
                },
                "required": ["hex"]
            }
        },
        {
            "name": "encode_packet",
            "description": "Encode TLV or AABB packet from primitives",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "protocol": { "type": "string", "enum": ["tlv", "aabb"] },
                    "direction": { "type": "string", "enum": ["fromDevice", "toDevice"] },
                    "tlv": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "t": { "type": "integer" },
                                "v": { "type": "integer" }
                            },
                            "required": ["t", "v"]
                        }
                    },
                    "body_hex": { "type": "string" }
                },
                "required": ["protocol"]
            }
        },
        {
            "name": "read_capture",
            "description": "Read a JSONL capture file (from rethink-capture)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer" },
                    "limit": { "type": "integer" },
                    "type": { "type": "string", "description": "packet or ack. Missing type on an event counts as packet. Non-wire events are excluded when type is set." }
                },
                "required": ["path"]
            }
        },
        {
            "name": "inject",
            "description": "Inject hex packet via management device WS (requires live device + inject_ok=true)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device_id": { "type": "string" },
                    "hex": { "type": "string" },
                    "from_device": { "type": "boolean" },
                    "inject_ok": { "type": "boolean" }
                },
                "required": ["device_id", "hex", "inject_ok"]
            }
        }
    ])
}

fn decode_local(hex: &str) -> Value {
    match decode_packet(hex) {
        Decoded::Tlv(t) => json!({
            "protocol": "Tlv",
            "direction": format!("{:?}", t.direction),
            "crc_ok": t.crc_ok,
            "kind": t.frame.kind,
            "tlv": t.tlv.iter().map(|e| json!({"t": e.t, "v": e.v})).collect::<Vec<_>>(),
        }),
        Decoded::Aabb(a) => json!({
            "protocol": "Aabb",
            "checksum_ok": a.checksum_ok,
            "body": a.body,
        }),
        Decoded::Unknown(u) => json!({
            "protocol": "Unknown",
            "reason": u.reason,
            "hex": u.hex,
        }),
    }
}

fn call_tool(name: &str, args: &Value) -> Result<Value> {
    match name {
        "set_mgmt_host" => {
            let host = args
                .get("host")
                .and_then(|h| h.as_str())
                .ok_or_else(|| anyhow!("host required"))?;
            *MGMT.lock().unwrap() = host.to_string();
            Ok(json!({"ok": true, "host": host}))
        }
        "list_devices" => http_get("/api/devices"),
        "health" => http_get("/api/health"),
        "decode_packet" => {
            let hex = args
                .get("hex")
                .and_then(|h| h.as_str())
                .ok_or_else(|| anyhow!("hex required"))?;
            if args.get("via_mgmt").and_then(|v| v.as_bool()).unwrap_or(false) {
                let mut body = json!({ "hex": hex });
                if let Some(d) = args.get("direction") {
                    body["direction"] = d.clone();
                }
                if let Some(m) = args.get("model_id") {
                    body["model_id"] = m.clone();
                }
                http_post_json("/api/decode", &body)
            } else {
                Ok(decode_local(hex))
            }
        }
        "encode_packet" => {
            let protocol = args
                .get("protocol")
                .and_then(|p| p.as_str())
                .unwrap_or("tlv");
            let input = match protocol {
                "aabb" => {
                    let body_hex = args
                        .get("body_hex")
                        .and_then(|h| h.as_str())
                        .ok_or_else(|| anyhow!("body_hex required for aabb"))?;
                    EncodeInput::Aabb(AabbEncodeInput {
                        body_hex: body_hex.into(),
                        direction: None,
                    })
                }
                _ => {
                    let dir = match args.get("direction").and_then(|d| d.as_str()).unwrap_or("toDevice")
                    {
                        "fromDevice" => Direction::FromDevice,
                        _ => Direction::ToDevice,
                    };
                    let tlv: Vec<Tlv> = args
                        .get("tlv")
                        .and_then(|a| a.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|e| {
                                    Some(Tlv::new(
                                        e.get("t")?.as_u64()? as u16,
                                        e.get("v")?.as_u64()? as u32,
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    EncodeInput::Tlv(TlvEncodeInput {
                        direction: dir,
                        tlv,
                        a: None,
                        s: None,
                        byte5: None,
                        byte6: None,
                        byte7: None,
                    })
                }
            };
            let (hex, _) = encode_packet(&input).map_err(|e| anyhow!("{e}"))?;
            Ok(json!({"hex": hex}))
        }
        "read_capture" => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| anyhow!("path required"))?;
            let offset = args.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
            let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;
            let want_type = args.get("type").and_then(|t| t.as_str());
            let file = File::open(Path::new(path)).with_context(|| format!("open {path}"))?;
            let lines: Vec<String> = BufReader::new(file).lines().collect::<std::io::Result<_>>()?;
            let slice: Vec<Value> = lines
                .into_iter()
                .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
                .filter(|ev| rethink_util::wire::capture_event_matches_type(ev, want_type))
                .skip(offset)
                .take(limit)
                .collect();
            Ok(json!({"offset": offset, "count": slice.len(), "events": slice}))
        }
        "inject" => {
            if !args.get("inject_ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Err(anyhow!("inject_ok must be true to allow injection"));
            }
            let device_id = args
                .get("device_id")
                .and_then(|d| d.as_str())
                .ok_or_else(|| anyhow!("device_id required"))?;
            let hex = args
                .get("hex")
                .and_then(|h| h.as_str())
                .ok_or_else(|| anyhow!("hex required"))?;
            let from_device = args
                .get("from_device")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Blocking TCP websocket via tungstenite
            let host = mgmt_host();
            let url = format!("ws://{host}/device?id={device_id}");
            let (mut socket, _) = tungstenite::connect(&url)
                .map_err(|e| anyhow!("ws connect {url}: {e}"))?;
            let msg = if from_device {
                json!({"sendFromDevice": hex})
            } else {
                json!({"sendToDevice": hex})
            };
            socket
                .send(tungstenite::Message::Text(msg.to_string().into()))
                .map_err(|e| anyhow!("ws send: {e}"))?;
            let _ = socket.close(None);
            Ok(json!({"ok": true, "device_id": device_id, "from_device": from_device}))
        }
        other => Err(anyhow!("unknown tool {other}")),
    }
}

fn respond(id: Value, result: Value) {
    let msg = json!({"jsonrpc": "2.0", "id": id, "result": result});
    let mut out = std::io::stdout().lock();
    writeln!(out, "{msg}").ok();
    out.flush().ok();
}

fn respond_err(id: Value, message: String) {
    let msg = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": message }
    });
    let mut out = std::io::stdout().lock();
    writeln!(out, "{msg}").ok();
    out.flush().ok();
}

fn main() {
    // Diagnostics to stderr only
    eprintln!("[rethink-mcp] ready mgmt={}", mgmt_host());
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[rethink-mcp] bad json: {e}");
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));

        match method {
            "initialize" => {
                respond(
                    id,
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "rethink-mcp", "version": "0.1.0" }
                    }),
                );
            }
            "notifications/initialized" | "initialized" => {
                // no response for notifications without id
            }
            "tools/list" => {
                respond(id, json!({ "tools": tool_list() }));
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match call_tool(name, &args) {
                    Ok(v) => respond(
                        id,
                        json!({
                            "content": [{ "type": "text", "text": v.to_string() }],
                            "structuredContent": v,
                            "isError": false
                        }),
                    ),
                    Err(e) => respond(
                        id,
                        json!({
                            "content": [{ "type": "text", "text": e.to_string() }],
                            "isError": true
                        }),
                    ),
                }
            }
            "ping" => respond(id, json!({})),
            _ => {
                if !id.is_null() {
                    respond_err(id, format!("method not found: {method}"));
                }
            }
        }
    }
}
