//! LG EMP OAuth2 signed requests (port of bridge/oauth2.ts).

use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha1 = Hmac<Sha1>;

const OAUTH2_SECRET: &[u8] = b"c053c2a6ddeb7ad97cb0eed0dcb31cf8";

/// Build `x-lge-oauth-date` timestamp (UTC HTTP-date with +0000).
fn oauth_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Format like: "Fri, 07 Aug 2026 12:00:00 +0000"
    let dt = chrono::DateTime::from_timestamp(secs as i64, 0)
        .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
    dt.format("%a, %d %b %Y %H:%M:%S +0000").to_string()
}

fn sign(path_and_maybe_body: &str, timestamp: &str) -> String {
    let mut mac = HmacSha1::new_from_slice(OAUTH2_SECRET).expect("HMAC key");
    let signed = format!("{path_and_maybe_body}\n{timestamp}");
    mac.update(signed.as_bytes());
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, mac.finalize().into_bytes())
}

/// Signed GET/POST to EMP OAuth endpoints.
pub async fn signed_request(
    url: &str,
    extra_headers: &[(&str, &str)],
    body: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let path = url::Url::parse(url)?.path().to_string();
    let timestamp = oauth_date();
    let mut to_sign = path;
    if let Some(b) = body {
        to_sign.push('?');
        to_sign.push_str(b);
    }
    let signature = sign(&to_sign, &timestamp);

    let client = reqwest::Client::new();
    let mut req = if body.is_some() {
        client.post(url)
    } else {
        client.get(url)
    };
    req = req
        .header("x-lge-oauth-signature", signature)
        .header("x-lge-oauth-date", timestamp)
        .header("Accept", "application/json")
        .header("x-lge-appkey", "LGAO221A02")
        .header("x-lge-app-os", "ANDROID")
        .header("X-Application-Key", "LGAO221A02")
        .header("lgemp-x-app-key", "LGAO221A02");
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    if let Some(b) = body {
        req = req
            .header(
                "Content-Type",
                "application/x-www-form-urlencoded;charset=UTF-8",
            )
            .body(b.to_string());
    }
    let resp = req.send().await?;
    let val = resp.json().await?;
    Ok(val)
}

#[derive(Debug, Clone)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    pub valid_until_ms: u64,
}

pub async fn from_code(auth_url: &str, code: &str) -> anyhow::Result<Token> {
    let sso_id = rethink_util::hex::encode(uuid::Uuid::new_v4().as_bytes());
    let body = format!(
        "code={}&grant_type=authorization_code&redirect_uri={}&sso_id={}",
        urlencoding(&code),
        urlencoding("https://kr.m.lgaccount.com/login/iabClose"),
        sso_id
    );
    let raw = signed_request(
        &format!("{auth_url}/oauth/1.0/oauth2/token"),
        &[],
        Some(&body),
    )
    .await?;
    let access = raw
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("OAuth2 sign-in failed: invalid response"))?
        .to_string();
    let refresh = raw
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("OAuth2 sign-in failed: invalid response"))?
        .to_string();
    let expires_in = raw
        .get("expires_in")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .ok_or_else(|| anyhow::anyhow!("OAuth2 sign-in failed: invalid response"))?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(Token {
        access_token: access,
        refresh_token: refresh,
        valid_until_ms: now_ms + expires_in * 1000,
    })
}

