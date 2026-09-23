//! ThinQ2 HTTPS provisioning routes (/route, certificates).

use crate::certs::{sign_csr, Ca};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rethink_core::config::Config;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct T2HttpState {
    pub config: Arc<Config>,
    pub ca: Arc<Ca>,
    /// PEM returned by `/route/certificate?name=…` (custom root, or the CA).
    pub root_pem: String,
}

pub fn routes(config: Arc<Config>, ca: Arc<Ca>, root_pem: String) -> Router {
    let state = T2HttpState {
        config,
        ca,
        root_pem,
    };
    Router::new()
        .route("/route", get(route))
        .route("/route/certificate", get(route_certificate))
        .route("/device/{device_id}/certificate", post(device_certificate))
        .fallback(fallback)
        .with_state(state)
}

async fn route(State(state): State<T2HttpState>) -> Json<Value> {
    rethink_core::logging::log("HTTPS", &["/route"]);
    Json(route_document(
        state
            .config
            .https_port
            .advertise_url
            .as_deref()
            .unwrap_or(""),
        state
            .config
            .mqtts_port
            .advertise_url
            .as_deref()
            .unwrap_or(""),
    ))
}

#[derive(Deserialize)]
struct CertQuery {
    name: Option<String>,
}

async fn route_certificate(
    State(state): State<T2HttpState>,
    Query(q): Query<CertQuery>,
) -> Json<Value> {
    Json(route_certificate_body(q.name.as_deref(), &state.root_pem))
}

#[derive(Deserialize)]
struct CsrBody {
    csr: String,
}

async fn device_certificate(
    State(state): State<T2HttpState>,
    Path(_device_id): Path<String>,
    Json(body): Json<CsrBody>,
) -> Response {
    match sign_csr(&state.ca, &body.csr) {
        Ok(pem) => Json(json!({
            "resultCode": "0000",
            "result": { "certificatePem": pem }
        }))
        .into_response(),
        Err(e) => {
            eprintln!("CSR sign failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "resultCode": "9999", "result": {} })),
            )
                .into_response()
        }
    }
}

/// Body of `GET /route/certificate`. A name serves the configured root PEM verbatim.
pub fn route_certificate_body(name: Option<&str>, root_pem: &str) -> Value {
    if name.is_some() {
        json!({
            "resultCode": "0000",
            "result": { "certificatePem": root_pem }
        })
    } else {
        json!({
            "resultCode": "0000",
            "result": ["common-server", "aws-iot"]
        })
    }
}

/// JSON body of `GET /route`, using the advertised URLs (which may not be the bind ports).
pub fn route_document(https_advertise: &str, mqtts_advertise: &str) -> Value {
    json!({
        "resultCode": "0000",
        "result": {
            "apiServer": https_advertise,
            "mqttServer": mqtts_advertise,
        }
    })
}

#[cfg(test)]
mod route_tests {
    use super::*;

    #[test]
    fn certificate_route_serves_custom_root_or_ca_pem() {
        let custom = "-----BEGIN CERTIFICATE-----\nCUSTOM\n-----END CERTIFICATE-----\n";
        let ca = "-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----\n";
        let named = route_certificate_body(Some("aws-iot"), custom);
        assert_eq!(named["result"]["certificatePem"], custom);
        let builtin = route_certificate_body(Some("common-server"), ca);
        assert_eq!(builtin["result"]["certificatePem"], ca);
        assert!(route_certificate_body(None, ca)["result"].is_array());
    }

    #[test]
    fn route_document_uses_advertise_urls() {
        let body = route_document("https://proxy.example/thinq", "ssl://proxy.example:8883");
        assert_eq!(body["result"]["apiServer"], "https://proxy.example/thinq");
        assert_eq!(body["result"]["mqttServer"], "ssl://proxy.example:8883");
    }
}

async fn fallback() -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/xml;charset=utf-8")],
        "",
    )
        .into_response()
}

pub fn generate_deploy_response(payload: &Value) -> Value {
    let did = payload
        .get("did")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cmd = payload
        .get("cmd")
        .and_then(|v| v.as_str())
        .unwrap_or("deploy")
        .to_string();
    let mid = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    json!({
        "did": did,
        "mid": mid,
        "cmd": "completeProvisioning",
        "type": 0,
        "data": {
            "result": 0,
            "host": "message",
            "appInfo": {
                "host": "message",
                "publication": {
                    "message": format!("clip/message/devices/{did}"),
                    "provisioning": format!("clip/provisioning/devices/{did}"),
                }
            },
            "provisioningType": cmd,
            "deployInterval": 600,
        }
    })
}
