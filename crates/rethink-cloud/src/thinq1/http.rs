//! ThinQ1 HTTPS routes (XML).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use rethink_util::sync::Mutex;
use rethink_core::config::Config;
use rethink_core::metadata::Metadata;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

const XML_HEADER: &str = r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>"#;

/// Node's base64 decoder does not insist on padding.
const BASE64_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

pub type MetaStore = Arc<Mutex<HashMap<String, Metadata>>>;

/// Receives each decoded diagmon report: device id, `diagMonType`, decoded `diagMonData`.
pub type DiagmonSink = Arc<dyn Fn(&str, &str, &Value) + Send + Sync>;

pub fn device_metadata_store() -> MetaStore {
    Arc::new(Mutex::new(HashMap::new()))
}

#[derive(Clone)]
pub struct T1HttpState {
    #[allow(dead_code)] // kept for handlers that need advertise host / ports
    pub config: Arc<Config>,
    pub meta: MetaStore,
    pub on_diagmon: DiagmonSink,
}

pub fn routes(config: Arc<Config>, meta: MetaStore, on_diagmon: DiagmonSink) -> Router {
    let state = T1HttpState {
        config,
        meta,
        on_diagmon,
    };
    Router::new()
        .route(
            "/lgehadm/api/Device/TotalDeviceInfoSvc",
            post(total_device_info),
        )
        .route(
            "/lgehadm/api/Grid/PowerSavingInfoSvc",
            post(power_saving),
        )
        .route(
            "/lgehadm/api/Rtos/FWInfoSettingSvc",
            post(fw_info),
        )
        .route("/lgehadm/report/diagmon", post(diagmon))
        .route("/api/product/sendPushMessage", post(send_push_message))
        .fallback(fallback)
        .with_state(state)
}

async fn parse_xml_body(body: Bytes) -> Value {
    if body.len() > 1_000_000 {
        return Value::Null;
    }
    let text = String::from_utf8_lossy(&body);
    // Prefer quick-xml if available via serde; fall back to empty.
    // We only need a few fields — do lightweight extraction.
    let mut map = serde_json::Map::new();
    if let Some(model) = extract_tag(&text, "modelName") {
        let mut root = serde_json::Map::new();
        root.insert("modelName".into(), Value::String(model));
        if let Some(item) = extract_tag(&text, "item") {
            let mut item_list = serde_json::Map::new();
            item_list.insert("item".into(), Value::String(item));
            root.insert("itemList".into(), Value::Object(item_list));
        }
        map.insert("lgedmRoot".into(), Value::Object(root));
    }
    Value::Object(map)
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

fn xml_response(body: &str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/xml;charset=utf-8")],
        body.to_string(),
    )
        .into_response()
}

async fn total_device_info(
    State(state): State<T1HttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    rethink_core::logging::log(
        "HTTPS",
        &["TotalDeviceInfoSvc"],
    );
    let parsed = parse_xml_body(body).await;
    let device_id = headers
        .get("x-lgedm-deviceid")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let device_type = headers
        .get("x-lgedm-devicetype")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let Some(device_id) = device_id else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let model_name = parsed
        .pointer("/lgedmRoot/modelName")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let (Some(model), Some(dt)) = (model_name.clone(), device_type.clone()) {
        state.meta.lock().insert(
            device_id.clone(),
            Metadata {
                model_id: model.clone(),
                model_name: model,
                device_type: Some(dt),
                sw_version: None,
                package_id: None,
            },
        );
    }

    let item = parsed
        .pointer("/lgedmRoot/itemList/item")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let inner = if item == "DM_SETTING_INFO_GET_URI" {
        r#"<returnCd>0000</returnCd><returnMsg>OK</returnMsg><itemList><elementList><elementCode>settingInfoList</elementCode><elementValueList><code>BlackBox</code><value>N</value></elementValueList></elementList><item>DM_SETTING_INFO_GET_URI</item><returnCode>0000</returnCode></itemList>"#
            .to_string()
    } else if item == "THINQ_TIME_SYNC_URI" {
        let utc = chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        format!(
            r#"<returnCd>0000</returnCd><returnMsg>OK</returnMsg><itemList><elementList><elementCode>utcTime</elementCode><elementValue>{utc}</elementValue></elementList><elementList><elementCode>timezone</elementCode><elementValue>0</elementValue></elementList><item>THINQ_TIME_SYNC_URI</item><returnCode>0000</returnCode></itemList>"#
        )
    } else {
        r#"<returnCd>0000</returnCd><returnMsg>OK</returnMsg>"#.to_string()
    };

    xml_response(&format!("{XML_HEADER}<lgedmRoot>{inner}</lgedmRoot>"))
}

