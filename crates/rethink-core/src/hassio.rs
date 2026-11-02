//! Home Assistant add-on options → rethink config (upstream `util/hassio.ts`).
//!
//! The Supervisor writes the add-on options to a JSON file in the container. The
//! add-on manifest points `RETHINK_HASSIO_OPTIONS` at it, and that variable is what
//! selects add-on mode. Argv/config-file startup does not call this module.

use crate::config::{
    strip_jsonc_comments_pub, Advertise, BridgeConfig, HaConfig, PortSpec, PortSpecFull, RawConfig,
};
use crate::logging::log;
use serde::Serialize;
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};

/// Rendered on every start, as a starting point for users who want to switch to a
/// hand-written configuration file via the `config_file` option.
pub const HASSIO_EXAMPLE: &str = "config.jsonc.example";

/// DoH by IP, matching the example configuration. Used when `bridge_dns` is omitted.
pub const DEFAULT_BRIDGE_DNS: &[&str] = &["https://1.1.1.1/dns-query", "https://8.8.8.8/dns-query"];

const NO_MQTT_BROKER: &str = "No MQTT broker available: the Supervisor did not provide one. \
     Install an MQTT broker add-on, or set the mqtt_url option to point at your own broker.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupMode {
    /// `RETHINK_HASSIO_OPTIONS` is unset — read argv / the config file.
    ConfigFile,
    /// Add-on mode. `data_dir` is `RETHINK_DATA_DIR`.
    Hassio {
        options_path: PathBuf,
        data_dir: PathBuf,
    },
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

/// The broker rethink publishes to, and the credentials for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MqttBroker {
    pub url: String,
    pub user: String,
    pub pass: String,
}

/// The broker in the Supervisor's answer to `GET /services/mqtt`, if it names one.
///
/// The Supervisor only knows about brokers published by another add-on. One that was
/// configured directly in the MQTT integration is not visible there, so the user has
/// to point `mqtt_url` at it.
pub fn broker_from_service(answer: &Value) -> Option<MqttBroker> {
    let data = answer.get("data")?;
    let host = data.get("host")?.as_str().filter(|host| !host.is_empty())?;
    let port = match data.get("port")? {
        Value::Number(port) if port.as_u64().is_some_and(|port| port != 0) => port.to_string(),
        Value::String(port) if !port.is_empty() => port.clone(),
        _ => return None,
    };
    let ssl = data.get("ssl").and_then(Value::as_bool).unwrap_or(false);
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(MqttBroker {
        url: format!("{}://{host}:{port}", if ssl { "mqtts" } else { "mqtt" }),
        user: text("username"),
        pass: text("password"),
    })
}

/// Build a [`RawConfig`] from the add-on options file, or read the file that the
/// `config_file` option points at. Relative paths are resolved against `config_dir`
/// by the caller.
///
/// An explicit `mqtt_url` wins, otherwise `ask_supervisor` is called for the Home
/// Assistant MQTT service. There is nothing sensible to fall back to - the container
/// has no broker of its own - so failing here beats starting up unable to publish.
pub async fn load_hassio_config<F, Fut>(
    config_dir: &Path,
    options_path: &Path,
    ask_supervisor: F,
) -> Result<RawConfig, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Option<MqttBroker>>,
{
    let text = std::fs::read_to_string(options_path)
        .map_err(|e| format!("read {}: {e}", options_path.display()))?;
    let opts: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", options_path.display()))?;

    if let Some(config_file) = opt_str(&opts, "config_file") {
        let path = config_dir.join(config_file);
        log(
            "status",
            &[&format!(
                "Ignoring the add-on options, reading the configuration from {}",
                path.display()
            )],
        );
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        return serde_json::from_str(&strip_jsonc_comments_pub(&text))
            .map_err(|e| format!("{}: {e}", path.display()));
    }

    let mqtt = match explicit_broker(&opts) {
        Some(broker) => broker,
        None => ask_supervisor()
            .await
            .ok_or_else(|| NO_MQTT_BROKER.to_string())?,
    };
    let config = raw_config_from_options(&opts, mqtt);
    write_example(&config, config_dir);
    Ok(config)
}

fn explicit_broker(opts: &Value) -> Option<MqttBroker> {
    Some(MqttBroker {
        url: opt_str(opts, "mqtt_url")?,
        user: opt_str(opts, "mqtt_user").unwrap_or_default(),
        pass: opt_str(opts, "mqtt_pass").unwrap_or_default(),
    })
}

