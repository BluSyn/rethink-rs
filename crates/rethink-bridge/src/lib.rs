//! Bridge mode: optional forwarding to the real LG ThinQ cloud.
//!
//! Session membership is **live-only** (mirrors TypeScript `bridgedDevices`):
//! - `sessions` contains only active bridge sessions with wired handlers
//! - a session is registered and wired before its LG connection is made; the connection runs
//!   (and reconnects) in the background until the session's [`StopSignal`] fires
//! - local `on_close` stops upstream and **removes** the session (want_enabled + storage remain)
//! - a new connection of the same appliance replaces and stops the older session; the older
//!   connection's close leaves the new one alone
//! - `status_for(id)` == live session present
//! - reconnect re-attaches via `start_session` when saved state exists

mod account;
pub mod oauth2;
pub mod pair;
pub mod resolver;
pub mod state;
pub mod stop;
pub mod thinq1_conn;
pub mod thinq2_conn;
pub mod thinq_api;
pub mod util;

#[cfg(test)]
mod test_support;

pub use state::{BridgeState, Credentials, Environment, JsonStorage};
pub use stop::StopSignal;
pub use util::{subprocess, SubprocessError, SubprocessOptions};

use rethink_util::sync::Mutex;
use pair::{Thinq1DeviceState, Thinq2DeviceState};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use thinq1_conn::{connect_thinq1, Thinq1Handle};
use thinq2_conn::{connect_thinq2, LgToDevice, Thinq2Handle};
use tokio::sync::mpsc;

/// Pause before a dropped LG link is reconnected (upstream `RECONNECT_PERIOD`).
#[cfg(not(test))]
pub const RECONNECT_PERIOD: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
pub const RECONNECT_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);

/// Told by an upstream connection whenever its link to LG comes up (`true`) or goes down.
pub type OnLink = Arc<dyn Fn(bool) + Send + Sync>;

/// What changed, for the management UI (upstream `Bridge` events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeEvent {
    /// A session was registered for the device.
    Started(String),
    /// The device's session was stopped and removed.
    Stopped(String),
    /// The device's link to LG came up (`true`) or went down.
    StateChanged(String, bool),
}

type Listener = Arc<dyn Fn(&BridgeEvent) + Send + Sync>;

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
    /// Tells this session apart from earlier and later ones of the same device.
    generation: u64,
    /// Fired when detaching: every task of the session exits and the LG socket closes.
    stop: StopSignal,
    /// Upstream MQTT (or equivalent) is up; kept current through the connection's [`OnLink`].
    link_up: Arc<AtomicBool>,
    upstream: UpstreamHandle,
}

impl BridgedSession {
    fn send_from_local(&self, buf: &[u8]) {
        if self.stop.is_stopped() {
            return;
        }
        match &self.upstream {
            UpstreamHandle::T2(h) => {
                let h = h.clone();
                let data = buf.to_vec();
                tokio::spawn(async move {
                    let _ = h.send_from_local(&data).await;
                });
            }
            UpstreamHandle::T1(h) => h.send_from_local(buf),
            UpstreamHandle::Mock => {}
        }
    }
}

pub struct Bridge {
    storage: Arc<dyn BridgeState>,
    /// Live sessions only — never zombies.
    sessions: Mutex<HashMap<String, Arc<BridgedSession>>>,
    want_enabled: Mutex<HashSet<String>>,
    logged_in: Mutex<bool>,
    /// What the owner calls each appliance, from the ThinQ account (see `account.rs`).
    device_names: Mutex<HashMap<String, String>>,
    names_changed: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
    next_generation: AtomicU64,
    /// Held while a session is registered and while disable/logout change what is wanted, so a
    /// session never outlives a disable or logout that raced with its start.
    registry: Mutex<()>,
    listeners: Mutex<Vec<Listener>>,
}

impl Bridge {
    pub fn new(storage: Arc<dyn BridgeState>) -> Arc<Self> {
        let logged_in = storage.get_credentials().is_some();
        Arc::new(Self {
            storage,
            sessions: Mutex::new(HashMap::new()),
            want_enabled: Mutex::new(HashSet::new()),
            logged_in: Mutex::new(logged_in),
            device_names: Mutex::new(HashMap::new()),
            names_changed: Mutex::new(Vec::new()),
            next_generation: AtomicU64::new(0),
            registry: Mutex::new(()),
            listeners: Mutex::new(Vec::new()),
        })
    }

