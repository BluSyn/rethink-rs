//! LG ThinQ cloud client (port of bridge/thinqApi.ts — gateway, auth, homes, devices).

use crate::oauth2;
use crate::state::Environment;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

const GATEWAY_URL: &str = "https://route.lgthinq.com:46030/v1/service/application/gateway-uri";

static GATEWAY_CACHE: Mutex<Option<HashMap<String, Value>>> = Mutex::new(None);

pub fn sign_in_url(web_base: &str, country_code: &str) -> String {
    let mut url = url::Url::parse(&format!("{web_base}signin")).expect("web base");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("callback_url", "https://kr.m.lgaccount.com/login/iabClose");
        q.append_pair("redirect_url", "https://kr.m.lgaccount.com/login/iabClose");
        q.append_pair("client_id", "LGAO221A02");
        q.append_pair("country", country_code);
        q.append_pair("language", "en");
        q.append_pair("svc_integrated", "Y");
        q.append_pair("state", "signin");
        q.append_pair("svc_code", "SVC202");
    }
    url.to_string()
}

pub struct Client {
    pub env: Environment,
    pub client_id: String,
    headers: HashMap<String, String>,
    gateway: Option<Value>,
    pub home_id: Option<String>,
}

impl Client {
    pub fn new(env: Environment) -> Self {
        let client_id = rethink_util::hex::encode(uuid::Uuid::new_v4().as_bytes())
            + &rethink_util::hex::encode(uuid::Uuid::new_v4().as_bytes());
        let mut headers = HashMap::new();
        headers.insert(
            "content-type".into(),
            "application/json;charset=UTF-8".into(),
        );
        headers.insert("accept".into(), "application/json".into());
        headers.insert("x-thinq-app-ver".into(), "4.1.5000".into());
        headers.insert("x-thinq-app-type".into(), "NUTS".into());
        headers.insert("x-thinq-app-level".into(), "PRD".into());
        headers.insert("x-thinq-app-os".into(), "ANDROID".into());
        headers.insert("x-service-code".into(), "SVC202".into());
        headers.insert("x-country-code".into(), env.country_code.clone());
        headers.insert(
            "x-language-code".into(),
            format!("en-{}", env.country_code),
        );
        headers.insert("x-service-phase".into(), "OP".into());
        headers.insert("x-origin".into(), "app-web-ANDROID".into());
        headers.insert("x-thinq-app-logintype".into(), "LGE".into());
        headers.insert("x-api-key".into(), "VGhpblEyLjAgU0VSVklDRQ==".into());
        headers.insert("x-client-id".into(), client_id.clone());
        Self {
            env,
            client_id,
            headers,
            gateway: None,
            home_id: None,
        }
    }