/// The container always binds the default ports and the Supervisor maps them to the
/// host side, so only the advertised value is configurable. It is needed when that
/// mapping is not 1:1, or when the appliances reach rethink through a proxy.
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

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn raw_config_from_options(opts: &Value, mqtt: MqttBroker) -> RawConfig {
    // An empty list selects the system resolver.
    let dns = match opts.get("bridge_dns") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => DEFAULT_BRIDGE_DNS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
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
    RawConfig {
        hostname: opt_str(opts, "hostname").unwrap_or_else(|| "rethink.lan".into()),
        homeassistant: HaConfig {
            mqtt_url: mqtt.url,
            discovery_prefix: opt_str(opts, "discovery_prefix")
                .unwrap_or_else(|| "homeassistant".into()),
            rethink_prefix: opt_str(opts, "rethink_prefix").unwrap_or_else(|| "rethink".into()),
            mqtt_user: mqtt.user,
            mqtt_pass: mqtt.pass,
        },
        ca_key_file: "ca.key".into(),
        ca_cert_file: "ca.cert".into(),
        custom_root_cert_file: opt_str(opts, "custom_root_cert_file"),
        http_port: None,
        https_port: Some(port(
            443,
            opts.get("https_advertise").and_then(|v| v.as_str()),
        )),
        mqtts_port: Some(port(
            8883,
            opts.get("mqtts_advertise").and_then(|v| v.as_str()),
        )),
        mqtt_port: Some(PortSpec::Number(1884)),
        management_port: Some(PortSpec::Number(44401)),
        thinq1_http_port: None,
        thinq1_https_port: None,
        thinq1_port: None,
        mqtt: None,
        // Bridge mode stays inert until an LG account is linked in the management
        // panel, so it costs nothing to always have the storage ready.
        bridge: Some(BridgeConfig {
            storage_path: "state".into(),
            dns: Some(dns),
        }),
        log: Some(log),
    }
}

/// The paths are written out already resolved, so that a hand-written copy of this
/// file keeps using the add-on data directory for whatever the user does not override.
fn write_example(config: &RawConfig, config_dir: &Path) {
    let resolve = |path: &str| config_dir.join(path).to_string_lossy().into_owned();
    let mut example = config.clone();
    example.ca_key_file = resolve(&config.ca_key_file);
    example.ca_cert_file = resolve(&config.ca_cert_file);
    example.custom_root_cert_file = config.custom_root_cert_file.as_deref().map(resolve);
    if let Some(bridge) = example.bridge.as_mut() {
        bridge.storage_path = resolve(&bridge.storage_path);
    }

    let path = config_dir.join(HASSIO_EXAMPLE);
    if let Err(e) = std::fs::write(&path, to_json(&example)) {
        log(
            "status",
            &[
                &format!("Could not write {}:", path.display()),
                &e.to_string(),
            ],
        );
    }
}

