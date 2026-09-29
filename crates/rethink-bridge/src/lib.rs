//! Bridge mode: optional forwarding to the real LG ThinQ cloud.
//!
//! Session membership is **live-only** (mirrors TypeScript `bridgedDevices`):
//! - `sessions` contains only active bridge sessions with wired handlers
//! - local `on_close` stops upstream and **removes** the session (want_enabled + storage remain)
//! - `status_for(id)` == live session present
//! - reconnect re-attaches via `start_session` when saved state / want_enabled exists

pub mod oauth2;
pub mod pair;
pub mod resolver;
pub mod state;
pub mod thinq1_conn;
pub mod thinq2_conn;
pub mod thinq_api;
pub mod util;

pub use state::{BridgeState, Credentials, Environment, JsonStorage};
pub use util::{subprocess, SubprocessError, SubprocessOptions};

use rethink_util::sync::Mutex;
use pair::{Thinq1DeviceState, Thinq2DeviceState};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thinq1_conn::{connect_thinq1, Thinq1Handle};
use thinq2_conn::{connect_thinq2, LgToDevice, Thinq2Handle};

/// Minimal local device view for bridge enable/disable (avoids cyclic deps on cloud).
pub trait LocalDevice: Send + Sync {
    fn id(&self) -> &str;
    fn platform(&self) -> &str;
    fn model_id(&self) -> &str;
    fn model_name(&self) -> &str;
    fn device_type(&self) -> Option<&str>;
    fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>);
    fn on_close(&self, handler: Box<dyn Fn() + Send + Sync>);
    fn send_to_local(&self, buf: &[u8]);
    /// Cloud delivery ack. Default drops it; the cloud adapter sends it as `cmd=ack`.
    fn send_ack_to_local(&self, _buf: &[u8]) {}
    /// A rethink handler is assigned. Cloud acks are not forwarded when this is true.
    fn is_managed(&self) -> bool {
        false
    }
    fn send_json_to_local(&self, body: serde_json::Value);
    /// The appliance's deploy/preDeploy message, if it has provisioned.
    fn deploy_payload(&self) -> Option<serde_json::Value> {
        None
    }
    /// How many `on_data` handlers are currently registered (tests / diagnostics).
    fn data_handler_count(&self) -> usize {
        0
    }
}

enum UpstreamHandle {
    T2(Thinq2Handle),
    T1(Thinq1Handle),
    /// Offline / unit-test session with no real LG socket.
    Mock,
}

struct BridgedSession {
    /// Session key (same as map key); kept for diagnostics / future reconnection logic.
    #[allow(dead_code)]
    device_id: String,
    /// Upstream pairing payload retained for the life of the live session.
    #[allow(dead_code)]
    lg_state: serde_json::Value,
    /// Set true when detaching; forward tasks exit.
    stopped: Arc<AtomicBool>,
    /// Upstream MQTT (or equivalent) is up.
    link_up: Arc<AtomicBool>,
    upstream: Mutex<Option<UpstreamHandle>>,
}

pub struct Bridge {
    storage: Arc<dyn BridgeState>,
    /// Live sessions only — never zombies.
    sessions: Mutex<HashMap<String, Arc<BridgedSession>>>,
    want_enabled: Mutex<HashSet<String>>,
    logged_in: Mutex<bool>,
}

impl Bridge {
    pub fn new(storage: Arc<dyn BridgeState>) -> Arc<Self> {
        let logged_in = storage.get_credentials().is_some();
        Arc::new(Self {
            storage,
            sessions: Mutex::new(HashMap::new()),
            want_enabled: Mutex::new(HashSet::new()),
            logged_in: Mutex::new(logged_in),
        })
    }

    pub fn is_logged_in(&self) -> bool {
        *self.logged_in.lock() || self.storage.get_credentials().is_some()
    }

    /// True only while a **live** session is registered (map membership == live).
    pub fn status_for(&self, id: &str) -> bool {
        self.sessions.lock().contains_key(id)
    }