    async fn api_fetch(&self, url: &str, method: &str, body: Option<Value>) -> anyhow::Result<Value> {
        let client = reqwest::Client::new();
        let mut last_err = None;
        for _ in 0..4 {
            let mut req = match method {
                "POST" => client.post(url),
                "DELETE" => client.delete(url),
                _ => client.get(url),
            };
            for (k, v) in &self.headers {
                req = req.header(k, v);
            }
            req = req.header("x-message-id", rethink_util::hex::encode(uuid::Uuid::new_v4().as_bytes()));
            if let Some(ref b) = body {
                req = req.json(b);
            }
            match req.send().await {
                Ok(resp) => {
                    let out: Value = resp.json().await?;
                    let code = out
                        .get("resultCode")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if code != "0000" {
                        anyhow::bail!(
                            "thinq error {code}: {}",
                            out.get("result").cloned().unwrap_or(Value::Null)
                        );
                    }
                    return Ok(out.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
        Err(last_err.unwrap().into())
    }

    pub async fn ensure_gateway(&mut self) -> anyhow::Result<&Value> {
        if self.gateway.is_none() {
            // cache per country
            {
                let cache = GATEWAY_CACHE.lock().unwrap();
                if let Some(map) = cache.as_ref() {
                    if let Some(g) = map.get(&self.env.country_code) {
                        self.gateway = Some(g.clone());
                    }
                }
            }
            if self.gateway.is_none() {
                let g = self.api_fetch(GATEWAY_URL, "GET", None).await?;
                let mut cache = GATEWAY_CACHE.lock().unwrap();
                let map = cache.get_or_insert_with(HashMap::new);
                map.insert(self.env.country_code.clone(), g.clone());
                self.gateway = Some(g);
            }
        }
        Ok(self.gateway.as_ref().unwrap())
    }

    pub async fn get_urls(&mut self) -> anyhow::Result<(String, String)> {
        let g = self.ensure_gateway().await?;
        let web = g
            .pointer("/uris/empFrontBaseUri2")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing empFrontBaseUri2"))?
            .to_string();
        let auth = g
            .pointer("/uris/empOauthBaseUri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing empOauthBaseUri"))?
            .to_string();
        Ok((web, auth))
    }

    pub async fn auth(&mut self, refresh_token: &str) -> anyhow::Result<()> {
        let g = self.ensure_gateway().await?.clone();
        let auth_url = g
            .pointer("/uris/empOauthBaseUri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing auth url"))?
            .to_string();
        let thinq2 = g
            .get("thinq2Uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing thinq2Uri"))?
            .to_string();

        let access = oauth2::refresh(&auth_url, refresh_token).await?;
        let profile = oauth2::signed_request(
            &format!("{auth_url}/users/profile"),
            &[
                ("Authorization", &format!("Bearer {access}")),
                ("X-Device-Type", "M01"),
                ("X-Device-Platform", "ADR"),
            ],
            None,
        )
        .await?;
        if profile.get("status").and_then(|v| v.as_i64()) != Some(1) {
            anyhow::bail!("Can't query user information");
        }
        let user_no = profile
            .pointer("/account/userNo")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.headers.insert("x-user-no".into(), user_no);
        self.headers.insert("x-emp-token".into(), access);

        // Register client
        let mut h = self.headers.clone();
        h.insert("x-device-type".into(), "601".into());
        // temporary override for this call
        let saved = self.headers.clone();
        self.headers = h;
        let _ = self
            .api_fetch(&format!("{thinq2}/service/users/client"), "POST", None)
            .await;
        self.headers = saved;

        let homes = self
            .api_fetch(&format!("{thinq2}/service/homes"), "GET", None)
            .await?;
        if let Some(items) = homes.get("item").and_then(|v| v.as_array()) {
            for home in items {
                if home.get("currentHomeYn").and_then(|v| v.as_str()) == Some("Y") {
                    self.home_id = home
                        .get("homeId")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
            }
        }
        Ok(())
    }

    pub fn home_devices_url(thinq2_uri: &str, home_id: &str) -> String {
        format!("{thinq2_uri}/service/homes/{home_id}")
    }

    /// Devices listed on a home payload (`GET /service/homes/{homeId}` result).
    pub fn parse_home_devices(home: &Value) -> Vec<Value> {
        home.get("devices")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    pub fn home_has_device(devices: &[Value], device_id: &str) -> bool {
        devices
            .iter()
            .any(|d| d.get("deviceId").and_then(|v| v.as_str()) == Some(device_id))
    }

    /// Appliances already registered in the current home (master `9b76e42`).
    pub async fn list_devices(&self) -> anyhow::Result<Vec<Value>> {
        let g = self
            .gateway
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gateway not loaded"))?;
        let thinq2 = g
            .get("thinq2Uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing thinq2Uri"))?;
        let home = self
            .home_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("home not set"))?;
        let body = self
            .api_fetch(&Self::home_devices_url(thinq2, home), "GET", None)
            .await?;
        Ok(Self::parse_home_devices(&body))
    }

    async fn thinq2_uri(&mut self) -> anyhow::Result<String> {
        self.ensure_gateway()
            .await?
            .get("thinq2Uri")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("missing thinq2Uri"))
    }

    /// The device's status snapshot as the LG cloud reports it.
    pub async fn device_status(&mut self, device_id: &str) -> anyhow::Result<Value> {
        let thinq2 = self.thinq2_uri().await?;
        self.api_fetch(&format!("{thinq2}/service/devices/{device_id}"), "GET", None)
            .await
    }

    /// Drive a device the way the LG app does (`control-sync`).
    pub async fn control_sync(&mut self, device_id: &str, body: Value) -> anyhow::Result<Value> {
        let thinq2 = self.thinq2_uri().await?;
        self.api_fetch(
            &format!("{thinq2}/service/devices/{device_id}/control-sync"),
            "POST",
            Some(body),
        )
        .await
    }

