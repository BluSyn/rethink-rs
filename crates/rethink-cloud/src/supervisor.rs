//! The Home Assistant Supervisor API, as seen from inside the add-on container.

use rethink_core::hassio::{broker_from_service, MqttBroker};
use rethink_core::logging::log;
use serde_json::Value;
use std::time::Duration;

/// Where the Supervisor tells add-ons about the MQTT broker another add-on provides.
pub const MQTT_SERVICE_URL: &str = "http://supervisor/services/mqtt";

/// Ask the Supervisor for the Home Assistant MQTT service.
///
/// `None` without a token (`SUPERVISOR_TOKEN` is only set for an add-on), when the
/// Supervisor has no broker to offer, and when it cannot be asked at all, which is logged.
pub async fn mqtt_service(url: &str, token: Option<&str>) -> Option<MqttBroker> {
    let token = token.filter(|token| !token.is_empty())?;
    match ask(url, token).await {
        Ok(answer) => answer.as_ref().and_then(broker_from_service),
        Err(e) => {
            let reason = format!("{:#}", anyhow::Error::new(e));
            log(
                "status",
                &[
                    "Could not ask the Supervisor for the MQTT service:",
                    &reason,
                ],
            );
            None
        }
    }
}

/// The JSON answer, or `None` for an unsuccessful status.
async fn ask(url: &str, token: &str) -> reqwest::Result<Option<Value>> {
    let client = reqwest::Client::builder()
        // The Supervisor is on the add-on network, which a proxy from the environment is not.
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client.get(url).bearer_auth(token).send().await?;
    if !response.status().is_success() {
        return Ok(None);
    }
    response.json().await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::config::normalize;
    use rethink_core::hassio::{load_hassio_config, HASSIO_EXAMPLE};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    const MOSQUITTO: &str = r#"{"result": "ok", "data": {"addon": "core_mosquitto",
        "host": "core-mosquitto", "port": 1883, "ssl": false, "username": "addons",
        "password": "secret", "protocol": "3.1.1"}}"#;

    /// A stand-in Supervisor on 127.0.0.1 that answers one request, which it hands back.
    async fn supervisor(status: &str, body: &str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/services/mqtt", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            let _ = stream.shutdown().await;
            String::from_utf8(request).unwrap()
        });
        (url, served)
    }

    fn mosquitto() -> MqttBroker {
        MqttBroker {
            url: "mqtt://core-mosquitto:1883".into(),
            user: "addons".into(),
            pass: "secret".into(),
        }
    }

    #[tokio::test]
    async fn the_broker_comes_from_the_mqtt_service_with_the_token() {
        let (url, served) = supervisor("200 OK", MOSQUITTO).await;
        assert_eq!(
            mqtt_service(&url, Some("token-123")).await,
            Some(mosquitto())
        );

        let request = served.await.unwrap().to_ascii_lowercase();
        assert!(
            request.starts_with("get /services/mqtt http/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains("\r\nauthorization: bearer token-123\r\n"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn a_refusal_or_an_unreadable_answer_names_no_broker() {
        let refusal = r#"{"result": "error", "message": "No access to mqtt service!"}"#;
        let (url, served) = supervisor("403 Forbidden", refusal).await;
        assert_eq!(mqtt_service(&url, Some("token")).await, None);
        served.await.unwrap();

        let (url, served) = supervisor("200 OK", "<html>not json</html>").await;
        assert_eq!(mqtt_service(&url, Some("token")).await, None);
        served.await.unwrap();

        // Answered, but with no broker in it.
        let (url, served) = supervisor("200 OK", r#"{"result": "ok", "data": {}}"#).await;
        assert_eq!(mqtt_service(&url, Some("token")).await, None);
        served.await.unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_supervisor_names_no_broker() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/services/mqtt", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(mqtt_service(&url, Some("token")).await, None);
    }

    #[tokio::test]
    async fn without_a_token_the_supervisor_is_not_asked() {
        let (url, served) = supervisor("200 OK", MOSQUITTO).await;
        assert_eq!(mqtt_service(&url, None).await, None);
        assert_eq!(mqtt_service(&url, Some("")).await, None);
        assert!(!served.is_finished(), "no request was made");
        served.abort();
    }

    #[tokio::test]
    async fn the_mosquitto_add_on_setup_needs_no_mqtt_url() {
        let dir = std::env::temp_dir().join(format!(
            "rethink-supervisor-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // The defaults from the add-on manifest.
        let options = dir.join("options.json");
        std::fs::write(
            &options,
            r#"{"hostname": "rethink.lan", "discovery_prefix": "homeassistant",
                "rethink_prefix": "rethink", "mqtt_url": "", "mqtt_user": "", "mqtt_pass": "",
                "https_advertise": "", "mqtts_advertise": "", "custom_root_cert_file": "",
                "config_file": "", "bridge_dns": ["https://1.1.1.1/dns-query",
                "https://8.8.8.8/dns-query"], "log": ["status", "incoming", "HTTPS",
                "publish", "MGMT"]}"#,
        )
        .unwrap();

        let (url, served) = supervisor("200 OK", MOSQUITTO).await;
        let raw = load_hassio_config(&dir, &options, || mqtt_service(&url, Some("token")))
            .await
            .unwrap();
        served.await.unwrap();

        let example = std::fs::read_to_string(dir.join(HASSIO_EXAMPLE)).unwrap();
        assert!(example.contains("\"mqtt_url\": \"mqtt://core-mosquitto:1883\""));
        let config = normalize(raw).unwrap();
        assert_eq!(config.homeassistant.mqtt_url, "mqtt://core-mosquitto:1883");
        assert_eq!(config.homeassistant.mqtt_user, "addons");
        assert_eq!(config.homeassistant.mqtt_pass, "secret");
        assert_eq!(
            config.https_port.advertise_url.as_deref(),
            Some("https://rethink.lan:443")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
