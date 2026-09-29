//! Capture device wire traffic from rethink-cloud management WebSocket to JSONL.
//! Replaces tools/rethink-capture.ts (device path; use bridge mode for cloud correlation).
//!
//! Usage:
//!   rethink-capture <mgmt-host[:port]> <device-uuid> [out.jsonl]
//!
//! Stdin lines become `{"k":"note","t":…,"text":…}` annotations.

use anyhow::{anyhow, Context, Result};
use rethink_util::packet_codec::{decode_packet, Decoded};
use serde_json::{json, Value};
use std::env;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn decode_summary(hex: &str) -> Value {
    match decode_packet(hex) {
        Decoded::Tlv(t) => json!({
            "protocol": "Tlv",
            "direction": format!("{:?}", t.direction),
            "crc_ok": t.crc_ok,
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
        }),
    }
}

enum Ev {
    Note(String),
    Ws(String),
    Closed,
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("Usage: rethink-capture <mgmt-host[:port]> <device-uuid> [out.jsonl]");
        std::process::exit(2);
    }
    let host = &args[0];
    let device_id = &args[1];
    let out_path = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| format!("{device_id}.jsonl"));

    let mut out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out_path)
        .with_context(|| format!("open {out_path}"))?;

    let write_ev = |out: &mut std::fs::File, ev: Value| -> Result<()> {
        writeln!(out, "{ev}")?;
        out.flush()?;
        Ok(())
    };

    write_ev(
        &mut out,
        json!({
            "k": "session",
            "t": now_ms(),
            "device_id": device_id,
            "mgmt": host,
        }),
    )?;

    let url = format!("ws://{host}/device?id={device_id}");
    eprintln!("[rethink-capture] connecting {url} → {out_path}");
    let (mut socket, _) =
        tungstenite::connect(&url).map_err(|e| anyhow!("ws connect: {e}"))?;

    let (tx, rx) = mpsc::channel::<Ev>();
    let tx_notes = tx.clone();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().flatten() {
            if tx_notes.send(Ev::Note(line)).is_err() {
                break;
            }
        }
    });
    thread::spawn(move || {
        loop {
            match socket.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    if tx.send(Ev::Ws(text.to_string())).is_err() {
                        break;
                    }
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => {
                    let _ = tx.send(Ev::Closed);
                    break;
                }
                Ok(_) => {}
            }
        }
    });

    while let Ok(ev) = rx.recv() {
        match ev {
            Ev::Note(note) => {
                if note.trim().is_empty() {
                    continue;
                }
                write_ev(
                    &mut out,
                    json!({"k": "note", "t": now_ms(), "text": note}),
                )?;
                eprintln!("[note] {note}");
            }
            Ev::Ws(text) => {
                let v: Value =
                    serde_json::from_str(&text).unwrap_or(json!({"raw": text}));
                let t = now_ms();
                if let Some(mut ev) = rethink_util::wire::capture_from_monitor_message(&v) {
                    ev["t"] = json!(t);
                    if let Some(hex) = ev.get("hex").and_then(|h| h.as_str()).map(|s| s.to_string())
                    {
                        eprintln!(
                            "[{}] {}",
                            ev["k"].as_str().unwrap_or("wire"),
                            &hex[..hex.len().min(32)]
                        );
                        ev["decode"] = decode_summary(&hex);
                    }
                    write_ev(&mut out, ev)?;
                } else if v.get("status").is_some() {
                    write_ev(&mut out, json!({"k": "status", "t": t, "msg": v}))?;
                    eprintln!("[status] {v}");
                } else {
                    write_ev(&mut out, json!({"k": "ws", "t": t, "msg": v}))?;
                }
            }
            Ev::Closed => {
                eprintln!("[rethink-capture] socket closed");
                break;
            }
        }
    }
    Ok(())
}