    pub async fn remove_device(&self, device_id: &str) -> anyhow::Result<()> {
        let g = self
            .gateway
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gateway not loaded"))?;
        let thinq2 = g
            .get("thinq2Uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing thinq2Uri"))?;
        let home = self
            .home_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("home not set"))?;
        // Best-effort remove
        let _ = self
            .api_fetch(
                &format!("{thinq2}/service/homes/{home}/devices/{device_id}"),
                "DELETE",
                None,
            )
            .await;
        Ok(())
    }

    pub async fn prepare_new_t2_device(&self) -> anyhow::Result<(String, String)> {
        let g = self
            .gateway
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gateway not loaded"))?;
        let thinq2 = g
            .get("thinq2Uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing thinq2Uri"))?;
        let otp = self
            .api_fetch(
                &format!("{thinq2}/service/devices/otp/certificate"),
                "POST",
                Some(json!({})),
            )
            .await?;
        let otp_str = otp
            .get("otp")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing otp"))?
            .to_string();
        let pubkey = otp
            .get("publicKey")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing publicKey"))?
            .to_string();
        Ok((otp_str, pubkey))
    }

    pub async fn add_device(
        &self,
        device_id: &str,
        alias: &str,
        model_name: &str,
        device_type: &str,
        platform_type: &str,
        ciphertext_b64: Option<&str>,
    ) -> anyhow::Result<Value> {
        let g = self
            .gateway
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gateway not loaded"))?;
        let thinq2 = g
            .get("thinq2Uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing thinq2Uri"))?;
        let home = self
            .home_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("home not set"))?;
        let mut body = json!({
            "deviceId": device_id,
            "countryCode": self.env.country_code,
            "deviceType": device_type,
            "modelName": model_name,
            "aliasPrefix": alias,
            "platformType": platform_type,
            "initDevice": false,
        });
        if let Some(ct) = ciphertext_b64 {
            body["ciphertext"] = json!(ct);
        }
        match self
            .api_fetch(
                &format!("{thinq2}/service/homes/{home}/devices"),
                "POST",
                Some(body.clone()),
            )
            .await
        {
            Ok(v) => Ok(v),
            Err(e) if e.to_string().contains("0125") => {
                // already registered — retry with initDevice
                body["initDevice"] = json!(true);
                self.api_fetch(
                    &format!("{thinq2}/service/homes/{home}/devices"),
                    "POST",
                    Some(body),
                )
                .await
            }
            Err(e) => Err(e),
        }
    }

    pub fn model_json_url(
        thinq2_uri: &str,
        device_id: &str,
        model_name: &str,
    ) -> anyhow::Result<String> {
        let mut url = url::Url::parse(&format!("{thinq2_uri}/service/application/modeljson"))?;
        url.query_pairs_mut()
            .append_pair("deviceId", device_id)
            .append_pair("modelName", model_name);
        Ok(url.to_string())
    }

    /// The modelJSON describes a device model's capabilities (field layouts, enum values, course
    /// tables). The descriptor call needs a device registered in the current account, but the URI
    /// it returns is a plain, time-limited link that takes no authentication.
    pub async fn get_model_json(
        &self,
        device_id: &str,
        model_name: &str,
    ) -> anyhow::Result<String> {
        let g = self
            .gateway
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gateway not loaded"))?;
        let thinq2 = g
            .get("thinq2Uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing thinq2Uri"))?;
        let url = Self::model_json_url(thinq2, device_id, model_name)?;
        let descriptor = self.api_fetch(&url, "GET", None).await?;
        let uri = descriptor
            .get("modelJsonUri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing modelJsonUri"))?;
        let resp = reqwest::Client::new().get(uri).send().await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("Can't download the modelJSON: HTTP {}", status.as_u16());
        }
        Ok(resp.text().await?)
    }

    pub fn thinq1_state(&self) -> anyhow::Result<Value> {
        let g = self
            .gateway
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gateway not loaded"))?;
        let thinq1 = g
            .get("thinq1Uri")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .replace("/api", "");
        let rti = g
            .get("rtiUri")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(json!({ "httpServer": thinq1, "rtiServer": rti }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_url_contains_required_params() {
        let u = sign_in_url("https://example.com/", "US");
        assert!(u.contains("client_id=LGAO221A02"));
        assert!(u.contains("country=US"));
        assert!(u.contains("svc_code=SVC202"));
        assert!(u.contains("signin"));
    }

    #[test]
    fn list_devices_url_is_home_resource() {
        assert_eq!(
            Client::home_devices_url("https://thinq2.example", "home-1"),
            "https://thinq2.example/service/homes/home-1"
        );
    }

    #[test]
    fn home_lookup_skips_reregistration_when_already_listed() {
        let home = json!({
            "devices": [
                {"deviceId": "other-device", "alias": "Dryer"},
                {"deviceId": "aaaabbbb-cccc-dddd-eeee-ffff00001111", "alias": "침실 에어컨"},
            ]
        });
        let devices = Client::parse_home_devices(&home);
        assert!(Client::home_has_device(
            &devices,
            "aaaabbbb-cccc-dddd-eeee-ffff00001111"
        ));
        assert!(!Client::home_has_device(&devices, "missing-id"));
        assert!(!Client::home_has_device(
            &Client::parse_home_devices(&json!({})),
            "aaaabbbb-cccc-dddd-eeee-ffff00001111"
        ));
    }

    // ── modelJSON ────────────────────────────────────────────────────────────

    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const MODEL_JSON: &str = "{\n  \"Info\": {\"modelName\": \"F3P2CYVAT__\"}\n}\n";

    #[derive(Debug, Clone)]
    struct Request {
        target: String,
        headers: Vec<(String, String)>,
    }

    impl Request {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// Plays the ThinQ2 API server and the file server its modelJSON links point at. The link
    /// for a model is `/files/{modelName}.json`; only F3P2CYVAT__ has a file.
    async fn fake_lg() -> (String, Arc<Mutex<Vec<Request>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (log, files) = (requests.clone(), base.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut chunk = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(n) if n > 0 => head.extend_from_slice(&chunk[..n]),
                        _ => break,
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let mut lines = head.lines();
                let target = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("");
                let headers = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();
                log.lock().unwrap().push(Request {
                    target: target.to_string(),
                    headers,
                });

                let url = url::Url::parse(&format!("http://lg{target}")).unwrap();
                let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
                let (status, body) = match url.path() {
                    "/service/application/modeljson" if query["deviceId"] == "unregistered" => {
                        let body = json!({"resultCode": "0101", "result": null});
                        ("200 OK", body.to_string())
                    }
                    "/service/application/modeljson" => {
                        let uri = format!("{files}/files/{}.json", query["modelName"]);
                        let result = json!({"modelJsonUri": uri, "modelJsonVer": "1.0"});
                        let body = json!({"resultCode": "0000", "result": result});
                        ("200 OK", body.to_string())
                    }
                    "/files/F3P2CYVAT__.json" => ("200 OK", MODEL_JSON.to_string()),
                    _ => ("404 Not Found", String::new()),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (base, requests)
    }

    /// A client as `auth` leaves it, talking to `base`.
    fn client_for(base: &str) -> Client {
        let mut client = Client::new(Environment {
            country_code: "US".into(),
            language_code: None,
        });
        client.gateway = Some(json!({ "thinq2Uri": base }));
        client.headers.insert("x-emp-token".into(), "token".into());
        client
    }

    #[test]
    fn model_json_descriptor_names_the_device_and_model() {
        assert_eq!(
            Client::model_json_url("https://thinq2.example/v1", "dev 1", "F3P2CYVAT__").unwrap(),
            "https://thinq2.example/v1/service/application/modeljson?deviceId=dev+1&modelName=F3P2CYVAT__"
        );
    }

    #[tokio::test]
    async fn model_json_is_downloaded_from_the_descriptor_link_without_credentials() {
        let (base, requests) = fake_lg().await;
        let client = client_for(&base);
        let model_json = client.get_model_json("dev-1", "F3P2CYVAT__").await.unwrap();
        assert_eq!(model_json, MODEL_JSON, "handed over as served");

        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].target,
            "/service/application/modeljson?deviceId=dev-1&modelName=F3P2CYVAT__"
        );
        assert_eq!(requests[0].header("x-emp-token"), Some("token"));
        assert_eq!(requests[1].target, "/files/F3P2CYVAT__.json");
        for name in ["x-emp-token", "x-api-key", "x-client-id"] {
            assert_eq!(requests[1].header(name), None, "no {name} for the file");
        }
    }

    #[tokio::test]
    async fn model_json_failures_say_why() {
        let (base, _) = fake_lg().await;
        let client = client_for(&base);
        let missing = client.get_model_json("dev-1", "NO_FILE").await;
        let missing = missing.unwrap_err().to_string();
        assert_eq!(missing, "Can't download the modelJSON: HTTP 404");
        let refused = client.get_model_json("unregistered", "F3P2CYVAT__").await;
        let refused = refused.unwrap_err().to_string();
        assert!(refused.contains("0101"), "{refused}");
    }
}