async fn power_saving() -> Response {
    xml_response(&format!(
        r#"{XML_HEADER}<lgedmRoot><returnCd>0108</returnCd><returnMsg>No Saving Data.</returnMsg></lgedmRoot>"#
    ))
}

async fn fw_info() -> Response {
    xml_response(&format!(
        r#"{XML_HEADER}<lgedmRoot><returnCd>0000</returnCd><returnMsg>OK</returnMsg></lgedmRoot>"#
    ))
}

async fn diagmon(State(state): State<T1HttpState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let body = parse_request_xml(&body);
    let report = &body["Report"];
    // Unlike the other lgehadm endpoints, this one carries the device id only in the body.
    let device_id = headers
        .get("x-lgedm-deviceid")
        .and_then(|v| v.to_str().ok())
        .or_else(|| report["devId"].as_str())
        .filter(|id| !id.is_empty());
    match (
        device_id,
        report["diagMonType"].as_str(),
        report["diagMonData"].as_str(),
    ) {
        (Some(id), Some(diag_mon_type), Some(data)) => {
            let decoded = decode_diag_mon_data(data);
            rethink_core::logging::log(
                "HTTPS",
                &[&format!("diagmon {id} {diag_mon_type}: {decoded}")],
            );
            (state.on_diagmon)(id, diag_mon_type, &decoded);
        }
        _ => rethink_core::logging::log(
            "HTTPS",
            &[&format!(
                "diagmon {}: {body}",
                device_id.unwrap_or("undefined")
            )],
        ),
    }
    StatusCode::OK
}

/// The appliance's own push-notification channel, not decoded yet: logged so a real
/// notification can be captured.
async fn send_push_message(headers: HeaderMap, body: Bytes) -> Response {
    let device_id = headers
        .get("x-lgedm-deviceid")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("undefined");
    let body = parse_request_xml(&body);
    rethink_core::logging::log("HTTPS", &[&format!("sendPushMessage {device_id}: {body}")]);
    xml_response(&format!(
        r#"{XML_HEADER}<lgedmRoot><returnCd>0000</returnCd><returnMsg>OK</returnMsg></lgedmRoot>"#
    ))
}

/// `diagMonData` is base64 of either XML (WasherMonitoring's tubInfo / courseInfo /
/// energyMonInfo) or a plain string such as ScomoCourse's course id.
fn decode_diag_mon_data(b64: &str) -> Value {
    let compact: String = b64.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let raw = BASE64_LENIENT.decode(compact).unwrap_or_default();
    let text = String::from_utf8_lossy(&raw);
    if raw.first() == Some(&b'<') {
        if let Some(parsed) = xml_to_json(&text) {
            return parsed;
        }
    }
    Value::String(text.into_owned())
}

fn parse_request_xml(body: &[u8]) -> Value {
    if body.len() > 1_000_000 {
        return Value::Null;
    }
    xml_to_json(&String::from_utf8_lossy(body)).unwrap_or(Value::Null)
}

/// Bounds the recursion on request bodies.
const XML_MAX_DEPTH: usize = 64;

/// The shape fast-xml-parser gives with its defaults: attributes and declarations dropped,
/// repeated elements as arrays, numeric text as numbers, an empty element as "", and text next
/// to child elements under "#text". `None` if a start tag never ends or nesting is too deep.
fn xml_to_json(xml: &str) -> Option<Value> {
    let mut rest = xml;
    xml_content(&mut rest, 0)
}

