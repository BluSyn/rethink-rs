//! Framing for the Whisen SoftAP setup protocol.
//!
//! HTTP/1.1 POST over TLS on port 9000, one request per connection, no authentication.
//! Bodies are bare JSON members separated by CRLF. `SetDeviceConfig` declares
//! `Content-Length: 0` and an empty `Session-Id:` although it carries a body; the LG app
//! has always sent it that way.

use serde_json::{Map, Value};

pub const WHISEN_PORT: u16 = 9000;

/// Headers the app puts on `/SetDeviceConfig`, body notwithstanding.
pub const DEVICE_CONFIG_HEADERS: &[&str] = &["Content-Length: 0", "Session-Id: "];

/// `POST` line, headers, a blank line, then the body. `None` uses `Content-Length`.
pub fn request(path: &str, body: &str, headers: Option<&[&str]>) -> String {
    let default_len = format!("Content-Length: {}", body.len());
    let post = format!("POST {path} HTTP/1.1");
    let mut lines: Vec<&str> = Vec::new();
    lines.push(&post);
    match headers {
        Some(h) if !h.is_empty() => lines.extend_from_slice(h),
        _ => lines.push(&default_len),
    }
    lines.push("");
    lines.push(body);
    lines.join("\r\n")
}

/// Timezone as the app formats it: sign, two-digit hours, two-digit minutes, e.g. `+0100`.
pub fn timezone(offset_minutes_east_of_utc: i32) -> String {
    let sign = if offset_minutes_east_of_utc < 0 {
        '-'
    } else {
        '+'
    };
    let a = offset_minutes_east_of_utc.abs();
    format!("{sign}{:02}{:02}", a / 60, a % 60)
}

fn json_token(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn member(key: &str, value: &str) -> String {
    format!("{}:{}", json_token(key), json_token(value))
}

/// Body for `/SetDeviceInfo`. The RAC_056905_WW firmware ignores `regionalCode` and picks its server from `Nation`.
pub fn device_info_body(nation: &str, regional_code: &str) -> String {
    format!(
        "{},\r\n{}\r\n",
        member("Nation", nation),
        member("regionalCode", regional_code)
    )
}

/// Body for `/SetDeviceConfig`. `key_type` is only sent for a hand-typed (hidden) SSID.
pub fn device_config_body(
    ssid: &str,
    password: &str,
    tz: &str,
    key_type: Option<&str>,
) -> String {
    let mut parts = vec![member("HomeApSSID", ssid), member("HomeApPW", password)];
    if let Some(key_type) = key_type {
        parts.push(member("HomeApKeyType", key_type));
    }
    parts.push(member("TimeZone", tz));
    parts.join(",\r\n")
}

/// The status code from a reply, or `None` when there is no status line.
pub fn status_code(reply: &str) -> Option<u16> {
    let rest = reply.strip_prefix("HTTP/1.")?;
    let (ver, after) = rest.split_once(' ')?;
    if ver != "0" && ver != "1" {
        return None;
    }
    let digits = after.get(..3)?;
    if digits.len() != 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// What the app keeps of a reply: everything from the first double quote onward, wrapped in braces.
pub fn parse_members(reply: &str) -> Option<Map<String, Value>> {
    let i = reply.find('"')?;
    let flat = reply[i..].replace("\r\n", "").replace('\n', "");
    let flat = flat.trim_end();
    let flat = flat.strip_suffix(',').unwrap_or(flat).trim_end();
    serde_json::from_str(&format!("{{{flat}}}")).ok()
}

/// One step of the SoftAP exchange the setup binary runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Start,
    AfterInit,
    AfterInfo,
    AfterSetInfo,
    AfterConfig,
    Done,
}

#[derive(Debug)]
pub struct ExchangeRequest {
    pub path: &'static str,
    pub body: String,
    /// Use [`DEVICE_CONFIG_HEADERS`] instead of a real Content-Length.
    pub config_headers: bool,
}

pub struct ExchangeCtx<'a> {
    pub nation: &'a str,
    pub regional_code: &'a str,
    pub ssid: &'a str,
    pub password: &'a str,
    pub tz: &'a str,
}

/// Next request given the previous reply. `SetDeviceInfo` and `SetDeviceConfig` must answer 200.
pub fn advance(
    step: Step,
    last_reply: Option<&str>,
    ctx: &ExchangeCtx<'_>,
) -> Result<(Step, Option<ExchangeRequest>), &'static str> {
    match step {
        Step::Start => Ok((
            Step::AfterInit,
            Some(ExchangeRequest {
                path: "/SetDeviceInit",
                body: String::new(),
                config_headers: false,
            }),
        )),
        Step::AfterInit => Ok((
            Step::AfterInfo,
            Some(ExchangeRequest {
                path: "/GetDeviceInfo",
                body: String::new(),
                config_headers: false,
            }),
        )),
        Step::AfterInfo => Ok((
            Step::AfterSetInfo,
            Some(ExchangeRequest {
                path: "/SetDeviceInfo",
                body: device_info_body(ctx.nation, ctx.regional_code),
                config_headers: false,
            }),
        )),
        Step::AfterSetInfo => {
            if status_code(last_reply.unwrap_or("")).unwrap_or(0) != 200 {
                return Err("SetDeviceInfo rejected");
            }
            Ok((
                Step::AfterConfig,
                Some(ExchangeRequest {
                    path: "/SetDeviceConfig",
                    body: device_config_body(ctx.ssid, ctx.password, ctx.tz, None),
                    config_headers: true,
                }),
            ))
        }
        Step::AfterConfig => {
            if status_code(last_reply.unwrap_or("")).unwrap_or(0) != 200 {
                return Err("SetDeviceConfig rejected, appliance left in AP mode");
            }
            Ok((
                Step::Done,
                Some(ExchangeRequest {
                    path: "/ReleaseDevAp",
                    body: String::new(),
                    config_headers: false,
                }),
            ))
        }
        Step::Done => Ok((Step::Done, None)),
    }
}