    /// `(bridge mode enabled, upstream link up)` for management `bridgeState`.
    pub fn bridge_flags(&self, id: &str) -> (bool, bool) {
        let enabled = self.want_enabled.lock().contains(id)
            || self.storage.get_device_state_json(id).is_some();
        let up = self
            .sessions
            .lock()
            .get(id)
            .map(|s| s.link_up.load(Ordering::SeqCst))
            .unwrap_or(false);
        (enabled, up)
    }

    pub fn storage(&self) -> &Arc<dyn BridgeState> {
        &self.storage
    }

    /// Stop upstream and remove from live map; keep want_enabled + device state.
    pub fn detach_session(&self, id: &str) {
        if let Some(sess) = self.sessions.lock().remove(id) {
            sess.stopped.store(true, Ordering::SeqCst);
            if let Some(up) = sess.upstream.lock().take() {
                match up {
                    UpstreamHandle::T2(h) => h.stop(),
                    UpstreamHandle::T1(h) => h.stop(),
                    UpstreamHandle::Mock => {}
                }
            }
        }
    }

    pub async fn begin_login(&self, country_code: &str) -> anyhow::Result<String> {
        let mut client = thinq_api::Client::new(Environment {
            country_code: country_code.into(),
            language_code: None,
        });
        let (web, _auth) = client.get_urls().await?;
        Ok(thinq_api::sign_in_url(&web, country_code))
    }