/// Parses up to the closing tag of the current element (or the end of the input).
fn xml_content(rest: &mut &str, depth: usize) -> Option<Value> {
    let mut children = Map::new();
    let mut text = String::new();
    while let Some(lt) = rest.find('<') {
        text.push_str(&xml_unescape(&rest[..lt]));
        let markup = &rest[lt..];
        if let Some(after) = markup.strip_prefix("</") {
            *rest = after.find('>').map_or("", |end| &after[end + 1..]);
            return Some(xml_element(children, &text));
        }
        if let Some(after) = markup.strip_prefix("<![CDATA[") {
            let end = after.find("]]>").unwrap_or(after.len());
            text.push_str(&after[..end]);
            *rest = after.get(end + 3..).unwrap_or("");
            continue;
        }
        if let Some(after) = markup.strip_prefix("<!--") {
            *rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        let end = xml_tag_end(markup)?;
        *rest = &markup[end + 1..];
        if markup.starts_with("<?") || markup.starts_with("<!") {
            continue;
        }
        let tag = &markup[1..end];
        let name = tag
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        let value = if tag.ends_with('/') {
            Value::String(String::new())
        } else if depth < XML_MAX_DEPTH {
            xml_content(rest, depth + 1)?
        } else {
            return None;
        };
        match children.get_mut(&name) {
            Some(Value::Array(items)) => items.push(value),
            Some(first) => *first = Value::Array(vec![first.take(), value]),
            None => {
                children.insert(name, value);
            }
        }
    }
    text.push_str(&xml_unescape(rest));
    *rest = "";
    Some(xml_element(children, &text))
}

/// Index of the `>` closing the tag that starts `markup`, skipping quoted attribute values.
fn xml_tag_end(markup: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in markup.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(i),
            None => {}
        }
    }
    None
}

fn xml_element(mut children: Map<String, Value>, text: &str) -> Value {
    let text = text.trim();
    if children.is_empty() {
        return xml_scalar(text);
    }
    if !text.is_empty() {
        children.insert("#text".into(), xml_scalar(text));
    }
    Value::Object(children)
}

