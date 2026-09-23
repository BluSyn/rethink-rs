//! rethink-cloud: emulates LG ThinQ cloud and bridges to Home Assistant MQTT.

mod bridge_adapter;
mod certs;
mod devmgr;
mod pki;
mod sni;
mod ha_bridge;
mod ha_client;
mod management;
mod mqtt_broker;
mod thinq1;
mod thinq2;

use anyhow::{Context, Result};
use axum::routing::get;
use axum::Json;
use rethink_bridge::{Bridge, JsonStorage};
use rethink_core::config::load_config;
use rethink_core::ha::HaMqttSink;
use rethink_core::hassio::{self, StartupMode};
use rethink_core::logging;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let mode = hassio::startup_mode(
        std::env::var("RETHINK_HASSIO_OPTIONS").ok().as_deref(),
        std::env::var("RETHINK_DATA_DIR").ok().as_deref(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    let (mut config, config_dir) = match mode {
        StartupMode::ConfigFile => {
            let config_path = std::env::args()
                .nth(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("./config.json"));
            let config_dir = config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let config = load_config(&config_path).with_context(|| {
                format!(
                    "failed to load config from {} (pass a JSONC config path as argv[1])",
                    config_path.display()
                )
            })?;
            (config, config_dir)
        }
        StartupMode::Hassio {
            options_path,
            data_dir,
        } => {
            eprintln!(
                "[status] Home Assistant add-on mode, loading options from {}",
                options_path.display()
            );
            let text = std::fs::read_to_string(&options_path).with_context(|| {
                format!("read hassio options {}", options_path.display())
            })?;
            let raw = hassio::load_hassio_options(&data_dir, &text).map_err(|e| anyhow::anyhow!(e))?;
            let config = rethink_core::config::normalize(raw)?;
            (config, data_dir)
        }
    };

    config.ca_key_file = config_dir
        .join(&config.ca_key_file)
        .to_string_lossy()
        .into();
    config.ca_cert_file = config_dir
        .join(&config.ca_cert_file)
        .to_string_lossy()
        .into();
    if let Some(ref path) = config.custom_root_cert_file {
        config.custom_root_cert_file = Some(
            config_dir
                .join(path)
                .to_string_lossy()
                .into(),
        );
    }
    if let Some(ref mut bridge) = config.bridge {
        bridge.storage_path = config_dir
            .join(&bridge.storage_path)
            .to_string_lossy()
            .into();
    }

    let enabled: std::collections::HashMap<String, bool> =
        config.log.iter().map(|k| (k.clone(), true)).collect();
    logging::set_filter(move |topic| {
        enabled.get(topic).copied().unwrap_or(false)
            || enabled.get("all").copied().unwrap_or(false)
    });

    eprintln!(
        "[status] rethink-cloud starting hostname={} https={:?} mqtts={:?} mqtt={:?}",
        config.hostname,
        config.https_port.bind_socket(),
        config.mqtts_port.bind_socket(),
        config.mqtt_port.bind_socket()
    );

    let ca_pem = certs::load_or_create(
        Path::new(&config.ca_key_file),
        Path::new(&config.ca_cert_file),
    )?;
    let ca = certs::Ca::from(ca_pem.clone());
    eprintln!("[status] CA certificate ready");
    let root_pem = match config.custom_root_cert_file.as_deref() {
        Some(path) => certs::load_custom_root(Path::new(path))?,
        None => ca.cert_pem.clone(),
    };
    let issuer = Arc::new(sni::CertificateIssuer::new(
        pki::load_issuer(&ca_pem)?,
        config.hostname.clone(),
    )?);
    if let Some(ref bridge) = config.bridge {
        rethink_bridge::resolver::set_servers(bridge.dns.clone().unwrap_or_default());
    }

    let manager = devmgr::DeviceManager::new();
    let frame_log = management::FrameLog::new();

    // Single shared HA MQTT sink — used by HaBridge publishes AND the rumqttc client.
    let ha_sink = HaMqttSink::new(config.homeassistant.clone());
    let ha_bridge = ha_bridge::HaBridge::new(ha_sink.clone());
    ha_bridge.attach_ha_mqtt_sink(&ha_sink);

    // Optional LG cloud bridge
    let lg_bridge: Option<Arc<Bridge>> = config.bridge.as_ref().map(|b| {
        let storage = Arc::new(JsonStorage::new(&b.storage_path));
        Bridge::new(storage)
    });

    {
        let ha_bridge = ha_bridge.clone();
        let lg_bridge = lg_bridge.clone();
        let frame_log = frame_log.clone();
        manager.on_new_device(move |dev| {
            // Always capture frames so the integrated monitor has history on select.
            frame_log.attach(&dev);
            ha_bridge.new_device(dev.clone());
            if let Some(ref br) = lg_bridge {
                br.on_local_device(Arc::new(bridge_adapter::ConnectedAsLocal(dev)));
            }
        });
    }

    // HA MQTT client — same sink instance so publish_fn is set where HaBridge publishes
    if config.mqtt {
        let sink = ha_sink.clone();
        tokio::spawn(async move {
            if let Err(e) = ha_client::start_ha_client(sink).await {
                eprintln!("[status] HA MQTT client ended: {e}");
            }
        });
    }

    let broker = Arc::new(mqtt_broker::Broker::new());
    let t2_acceptor = thinq2::device::DeviceAcceptor::new(broker.clone(), manager.clone());
    let _ = t2_acceptor;

    // Plain MQTT for local testing. Omitted mqtt_port does not bind.
    if config.mqtt {
        if let Some(addr) = config.mqtt_port.bind_socket() {
        let b = broker.clone();
        tokio::spawn(async move {
            match TcpListener::bind(&addr).await {
                Ok(listener) => {
                    eprintln!("[status] MQTT listening on {addr}");
                    loop {
                        match listener.accept().await {
                            Ok((stream, _)) => {
                                let b = b.clone();
                                tokio::spawn(async move {
                                    b.accept_tcp(stream).await;
                                });
                            }
                            Err(e) => {
                                eprintln!("[status] MQTT accept error: {e}");
                                break;
                            }
                        }
                    }
                }
                Err(e) => eprintln!("[status] MQTT bind failed on {addr}: {e}"),
            }
        });
        }
    }

    // Device-facing TLS (HTTPS + MQTTS): OpenSSL with legacy CBC-SHA / TLS1.0
    // so RTK_RTL8711am and similar CLIP modules can complete handshake (PR#131).
    // The default leaf is for `hostname`; SNI mints a leaf per requested name.
    let device_tls = match certs::device_ssl_acceptor(&issuer) {
        Ok(a) => {
            eprintln!("[status] device TLS: OpenSSL legacy profile (TLS1.0+, SECLEVEL=0)");
            Some(a)
        }
        Err(e) => {
            eprintln!("[status] device OpenSSL TLS config failed: {e:#}");
            None
        }
    };

    // MQTTS
    if let Some(ssl_acceptor) = device_tls.clone() {
        if let Some(addr) = config.mqtts_port.bind_socket() {
        let b = broker.clone();
        tokio::spawn(async move {
            match TcpListener::bind(&addr).await {
                Ok(listener) => {
                    eprintln!("[status] MQTTS listening on {addr} (legacy device TLS)");
                    loop {
                        match listener.accept().await {
                            Ok((stream, peer)) => {
                                let ssl_acceptor = ssl_acceptor.clone();
                                let b = b.clone();
                                tokio::spawn(async move {
                                    match certs::accept_device_tls(&ssl_acceptor, stream).await {
                                        Ok(tls) => b.accept_tls(tls).await,
                                        Err(e) => {
                                            tracing::debug!(
                                                %peer,
                                                "MQTTS TLS accept failed (legacy module?): {e:#}"
                                            );
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                eprintln!("[status] MQTTS accept error: {e}");
                                break;
                            }
                        }
                    }
                }
                Err(e) => eprintln!("[status] MQTTS bind failed on {addr}: {e}"),
            }
        });
        }
    }

    // HTTPS ThinQ2 provisioning (/route, certificate, …) and optional cleartext HTTP.
    {
        let ca = Arc::new(ca.clone());
        let cfg = Arc::new(config.clone());
        let router = thinq2::provisioning::routes(cfg, ca, root_pem);
        if let (Some(ssl_acceptor), Some(addr)) = (device_tls.clone(), config.https_port.bind_socket()) {
        let router = router.clone();
        tokio::spawn(async move {
            match TcpListener::bind(&addr).await {
                Ok(listener) => {
                    eprintln!("[status] HTTPS listening on {addr} (legacy device TLS)");
                    loop {
                        match listener.accept().await {
                            Ok((stream, peer)) => {
                                let ssl_acceptor = ssl_acceptor.clone();
                                let router = router.clone();
                                tokio::spawn(async move {
                                    match certs::accept_device_tls(&ssl_acceptor, stream).await {
                                        Ok(tls) => {
                                            let _ = hyper_util::server::conn::auto::Builder::new(
                                                hyper_util::rt::TokioExecutor::new(),
                                            )
                                            .serve_connection(
                                                hyper_util::rt::TokioIo::new(tls),
                                                hyper_util::service::TowerToHyperService::new(
                                                    router,
                                                ),
                                            )
                                            .await;
                                        }
                                        Err(e) => {
                                            tracing::debug!(
                                                %peer,
                                                "HTTPS TLS accept failed (legacy module?): {e:#}"
                                            );
                                        }
                                    }
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(e) => eprintln!("[status] HTTPS bind failed on {addr}: {e}"),
            }
        });
        }
        if let Some(addr) = config.http_port.bind_socket() {
            let router = router.clone();
            tokio::spawn(async move {
                match TcpListener::bind(&addr).await {
                    Ok(listener) => {
                        eprintln!("[status] HTTP listening on {addr}");
                        if let Err(e) = axum::serve(listener, router).await {
                            eprintln!("[status] HTTP ended: {e}");
                        }
                    }
                    Err(e) => eprintln!("[status] HTTP bind failed on {addr}: {e}"),
                }
            });
        }
    }

    // ThinQ1 HTTP (cleartext) and HTTPS (same app, device TLS).
    {
        let cfg = Arc::new(config.clone());
        let meta = thinq1::http::device_metadata_store();
        let router = thinq1::http::routes(cfg, meta.clone());
        let acceptor = thinq1::device::DeviceAcceptor::new(meta, manager.clone());
        if let Some(addr) = config.thinq1_http_port.bind_socket() {
            let router = router.clone();
            tokio::spawn(async move {
                match TcpListener::bind(&addr).await {
                    Ok(listener) => {
                        eprintln!("[status] ThinQ1 HTTP listening on {addr}");
                        if let Err(e) = axum::serve(listener, router).await {
                            eprintln!("[status] ThinQ1 HTTP ended: {e}");
                        }
                    }
                    Err(e) => eprintln!("[status] ThinQ1 HTTP bind failed on {addr}: {e}"),
                }
            });
        }
        if let (Some(ssl_acceptor), Some(addr)) =
            (device_tls.clone(), config.thinq1_https_port.bind_socket())
        {
            let router = router.clone();
            tokio::spawn(async move {
                match TcpListener::bind(&addr).await {
                    Ok(listener) => {
                        eprintln!("[status] ThinQ1 HTTPS listening on {addr}");
                        loop {
                            match listener.accept().await {
                                Ok((stream, _)) => {
                                    let ssl_acceptor = ssl_acceptor.clone();
                                    let router = router.clone();
                                    tokio::spawn(async move {
                                        if let Ok(tls) =
                                            certs::accept_device_tls(&ssl_acceptor, stream).await
                                        {
                                            let _ = hyper_util::server::conn::auto::Builder::new(
                                                hyper_util::rt::TokioExecutor::new(),
                                            )
                                            .serve_connection(
                                                hyper_util::rt::TokioIo::new(tls),
                                                hyper_util::service::TowerToHyperService::new(
                                                    router,
                                                ),
                                            )
                                            .await;
                                        }
                                    });
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    Err(e) => eprintln!("[status] ThinQ1 HTTPS bind failed on {addr}: {e}"),
                }
            });
        }
        let _ = acceptor;
    }

    // ThinQ1 device TCP port (cleartext framing; omitted port does not bind)
    if let Some(addr) = config.thinq1_port.bind_socket() {
        let meta = thinq1::http::device_metadata_store();
        let acceptor = thinq1::device::DeviceAcceptor::new(meta, manager.clone());
        tokio::spawn(async move {
            match TcpListener::bind(&addr).await {
                Ok(listener) => {
                    eprintln!("[status] ThinQ1 device port listening on {addr}");
                    loop {
                        match listener.accept().await {
                            Ok((stream, _)) => {
                                let a = acceptor.clone();
                                tokio::spawn(async move {
                                    a.accept(stream).await;
                                });
                            }
                            Err(e) => {
                                eprintln!("[status] ThinQ1 accept error: {e}");
                                break;
                            }
                        }
                    }
                }
                Err(e) => eprintln!("[status] ThinQ1 port bind failed on {addr}: {e}"),
            }
        });
    }

    // Management UI. Omitted management_port does not bind.
    let mgmt_port = config.management_port.bind;

    let mgmt_state = management::MgmtState {
        ha: ha_sink.clone(),
        ha_bridge: ha_bridge.clone(),
        manager: manager.clone(),
        bridge: lg_bridge.clone(),
        subscribers: Arc::new(rethink_util::sync::Mutex::new(Vec::new())),
        frame_log: frame_log.clone(),
    };

    let app = management::router(mgmt_state)
        .route(
            "/api/health",
            get(|| async {
                Json(serde_json::json!({"ok": true, "service": "rethink-cloud"}))
            }),
        )
        .route(
            "/api/devices",
            get({
                let m = manager.clone();
                move || {
                    let m = m.clone();
                    async move { Json(m.list_json()) }
                }
            }),
        );

    if let Some(mgmt_port) = mgmt_port {
        let addr = SocketAddr::from(([0, 0, 0, 0], mgmt_port));
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                eprintln!("[status] management UI listening on {mgmt_port}");
                eprintln!("[status] rethink-cloud ready");
                axum::serve(listener, app).await?;
            }
            Err(e) => {
                eprintln!("[status] management bind failed on {mgmt_port}: {e}");
                eprintln!("[status] rethink-cloud running without management UI");
                std::future::pending::<()>().await;
            }
        }
    } else {
        eprintln!("[status] management UI disabled");
        eprintln!("[status] rethink-cloud ready");
        std::future::pending::<()>().await;
    }

    Ok(())
}