pub fn encode_exchange_request(req: &ExchangeRequest) -> String {
    if req.config_headers {
        request(req.path, &req.body, Some(DEVICE_CONFIG_HEADERS))
    } else {
        request(req.path, &req.body, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY_200: &str = "HTTP/1.1 200\r\n\r\n";
    const REPLY_500: &str = "HTTP/1.1 500\r\n\r\n";

    fn ctx() -> ExchangeCtx<'static> {
        ExchangeCtx {
            nation: "DE",
            regional_code: "rethink",
            ssid: "HomeWifi",
            password: "secret",
            tz: "+0100",
        }
    }

    #[test]
    fn port_is_9000_and_request_framing_matches_the_app() {
        assert_eq!(WHISEN_PORT, 9000);
        assert_eq!(
            request("/SetDeviceInit", "", None),
            "POST /SetDeviceInit HTTP/1.1\r\nContent-Length: 0\r\n\r\n"
        );
        let body = device_info_body("DE", "rethink");
        assert_eq!(body, "\"Nation\":\"DE\",\r\n\"regionalCode\":\"rethink\"\r\n");
        assert_eq!(
            request("/SetDeviceInfo", &body, None),
            format!("POST /SetDeviceInfo HTTP/1.1\r\nContent-Length: 42\r\n\r\n{body}")
        );
    }

    #[test]
    fn set_device_config_omits_key_type_unless_asked_and_uses_app_headers() {
        let body = device_config_body("HomeWifi", "secret", "+0100", None);
        assert_eq!(
            body,
            "\"HomeApSSID\":\"HomeWifi\",\r\n\"HomeApPW\":\"secret\",\r\n\"TimeZone\":\"+0100\""
        );
        assert_eq!(
            request("/SetDeviceConfig", &body, Some(DEVICE_CONFIG_HEADERS)),
            format!("POST /SetDeviceConfig HTTP/1.1\r\nContent-Length: 0\r\nSession-Id: \r\n\r\n{body}")
        );
        assert!(device_config_body("x", "y", "+0000", Some("WPA/WPA2"))
            .contains("\"HomeApKeyType\":\"WPA/WPA2\",\r\n"));
    }

    #[test]
    fn quotes_and_backslashes_are_escaped_as_in_json() {
        assert_eq!(
            device_config_body("rt\"q", "pass\\word\"12", "+0100", None),
            "\"HomeApSSID\":\"rt\\\"q\",\r\n\"HomeApPW\":\"pass\\\\word\\\"12\",\r\n\"TimeZone\":\"+0100\""
        );
        assert!(device_config_body("a b,c:d'{é}", "y", "+0100", None)
            .starts_with("\"HomeApSSID\":\"a b,c:d'{é}\",\r\n"));
    }

    #[test]
    fn timezone_status_and_member_parsing() {
        assert_eq!(timezone(120), "+0200");
        assert_eq!(timezone(-330), "-0530");
        assert_eq!(timezone(0), "+0000");
        assert_eq!(timezone(60), "+0100");
        assert_eq!(status_code(REPLY_200), Some(200));
        assert_eq!(status_code(REPLY_500), Some(500));
        assert_eq!(status_code(""), None);
        assert_eq!(status_code("\"a\":\"b\""), None);
        let parsed = parse_members("HTTP/1.1 200\r\n\r\n\"Model\":\"RAC_056905_WW\",\r\n\"Ver\":\"1\"\r\n")
            .unwrap();
        assert_eq!(parsed["Model"], "RAC_056905_WW");
        assert_eq!(parsed["Ver"], "1");
        assert!(parse_members(REPLY_500).is_none());
    }

    #[test]
    fn exchange_on_port_9000_accepts_captured_replies_and_rejects_a_bad_write() {
        let c = ctx();
        let (step, req) = advance(Step::Start, None, &c).unwrap();
        let req = req.unwrap();
        assert_eq!(req.path, "/SetDeviceInit");
        assert_eq!(
            encode_exchange_request(&req),
            "POST /SetDeviceInit HTTP/1.1\r\nContent-Length: 0\r\n\r\n"
        );

        let (step, req) = advance(step, Some(REPLY_200), &c).unwrap();
        assert_eq!(req.unwrap().path, "/GetDeviceInfo");

        // GetDeviceInfo answers 500 on the RAC_056905_WW firmware and the exchange continues.
        let (step, req) = advance(step, Some(REPLY_500), &c).unwrap();
        let info = req.unwrap();
        assert_eq!(info.path, "/SetDeviceInfo");
        assert_eq!(
            info.body,
            "\"Nation\":\"DE\",\r\n\"regionalCode\":\"rethink\"\r\n"
        );

        assert_eq!(
            advance(step, Some(REPLY_500), &c).unwrap_err(),
            "SetDeviceInfo rejected"
        );

        let (step, req) = advance(step, Some(REPLY_200), &c).unwrap();
        let cfg = req.unwrap();
        assert_eq!(cfg.path, "/SetDeviceConfig");
        assert!(cfg.config_headers);
        assert!(cfg.body.contains("\"HomeApSSID\":\"HomeWifi\""));
        assert_eq!(WHISEN_PORT, 9000);

        let (step, req) = advance(step, Some(REPLY_200), &c).unwrap();
        assert_eq!(req.unwrap().path, "/ReleaseDevAp");
        assert_eq!(step, Step::Done);

        assert_eq!(
            advance(Step::AfterConfig, Some(REPLY_500), &c).unwrap_err(),
            "SetDeviceConfig rejected, appliance left in AP mode"
        );
    }
}
