//! Config loading and normalization (JSONC-compatible).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaConfig {
    pub mqtt_url: String,
    pub discovery_prefix: String,
    pub rethink_prefix: String,
    #[serde(default)]
    pub mqtt_user: String,
    #[serde(default)]
    pub mqtt_pass: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Advertise {
    Number(u16),
    Url(String),
}

/// A port as written in JSONC. `bind` and `advertise` are both optional:
/// a port may be advertised without being bound, and `advertise` may be a full URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortSpecFull {
    #[serde(default)]
    pub bind: Option<u16>,
    #[serde(default)]
    pub advertise: Option<Advertise>,
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PortSpec {
    Number(u16),
    Full(PortSpecFull),
}

/// Normalized listen/advertise port.
///
/// `bind == None` means do not listen. `advertise_url` is set only for the
/// ports the appliances are told about (`https` and `mqtts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub bind: Option<u16>,
    pub address: Option<String>,
    pub advertise_url: Option<String>,
}

impl Port {
    pub fn unbound() -> Self {
        Self {
            bind: None,
            address: None,
            advertise_url: None,
        }
    }

    pub fn should_bind(&self) -> bool {
        self.bind.is_some()
    }

    /// `host:port` for `TcpListener::bind`, or `None` when this port is not bound.
    pub fn bind_socket(&self) -> Option<String> {
        self.bind.map(|port| {
            let host = self.address.as_deref().unwrap_or("0.0.0.0");
            format!("{host}:{port}")
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub storage_path: String,
    /// DoH URLs or plain DNS addresses, tried in order. Absent or empty uses the system resolver.
    #[serde(default)]
    pub dns: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawConfig {
    pub hostname: String,
    pub homeassistant: HaConfig,
    pub ca_key_file: String,
    pub ca_cert_file: String,
    /// PEM served from `/route/certificate` instead of the builtin CA.
    #[serde(default)]
    pub custom_root_cert_file: Option<String>,
    #[serde(default)]
    pub http_port: Option<PortSpec>,
    pub https_port: PortSpec,
    pub mqtts_port: PortSpec,
    #[serde(default)]
    pub mqtt_port: Option<PortSpec>,
    #[serde(default)]
    pub management_port: Option<PortSpec>,
    #[serde(default)]
    pub thinq1_http_port: Option<PortSpec>,
    #[serde(default)]
    pub thinq1_https_port: Option<PortSpec>,
    #[serde(default)]
    pub thinq1_port: Option<PortSpec>,
    #[serde(default)]
    pub mqtt: Option<bool>,
    #[serde(default)]
    pub bridge: Option<BridgeConfig>,
    #[serde(default)]
    pub log: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub hostname: String,
    pub homeassistant: HaConfig,
    pub ca_key_file: String,
    pub ca_cert_file: String,
    pub custom_root_cert_file: Option<String>,
    pub http_port: Port,
    pub https_port: Port,
    pub mqtts_port: Port,
    pub mqtt_port: Port,
    pub management_port: Port,
    pub thinq1_http_port: Port,
    pub thinq1_https_port: Port,
    pub thinq1_port: Port,
    pub mqtt: bool,
    pub bridge: Option<BridgeConfig>,
    pub log: Vec<String>,
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Json(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::Json(s) => write!(f, "JSON parse error: {s}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(_) => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn nonzero_bind(port: u16) -> Option<u16> {
    // JSON `0` is falsy in the upstream parser (`input.bind`), so it does not bind.
    if port == 0 {
        None
    } else {
        Some(port)
    }
}

fn listen_parts(input: &PortSpec) -> (Option<u16>, Option<String>, Option<Advertise>) {
    match input {
        // A bare number always binds, including 0 (upstream `parsePort`).
        PortSpec::Number(n) => (Some(*n), None, None),
        PortSpec::Full(full) => (
            full.bind.and_then(nonzero_bind),
            full.address.clone(),
            full.advertise.clone(),
        ),
    }
}

fn parse_listen(input: Option<&PortSpec>) -> Port {
    let Some(input) = input else {
        return Port::unbound();
    };
    let (bind, address, _) = listen_parts(input);
    Port {
        bind,
        address,
        advertise_url: None,
    }
}

fn parse_advertised(input: &PortSpec, url_base: &str, label: &str) -> Result<Port, ConfigError> {
    let (bind, address, advertise) = listen_parts(input);
    let advertise_url = match advertise {
        Some(Advertise::Url(url)) => url,
        Some(Advertise::Number(n)) => format!("{url_base}:{n}"),
        None => match bind {
            Some(port) => format!("{url_base}:{port}"),
            None => {
                return Err(ConfigError::Json(format!(
                    "The {label} port must be advertised"
                )))
            }
        },
    };
    Ok(Port {
        bind,
        address,
        advertise_url: Some(advertise_url),
    })
}

/// True when bridge lookups should use the system resolver (absent or empty `bridge.dns`).
pub fn bridge_dns_uses_system(dns: Option<&[String]>) -> bool {
    dns.map(|entries| entries.is_empty()).unwrap_or(true)
}

pub fn normalize(raw: RawConfig) -> Result<Config, ConfigError> {
    let hostname = raw.hostname.clone();
    Ok(Config {
        https_port: parse_advertised(
            &raw.https_port,
            &format!("https://{hostname}"),
            "https",
        )?,
        mqtts_port: parse_advertised(&raw.mqtts_port, &format!("ssl://{hostname}"), "mqtts")?,
        http_port: parse_listen(raw.http_port.as_ref()),
        mqtt_port: parse_listen(raw.mqtt_port.as_ref()),
        management_port: parse_listen(raw.management_port.as_ref()),
        thinq1_http_port: parse_listen(raw.thinq1_http_port.as_ref()),
        thinq1_https_port: match raw.thinq1_https_port.as_ref() {
            Some(spec) => parse_listen(Some(spec)),
            None => parse_listen(Some(&PortSpec::Number(46030))),
        },
        thinq1_port: match raw.thinq1_port.as_ref() {
            Some(spec) => parse_listen(Some(spec)),
            None => parse_listen(Some(&PortSpec::Number(47878))),
        },
        hostname: raw.hostname,
        homeassistant: raw.homeassistant,
        ca_key_file: raw.ca_key_file,
        ca_cert_file: raw.ca_cert_file,
        custom_root_cert_file: raw.custom_root_cert_file,
        mqtt: raw.mqtt.unwrap_or(true),
        bridge: raw.bridge,
        log: raw
            .log
            .unwrap_or_else(|| vec!["status".into(), "incoming".into(), "HTTPS".into()]),
    })
}

/// Parse JSON or JSONC text into a normalized Config.
pub fn parse_config_text(text: &str) -> Result<Config, ConfigError> {
    // Strip // and /* */ comments for JSONC support
    let stripped = strip_jsonc_comments(text);
    let raw: RawConfig =
        serde_json::from_str(&stripped).map_err(|e| ConfigError::Json(e.to_string()))?;
    normalize(raw)
}

pub fn load_config(path: &std::path::Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path)?;
    parse_config_text(&text)
}

/// Minimal JSONC comment stripper (handles // line and /* */ block comments outside strings).
pub fn strip_jsonc_comments_pub(input: &str) -> String {
    strip_jsonc_comments(input)
}

fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                // line comment
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_jsonc_with_comments() {
        let text = r#"{
  "hostname": "rethink.lan", // comment
  "homeassistant": {
    "mqtt_url": "mqtt://localhost:1883",
    "discovery_prefix": "homeassistant",
    "rethink_prefix": "rethink",
    "mqtt_user": "",
    "mqtt_pass": ""
  },
  "ca_key_file": "ca.key",
  "ca_cert_file": "ca.cert",
  "https_port": 443,
  "mqtts_port": 8883,
  "mqtt_port": 1884
}"#;
        let cfg = parse_config_text(text).unwrap();
        assert_eq!(cfg.hostname, "rethink.lan");
        assert_eq!(cfg.https_port.bind, Some(443));
        assert_eq!(
            cfg.https_port.advertise_url.as_deref(),
            Some("https://rethink.lan:443")
        );
        assert_eq!(cfg.thinq1_https_port.bind, Some(46030));
        assert!(!cfg.http_port.should_bind(), "omitted http_port does not bind");
        assert!(!cfg.thinq1_http_port.should_bind());
        assert!(cfg.mqtt);
    }

    #[test]
    fn parse_port_object() {
        let text = r#"{
  "hostname": "x",
  "homeassistant": {
    "mqtt_url": "mqtt://localhost:1883",
    "discovery_prefix": "homeassistant",
    "rethink_prefix": "rethink",
    "mqtt_user": "",
    "mqtt_pass": ""
  },
  "ca_key_file": "k",
  "ca_cert_file": "c",
  "https_port": { "bind": 4433, "advertise": 443 },
  "mqtts_port": 8883,
  "mqtt_port": 1884
}"#;
        let cfg = parse_config_text(text).unwrap();
        assert_eq!(cfg.https_port.bind, Some(4433));
        assert_eq!(
            cfg.https_port.advertise_url.as_deref(),
            Some("https://x:443")
        );
    }

    #[test]
    fn advertise_may_be_a_url_and_bind_may_be_absent() {
        let text = r#"{
  "hostname": "rethink.lan",
  "homeassistant": {
    "mqtt_url": "mqtt://localhost:1883",
    "discovery_prefix": "homeassistant",
    "rethink_prefix": "rethink"
  },
  "ca_key_file": "k",
  "ca_cert_file": "c",
  "https_port": { "advertise": "https://proxy.example/thinq" },
  "mqtts_port": { "advertise": "ssl://proxy.example:8883" },
  "bridge": { "storage_path": "state", "dns": ["https://1.1.1.1/dns-query", "8.8.8.8"] }
}"#;
        let cfg = parse_config_text(text).unwrap();
        assert!(!cfg.https_port.should_bind());
        assert_eq!(
            cfg.https_port.advertise_url.as_deref(),
            Some("https://proxy.example/thinq")
        );
        assert!(!cfg.mqtts_port.should_bind());
        assert_eq!(
            cfg.mqtts_port.advertise_url.as_deref(),
            Some("ssl://proxy.example:8883")
        );
        assert!(!cfg.mqtt_port.should_bind());
        assert!(!cfg.http_port.should_bind());
        let dns = cfg.bridge.unwrap().dns.unwrap();
        assert_eq!(dns, ["https://1.1.1.1/dns-query", "8.8.8.8"]);
        assert!(!bridge_dns_uses_system(Some(&dns)));
        assert!(bridge_dns_uses_system(None));
        assert!(bridge_dns_uses_system(Some(&[])));
    }
}
