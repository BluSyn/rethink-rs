//! Home Assistant add-on options → rethink config (upstream `util/hassio.ts`).
//!
//! Selected only when `RETHINK_HASSIO_OPTIONS` is set. Argv/config-file startup
//! does not call this module.

use crate::config::{Advertise, BridgeConfig, HaConfig, PortSpec, PortSpecFull, RawConfig};
use std::path::{Path, PathBuf};

pub const HASSIO_EXAMPLE: &str = "config.jsonc.example";

/// DoH by IP, matching the example configuration. Used when `bridge_dns` is omitted.
pub const DEFAULT_BRIDGE_DNS: &[&str] = &[
    "https://1.1.1.1/dns-query",
    "https://8.8.8.8/dns-query",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupMode {
    /// `RETHINK_HASSIO_OPTIONS` is unset — read argv / the config file.
    ConfigFile,
    /// Add-on mode. `data_dir` is `RETHINK_DATA_DIR`.
    Hassio { options_path: PathBuf, data_dir: PathBuf },
}

/// How `rethink-cloud` chooses its configuration. Hassio mode runs only when the env var is set.
pub fn startup_mode(
    hassio_options: Option<&str>,
    data_dir: Option<&str>,
) -> Result<StartupMode, String> {
    match hassio_options {
        None | Some("") => Ok(StartupMode::ConfigFile),
        Some(options) => {
            let data = data_dir.filter(|s| !s.is_empty()).ok_or_else(|| {
                "RETHINK_DATA_DIR must be set together with RETHINK_HASSIO_OPTIONS".to_string()
            })?;
            Ok(StartupMode::Hassio {
                options_path: PathBuf::from(options),
                data_dir: PathBuf::from(data),
            })
        }
    }
}

fn port(bind: u16, advertise: Option<&str>) -> PortSpec {
    match advertise {
        None | Some("") => PortSpec::Number(bind),
        Some(adv) if adv.chars().all(|c| c.is_ascii_digit()) => PortSpec::Full(PortSpecFull {
            bind: Some(bind),
            advertise: adv.parse::<u16>().ok().map(Advertise::Number),
            address: None,
        }),
        Some(adv) => PortSpec::Full(PortSpecFull {
            bind: Some(bind),
            advertise: Some(Advertise::Url(adv.to_string())),
            address: None,
        }),
    }
}

fn opt_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Build a [`RawConfig`] from add-on options JSON.
///
/// An explicit `mqtt_url` wins. Without one, this returns the same error upstream
/// raises when the Supervisor did not provide a broker — the HTTP call itself is
/// not made here (no Supervisor in unit tests or in a normal process).
pub fn raw_config_from_options(opts: &serde_json::Value) -> Result<RawConfig, String> {
    if let Some(config_file) = opt_str(opts, "config_file") {
        return Err(format!("config_file:{config_file}"));
    }
    let mqtt_url = opt_str(opts, "mqtt_url").ok_or_else(|| {
        "No MQTT broker available: the Supervisor did not provide one. Install an MQTT broker add-on, \
         or set the mqtt_url option to point at your own broker."
            .to_string()
    })?;
    let dns = match opts.get("bridge_dns") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => DEFAULT_BRIDGE_DNS.iter().map(|s| (*s).to_string()).collect(),
    };
    let log = opts
        .get("log")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_else(|| {
            ["status", "incoming", "HTTPS", "publish", "MGMT"]
                .into_iter()
                .map(|s| s.to_string())
                .collect()
        });
    Ok(RawConfig {
        hostname: opt_str(opts, "hostname").unwrap_or_else(|| "rethink.lan".into()),
        homeassistant: HaConfig {
            mqtt_url,
            discovery_prefix: opt_str(opts, "discovery_prefix")
                .unwrap_or_else(|| "homeassistant".into()),
            rethink_prefix: opt_str(opts, "rethink_prefix").unwrap_or_else(|| "rethink".into()),
            mqtt_user: opt_str(opts, "mqtt_user").unwrap_or_default(),
            mqtt_pass: opt_str(opts, "mqtt_pass").unwrap_or_default(),
        },
        ca_key_file: "ca.key".into(),
        ca_cert_file: "ca.cert".into(),
        custom_root_cert_file: opt_str(opts, "custom_root_cert_file"),
        http_port: None,
        https_port: port(443, opts.get("https_advertise").and_then(|v| v.as_str())),
        mqtts_port: port(8883, opts.get("mqtts_advertise").and_then(|v| v.as_str())),
        mqtt_port: Some(PortSpec::Number(1884)),
        management_port: Some(PortSpec::Number(44401)),
        thinq1_http_port: None,
        thinq1_https_port: None,
        thinq1_port: None,
        mqtt: None,
        bridge: Some(BridgeConfig {
            storage_path: "state".into(),
            dns: Some(dns),
        }),
        log: Some(log),
    })
}

