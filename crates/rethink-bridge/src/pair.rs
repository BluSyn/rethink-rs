//! ThinQ2 device pairing (port of Thinq2Device.pair in bridge/thinqApi.ts).

use crate::state::Environment;
use base64::Engine;
use rand::RngCore;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const IOT_BASE_URL: &str = "https://common.lgthinq.com";

/// CA fetch URL after `/route` has named the regional `apiServer`.
pub fn route_certificate_url(api_server: &str) -> String {
    format!("{api_server}/route/certificate?name=aws-iot")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thinq2DeviceState {
    pub country_code: String,
    pub api_server: String,
    pub mqtt_server: String,
    pub ca_certificate: String,
    pub private_key: String,
    pub certificate: String,
    pub pub_topic: String,
    pub prov_topic: String,
    pub sub_topic: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thinq1DeviceState {
    pub rti_server: String,
    pub http_server: String,
}

/// Result of pair(): LG-side state + ciphertext for addDevice.
pub struct PairResult {
    pub state: Thinq2DeviceState,
    /// Final ciphertext buffer returned by pair() for addDevice body.
    pub add_device_ciphertext: Vec<u8>,
}

async fn api_fetch_json(url: &str, method: &str, headers: &[(&str, &str)], body: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let mut req = match method {
        "POST" => client.post(url),
        _ => client.get(url),
    };
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").body(b.to_string());
    }
    let resp = req.send().await?;
    let out: serde_json::Value = resp.json().await?;
    let code = out.get("resultCode").and_then(|v| v.as_str()).unwrap_or("");
    if !code.is_empty() && code != "0000" {
        anyhow::bail!("thinq route error {code}: {out}");
    }
    // Some IoT endpoints return result wrapper, others return flat body
    Ok(out.get("result").cloned().unwrap_or(out))
}

fn rsa_pkcs1_encrypt(public_key_pem: &str, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    // OTP publicKey may be bare base64 or PEM
    let pem = if public_key_pem.contains("BEGIN") {
        public_key_pem.to_string()
    } else {
        format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----",
            public_key_pem
                .as_bytes()
                .chunks(64)
                .map(|c| std::str::from_utf8(c).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let pubkey = RsaPublicKey::from_public_key_pem(&pem)
        .or_else(|_| {
            // try SPKI DER from base64
            let der = base64::engine::general_purpose::STANDARD.decode(public_key_pem.trim())?;
            RsaPublicKey::from_public_key_der(&der).map_err(|e| anyhow::anyhow!("{e}"))
        })?;
    let mut rng = rand::thread_rng();
    let encrypted = pubkey
        .encrypt(&mut rng, Pkcs1v15Encrypt, plaintext)
        .map_err(|e| anyhow::anyhow!("RSA encrypt: {e}"))?;
    Ok(encrypted)
}

/// Generate an EC P-256 key and CSR in-process (no `openssl` CLI).
async fn generate_ec_key_and_csr() -> anyhow::Result<(String, String, String)> {
    let group = openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1)?;
    let ec = openssl::ec::EcKey::generate(&group)?;
    let key = openssl::pkey::PKey::from_ec_key(ec)?;
    let mut name = openssl::x509::X509NameBuilder::new()?;
    name.append_entry_by_text("CN", "*.clip.com")?;
    name.append_entry_by_text("O", "LGE")?;
    name.append_entry_by_text("C", "KR")?;
    let mut req = openssl::x509::X509ReqBuilder::new()?;
    req.set_subject_name(name.build().as_ref())?;
    req.set_pubkey(&key)?;
    req.sign(&key, openssl::hash::MessageDigest::sha256())?;
    let private_key = String::from_utf8(key.private_key_to_pem_pkcs8()?)?;
    let public_key = String::from_utf8(key.public_key_to_pem()?)?;
    let csr = String::from_utf8(req.build().to_pem()?)?;
    Ok((private_key, public_key, csr))
}

/// Full ThinQ2 pair flow: route → CA → EC key/CSR → certificate → mqtt state.
pub async fn pair_thinq2(
    env: &Environment,
    device_id: &str,
    otp: &str,
    public_key_pem: &str,
) -> anyhow::Result<PairResult> {
    let mut nonce = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut nonce);

    // Route (with timeout race similar to TS)
    let route_url = format!("{IOT_BASE_URL}/route");
    let country = env.country_code.clone();
    let servers = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        api_fetch_json(
            &route_url,
            "GET",
            &[
                ("x-country-code", country.as_str()),
                ("x-service-phase", "OP"),
                ("accept", "application/json"),
            ],
            None,
        )
        .await
    })
    .await
    .map_err(|_| anyhow::anyhow!("route fetch failed on {IOT_BASE_URL}"))?
    .map_err(|e| {
        anyhow::anyhow!(
            "Failed to fetch {IOT_BASE_URL}, make sure that you are not redirecting this address: {e}"
        )
    })?;

    let api_server = servers
        .get("apiServer")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing apiServer"))?
        .to_string();
    let mqtt_server = servers
        .get("mqttServer")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing mqttServer"))?
        .to_string();

    // Region apiServer, not common.lgthinq.com — master 980b42e. The common
    // host returned a root that only matched some regions' MQTT brokers.
    let ca_resp = api_fetch_json(
        &route_certificate_url(&api_server),
        "GET",
        &[("accept", "application/json")],
        None,
    )
    .await?;
    let ca = ca_resp
        .get("certificatePem")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing CA certificatePem"))?
        .to_string();

    let (private_key, public_key, csr) = generate_ec_key_and_csr().await?;

    let mut plain = Vec::new();
    plain.extend_from_slice(&nonce);
    plain.extend_from_slice(otp.as_bytes());
    plain.extend_from_slice(&Sha256::digest(device_id.as_bytes()));
    plain.extend_from_slice(&Sha256::digest(csr.as_bytes()));
    plain.extend_from_slice(&Sha256::digest(public_key.as_bytes()));
    let ciphertext = rsa_pkcs1_encrypt(public_key_pem, &plain)?;

    let body = serde_json::json!({
        "otp": otp,
        "csr": csr,
        "publickey": public_key,
        "ciphertext": base64::engine::general_purpose::STANDARD.encode(&ciphertext),
    });
    let device_config = api_fetch_json(
        &format!("{api_server}/device/{device_id}/certificate"),
        "POST",
        &[
            ("x-provide-type", "immediate"),
            ("Content-type", "application/json"),
        ],
        Some(&body.to_string()),
    )
    .await?;

    let certificate = device_config
        .get("certificatePem")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing device certificatePem"))?
        .to_string();
    let pub_topic = device_config
        .pointer("/publication/message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing publication.message"))?
        .to_string();
    let prov_topic = device_config
        .pointer("/publication/provisioning")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing publication.provisioning"))?
        .to_string();
    let sub_topic = device_config
        .pointer("/subscription/message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing subscription.message"))?
        .to_string();

    let state = Thinq2DeviceState {
        country_code: env.country_code.clone(),
        api_server,
        mqtt_server,
        ca_certificate: ca,
        private_key,
        certificate,
        pub_topic,
        prov_topic,
        sub_topic,
    };

    // Final ciphertext for addDevice: RSA(nonce || sha256(deviceId))
    let mut final_plain = Vec::new();
    final_plain.extend_from_slice(&nonce);
    final_plain.extend_from_slice(&Sha256::digest(device_id.as_bytes()));
    let add_device_ciphertext = rsa_pkcs1_encrypt(public_key_pem, &final_plain)?;

    Ok(PairResult {
        state,
        add_device_ciphertext,
    })
}