    /// Called for every [`BridgeEvent`], on whichever task caused it. Listeners must not call
    /// back into `enable`/`disable`/`logout`.
    pub fn on_event(&self, listener: impl Fn(&BridgeEvent) + Send + Sync + 'static) {
        self.listeners.lock().push(Arc::new(listener));
    }

    fn emit(&self, event: BridgeEvent) {
        let listeners = self.listeners.lock().clone();
        for listener in listeners {
            listener(&event);
        }
    }

    pub fn is_logged_in(&self) -> bool {
        *self.logged_in.lock() || self.storage.get_credentials().is_some()
    }

    /// True only while a **live** session is registered (map membership == live).
    pub fn status_for(&self, id: &str) -> bool {
        self.sessions.lock().contains_key(id)
    }

    /// `(bridge mode enabled, upstream link up)` for management `bridgeState`. Like upstream's
    /// `status()`, bridge mode is on while a session is registered.
    pub fn bridge_flags(&self, id: &str) -> (bool, bool) {
        match self.sessions.lock().get(id) {
            Some(sess) => (true, sess.link_up.load(Ordering::SeqCst)),
            None => (false, false),
        }
    }

    pub fn storage(&self) -> &Arc<dyn BridgeState> {
        &self.storage
    }

    /// Stop upstream and remove from live map; keep want_enabled + device state.
    pub fn detach_session(&self, id: &str) {
        let removed = self.sessions.lock().remove(id);
        if let Some(sess) = removed {
            sess.stop.stop();
            self.emit(BridgeEvent::Stopped(id.to_string()));
        }
    }