/// Read add-on options. `config_file`, when set, is loaded as JSONC from `config_dir`.
pub fn load_hassio_options(config_dir: &Path, options_json: &str) -> Result<RawConfig, String> {
    let opts: serde_json::Value =
        serde_json::from_str(options_json).map_err(|e| format!("options JSON: {e}"))?;
    if let Some(config_file) = opt_str(&opts, "config_file") {
        let path = config_dir.join(config_file);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let stripped = crate::config::strip_jsonc_comments_pub(&text);
        return serde_json::from_str(&stripped).map_err(|e| format!("config file JSON: {e}"));
    }
    raw_config_from_options(&opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{bridge_dns_uses_system, normalize};

    #[test]
    fn hassio_mode_is_off_unless_the_env_var_is_set() {
        assert_eq!(startup_mode(None, None).unwrap(), StartupMode::ConfigFile);
        assert_eq!(
            startup_mode(None, Some("/data")).unwrap(),
            StartupMode::ConfigFile
        );
        assert_eq!(
            startup_mode(Some(""), Some("/data")).unwrap(),
            StartupMode::ConfigFile
        );
        assert!(startup_mode(Some("/data/options.json"), None).is_err());
    }

    #[test]
    fn options_map_to_config_with_default_doh_when_bridge_dns_omitted() {
        let opts = serde_json::json!({
            "mqtt_url": "mqtt://ha.local:1883",
            "mqtt_user": "u",
            "mqtt_pass": "p",
            "https_advertise": "https://proxy.example/thinq"
        });
        let raw = raw_config_from_options(&opts).unwrap();
        let cfg = normalize(raw).unwrap();
        assert_eq!(cfg.hostname, "rethink.lan");
        assert_eq!(cfg.homeassistant.mqtt_url, "mqtt://ha.local:1883");
        assert_eq!(cfg.https_port.bind, Some(443));
        assert_eq!(
            cfg.https_port.advertise_url.as_deref(),
            Some("https://proxy.example/thinq")
        );
        assert_eq!(cfg.mqtts_port.bind, Some(8883));
        let dns = cfg.bridge.unwrap().dns.unwrap();
        assert_eq!(
            dns,
            [
                "https://1.1.1.1/dns-query",
                "https://8.8.8.8/dns-query"
            ]
        );
        assert!(!bridge_dns_uses_system(Some(&dns)));
    }

    #[test]
    fn explicit_bridge_dns_order_is_kept_and_empty_means_system() {
        let opts = serde_json::json!({
            "mqtt_url": "mqtts://broker:8883",
            "bridge_dns": ["8.8.8.8:53", "https://1.1.1.1/dns-query"]
        });
        let cfg = normalize(raw_config_from_options(&opts).unwrap()).unwrap();
        assert_eq!(
            cfg.bridge.unwrap().dns.unwrap(),
            ["8.8.8.8:53", "https://1.1.1.1/dns-query"]
        );

        let opts = serde_json::json!({
            "mqtt_url": "mqtt://broker:1883",
            "bridge_dns": []
        });
        let cfg = normalize(raw_config_from_options(&opts).unwrap()).unwrap();
        let dns = cfg.bridge.unwrap().dns.unwrap();
        assert!(dns.is_empty());
        assert!(bridge_dns_uses_system(Some(&dns)));
    }

    #[test]
    fn missing_mqtt_url_does_not_invent_a_broker() {
        let err = raw_config_from_options(&serde_json::json!({})).unwrap_err();
        assert!(err.contains("No MQTT broker"));
    }
}
