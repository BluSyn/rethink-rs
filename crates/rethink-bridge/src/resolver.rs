//! DNS for bridge-initiated connections (upstream `bridge/resolver.ts`).
//!
//! Each configured entry is a DoH URL (RFC 8484) or a plain DNS address.
//! An absent or empty list means the system resolver. Tests cover encode/parse
//! and the ordered list; they do not contact `1.1.1.1`.

use std::net::Ipv6Addr;
use std::sync::Mutex;

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub address: String,
    pub family: u8,
    pub ttl: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerKind {
    Doh(String),
    Plain(String),
}

static SERVERS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Install the resolver list. Empty clears it (system resolver).
pub fn set_servers(entries: Vec<String>) {
    *SERVERS.lock().unwrap_or_else(|e| e.into_inner()) = entries;
}

pub fn configured_servers() -> Vec<String> {
    SERVERS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn uses_system_resolver() -> bool {
    configured_servers().is_empty()
}

/// Classify entries in order. `https://` is DoH; anything else is a plain DNS address.
pub fn classify_servers(entries: &[String]) -> Vec<ServerKind> {
    entries
        .iter()
        .map(|entry| {
            if entry.starts_with("https://") {
                ServerKind::Doh(entry.clone())
            } else {
                ServerKind::Plain(entry.clone())
            }
        })
        .collect()
}

/// DNS query message. ID stays 0 and RD is set, as RFC 8484 recommends for DoH.
pub fn encode_query(hostname: &str, qtype: u16) -> Result<Vec<u8>, String> {
    let mut msg = vec![0u8; 12];
    msg[2] = 0x01; // recursion desired
    msg[5] = 1; // one question
    let host = hostname.trim_end_matches('.');
    for label in host.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return Err(format!("Invalid hostname {hostname}"));
        }
        msg.push(bytes.len() as u8);
        msg.extend_from_slice(bytes);
    }
    msg.push(0);
    msg.extend_from_slice(&qtype.to_be_bytes());
    msg.extend_from_slice(&1u16.to_be_bytes()); // class IN
    Ok(msg)
}

pub fn encode_a_query(hostname: &str) -> Result<Vec<u8>, String> {
    encode_query(hostname, TYPE_A)
}

fn skip_name(msg: &[u8], mut offset: usize) -> Result<usize, String> {
    loop {
        let length = *msg.get(offset).ok_or("Truncated DNS message")?;
        if length == 0 {
            return Ok(offset + 1);
        }
        if length & 0xc0 == 0xc0 {
            return Ok(offset + 2);
        }
        offset += length as usize + 1;
    }
}

/// Addresses of `qtype` in the answer section. Other types (CNAME) are skipped.
pub fn parse_response(msg: &[u8], qtype: u16) -> Result<Vec<Record>, String> {
    if msg.len() < 12 {
        return Err("Truncated DNS message".into());
    }
    let flags = u16::from_be_bytes([msg[2], msg[3]]);
    let rcode = flags & 0x0f;
    if rcode == 3 {
        return Ok(Vec::new());
    }
    if rcode != 0 {
        return Err(format!("DNS error code {rcode}"));
    }
    let questions = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let answers = u16::from_be_bytes([msg[6], msg[7]]) as usize;
    let mut offset = 12;
    for _ in 0..questions {
        offset = skip_name(msg, offset)? + 4;
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        offset = skip_name(msg, offset)?;
        if offset + 10 > msg.len() {
            return Err("Truncated DNS message".into());
        }
        let rtype = u16::from_be_bytes([msg[offset], msg[offset + 1]]);
        let ttl = u32::from_be_bytes([
            msg[offset + 4],
            msg[offset + 5],
            msg[offset + 6],
            msg[offset + 7],
        ]);
        let length = u16::from_be_bytes([msg[offset + 8], msg[offset + 9]]) as usize;
        let start = offset + 10;
        let end = start + length;
        if end > msg.len() {
            return Err("Truncated DNS message".into());
        }
        let data = &msg[start..end];
        offset = end;
        if rtype != qtype {
            continue;
        }
        if qtype == TYPE_A && length == 4 {
            records.push(Record {
                address: format!("{}.{}.{}.{}", data[0], data[1], data[2], data[3]),
                family: 4,
                ttl,
            });
        } else if qtype == TYPE_AAAA && length == 16 {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(data);
            records.push(Record {
                address: Ipv6Addr::from(octets).to_string(),
                family: 6,
                ttl,
            });
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_list_keeps_order_and_empty_is_the_system_resolver() {
        set_servers(vec![
            "https://1.1.1.1/dns-query".into(),
            "8.8.8.8".into(),
        ]);
        assert!(!uses_system_resolver());
        assert_eq!(
            classify_servers(&configured_servers()),
            vec![
                ServerKind::Doh("https://1.1.1.1/dns-query".into()),
                ServerKind::Plain("8.8.8.8".into()),
            ]
        );
        set_servers(vec![]);
        assert!(uses_system_resolver());
        assert!(classify_servers(&[]).is_empty());
    }

    #[test]
    fn encode_and_parse_a_fixture_without_the_network() {
        let query = encode_a_query("example.com").unwrap();
        assert_eq!(&query[2..4], &[0x01, 0x00]);
        assert_eq!(query[5], 1);
        assert!(query.windows(b"example".len()).any(|w| w == b"example"));

        // Header + question for example.com A + one A answer 1.2.3.4 ttl 120.
        let mut msg = encode_a_query("example.com").unwrap();
        // ancount = 1
        msg[6] = 0;
        msg[7] = 1;
        // name pointer to offset 12
        msg.extend_from_slice(&[0xc0, 0x0c]);
        msg.extend_from_slice(&TYPE_A.to_be_bytes());
        msg.extend_from_slice(&1u16.to_be_bytes());
        msg.extend_from_slice(&120u32.to_be_bytes());
        msg.extend_from_slice(&4u16.to_be_bytes());
        msg.extend_from_slice(&[1, 2, 3, 4]);

        let records = parse_response(&msg, TYPE_A).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].address, "1.2.3.4");
        assert_eq!(records[0].family, 4);
        assert_eq!(records[0].ttl, 120);

        msg[3] = 3; // NXDOMAIN
        assert!(parse_response(&msg, TYPE_A).unwrap().is_empty());
    }
}