    /// [`Bridge::detach_session`], but only while `generation` is still the live session.
    fn detach_generation(&self, id: &str, generation: u64) {
        let removed = {
            let mut sessions = self.sessions.lock();
            match sessions.get(id) {
                Some(sess) if sess.generation == generation => sessions.remove(id),
                _ => None,
            }
        };
        if let Some(sess) = removed {
            sess.stop.stop();
            self.emit(BridgeEvent::Stopped(id.to_string()));
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

    /// `pasted` is the post-login URL, or the signInAct response when LG's sign-in page stalls.
    pub async fn complete_login(
        self: &Arc<Self>,
        country_code: &str,
        pasted: &str,
    ) -> anyhow::Result<bool> {
        let mut client = thinq_api::Client::new(Environment {
            country_code: country_code.into(),
            language_code: None,
        });
        let (web, auth) = client.get_urls().await?;
        let Some(token) = oauth2::from_sign_in(&web, &auth, country_code, pasted).await? else {
            return Ok(false);
        };
        self.storage.set_credentials(Some(Credentials {
            refresh_token: token.refresh_token,
            env: Environment {
                country_code: country_code.into(),
                language_code: None,
            },
        }));
        *self.logged_in.lock() = true;
        let this = self.clone();
        tokio::spawn(async move { this.refresh_names().await });
        Ok(true)
    }

    pub async fn logout(&self) -> anyhow::Result<()> {
        let _registry = self.registry.lock();
        self.storage.set_credentials(None);
        *self.logged_in.lock() = false;
        self.clear_names();
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
            self.start_session(device, saved)?;
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
        self.start_session(device, lg_state)?;
        // Registering may have just given the appliance its name.
        let this = self.clone();
        tokio::spawn(async move { this.refresh_names_with(&client, &creds.refresh_token).await });
        Ok(true)
    }

    /// Register a live session for `device` and wire both directions, replacing (and stopping)
    /// an earlier session of the same device. Nothing here waits for LG: the connection runs in
    /// the background until the session stops, so a disable, logout or close always finds the
    /// session it has to stop. A device that is no longer wanted (disabled, logged out) gets
    /// no session.
    pub fn start_session(
        self: &Arc<Self>,
        device: Arc<dyn LocalDevice>,
        lg_state: serde_json::Value,
    ) -> anyhow::Result<()> {
        let id = device.id().to_string();
        let model_name = device.model_name().to_string();
        let device_type = device.device_type().map(|s| s.to_string());

        let test_mode = lg_state
            .get("testMode")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let is_t2 = device.platform() == "thinq2"
            || lg_state.get("mqttServer").is_some()
            || lg_state.get("mqtt_server").is_some()
            || lg_state.get("platform").and_then(|v| v.as_str()) == Some("thinq2");

        let registry = self.registry.lock();
        if !self.is_logged_in() || self.storage.get_device_state_json(&id).is_none() {
            return Ok(());
        }

        let stop = StopSignal::new();
        let link_up = Arc::new(AtomicBool::new(test_mode));
        let on_link: OnLink = {
            let bridge = Arc::downgrade(self);
            let (stop, link_up, id) = (stop.clone(), link_up.clone(), id.clone());
            Arc::new(move |up| {
                if stop.is_stopped() || link_up.swap(up, Ordering::SeqCst) == up {
                    return;
                }
                if let Some(bridge) = bridge.upgrade() {
                    bridge.emit(BridgeEvent::StateChanged(id.clone(), up));
                }
            })
        };
        let upstream = if test_mode {
            // Unit-test path: no network; still wire local handlers for lifecycle.
            UpstreamHandle::Mock
        } else if is_t2 {
            let state: Thinq2DeviceState =
                serde_json::from_value(normalize_t2_state(lg_state.clone()))?;
            if state.mqtt_server.is_empty() {
                anyhow::bail!("ThinQ2 state missing mqttServer — re-enable to re-pair");
            }
            let (handle, from_lg) = connect_thinq2(
                &state,
                &id,
                &model_name,
                device.deploy_payload(),
                stop.clone(),
                on_link,
            )?;
            let dev = device.clone();
            spawn_forward(stop.clone(), from_lg, move |msg| {
                deliver_lg_to_device(dev.as_ref(), msg)
            });
            UpstreamHandle::T2(handle)
        } else {
            let t1 = parse_t1_state(&lg_state)?;
            let (handle, from_lg) = connect_thinq1(
                &t1,
                &id,
                &model_name,
                device_type.as_deref(),
                stop.clone(),
                on_link,
            )?;
            let dev = device.clone();
            spawn_forward(stop.clone(), from_lg, move |body| {
                dev.send_json_to_local(body)
            });
            UpstreamHandle::T1(handle)
        };

        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let session = Arc::new(BridgedSession {
            device_id: id.clone(),
            lg_state,
            generation,
            stop,
            link_up,
            upstream,
        });
        let replaced = self.sessions.lock().insert(id.clone(), session.clone());
        if let Some(old) = replaced {
            old.stop.stop();
        }
        self.want_enabled.lock().insert(id.clone());
        drop(registry);

        device.on_data(Box::new(move |buf| session.send_from_local(buf)));

        // Local close → detach this session (a newer one stays). want_enabled stays.
        let bridge = self.clone();
        let id_close = id.clone();
        device.on_close(Box::new(move || {
            bridge.detach_generation(&id_close, generation);
        }));
        self.emit(BridgeEvent::Started(id));
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
        let _registry = self.registry.lock();
        self.storage.set_device_state_json(device_id, None);
        self.detach_session(device_id);
        self.want_enabled.lock().remove(device_id);
        Ok(())
    }

    /// When a local device appears, auto-restore bridge if previously enabled. A session still
    /// registered for the id belongs to an earlier connection of the appliance and is replaced.
    pub fn on_local_device(self: &Arc<Self>, device: Arc<dyn LocalDevice>) {
        let id = device.id().to_string();
        let Some(state) = self.storage.get_device_state_json(&id) else {
            return;
        };
        if let Err(e) = self.start_session(device, state) {
            eprintln!("[bridge] auto-restore failed for {id}: {e}");
        }
    }
}

/// Deliver LG → device messages until the session stops.
fn spawn_forward<T: Send + 'static>(
    stop: StopSignal,
    mut from_lg: mpsc::UnboundedReceiver<T>,
    deliver: impl Fn(T) + Send + 'static,
) {
    tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                biased;
                _ = stop.stopped() => break,
                msg = from_lg.recv() => msg,
            };
            match msg {
                Some(msg) => deliver(msg),
                None => break,
            }
        }
    });
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
    use crate::test_support::{
        eventually, expect_closed, read_t1_frame, write_t1_frame, HttpStub, Reply, TestPki,
        TlsStub, WAIT,
    };
    use crate::thinq1_conn::format_status_body;
    use crate::thinq2_conn::LgToDevice;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    /// Real mock: stores handlers; simulate_close fires them.
    struct MockLocal {
        id: String,
        platform: String,
        to_local: Mutex<Vec<Vec<u8>>>,
        json_to_local: Mutex<Vec<serde_json::Value>>,
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
                json_to_local: Mutex::new(Vec::new()),
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
        fn send_json_to_local(&self, body: serde_json::Value) {
            self.json_to_local.lock().push(body);
        }
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
        // New device Arc (reconnect); the session is registered before on_local_device returns.
        let local2 = MockLocal::new(id, "thinq2");
        bridge.on_local_device(local2.clone() as Arc<dyn LocalDevice>);
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

    #[tokio::test]
    async fn a_reconnected_device_replaces_the_old_session_and_its_late_close_is_ignored() {
        let bridge = test_bridge();
        let id = "dev-replace";
        bridge
            .storage
            .set_device_state_json(id, Some(mock_saved_state()));

        let old = MockLocal::new(id, "thinq2");
        bridge.on_local_device(old.clone() as Arc<dyn LocalDevice>);
        let first = bridge
            .sessions
            .lock()
            .get(id)
            .cloned()
            .expect("bridged on connect");

        // The appliance reconnects before the close of its old connection has come through.
        let new = MockLocal::new(id, "thinq2");
        bridge.on_local_device(new.clone() as Arc<dyn LocalDevice>);
        let second = bridge
            .sessions
            .lock()
            .get(id)
            .cloned()
            .expect("bridged on reconnect");
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(first.stop.is_stopped(), "the replaced session is stopped");
        assert!(!second.stop.is_stopped());

        old.simulate_close();
        assert!(
            bridge
                .sessions
                .lock()
                .get(id)
                .is_some_and(|s| Arc::ptr_eq(s, &second)),
            "the old connection's close leaves the new session alone"
        );
        new.simulate_close();
        assert!(!bridge.status_for(id));
        assert!(second.stop.is_stopped());
    }

    #[tokio::test]
    async fn a_logged_out_bridge_does_not_restore_sessions() {
        let bridge = test_bridge();
        let id = "dev-logged-out";
        bridge
            .storage
            .set_device_state_json(id, Some(mock_saved_state()));
        bridge.logout().await.unwrap();
        bridge.on_local_device(MockLocal::new(id, "thinq2") as Arc<dyn LocalDevice>);
        assert!(!bridge.status_for(id));
        assert!(!bridge.want_enabled.lock().contains(id));
    }

    fn t1_saved_state(rti_port: u16, http_url: &str) -> serde_json::Value {
        serde_json::json!({
            "platform": "thinq1",
            "rtiServer": format!("127.0.0.1:{rti_port}"),
            "httpServer": http_url,
        })
    }

    #[tokio::test]
    async fn stopping_a_thinq1_session_closes_the_lg_link() {
        let pki = TestPki::new();
        let mut rti = TlsStub::start(pki.server_config(&pki.leaf(&["rti.test"]), false)).await;
        let http = HttpStub::start(false).await;
        let bridge = test_bridge();
        let id = "t1-stop";
        bridge
            .storage
            .set_device_state_json(id, Some(t1_saved_state(rti.port, &http.url)));
        let local = MockLocal::new(id, "thinq1");
        assert!(bridge
            .enable(local.clone() as Arc<dyn LocalDevice>, None, None)
            .await
            .unwrap());

        let (mut lg, _) = rti.next().await;
        assert_eq!(
            read_t1_frame(&mut lg).await.unwrap()["Body"]["Cmd"],
            "Alive"
        );
        assert_eq!(
            http.accepted.load(Ordering::SeqCst),
            1,
            "TotalDeviceInfoSvc first"
        );

        // LG starts the session; the appliance's status goes up.
        write_t1_frame(&mut lg, &serde_json::json!({"Body": {"CmdOpt": "Start"}})).await;
        local.emit_data(&[0xAA, 0x01]);
        let status = read_t1_frame(&mut lg).await.unwrap();
        assert_eq!(status["Body"]["CmdWId"], format!("n-{id}"));
        assert_eq!(status["Body"]["Data"], "qgE=");

        // A command reaches the appliance and is acked.
        let command = serde_json::json!({
            "Header": {"x-lgedm-deviceId": id},
            "Body": {"CmdWId": "cmd-1", "Cmd": "Control"},
        });
        write_t1_frame(&mut lg, &command).await;
        assert_eq!(
            read_t1_frame(&mut lg).await.unwrap()["Body"],
            serde_json::json!({"CmdWId": "cmd-1", "ReturnCode": "0000"})
        );
        eventually("the command is forwarded", || {
            local.json_to_local.lock().len() == 1
        })
        .await;

        bridge.disable(id).await.unwrap();
        // What LG sends now is neither acked nor forwarded: the link is gone.
        write_t1_frame(
            &mut lg,
            &serde_json::json!({"Body": {"CmdWId": "cmd-2", "Cmd": "Control"}}),
        )
        .await;
        assert!(
            read_t1_frame(&mut lg).await.is_none(),
            "the LG link closes on stop"
        );
        local.emit_data(&[0xAA, 0x02]);
        tokio::time::sleep(RECONNECT_PERIOD * 3).await;
        assert_eq!(local.json_to_local.lock().len(), 1);
        assert!(
            rti.accepted.try_recv().is_err(),
            "a stopped session does not reconnect"
        );
        assert_eq!(http.accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_thinq1_session_stopped_while_connecting_is_gone_for_good() {
        for how in ["disable", "logout", "close"] {
            let http = HttpStub::start(true).await;
            let rti = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bridge = test_bridge();
            let id = "t1-connecting";
            let state = t1_saved_state(rti.local_addr().unwrap().port(), &http.url);
            bridge.storage.set_device_state_json(id, Some(state));
            let local = MockLocal::new(id, "thinq1");
            assert!(bridge
                .enable(local.clone() as Arc<dyn LocalDevice>, None, None)
                .await
                .unwrap());
            assert!(bridge.status_for(id), "{how}: registered before LG answers");
            eventually("TotalDeviceInfoSvc in flight", || {
                http.accepted.load(Ordering::SeqCst) == 1
            })
            .await;

            match how {
                "disable" => bridge.disable(id).await.unwrap(),
                "logout" => bridge.logout().await.unwrap(),
                _ => local.simulate_close(),
            }
            assert!(!bridge.status_for(id), "{how}");
            eventually("the in-flight POST is dropped", || {
                http.closed.load(Ordering::SeqCst) == 1
            })
            .await;

            // Nothing comes back afterwards, and the RTI server never hears from it.
            tokio::time::sleep(RECONNECT_PERIOD * 3).await;
            assert!(!bridge.status_for(id), "{how}");
            assert!(
                tokio::time::timeout(Duration::from_millis(50), rti.accept())
                    .await
                    .is_err(),
                "{how}: no RTI connection"
            );
            assert_eq!(http.accepted.load(Ordering::SeqCst), 1, "{how}: no retry");
            assert_eq!(
                bridge.want_enabled.lock().contains(id),
                how == "close",
                "{how}: only a close keeps the device wanted"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_total_device_info_post_aborts_the_attempt() {
        let http = HttpStub::serve(|_| Reply::Close).await;
        let rti = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge = test_bridge();
        let id = "t1-post-fails";
        let state = t1_saved_state(rti.local_addr().unwrap().port(), &http.url);
        bridge.storage.set_device_state_json(id, Some(state));
        let local = MockLocal::new(id, "thinq1");
        bridge
            .enable(local.clone() as Arc<dyn LocalDevice>, None, None)
            .await
            .unwrap();

        eventually("the POST is retried", || {
            http.accepted.load(Ordering::SeqCst) >= 2
        })
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rti.accept())
                .await
                .is_err(),
            "no RTI connection without TotalDeviceInfoSvc"
        );
        assert_eq!(bridge.bridge_flags(id), (true, false));
        bridge.disable(id).await.unwrap();
    }

    #[tokio::test]
    async fn a_thinq1_handshake_in_flight_is_dropped_on_stop() {
        use tokio::io::AsyncReadExt;
        let http = HttpStub::start(false).await;
        // Accepts and never answers the ClientHello.
        let rti = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge = test_bridge();
        let id = "t1-handshake";
        let state = t1_saved_state(rti.local_addr().unwrap().port(), &http.url);
        bridge.storage.set_device_state_json(id, Some(state));
        let local = MockLocal::new(id, "thinq1");
        bridge
            .enable(local.clone() as Arc<dyn LocalDevice>, None, None)
            .await
            .unwrap();
        let (mut sock, _) = tokio::time::timeout(WAIT, rti.accept())
            .await
            .unwrap()
            .unwrap();
        let mut record = [0u8; 1];
        sock.read_exact(&mut record).await.unwrap();
        assert_eq!(record[0], 0x16, "a TLS handshake record");

        bridge.disable(id).await.unwrap();
        expect_closed(&mut sock, "the handshake socket").await;
    }

    fn record_events(bridge: &Bridge) -> Arc<Mutex<Vec<BridgeEvent>>> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        bridge.on_event(move |e| sink.lock().push(e.clone()));
        events
    }

    #[tokio::test]
    async fn sessions_report_start_stop_and_their_link_state() {
        let bridge = test_bridge();
        let events = record_events(&bridge);
        let id = "dev-events";
        assert_eq!(
            bridge.bridge_flags(id),
            (false, false),
            "no session: disabled"
        );
        bridge
            .storage
            .set_device_state_json(id, Some(mock_saved_state()));
        let local = MockLocal::new(id, "thinq2");
        bridge.on_local_device(local.clone() as Arc<dyn LocalDevice>);
        assert_eq!(
            bridge.bridge_flags(id),
            (true, true),
            "test mode is always online"
        );
        local.simulate_close();
        assert_eq!(bridge.bridge_flags(id), (false, false));
        assert_eq!(
            events.lock().clone(),
            [
                BridgeEvent::Started(id.into()),
                BridgeEvent::Stopped(id.into())
            ]
        );

        // Saved state alone is not bridge mode: logged out, nothing runs.
        bridge.logout().await.unwrap();
        bridge.on_local_device(MockLocal::new(id, "thinq2") as Arc<dyn LocalDevice>);
        assert_eq!(bridge.bridge_flags(id), (false, false));
    }

    #[tokio::test]
    async fn the_link_state_follows_the_thinq1_connection() {
        let pki = TestPki::new();
        let mut rti = TlsStub::start(pki.server_config(&pki.leaf(&["rti.test"]), false)).await;
        let http = HttpStub::start(false).await;
        let bridge = test_bridge();
        let events = record_events(&bridge);
        let id = "t1-link";
        bridge
            .storage
            .set_device_state_json(id, Some(t1_saved_state(rti.port, &http.url)));
        let local = MockLocal::new(id, "thinq1");
        bridge
            .enable(local.clone() as Arc<dyn LocalDevice>, None, None)
            .await
            .unwrap();
        assert_eq!(
            bridge.bridge_flags(id),
            (true, false),
            "registered, still connecting"
        );

        let (lg, _) = rti.next().await;
        eventually("online", || bridge.bridge_flags(id) == (true, true)).await;
        drop(lg);
        eventually("offline", || bridge.bridge_flags(id) == (true, false)).await;
        let (_again, _) = rti.next().await;
        eventually("online again", || bridge.bridge_flags(id) == (true, true)).await;

        bridge.disable(id).await.unwrap();
        assert_eq!(bridge.bridge_flags(id), (false, false));
        tokio::time::sleep(RECONNECT_PERIOD * 3).await;
        let started = BridgeEvent::Started(id.into());
        let up = BridgeEvent::StateChanged(id.into(), true);
        let down = BridgeEvent::StateChanged(id.into(), false);
        assert_eq!(
            events.lock().clone(),
            [
                started,
                up.clone(),
                down,
                up,
                BridgeEvent::Stopped(id.into())
            ],
            "nothing is reported for a stopped session"
        );
    }

    #[tokio::test]
    async fn a_dropped_thinq1_link_is_reconnected() {
        let pki = TestPki::new();
        let mut rti = TlsStub::start(pki.server_config(&pki.leaf(&["rti.test"]), false)).await;
        let http = HttpStub::start(false).await;
        let bridge = test_bridge();
        let id = "t1-reconnect";
        bridge
            .storage
            .set_device_state_json(id, Some(t1_saved_state(rti.port, &http.url)));
        let local = MockLocal::new(id, "thinq1");
        bridge
            .enable(local.clone() as Arc<dyn LocalDevice>, None, None)
            .await
            .unwrap();

        let (mut lg, _) = rti.next().await;
        assert_eq!(
            read_t1_frame(&mut lg).await.unwrap()["Body"]["Cmd"],
            "Alive"
        );
        drop(lg);

        let (mut again, _) = rti.next().await;
        assert_eq!(
            read_t1_frame(&mut again).await.unwrap()["Body"]["Cmd"],
            "Alive"
        );
        assert_eq!(
            http.accepted.load(Ordering::SeqCst),
            2,
            "TotalDeviceInfoSvc before every connection"
        );
        assert!(bridge.status_for(id));

        bridge.disable(id).await.unwrap();
        expect_closed(&mut again, "the link after disable").await;
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