    pub async fn complete_login(
        &self,
        country_code: &str,
        callback_url: &str,
    ) -> anyhow::Result<bool> {
        let mut client = thinq_api::Client::new(Environment {
            country_code: country_code.into(),
            language_code: None,
        });
        let (_web, auth) = client.get_urls().await?;
        let url = url::Url::parse(callback_url)?;
        let code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string());
        let Some(code) = code else {
            return Ok(false);
        };
        let token = oauth2::from_code(&auth, &code).await?;
        self.storage.set_credentials(Some(Credentials {
            refresh_token: token.refresh_token,
            env: Environment {
                country_code: country_code.into(),
                language_code: None,
            },
        }));
        *self.logged_in.lock() = true;
        Ok(true)
    }

    pub async fn logout(&self) -> anyhow::Result<()> {
        self.storage.set_credentials(None);
        *self.logged_in.lock() = false;
        let ids: Vec<String> = self.sessions.lock().keys().cloned().collect();
        for id in ids {
            self.detach_session(&id);
        }
        self.want_enabled.lock().clear();
        Ok(())
    }

    pub async fn enable(
        self: &Arc<Self>,
        device: Arc<dyn LocalDevice>,
        device_type: Option<&str>,
        mut status: Option<Box<dyn FnMut(&str) + Send>>,
    ) -> anyhow::Result<bool> {
        let mut report = |s: &str| {
            if let Some(ref mut cb) = status {
                cb(s);
            }
        };

        if !self.is_logged_in() {
            report("not logged in");
            return Ok(false);
        }
        let id = device.id().to_string();

        // Live session already — only short-circuit if truly live (map membership).
        if self.sessions.lock().contains_key(&id) {
            return Ok(true);
        }

        let creds = self
            .storage
            .get_credentials()
            .ok_or_else(|| anyhow::anyhow!("Not logged in"))?;

        // Saved state: re-attach to current LocalDevice Arc (reconnect path).
        if let Some(saved) = self.storage.get_device_state_json(&id) {
            report("Restoring saved bridge session");
            self.start_session(device, saved).await?;
            self.want_enabled.lock().insert(id);
            return Ok(true);
        }

        let mut client = thinq_api::Client::new(creds.env.clone());
        client.auth(&creds.refresh_token).await?;

        let dtype = device_type
            .map(|s| s.to_string())
            .or_else(|| device.device_type().map(|s| s.to_string()))
            .ok_or_else(|| anyhow::anyhow!("Device type must be specified"))?;

        // Keep an existing home membership (master 9b76e42). remove+addDevice
        // renamed the appliance to "Rethink xxxxxxxx" and bounced it out of
        // every linked app. Pairing still runs; only the home row is reused.
        let home_devices = client.list_devices().await?;
        let has_device = thinq_api::Client::home_has_device(&home_devices, &id);
        let alias = format!("Rethink {}", &id[..id.len().min(8)]);

        if !has_device {
            report("Removing device from home");
            let _ = client.remove_device(&id).await;
        }

        let lg_state = if device.platform() == "thinq1" {
            let state = client.thinq1_state()?;
            if !has_device {
                report("Adding ThinQ1 device to home");
                client
                    .add_device(&id, &alias, device.model_name(), &dtype, "thinq1", None)
                    .await?;
            }
            serde_json::json!({
                "rtiServer": state.get("rtiServer").cloned().unwrap_or(serde_json::Value::Null),
                "httpServer": state.get("httpServer").cloned().unwrap_or(serde_json::Value::Null),
                "platform": "thinq1",
            })
        } else {
            report("Fetching otp key");
            let (otp, pubkey) = client.prepare_new_t2_device().await?;
            report("Pairing ThinQ2 device (route + certificate)");
            let pair = match pair::pair_thinq2(&creds.env, &id, &otp, &pubkey).await {
                Ok(p) => p,
                Err(e) => {
                    report(&format!(
                        "Pairing failed: {e}. Make sure common.lgthinq.com is not redirected"
                    ));
                    return Err(e);
                }
            };
            if !has_device {
                report("Adding ThinQ2 device to home");
                let ct_b64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &pair.add_device_ciphertext,
                );
                client
                    .add_device(
                        &id,
                        &alias,
                        device.model_name(),
                        &dtype,
                        "thinq2",
                        Some(&ct_b64),
                    )
                    .await?;
            }
            serde_json::to_value(&pair.state)?
        };

        report("Device registered successfully");
        self.storage
            .set_device_state_json(&id, Some(lg_state.clone()));
        self.start_session(device, lg_state).await?;
        self.want_enabled.lock().insert(id);
        Ok(true)
    }

    /// Open upstream (or mock) and wire bidirectional forward; register live session.
    pub async fn start_session(
        self: &Arc<Self>,
        device: Arc<dyn LocalDevice>,
        lg_state: serde_json::Value,
    ) -> anyhow::Result<()> {
        let id = device.id().to_string();
        // Replace any stale entry (should not happen if detach is correct).
        self.detach_session(&id);

        let model_name = device.model_name().to_string();
        let device_type = device.device_type().map(|s| s.to_string());
        let stopped = Arc::new(AtomicBool::new(false));

        let test_mode = lg_state
            .get("testMode")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let is_t2 = device.platform() == "thinq2"
            || lg_state.get("mqttServer").is_some()
            || lg_state.get("mqtt_server").is_some()
            || lg_state.get("platform").and_then(|v| v.as_str()) == Some("thinq2");

        let upstream = if test_mode {
            // Unit-test path: no network; still wire local handlers for lifecycle.
            let stop = stopped.clone();
            device.on_data(Box::new(move |_buf| {
                let _ = stop.load(Ordering::SeqCst);
            }));
            UpstreamHandle::Mock
        } else if is_t2 {
            let state: Thinq2DeviceState =
                serde_json::from_value(normalize_t2_state(lg_state.clone()))?;
            if state.mqtt_server.is_empty() {
                anyhow::bail!("ThinQ2 state missing mqttServer — re-enable to re-pair");
            }
            let (handle, mut from_lg) =
                connect_thinq2(&state, &id, &model_name, device.deploy_payload()).await?;

            let dev = device.clone();
            let stop_f = stopped.clone();
            tokio::spawn(async move {
                while let Some(msg) = from_lg.recv().await {
                    if stop_f.load(Ordering::SeqCst) {
                        break;
                    }
                    deliver_lg_to_device(dev.as_ref(), msg);
                }
            });

            let h = handle.clone();
            let stop_l = stopped.clone();
            device.on_data(Box::new(move |buf| {
                if stop_l.load(Ordering::SeqCst) {
                    return;
                }
                let h = h.clone();
                let data = buf.to_vec();
                tokio::spawn(async move {
                    let _ = h.send_from_local(&data).await;
                });
            }));

            UpstreamHandle::T2(handle)
        } else {
            let t1 = parse_t1_state(&lg_state)?;
            let (handle, mut from_lg) =
                connect_thinq1(&t1, &id, &model_name, device_type.as_deref()).await?;

            let dev = device.clone();
            let stop_f = stopped.clone();
            tokio::spawn(async move {
                while let Some(body) = from_lg.recv().await {
                    if stop_f.load(Ordering::SeqCst) {
                        break;
                    }
                    dev.send_json_to_local(body);
                }
            });

            let h = handle.clone();
            let stop_l = stopped.clone();
            device.on_data(Box::new(move |buf| {
                if stop_l.load(Ordering::SeqCst) {
                    return;
                }
                h.send_from_local(buf);
            }));

            UpstreamHandle::T1(handle)
        };

        let session = Arc::new(BridgedSession {
            device_id: id.clone(),
            lg_state,
            stopped: stopped.clone(),
            link_up: Arc::new(AtomicBool::new(test_mode)),
            upstream: Mutex::new(Some(upstream)),
        });

        // Local close → detach (remove from map + stop upstream). want_enabled stays.
        let bridge = self.clone();
        let id_close = id.clone();
        device.on_close(Box::new(move || {
            bridge.detach_session(&id_close);
        }));

        self.sessions.lock().insert(id, session);
        Ok(())
    }

    pub async fn enable_id(
        self: &Arc<Self>,
        device_id: &str,
        device_type: Option<&str>,
        mut status: Option<Box<dyn FnMut(&str) + Send>>,
        lookup: &dyn Fn(&str) -> Option<Arc<dyn LocalDevice>>,
    ) -> anyhow::Result<bool> {
        let Some(dev) = lookup(device_id) else {
            if let Some(ref mut cb) = status {
                cb("device not connected");
            }
            return Ok(false);
        };
        self.enable(dev, device_type, status).await
    }

    pub async fn disable(&self, device_id: &str) -> anyhow::Result<()> {
        self.storage.set_device_state_json(device_id, None);
        self.detach_session(device_id);
        self.want_enabled.lock().remove(device_id);
        Ok(())
    }

    /// When a local device appears, auto-restore bridge if previously enabled and not live.
    pub fn on_local_device(self: &Arc<Self>, device: Arc<dyn LocalDevice>) {
        let id = device.id().to_string();
        if !self.want_enabled.lock().contains(&id)
            && self.storage.get_device_state_json(&id).is_none()
        {
            return;
        }
        // Live session already — do not double-attach.
        if self.sessions.lock().contains_key(&id) {
            return;
        }
        let Some(state) = self.storage.get_device_state_json(&id) else {
            return;
        };
        let this = self.clone();
        let id_for_log = id.clone();
        tokio::spawn(async move {
            match this.start_session(device, state).await {
                Ok(()) => {
                    this.want_enabled.lock().insert(id);
                }
                Err(e) => {
                    eprintln!("[bridge] auto-restore failed for {id_for_log}: {e}");
                }
            }
        });
    }
}