/// Text that reads as a number becomes one, unless it would lose integer precision.
fn xml_scalar(text: &str) -> Value {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    let number = Some(text)
        .filter(|t| !t.is_empty())
        .and_then(rethink_devices::js::number)
        .filter(|n| n.is_finite());
    match number {
        Some(n) if n.fract() != 0.0 => {
            serde_json::Number::from_f64(n).map_or_else(|| text.into(), Value::Number)
        }
        Some(n) if n.abs() <= MAX_SAFE_INTEGER => Value::from(n as i64),
        _ => Value::String(text.into()),
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

async fn fallback() -> axum::Json<Value> {
    axum::Json(serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    // Decoded diagMonData of a real WasherMonitoring report from a completing Normal cycle.
    const ENERGY_MON_INFO_XML: &str = "<lgedmRoot><energyMonInfo><event>2</event><course>5</course><power>154</power><option>FAERAREFAAMFBAIAAAAAgAIAAAVqMAAA</option><dlCourse>106</dlCourse><useDate>20260822 23:48:24</useDate></energyMonInfo></lgedmRoot>";

    type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;

    fn state() -> (T1HttpState, Calls) {
        let calls: Calls = Arc::default();
        let sink = calls.clone();
        let config = rethink_core::config::parse_config_text(
            r#"{"hostname": "rethink.lan", "homeassistant": {"mqtt_url": "mqtt://localhost:1883",
            "discovery_prefix": "homeassistant", "rethink_prefix": "rethink", "mqtt_user": "",
            "mqtt_pass": ""}, "ca_key_file": "ca.key", "ca_cert_file": "ca.cert",
            "https_port": 443, "mqtts_port": 8883}"#,
        )
        .unwrap();
        let state = T1HttpState {
            config: Arc::new(config),
            meta: device_metadata_store(),
            on_diagmon: Arc::new(move |id: &str, diag_mon_type: &str, decoded: &Value| {
                sink.lock()
                    .push((id.to_string(), diag_mon_type.to_string(), decoded.clone()));
            }),
        };
        (state, calls)
    }

    fn report(dev_id: &str, diag_mon_type: &str, data: &str) -> Bytes {
        let data = BASE64_LENIENT.encode(data);
        Bytes::from(format!(
            r#"<?xml version="1.0" encoding="utf-8"?><Report><devId>{dev_id}</devId><diagMonType>{diag_mon_type}</diagMonType><diagMonData>{data}</diagMonData></Report>"#
        ))
    }

    #[test]
    fn diag_mon_data_decodes_xml_like_fast_xml_parser() {
        let encoded = BASE64_LENIENT.encode(ENERGY_MON_INFO_XML);
        assert_eq!(
            decode_diag_mon_data(&encoded),
            json!({
                "lgedmRoot": {
                    "energyMonInfo": {
                        "event": 2,
                        "course": 5,
                        "power": 154,
                        "option": "FAERAREFAAMFBAIAAAAAgAIAAAVqMAAA",
                        "dlCourse": 106,
                        "useDate": "20260822 23:48:24"
                    }
                }
            })
        );
        // Not XML: the decoded text itself.
        assert_eq!(
            decode_diag_mon_data(&BASE64_LENIENT.encode("106")),
            json!("106")
        );
    }

    #[test]
    fn xml_shapes_match_fast_xml_parser_defaults() {
        // Expected values were produced by fast-xml-parser 5 with default options.
        assert_eq!(
            xml_to_json(
                r#"<a><b>1</b><b>2</b><c/><d></d><e> 0005 </e><f>1.50</f><g>0x1A</g><h>-3</h><j>&lt;x&gt;&amp;</j><k attr="1>2">t</k><l>t<m>1</m></l><n><![CDATA[cd]]></n><q>12345678901234567890</q></a>"#
            ),
            Some(json!({"a": {
                "b": [1, 2], "c": "", "d": "", "e": 5, "f": 1.5, "g": 26, "h": -3, "j": "<x>&",
                "k": "t", "l": {"m": 1, "#text": "t"}, "n": "cd", "q": "12345678901234567890"
            }}))
        );
        assert_eq!(
            xml_to_json("<!-- c --><a>1</a><z>2</z>"),
            Some(json!({"a": 1, "z": 2}))
        );
        assert_eq!(xml_to_json("<a><b>1</a>"), Some(json!({"a": {"b": 1}})));
        assert_eq!(xml_to_json("<a"), None);
        assert_eq!(xml_to_json(&"<a>".repeat(100_000)), None);
    }

    #[tokio::test]
    async fn diagmon_reports_reach_the_sink_by_the_body_device_id() {
        let (state, calls) = state();
        let status = diagmon(
            State(state),
            HeaderMap::new(),
            report("dev-1", "WasherMonitoring", ENERGY_MON_INFO_XML),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let calls = calls.lock().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "dev-1");
        assert_eq!(calls[0].1, "WasherMonitoring");
        assert_eq!(calls[0].2["lgedmRoot"]["energyMonInfo"]["power"], 154);
    }

    #[tokio::test]
    async fn the_device_id_header_wins_and_incomplete_reports_are_only_logged() {
        let (state, calls) = state();
        let mut headers = HeaderMap::new();
        headers.insert("x-lgedm-deviceid", HeaderValue::from_static("dev-2"));
        diagmon(
            State(state.clone()),
            headers,
            report("dev-1", "ScomoCourse", "106"),
        )
        .await;
        diagmon(
            State(state),
            HeaderMap::new(),
            Bytes::from("<Report><devId>dev-1</devId><diagMonType>X</diagMonType></Report>"),
        )
        .await;
        assert_eq!(
            *calls.lock(),
            [("dev-2".to_string(), "ScomoCourse".to_string(), json!("106"))]
        );
    }

    #[tokio::test]
    async fn push_messages_are_acknowledged() {
        let response = send_push_message(HeaderMap::new(), Bytes::from("<a>1</a>")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            body,
            format!("{XML_HEADER}<lgedmRoot><returnCd>0000</returnCd><returnMsg>OK</returnMsg></lgedmRoot>")
        );
    }
}