/// Takes what the user pasted after logging in on `sign_in_url()`'s page: the post-login URL, or,
/// when the page stays put after its signInAct request, the JSON answer to that request, whose
/// loginSessionID the page's own next step (signInDone / oAuthResult in its js/signin/signin.js)
/// trades for the post-login URL. `Ok(None)` when there is no code to exchange.
pub async fn from_sign_in(
    web_url: &str,
    auth_url: &str,
    country_code: &str,
    pasted: &str,
) -> anyhow::Result<Option<Token>> {
    let text = pasted.trim();
    let Some(account) = sign_in_account(text) else {
        return match url::Url::parse(text).ok().and_then(|u| query_param(&u, "code")) {
            Some(code) => Ok(Some(from_code(auth_url, &code).await?)),
            None => Ok(None),
        };
    };

    let body: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("loginSessionID", &js_string(account.get("loginSessionID")))
        .append_pair("clientId", "LGAO221A02")
        .append_pair("userName", &js_string(account.get("userID")))
        .append_pair("accountType", &js_string(account.get("userIDType")))
        .append_pair("countryCode", country_code)
        .append_pair("redirectUri", "https://kr.m.lgaccount.com/login/iabClose")
        .append_pair("state", "signin")
        .finish();
    let answer: serde_json::Value = reqwest::Client::new()
        .post(url::Url::parse(web_url)?.join("/lgacc/front/v1/signin/oauth")?)
        .header("Content-Type", "application/x-www-form-urlencoded;charset=UTF-8")
        .body(body)
        .send()
        .await?
        .json()
        .await?;

    match sign_in_redirect(&answer) {
        Ok((code, backend)) => Ok(Some(
            from_code(backend.as_deref().unwrap_or(auth_url), &code).await?,
        )),
        Err(why) => {
            eprintln!("[bridge] {}", refusal_message(&answer, why));
            Ok(None)
        }
    }
}

/// LG's own error strings only: the redirect_uri can hold a code, and the pasted answer holds the
/// account.
fn refusal_message(answer: &serde_json::Value, why: &str) -> String {
    let reason = answer
        .get("lgoauth_error_code")
        .and_then(|v| v.as_str())
        .unwrap_or(why);
    let message = answer.get("message").and_then(|v| v.as_str()).unwrap_or("");
    format!("sign-in oauth step gave no code: {reason} {message}")
        .trim_end()
        .to_string()
}

/// The `account` of a pasted signInAct response, if it carries a login session.
fn sign_in_account(text: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let account = parsed.get("account")?.as_object()?;
    js_truthy(account.get("loginSessionID")).then(|| account.clone())
}

/// The code and optional OAuth backend from the signin/oauth answer's `redirect_uri` (the
/// post-login URL, percent-encoded once more), or why there is none.
fn sign_in_redirect(answer: &serde_json::Value) -> Result<(String, Option<String>), &'static str> {
    let redirect = answer.get("redirect_uri").filter(|v| js_truthy(Some(v)));
    let url = redirect
        .and_then(|v| decode_uri_component(&js_string(Some(v))))
        .and_then(|s| url::Url::parse(&s).ok());
    let Some(url) = url else {
        return Err(if redirect.is_none() {
            "no redirect_uri"
        } else {
            "malformed redirect_uri"
        });
    };
    let Some(code) = query_param(&url, "code") else {
        return Err("no code in redirect_uri");
    };
    // It ends in a slash, and from_code() adds its own.
    let backend = query_param(&url, "oauth2_backend_url")
        .map(|b| b.trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty());
    Ok((code, backend))
}

/// First non-empty value of `name` in the query, like `searchParams.get(name)` used as a truthy value.
fn query_param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