fn normalize_t2_state(v: serde_json::Value) -> serde_json::Value {
    if v.get("mqtt_server").is_some() && v.get("mqttServer").is_none() {
        return serde_json::json!({
            "countryCode": v.get("country_code").cloned().unwrap_or(serde_json::json!("US")),
            "apiServer": v.get("api_server").cloned().unwrap_or(serde_json::json!("")),
            "mqttServer": v.get("mqtt_server").cloned().unwrap_or(serde_json::json!("")),
            "caCertificate": v.get("ca_certificate").cloned().unwrap_or(serde_json::json!("")),
            "privateKey": v.get("private_key").cloned().unwrap_or(serde_json::json!("")),
            "certificate": v.get("certificate").cloned().unwrap_or(serde_json::json!("")),
            "pubTopic": v.get("pub_topic").cloned().unwrap_or(serde_json::json!("")),
            "provTopic": v.get("prov_topic").cloned().unwrap_or(serde_json::json!("")),
            "subTopic": v.get("sub_topic").cloned().unwrap_or(serde_json::json!("")),
        });
    }
    serde_json::json!({
        "countryCode": v.get("countryCode").or_else(|| v.get("country_code")).cloned().unwrap_or(serde_json::json!("US")),
        "apiServer": v.get("apiServer").or_else(|| v.get("api_server")).cloned().unwrap_or(serde_json::json!("")),
        "mqttServer": v.get("mqttServer").or_else(|| v.get("mqtt_server")).cloned().unwrap_or(serde_json::json!("")),
        "caCertificate": v.get("caCertificate").or_else(|| v.get("ca_certificate")).cloned().unwrap_or(serde_json::json!("")),
        "privateKey": v.get("privateKey").or_else(|| v.get("private_key")).cloned().unwrap_or(serde_json::json!("")),
        "certificate": v.get("certificate").cloned().unwrap_or(serde_json::json!("")),
        "pubTopic": v.get("pubTopic").or_else(|| v.get("pub_topic")).cloned().unwrap_or(serde_json::json!("")),
        "provTopic": v.get("provTopic").or_else(|| v.get("prov_topic")).cloned().unwrap_or(serde_json::json!("")),
        "subTopic": v.get("subTopic").or_else(|| v.get("sub_topic")).cloned().unwrap_or(serde_json::json!("")),
    })
}