/// Four-space indented, with a final newline, like upstream's `JSON.stringify`.
fn to_json(config: &RawConfig) -> String {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    config
        .serialize(&mut serializer)
        .expect("a RawConfig always serializes");
    let mut text = String::from_utf8(out).expect("serde_json writes UTF-8");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{bridge_dns_uses_system, normalize, parse_config_text};
    use serde_json::json;
    use std::future::{ready, Ready};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scratch add-on data directory, removed afterwards.
    struct DataDir(PathBuf);

    impl DataDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rethink-hassio-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, options: Value) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, options.to_string()).unwrap();
            path
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }

        fn example(&self) -> Value {
            let text = std::fs::read_to_string(self.0.join(HASSIO_EXAMPLE)).unwrap();
            serde_json::from_str(&text).unwrap()
        }

        async fn load(&self, options: &Path) -> Result<RawConfig, String> {
            load_hassio_config(&self.0, options, never_asked).await
        }
    }

    impl Drop for DataDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn never_asked() -> Ready<Option<MqttBroker>> {
        panic!("the Supervisor must not be asked here")
    }

    /// Without a broker from either the options or the Supervisor, loading fails
    /// outright, so every test that is about something else has to supply one.
    fn with_broker(options: Value) -> Value {
        let mut all = json!({ "mqtt_url": "mqtt://10.0.0.5:1883" });
        all.as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        all
    }

    fn as_json(value: &impl Serialize) -> Value {
        serde_json::to_value(value).unwrap()
    }

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

    #[tokio::test]
    async fn an_explicit_mqtt_url_is_used_verbatim() {
        let dir = DataDir::new();
        let path = dir.write(
            "explicit.json",
            json!({
                "hostname": "my.lan",
                "mqtt_url": "mqtt://10.0.0.5:1883",
                "mqtt_user": "u",
                "mqtt_pass": "p",
                "log": ["status", "all"],
            }),
        );
        let c = dir.load(&path).await.unwrap();

        assert_eq!(c.hostname, "my.lan");
        assert_eq!(c.homeassistant.mqtt_url, "mqtt://10.0.0.5:1883");
        assert_eq!(c.homeassistant.mqtt_user, "u");
        assert_eq!(c.homeassistant.mqtt_pass, "p");
        let c = as_json(&c);
        assert_eq!(c["https_port"], 443);
        assert_eq!(c["mqtts_port"], 8883); // default applied
        assert_eq!(c["management_port"], 44401);
        assert_eq!(c["bridge"]["storage_path"], "state");
        assert_eq!(c["log"], json!(["status", "all"]));
    }

    #[tokio::test]
    async fn no_broker_from_the_options_or_the_supervisor_is_a_hard_failure() {
        let dir = DataDir::new();
        let path = dir.write("nobroker.json", json!({}));
        let err = load_hassio_config(&dir.0, &path, || ready(None))
            .await
            .unwrap_err();
        assert!(err.contains("No MQTT broker available"), "{err}");
        assert!(!dir.0.join(HASSIO_EXAMPLE).exists());
    }

    #[tokio::test]
    async fn without_an_mqtt_url_the_supervisor_broker_is_used() {
        // The manifest's defaults leave the optional fields as empty strings.
        let dir = DataDir::new();
        let path = dir.write(
            "defaults.json",
            json!({
                "hostname": "rethink.lan",
                "discovery_prefix": "homeassistant",
                "rethink_prefix": "rethink",
                "mqtt_url": "",
                "mqtt_user": "",
                "mqtt_pass": "",
                "https_advertise": "",
                "mqtts_advertise": "",
                "custom_root_cert_file": "",
                "config_file": "",
                "bridge_dns": DEFAULT_BRIDGE_DNS,
                "log": ["status", "incoming", "HTTPS", "publish", "MGMT"],
            }),
        );
        let mosquitto = MqttBroker {
            url: "mqtt://core-mosquitto:1883".into(),
            user: "addons".into(),
            pass: "secret".into(),
        };
        let c = load_hassio_config(&dir.0, &path, || ready(Some(mosquitto)))
            .await
            .unwrap();

        assert_eq!(c.homeassistant.mqtt_url, "mqtt://core-mosquitto:1883");
        assert_eq!(c.homeassistant.mqtt_user, "addons");
        assert_eq!(c.homeassistant.mqtt_pass, "secret");
        assert_eq!(c.custom_root_cert_file, None);
        let cfg = normalize(c).unwrap();
        assert_eq!(
            cfg.https_port.advertise_url.as_deref(),
            Some("https://rethink.lan:443")
        );
        assert_eq!(cfg.mqtts_port.bind, Some(8883));
    }

    #[tokio::test]
    async fn empty_options_fall_back_to_defaults() {
        let dir = DataDir::new();
        let path = dir.write("empty.json", with_broker(json!({})));
        let c = dir.load(&path).await.unwrap();

        assert_eq!(c.hostname, "rethink.lan");
        assert_eq!(c.homeassistant.discovery_prefix, "homeassistant");
        assert_eq!(c.homeassistant.rethink_prefix, "rethink");
        assert!(c.thinq1_https_port.is_none());
        assert!(c.thinq1_port.is_none());
        assert!(c.custom_root_cert_file.is_none());
        // Bridge mode is always set up in add-on mode, with no option to turn it off
        assert_eq!(
            as_json(&c.bridge),
            json!({ "storage_path": "state", "dns": DEFAULT_BRIDGE_DNS })
        );
        assert_eq!(
            c.log.clone().unwrap(),
            ["status", "incoming", "HTTPS", "publish", "MGMT"]
        );
        let cfg = normalize(c).unwrap();
        assert!(!bridge_dns_uses_system(cfg.bridge.unwrap().dns.as_deref()));
    }

    #[tokio::test]
    async fn bridge_dns_servers_are_taken_verbatim_an_empty_list_included() {
        let dir = DataDir::new();
        for bridge_dns in [
            json!(["192.168.1.1", "https://9.9.9.9/dns-query"]),
            json!([]),
        ] {
            let options = with_broker(json!({ "bridge_dns": bridge_dns.clone() }));
            let c = dir
                .load(&dir.write("bridge-dns.json", options))
                .await
                .unwrap();
            assert_eq!(as_json(&c.bridge.as_ref().unwrap().dns), bridge_dns);

            let dns = normalize(c).unwrap().bridge.unwrap().dns;
            let empty = bridge_dns.as_array().unwrap().is_empty();
            assert_eq!(bridge_dns_uses_system(dns.as_deref()), empty);
        }
    }

    #[tokio::test]
    async fn an_advertised_port_is_taken_as_a_number_anything_else_as_a_verbatim_url() {
        let dir = DataDir::new();
        let path = dir.write(
            "advertise.json",
            with_broker(json!({
                "https_advertise": "443",
                "mqtts_advertise": "ssl://proxy.lan:8883",
            })),
        );
        let c = dir.load(&path).await.unwrap();

        assert_eq!(
            as_json(&c.https_port),
            json!({ "bind": 443, "advertise": 443 })
        );
        assert_eq!(
            as_json(&c.mqtts_port),
            json!({ "bind": 8883, "advertise": "ssl://proxy.lan:8883" })
        );
        let cfg = normalize(c).unwrap();
        assert_eq!(
            cfg.https_port.advertise_url.as_deref(),
            Some("https://rethink.lan:443")
        );
        assert_eq!(
            cfg.mqtts_port.advertise_url.as_deref(),
            Some("ssl://proxy.lan:8883")
        );
    }

    #[tokio::test]
    async fn the_rendered_example_carries_paths_resolved_against_the_data_directory() {
        let dir = DataDir::new();
        let path = dir.write("example.json", with_broker(json!({})));
        dir.load(&path).await.unwrap();
        let example = dir.example();

        assert_eq!(example["ca_key_file"], dir.path("ca.key"));
        assert_eq!(example["ca_cert_file"], dir.path("ca.cert"));
        assert_eq!(
            example["bridge"],
            json!({ "storage_path": dir.path("state"), "dns": DEFAULT_BRIDGE_DNS })
        );

        // Laid out like upstream's, and usable as it is through config_file.
        let text = std::fs::read_to_string(dir.0.join(HASSIO_EXAMPLE)).unwrap();
        assert!(text.starts_with("{\n    \"hostname\": \"rethink.lan\",\n"));
        assert!(text.ends_with("]\n}\n"));
        assert!(!text.contains("null"), "unset options are left out");
        parse_config_text(&text).unwrap();
    }

    #[tokio::test]
    async fn a_custom_root_certificate_is_passed_through_and_resolved_in_the_example() {
        let dir = DataDir::new();
        let path = dir.write(
            "root-cert.json",
            with_broker(json!({ "custom_root_cert_file": "proxy-root.pem" })),
        );
        let c = dir.load(&path).await.unwrap();

        assert_eq!(c.custom_root_cert_file.as_deref(), Some("proxy-root.pem"));
        assert_eq!(
            dir.example()["custom_root_cert_file"],
            dir.path("proxy-root.pem")
        );
    }

    #[tokio::test]
    async fn config_file_wins_over_the_options_and_is_read_relative_to_the_data_directory() {
        let dir = DataDir::new();
        // Upstream's test file holds only the keys it checks; a RawConfig needs the rest.
        std::fs::write(
            dir.0.join("custom.jsonc"),
            r#"{ /* hand-written */ "hostname": "custom.lan",
                "homeassistant": { "mqtt_url": "mqtt://broker.lan:1883",
                    "discovery_prefix": "homeassistant", "rethink_prefix": "rethink" },
                "ca_key_file": "ca.key", "ca_cert_file": "ca.cert",
                "https_port": { "bind": 4443, "advertise": 443 }, "mqtts_port": 8883 }"#,
        )
        .unwrap();
        let path = dir.write(
            "custom-options.json",
            json!({ "hostname": "ignored.lan", "config_file": "custom.jsonc" }),
        );
        let c = dir.load(&path).await.unwrap();

        assert_eq!(c.hostname, "custom.lan");
        assert_eq!(
            as_json(&c.https_port),
            json!({ "bind": 4443, "advertise": 443 })
        );
        assert!(
            !dir.0.join(HASSIO_EXAMPLE).exists(),
            "the example is rendered from the options only"
        );
    }

    #[test]
    fn the_supervisor_service_answer_names_the_broker() {
        let answer = json!({
            "result": "ok",
            "data": {
                "addon": "core_mosquitto",
                "host": "core-mosquitto",
                "port": 1883,
                "ssl": false,
                "username": "addons",
                "password": "pw",
                "protocol": "3.1.1",
            },
        });
        assert_eq!(
            broker_from_service(&answer),
            Some(MqttBroker {
                url: "mqtt://core-mosquitto:1883".into(),
                user: "addons".into(),
                pass: "pw".into(),
            })
        );
        let tls = json!({ "data": { "host": "broker", "port": 8883, "ssl": true } });
        assert_eq!(
            broker_from_service(&tls),
            Some(MqttBroker {
                url: "mqtts://broker:8883".into(),
                user: String::new(),
                pass: String::new(),
            })
        );

        // A broker takes both a host and a port.
        for data in [
            json!({ "host": "broker" }),
            json!({ "port": 1883 }),
            json!({ "host": "", "port": 1883 }),
            json!({ "host": "broker", "port": 0 }),
        ] {
            assert_eq!(
                broker_from_service(&json!({ "data": data })),
                None,
                "{data}"
            );
        }
        let refused = json!({ "result": "error", "message": "No access to mqtt service!" });
        assert_eq!(broker_from_service(&refused), None);
    }
}