/// Build the CLIP device_packet JSON published to LG MQTT (shared with tests).
pub fn format_device_packet(
    mid: u32,
    device_id: &str,
    model_name: &str,
    data_hex: &str,
) -> String {
    serde_json::json!({
        "mid": mid,
        "did": device_id,
        "kind": model_name,
        "cmd": "device_packet",
        "rssi": -48,
        "fs": "idle",
        "data": data_hex,
        "type": 1,
    })
    .to_string()
}

/// Upstream preDeploy built from the appliance's own deploy message.
///
/// `appInfo` is forwarded verbatim (including `protocolVer` and any extra
/// fields). `platformInfo` is included only when the appliance sent an object;
/// a missing key stays missing. The old RAC placeholder set (`softVer`
/// `690409`, `protocolVer` `"1"`) is never substituted.
pub fn upstream_pre_deploy(
    mid: u32,
    device_id: &str,
    model_name: &str,
    deploy: Option<&serde_json::Value>,
) -> serde_json::Value {
    let app_info = deploy
        .and_then(|d| d.pointer("/data/appInfo").or_else(|| d.get("appInfo")))
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "modelName": model_name }));
    let mut data = serde_json::Map::new();
    data.insert("appInfo".into(), app_info);
    if let Some(platform) = deploy.and_then(|d| {
        d.pointer("/data/platformInfo")
            .or_else(|| d.get("platformInfo"))
    }) {
        if platform.is_object() {
            data.insert("platformInfo".into(), platform.clone());
        }
    }
    serde_json::json!({
        "mid": mid,
        "did": device_id,
        "kind": model_name,
        "cmd": "preDeploy",
        "rssi": -48,
        "fs": "idle",
        "data": data,
        "type": 0,
    })
}

/// What an LG MQTT downlink is, from the bridge's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LgDownlink {
    Packet(Vec<u8>),
    /// Delivery ack. `data` must be a hex string; anything else is [`LgDownlink::Dropped`].
    Ack(Vec<u8>),
    CompleteProvisioning,
    /// Not forwarded. The string is the full JSON payload, for the bridge log.
    Dropped(String),
}

fn hex_data(json: &serde_json::Value) -> Option<Vec<u8>> {
    let data = json.get("data").and_then(|v| v.as_str())?;
    rethink_util::hex::decode(data).ok()
}