fn js_truthy(v: Option<&serde_json::Value>) -> bool {
    match v {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `String(v)` for the JSON values a signInAct response holds.
fn js_string(v: Option<&serde_json::Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// `decodeURIComponent`: `None` where JavaScript throws (bad escape or invalid UTF-8).
fn decode_uri_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub async fn refresh(auth_url: &str, refresh_token: &str) -> anyhow::Result<String> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={}",
        urlencoding(refresh_token)
    );
    let raw = signed_request(
        &format!("{auth_url}/oauth/1.0/oauth2/token"),
        &[],
        Some(&body),
    )
    .await?;
    raw.get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("OAuth2 refresh failed: invalid response"))
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_is_stable_for_known_input() {
        // HMAC-SHA1 of "path\ntimestamp" with known secret must be deterministic.
        let s1 = sign("/oauth/1.0/oauth2/token", "Mon, 01 Jan 2020 00:00:00 +0000");
        let s2 = sign("/oauth/1.0/oauth2/token", "Mon, 01 Jan 2020 00:00:00 +0000");
        assert_eq!(s1, s2);
        assert!(!s1.is_empty());
        // base64 alphabet
        assert!(s1.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }


    // ── Sign-in result (port of upstream PR #275's tests) ────────────────────

    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const IAB_CLOSE: &str = "https://kr.m.lgaccount.com/login/iabClose?state=signin";

    fn sign_in_act() -> String {
        json!({"account": {"loginSessionID": "session-1", "userID": "user@example.com", "userIDType": "LGE", "country": "TH"}})
            .to_string()
    }

    /// JavaScript's encodeURIComponent.
    fn encode_uri_component(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*'
                | b'\'' | b'(' | b')' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    /// The oauth step's answer: every field null but redirect_uri.
    fn answer(redirect_uri: Option<String>) -> serde_json::Value {
        json!({
            "access_token": null, "code": null, "errCode": null, "lgoauth_error_code": null,
            "message": null, "oauth2_backend_url": null, "redirect_uri": redirect_uri,
            "refresh_token": null, "state": null, "status": null, "user_number": null,
        })
    }

    #[derive(Debug, Clone)]
    struct Received {
        method: String,
        path: String,
        content_type: String,
        form: Vec<(String, String)>,
    }

    /// Plays the sign-in page's server (webUrl) and the token servers (authUrl,
    /// oauth2_backend_url), told apart by path.
    struct LgServer {
        base: String,
        received: Arc<Mutex<Vec<Received>>>,
        oauth_answer: Arc<Mutex<serde_json::Value>>,
    }

    impl LgServer {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let received = Arc::new(Mutex::new(Vec::new()));
            let oauth_answer = Arc::new(Mutex::new(serde_json::Value::Null));
            let (rec, ans) = (received.clone(), oauth_answer.clone());
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else { return };
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        let n = sock.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let header = |name: &str| {
                        head.lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                            })
                            .unwrap_or_default()
                    };
                    let len: usize = header("content-length").parse().unwrap_or(0);
                    while buf.len() < header_end + len {
                        let n = sock.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let mut first = head.lines().next().unwrap_or("").split(' ');
                    let method = first.next().unwrap_or("").to_string();
                    let path = first.next().unwrap_or("").to_string();
                    let form = url::form_urlencoded::parse(&buf[header_end..header_end + len])
                        .into_owned()
                        .collect();
                    let body = if path.ends_with("/oauth/1.0/oauth2/token") {
                        json!({"access_token": "access", "refresh_token": "refresh", "expires_in": 3600})
                    } else {
                        ans.lock().unwrap().clone()
                    }
                    .to_string();
                    rec.lock().unwrap().push(Received {
                        method,
                        path,
                        content_type: header("content-type"),
                        form,
                    });
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            });
            Self { base, received, oauth_answer }
        }

        fn answer_with(&self, value: serde_json::Value) {
            *self.oauth_answer.lock().unwrap() = value;
        }

        fn received(&self) -> Vec<Received> {
            self.received.lock().unwrap().clone()
        }

        fn paths(&self) -> Vec<String> {
            self.received().into_iter().map(|r| r.path).collect()
        }

        /// A post-login URL naming this server's /backend/ as the token server.
        fn post_login_url(&self) -> String {
            format!(
                "{IAB_CLOSE}&code=Abc123&user_number=TH0000000000000&oauth2_backend_url={}",
                encode_uri_component(&format!("{}/backend/", self.base))
            )
        }

        async fn sign_in(&self, pasted: &str) -> Option<Token> {
            from_sign_in(
                &format!("{}/front/", self.base),
                &format!("{}/gateway", self.base),
                "TH",
                pasted,
            )
            .await
            .unwrap()
        }
    }

    fn form_value(form: &[(String, String)], name: &str) -> Option<String> {
        form.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[tokio::test]
    async fn a_sign_in_act_answer_goes_to_the_sign_in_pages_oauth_step() {
        let lg = LgServer::start().await;
        lg.answer_with(answer(Some(encode_uri_component(&lg.post_login_url()))));
        lg.sign_in(&sign_in_act()).await;
        let oauth = &lg.received()[0];
        assert_eq!(oauth.method, "POST");
        assert_eq!(oauth.path, "/lgacc/front/v1/signin/oauth", "on the origin of webUrl");
        assert!(oauth.content_type.starts_with("application/x-www-form-urlencoded"));
        let expected: Vec<(String, String)> = [
            ("loginSessionID", "session-1"),
            ("clientId", "LGAO221A02"),
            ("userName", "user@example.com"),
            ("accountType", "LGE"),
            ("countryCode", "TH"),
            ("redirectUri", "https://kr.m.lgaccount.com/login/iabClose"),
            ("state", "signin"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(oauth.form, expected);
    }

    #[tokio::test]
    async fn the_code_and_backend_come_out_of_the_doubly_encoded_redirect_uri() {
        let lg = LgServer::start().await;
        lg.answer_with(answer(Some(encode_uri_component(&lg.post_login_url()))));
        let token = lg.sign_in(&sign_in_act()).await.unwrap();
        assert_eq!(token.refresh_token, "refresh");
        let exchange = &lg.received()[1];
        assert_eq!(exchange.path, "/backend/oauth/1.0/oauth2/token");
        assert_eq!(form_value(&exchange.form, "code").as_deref(), Some("Abc123"));
    }

    #[tokio::test]
    async fn without_a_backend_the_code_goes_to_the_gateway() {
        let lg = LgServer::start().await;
        lg.answer_with(answer(Some(encode_uri_component(&format!("{IAB_CLOSE}&code=Abc123")))));
        lg.sign_in(&sign_in_act()).await.unwrap();
        assert_eq!(lg.received()[1].path, "/gateway/oauth/1.0/oauth2/token");
    }

    #[tokio::test]
    async fn a_pasted_post_login_url_skips_the_oauth_step_and_keeps_the_gateway() {
        let lg = LgServer::start().await;
        let token = lg.sign_in(&lg.post_login_url()).await.unwrap();
        assert_eq!(token.refresh_token, "refresh");
        assert_eq!(lg.paths(), ["/gateway/oauth/1.0/oauth2/token"]);
        assert_eq!(form_value(&lg.received()[0].form, "code").as_deref(), Some("Abc123"));
    }

    #[tokio::test]
    async fn unusable_input_and_refused_oauth_steps_exchange_no_code() {
        let lg = LgServer::start().await;
        // JSON without a loginSessionID is taken for a URL.
        let no_session = json!({"account": {"userID": "user@example.com"}}).to_string();
        for pasted in ["not a URL", IAB_CLOSE, no_session.as_str()] {
            assert!(lg.sign_in(pasted).await.is_none(), "{pasted}");
        }
        assert!(lg.received().is_empty(), "LG is not asked");

        let mut expired = answer(None);
        expired["lgoauth_error_code"] = json!("LG.OAUTH.EC.2002");
        expired["message"] = json!("EMP processing error.");
        let refused = [
            (expired, "LG.OAUTH.EC.2002 EMP processing error."),
            (answer(None), "no redirect_uri"),
            (answer(Some(encode_uri_component(IAB_CLOSE))), "no code in redirect_uri"),
            (
                answer(Some(format!(
                    "{}%E0%A4%A",
                    encode_uri_component(&format!("{IAB_CLOSE}&code=Abc123"))
                ))),
                "malformed redirect_uri",
            ),
        ];
        for (oauth, why) in refused {
            let reason = sign_in_redirect(&oauth).unwrap_err();
            assert_eq!(
                refusal_message(&oauth, reason),
                format!("sign-in oauth step gave no code: {why}")
            );
            lg.answer_with(oauth);
            assert!(lg.sign_in(&sign_in_act()).await.is_none(), "{why}");
        }
        assert!(
            !lg.paths().iter().any(|p| p.ends_with("/oauth/1.0/oauth2/token")),
            "no code exchanged"
        );
    }

    #[test]
    fn decode_uri_component_rejects_what_javascript_rejects() {
        assert_eq!(decode_uri_component("a%20b%2Fc").as_deref(), Some("a b/c"));
        assert_eq!(decode_uri_component("%E2%82%AC").as_deref(), Some("€"));
        for bad in ["%", "%A", "%+F", "%zz", "%E0%A4%A", "%FF"] {
            assert!(decode_uri_component(bad).is_none(), "{bad}");
        }
    }
}