/// Forward one classified downlink. Packets always go through. Cloud acks go through
/// only while no rethink handler owns the device (that handler acks locally if it needs to).
pub fn deliver_lg_to_device(dev: &dyn LocalDevice, msg: LgToDevice) {
    match msg {
        LgToDevice::Packet(buf) => dev.send_to_local(&buf),
        LgToDevice::Ack(buf) => {
            if !dev.is_managed() {
                dev.send_ack_to_local(&buf);
            }
        }
    }
}

fn parse_t1_state(v: &serde_json::Value) -> anyhow::Result<Thinq1DeviceState> {
    let rti = v
        .get("rtiServer")
        .or_else(|| v.get("rti_server"))
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing rtiServer"))?
        .to_string();
    let http = v
        .get("httpServer")
        .or_else(|| v.get("http_server"))
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing httpServer"))?
        .to_string();
    Ok(Thinq1DeviceState {
        rti_server: rti,
        http_server: http,
    })
}

// ── Lifecycle tests (close → reconnect → re-wire) ──────────────────────────

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::pair::{format_device_packet, parse_lg_packet_payload};
    use crate::thinq1_conn::format_status_body;
    use crate::thinq2_conn::LgToDevice;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Real mock: stores handlers; simulate_close fires them.
    struct MockLocal {
        id: String,
        platform: String,
        to_local: Mutex<Vec<Vec<u8>>>,
        acks: Mutex<Vec<Vec<u8>>>,
        managed: AtomicBool,
        data_handlers: Mutex<Vec<Box<dyn Fn(&[u8]) + Send + Sync>>>,
        close_handlers: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
        data_handler_regs: AtomicUsize,
    }

    impl MockLocal {
        fn new(id: &str, platform: &str) -> Arc<Self> {
            Arc::new(Self {
                id: id.into(),
                platform: platform.into(),
                to_local: Mutex::new(Vec::new()),
                acks: Mutex::new(Vec::new()),
                managed: AtomicBool::new(false),
                data_handlers: Mutex::new(Vec::new()),
                close_handlers: Mutex::new(Vec::new()),
                data_handler_regs: AtomicUsize::new(0),
            })
        }

        fn simulate_close(&self) {
            let handlers: Vec<_> = self.close_handlers.lock().drain(..).collect();
            for h in handlers {
                h();
            }
        }

        fn emit_data(&self, buf: &[u8]) {
            for h in self.data_handlers.lock().iter() {
                h(buf);
            }
        }
    }

    impl LocalDevice for MockLocal {
        fn id(&self) -> &str {
            &self.id
        }
        fn platform(&self) -> &str {
            &self.platform
        }
        fn model_id(&self) -> &str {
            "MODEL"
        }
        fn model_name(&self) -> &str {
            "MODEL"
        }
        fn device_type(&self) -> Option<&str> {
            Some("401")
        }
        fn on_data(&self, handler: Box<dyn Fn(&[u8]) + Send + Sync>) {
            self.data_handler_regs.fetch_add(1, Ordering::SeqCst);
            self.data_handlers.lock().push(handler);
        }
        fn on_close(&self, handler: Box<dyn Fn() + Send + Sync>) {
            self.close_handlers.lock().push(handler);
        }
        fn send_to_local(&self, buf: &[u8]) {
            self.to_local.lock().push(buf.to_vec());
        }
        fn send_ack_to_local(&self, buf: &[u8]) {
            self.acks.lock().push(buf.to_vec());
        }
        fn is_managed(&self) -> bool {
            self.managed.load(Ordering::SeqCst)
        }
        fn send_json_to_local(&self, _body: serde_json::Value) {}
        fn data_handler_count(&self) -> usize {
            self.data_handler_regs.load(Ordering::SeqCst)
        }
    }

    fn test_bridge() -> Arc<Bridge> {
        let dir = tempfile_dir();
        let storage = Arc::new(JsonStorage::new(&dir));
        // Pretend logged in
        storage.set_credentials(Some(Credentials {
            refresh_token: "test-refresh".into(),
            env: Environment {
                country_code: "US".into(),
                language_code: None,
            },
        }));
        Bridge::new(storage)
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rethink-bridge-lc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn mock_saved_state() -> serde_json::Value {
        serde_json::json!({
            "testMode": true,
            "platform": "thinq2",
            "mqttServer": "mock://test",
        })
    }

    #[tokio::test]
    async fn close_removes_session_and_reconnect_rewires() {
        let bridge = test_bridge();
        let id = "dev-lifecycle";

        // Seed saved state (as if previously registered with LG).
        bridge
            .storage
            .set_device_state_json(id, Some(mock_saved_state()));
        bridge.want_enabled.lock().insert(id.to_string());

        let local1 = MockLocal::new(id, "thinq2");
        // enable with saved state → start_session (testMode, no network)
        let ok = bridge
            .enable(local1.clone() as Arc<dyn LocalDevice>, Some("401"), None)
            .await
            .unwrap();
        assert!(ok);
        assert!(
            bridge.status_for(id),
            "status_for must be true while live session exists"
        );
        assert!(
            local1.data_handler_count() >= 1,
            "on_data must be registered on live attach"
        );
        let regs_after_enable = local1.data_handler_count();

        // Close local device → detach (map empty, want_enabled kept, storage kept)
        local1.simulate_close();
        assert!(
            !bridge.status_for(id),
            "status_for must be false after close (no zombie session)"
        );
        assert!(
            !bridge.sessions.lock().contains_key(id),
            "sessions map must not contain id after close"
        );
        assert!(
            bridge.want_enabled.lock().contains(id),
            "want_enabled must remain so reconnect can re-attach"
        );
        assert!(
            bridge.storage.get_device_state_json(id).is_some(),
            "device state must remain for restore"
        );

        // enable short-circuit must NOT return Ok(true) with no live session
        // New device Arc (reconnect)
        let local2 = MockLocal::new(id, "thinq2");
        bridge.on_local_device(local2.clone() as Arc<dyn LocalDevice>);
        // on_local_device spawns async — poll until live or timeout
        for _ in 0..50 {
            if bridge.status_for(id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            bridge.status_for(id),
            "on_local_device must re-attach live session after close"
        );
        assert!(
            local2.data_handler_count() >= 1,
            "new LocalDevice Arc must get on_data handlers re-registered"
        );

        // disable clears everything
        bridge.disable(id).await.unwrap();
        assert!(!bridge.status_for(id));
        assert!(!bridge.want_enabled.lock().contains(id));
        assert!(bridge.storage.get_device_state_json(id).is_none());

        let _ = regs_after_enable;
    }

    #[test]
    fn cloud_delivery_ack_reaches_only_an_unmanaged_device() {
        let local = MockLocal::new("dev-ack", "thinq2");
        let ack = rethink_util::hex::decode("AA08F0004D04A6BB").unwrap();
        let packet = rethink_util::hex::decode("AA12F0ED1121010000001804111200005EBB").unwrap();

        deliver_lg_to_device(local.as_ref(), LgToDevice::Ack(ack.clone()));
        deliver_lg_to_device(local.as_ref(), LgToDevice::Packet(packet.clone()));
        assert_eq!(
            local.acks.lock().clone(),
            vec![ack.clone()],
            "unmanaged device receives the cloud ack"
        );
        assert_eq!(local.to_local.lock().clone(), vec![packet.clone()]);

        local.managed.store(true, Ordering::SeqCst);
        deliver_lg_to_device(local.as_ref(), LgToDevice::Ack(ack));
        deliver_lg_to_device(local.as_ref(), LgToDevice::Packet(packet.clone()));
        assert_eq!(
            local.acks.lock().len(),
            1,
            "a handler acks for itself; the cloud ack is not forwarded"
        );
        assert_eq!(
            local.to_local.lock().clone(),
            vec![packet.clone(), packet],
            "packets are still forwarded when the device is managed"
        );
    }

    #[tokio::test]
    async fn enable_does_not_short_circuit_on_absent_session() {
        let bridge = test_bridge();
        let id = "dev-short";
        bridge
            .storage
            .set_device_state_json(id, Some(mock_saved_state()));

        // No live session — enable must start_session, not pretend success without wiring
        assert!(!bridge.status_for(id));
        let local = MockLocal::new(id, "thinq2");
        bridge
            .enable(local.clone() as Arc<dyn LocalDevice>, None, None)
            .await
            .unwrap();
        assert!(bridge.status_for(id));
        assert!(local.data_handler_count() >= 1);

        // Second enable while live — short-circuit Ok(true) without double-start
        let before = local.data_handler_count();
        bridge
            .enable(local.clone() as Arc<dyn LocalDevice>, None, None)
            .await
            .unwrap();
        assert_eq!(
            local.data_handler_count(),
            before,
            "live short-circuit must not re-register handlers"
        );
    }

    #[tokio::test]
    async fn simulate_close_then_enable_restores() {
        let bridge = test_bridge();
        let id = "dev-enable-restore";
        bridge
            .storage
            .set_device_state_json(id, Some(mock_saved_state()));

        let a = MockLocal::new(id, "thinq2");
        bridge
            .enable(a.clone() as Arc<dyn LocalDevice>, None, None)
            .await
            .unwrap();
        a.simulate_close();
        assert!(!bridge.status_for(id));

        let b = MockLocal::new(id, "thinq2");
        bridge
            .enable(b.clone() as Arc<dyn LocalDevice>, None, None)
            .await
            .unwrap();
        assert!(bridge.status_for(id));
        assert!(b.data_handler_count() >= 1);
    }

    #[test]
    fn keep_existing_home_registration_when_listed() {
        let devices = serde_json::json!([
            {"deviceId": "other-device", "alias": "Dryer"},
            {"deviceId": "aaaabbbb-cccc-dddd-eeee-ffff00001111", "alias": "침실 에어컨"},
        ]);
        let arr = devices.as_array().unwrap();
        assert!(thinq_api::Client::home_has_device(
            arr,
            "aaaabbbb-cccc-dddd-eeee-ffff00001111"
        ));
        assert!(!thinq_api::Client::home_has_device(arr, "missing-id"));
    }

    // Packet format tests (real shipped formatters)
    #[test]
    fn local_to_lg_t2_uses_device_packet_cmd() {
        let msg = format_device_packet(10001, "mock-dev", "MODEL", "AABB");
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["cmd"], "device_packet");
        assert_eq!(v["data"], "AABB");
    }

    #[test]
    fn lg_to_local_t2_parse() {
        let buf = parse_lg_packet_payload(&serde_json::json!({"cmd":"packet","data":"0102"}))
            .unwrap();
        assert_eq!(buf, vec![1, 2]);
    }

    #[test]
    fn t1_status_b64() {
        let body = format_status_body("id-1", &[0xDE, 0xAD]);
        assert_eq!(body["Body"]["Format"], "B64");
    }
}