/// Classify one LG → device MQTT message. `completeProvisioning` is answered by the
/// connection itself and is not a device packet.
pub fn classify_lg_downlink(json: &serde_json::Value) -> LgDownlink {
    match json.get("cmd").and_then(|v| v.as_str()) {
        Some("completeProvisioning") => LgDownlink::CompleteProvisioning,
        Some("packet") => match hex_data(json) {
            Some(buf) => LgDownlink::Packet(buf),
            None => LgDownlink::Dropped(json.to_string()),
        },
        Some("ack") => match hex_data(json) {
            Some(buf) => LgDownlink::Ack(buf),
            None => LgDownlink::Dropped(json.to_string()),
        },
        _ => LgDownlink::Dropped(json.to_string()),
    }
}

/// Parse LG → local packet command from MQTT JSON.
pub fn parse_lg_packet_payload(json: &serde_json::Value) -> Option<Vec<u8>> {
    match classify_lg_downlink(json) {
        LgDownlink::Packet(buf) => Some(buf),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_packet_json_matches_ts_shape() {
        let s = format_device_packet(10001, "dev-1", "RAC_056905_WW", "AABBCC");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cmd"], "device_packet");
        assert_eq!(v["did"], "dev-1");
        assert_eq!(v["data"], "AABBCC");
        assert_eq!(v["type"], 1);
        assert_eq!(v["mid"], 10001);
    }

    #[test]
    fn upstream_pre_deploy_keeps_real_app_info_and_omits_platform_info() {
        let deploy = serde_json::json!({
            "cmd": "deploy",
            "did": "dev-1",
            "data": {
                "appInfo": {
                    "modelName": "REAL-MODEL",
                    "protocolVer": "2.1",
                    "softVer": "424242",
                    "countryCode": "US",
                    "panelCode": "ZZ"
                }
            }
        });
        let v = upstream_pre_deploy(10000, "dev-1", "REAL-MODEL", Some(&deploy));
        assert_eq!(v["cmd"], "preDeploy");
        assert_eq!(v["data"]["appInfo"]["protocolVer"], "2.1");
        assert_eq!(v["data"]["appInfo"]["softVer"], "424242");
        assert_eq!(v["data"]["appInfo"]["panelCode"], "ZZ");
        assert_eq!(v["data"]["appInfo"]["countryCode"], "US");
        assert!(v["data"].get("platformInfo").is_none());
        let text = v.to_string();
        assert!(!text.contains("690409"), "RAC placeholder softVer must not be substituted");
        assert!(!text.contains("clip_v2.00.15.05"));
    }

    #[test]
    fn ca_cert_url_uses_region_api_server_not_common() {
        let url = route_certificate_url("https://kic-api.lgthinq.com:46030");
        assert_eq!(
            url,
            "https://kic-api.lgthinq.com:46030/route/certificate?name=aws-iot"
        );
        assert!(!url.contains("common.lgthinq.com"));
        assert_ne!(url, format!("{IOT_BASE_URL}/route/certificate?name=aws-iot"));
    }

    #[test]
    fn parse_lg_packet_hex() {
        let v = serde_json::json!({"cmd":"packet","data":"0102"});
        assert_eq!(parse_lg_packet_payload(&v), Some(vec![1, 2]));
        let v2 = serde_json::json!({"cmd":"other","data":"0102"});
        assert!(parse_lg_packet_payload(&v2).is_none());
    }

    #[test]
    fn classify_forwards_ack_and_packet_and_drops_the_rest() {
        let ack = serde_json::json!({"cmd":"ack","data":"AA08F0004D04A6BB"});
        assert_eq!(
            classify_lg_downlink(&ack),
            LgDownlink::Ack(rethink_util::hex::decode("AA08F0004D04A6BB").unwrap())
        );
        assert!(parse_lg_packet_payload(&ack).is_none());

        let packet = serde_json::json!({"cmd":"packet","data":"0102"});
        assert_eq!(classify_lg_downlink(&packet), LgDownlink::Packet(vec![1, 2]));

        assert_eq!(
            classify_lg_downlink(&serde_json::json!({"cmd":"completeProvisioning"})),
            LgDownlink::CompleteProvisioning
        );

        let other = serde_json::json!({"cmd":"somethingNew","data":"AA00"});
        match classify_lg_downlink(&other) {
            LgDownlink::Dropped(text) => assert!(text.contains("somethingNew")),
            other => panic!("expected drop, got {other:?}"),
        }
        // A non-string ack is not a delivery ack.
        assert!(matches!(
            classify_lg_downlink(&serde_json::json!({"cmd":"ack","data":{"nope":true}})),
            LgDownlink::Dropped(_)
        ));
    }

    #[tokio::test]
    async fn openssl_keygen_works() {
        let (pk, pubk, csr) = generate_ec_key_and_csr().await.unwrap();
        assert!(pk.contains("PRIVATE KEY") || pk.contains("EC PRIVATE"));
        assert!(pubk.contains("PUBLIC KEY"));
        assert!(csr.contains("CERTIFICATE REQUEST") || csr.contains("BEGIN"));
    }
}
