//! Real Home Assistant MQTT client via rumqttc, wired to HaMqttSink.

use anyhow::{Context, Result};
use rethink_core::ha::HaMqttSink;
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS, Transport};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use url::Url;

pub async fn start_ha_client(sink: Arc<HaMqttSink>) -> Result<()> {
    let cfg = sink.config.clone();
    let (host, port, use_tls) = parse_mqtt_url(&cfg.mqtt_url)?;
    let mut opts = MqttOptions::new("rethink-cloud", host, port);
    opts.set_keep_alive(Duration::from_secs(30));
    if !cfg.mqtt_user.is_empty() {
        opts.set_credentials(&cfg.mqtt_user, &cfg.mqtt_pass);
    }
    let will = LastWill::new(
        format!("{}/availability", cfg.rethink_prefix),
        b"offline".to_vec(),
        QoS::AtLeastOnce,
        true,
    );
    opts.set_last_will(will);
    if use_tls {
        // System CA roots, no client cert — same transport path as thinq2_conn.
        opts.set_transport(Transport::tls_with_default_config());
    }

    let (client, mut eventloop) = AsyncClient::new(opts, 64);

    // Single ordered publisher task: preserves retain order for discovery bursts
    // and surfaces publish errors instead of fire-and-forget spawns.
    let (pub_tx, mut pub_rx) = mpsc::unbounded_channel::<(String, Vec<u8>, bool)>();
    let client_pub = client.clone();
    tokio::spawn(async move {
        while let Some((topic, payload, retain)) = pub_rx.recv().await {
            if let Err(e) = client_pub
                .publish(&topic, QoS::AtLeastOnce, retain, payload)
                .await
            {
                tracing::warn!(
                    target: "rethink_ha",
                    %topic,
                    error = %e,
                    "HA MQTT publish failed"
                );
            }
        }
    });

    sink.set_publish_fn(move |topic, payload, retain| {
        if pub_tx.send((topic, payload, retain)).is_err() {
            tracing::warn!(
                target: "rethink_ha",
                "HA MQTT publisher channel closed; dropping publish"
            );
        }
    });

    let prefix = cfg.rethink_prefix.clone();
    let discovery = cfg.discovery_prefix.clone();
    let sink2 = sink.clone();

    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    *sink2.connected.lock() = true;
                    rethink_core::logging::log("status", &["HA mqtt connection established"]);
                    sink2.published_availability.lock().clear();
                    let _ = client
                        .subscribe(format!("{discovery}/status"), QoS::AtLeastOnce)
                        .await;
                    let _ = client
                        .subscribe(format!("{prefix}/+/+/set"), QoS::AtLeastOnce)
                        .await;
                    let _ = client
                        .subscribe(format!("{prefix}/+/+/+/set"), QoS::AtLeastOnce)
                        .await;
                    let _ = client
                        .subscribe(format!("{prefix}/+/availability"), QoS::AtLeastOnce)
                        .await;
                    let _ = client
                        .publish(
                            format!("{prefix}/availability"),
                            QoS::AtLeastOnce,
                            true,
                            b"online".to_vec(),
                        )
                        .await;
                    sink2.emit_discovery();
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    sink2.handle_message(&p.topic, &p.payload, p.retain);
                }
                Ok(Event::Incoming(Incoming::Disconnect)) => {
                    *sink2.connected.lock() = false;
                    rethink_core::logging::log("status", &["HA mqtt connection lost"]);
                }
                Ok(_) => {}
                Err(e) => {
                    *sink2.connected.lock() = false;
                    rethink_core::logging::log("status", &[&format!("HA mqtt error: {e}")]);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });

    Ok(())
}

fn parse_mqtt_url(url: &str) -> Result<(String, u16, bool)> {
    let u = Url::parse(url).with_context(|| format!("parse mqtt_url {url}"))?;
    let host = u.host_str().unwrap_or("127.0.0.1").to_string();
    let use_tls = u.scheme() == "mqtts" || u.scheme() == "ssl";
    let port = u.port().unwrap_or(if use_tls { 8883 } else { 1883 });
    Ok((host, port, use_tls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mqtt_plain() {
        let (host, port, tls) = parse_mqtt_url("mqtt://ha.local:1883").unwrap();
        assert_eq!(host, "ha.local");
        assert_eq!(port, 1883);
        assert!(!tls);
    }

    #[test]
    fn parse_mqtts_defaults_port_and_tls() {
        let (host, port, tls) = parse_mqtt_url("mqtts://broker.example").unwrap();
        assert_eq!(host, "broker.example");
        assert_eq!(port, 8883);
        assert!(tls);
    }

    #[test]
    fn parse_ssl_scheme() {
        let (_, port, tls) = parse_mqtt_url("ssl://10.0.0.1:8884").unwrap();
        assert_eq!(port, 8884);
        assert!(tls);
    }
}
