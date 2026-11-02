//! What the linked ThinQ account knows about the appliances, for the management panel.
//!
//! rethink knows a device by its id and model, which is useless for telling four identical
//! ceiling cassettes apart. The account holds the names the owner gave them in the app; they are
//! best-effort, so a cloud that does not answer costs a log line and leaves the names alone.
//! For an appliance registered with the account, LG also hands out its modelJSON.

use crate::{thinq_api, Bridge};
use serde_json::Value;
use std::collections::HashMap;

impl Bridge {
    /// What the owner calls the appliance, once the account has been read.
    pub fn name(&self, id: &str) -> Option<String> {
        self.device_names.lock().get(id).cloned()
    }

    /// Called whenever the names change.
    pub fn on_names_changed(&self, listener: impl Fn() + Send + Sync + 'static) {
        self.names_changed.lock().push(Box::new(listener));
    }

    /// Re-read the names from the account. Without credentials this clears them.
    pub async fn refresh_names(&self) {
        let Some(creds) = self.storage.get_credentials() else {
            self.clear_names();
            return;
        };
        let mut client = thinq_api::Client::new(creds.env.clone());
        if let Err(e) = client.auth(&creds.refresh_token).await {
            warn_names_unavailable(&e);
            return;
        }
        self.refresh_names_with(&client, &creds.refresh_token).await;
    }

    /// Re-read the names with a client already authenticated with `refresh_token`.
    pub(crate) async fn refresh_names_with(&self, client: &thinq_api::Client, refresh_token: &str) {
        match client.list_devices().await {
            Ok(devices) => self.apply_account_names(refresh_token, &devices),
            Err(e) => warn_names_unavailable(&e),
        }
    }

    /// Replace the names. Listeners are only told when something actually changed.
    pub fn set_names(&self, names: HashMap<String, String>) {
        let changed = {
            let mut current = self.device_names.lock();
            let changed = *current != names;
            *current = names;
            changed
        };
        if changed {
            for listener in self.names_changed.lock().iter() {
                listener();
            }
        }
    }

    pub fn clear_names(&self) {
        self.set_names(HashMap::new());
    }

    fn apply_account_names(&self, refresh_token: &str, devices: &[Value]) {
        // A login or logout while the account was being read: the list belongs to other credentials.
        let current = self.storage.get_credentials();
        if current.as_ref().map(|c| c.refresh_token.as_str()) != Some(refresh_token) {
            return;
        }
        self.set_names(device_aliases(devices));
    }

    /// The modelJSON of an appliance registered with the ThinQ cloud, i.e. one with a live bridge
    /// session. `model_name` is `None` when the appliance is not connected.
    pub async fn get_model_json(
        &self,
        device_id: &str,
        model_name: Option<&str>,
    ) -> anyhow::Result<String> {
        let creds = self
            .storage
            .get_credentials()
            .ok_or_else(|| anyhow::anyhow!("Not logged in"))?;
        let model_name = model_name.ok_or_else(|| anyhow::anyhow!("Unknown device"))?;
        if !self.status_for(device_id) {
            anyhow::bail!("Bridge mode is not enabled for this device");
        }
        let mut client = thinq_api::Client::new(creds.env);
        client.auth(&creds.refresh_token).await?;
        client.get_model_json(device_id, model_name).await
    }
}

fn warn_names_unavailable(e: &anyhow::Error) {
    eprintln!("[bridge] Could not read the device names from the ThinQ account: {e:#}");
}

/// `deviceId` → `alias` for the devices of a home that have a name.
fn device_aliases(devices: &[Value]) -> HashMap<String, String> {
    devices
        .iter()
        .filter_map(|dev| {
            let id = dev.get("deviceId")?.as_str()?;
            let alias = dev
                .get("alias")?
                .as_str()
                .filter(|alias| !alias.is_empty())?;
            Some((id.to_string(), alias.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BridgeState, Credentials, Environment, JsonStorage};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn credentials(refresh_token: &str) -> Credentials {
        Credentials {
            refresh_token: refresh_token.into(),
            env: Environment {
                country_code: "US".into(),
                language_code: None,
            },
        }
    }

    fn bridge(credentials: Option<Credentials>) -> Arc<Bridge> {
        let dir = std::env::temp_dir().join(format!("rethink-names-{}", uuid::Uuid::new_v4()));
        let storage = Arc::new(JsonStorage::new(dir));
        storage.set_credentials(credentials);
        Bridge::new(storage)
    }

    fn announcements(bridge: &Bridge) -> Arc<AtomicUsize> {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        bridge.on_names_changed(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        count
    }

    fn names(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect()
    }

    #[test]
    fn there_are_none_until_an_account_has_been_read() {
        let bridge = bridge(None);
        assert_eq!(bridge.name("any-device"), None);
    }

    #[tokio::test]
    async fn logging_out_clears_them_and_says_so() {
        // Built logged out; the names are put in place as a successful read would have left them.
        let bridge = bridge(None);
        bridge.set_names(names(&[("cassette", "Bedroom AC")]));
        let announced = announcements(&bridge);

        bridge.logout().await.unwrap();

        assert_eq!(bridge.name("cassette"), None);
        assert_eq!(announced.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn names_come_from_the_aliases_in_the_home_device_list() {
        let devices = json!([
            {"deviceId": "cassette-1", "alias": "Bedroom AC", "modelName": "RAC_056905_WW"},
            {"deviceId": "cassette-2", "alias": "침실 에어컨"},
            {"deviceId": "unnamed", "alias": ""},
            {"deviceId": "no-alias"},
            {"alias": "no id"},
        ]);
        assert_eq!(
            device_aliases(devices.as_array().unwrap()),
            names(&[("cassette-1", "Bedroom AC"), ("cassette-2", "침실 에어컨")])
        );
    }

    #[test]
    fn a_read_finished_after_the_credentials_changed_is_dropped() {
        let bridge = bridge(Some(credentials("token-a")));
        let devices = json!([{"deviceId": "cassette", "alias": "Bedroom AC"}]);
        bridge.apply_account_names("token-a", devices.as_array().unwrap());
        assert_eq!(bridge.name("cassette").as_deref(), Some("Bedroom AC"));

        let renamed = json!([{"deviceId": "cassette", "alias": "Guest room AC"}]);
        bridge.storage.set_credentials(Some(credentials("token-b")));
        bridge.apply_account_names("token-a", renamed.as_array().unwrap());
        assert_eq!(
            bridge.name("cassette").as_deref(),
            Some("Bedroom AC"),
            "re-login"
        );

        bridge.storage.set_credentials(None);
        bridge.apply_account_names("token-b", renamed.as_array().unwrap());
        assert_eq!(
            bridge.name("cassette").as_deref(),
            Some("Bedroom AC"),
            "logout"
        );
    }

    #[tokio::test]
    async fn listeners_only_hear_about_changes() {
        let bridge = bridge(Some(credentials("token")));
        let announced = announcements(&bridge);
        let devices = json!([{"deviceId": "cassette", "alias": "Bedroom AC"}]);

        bridge.apply_account_names("token", devices.as_array().unwrap());
        bridge.apply_account_names("token", devices.as_array().unwrap());
        assert_eq!(announced.load(Ordering::SeqCst), 1, "the same names again");

        bridge.storage.set_credentials(None);
        bridge.refresh_names().await;
        assert_eq!(bridge.name("cassette"), None, "no credentials, no names");
        assert_eq!(announced.load(Ordering::SeqCst), 2);

        bridge.clear_names();
        assert_eq!(announced.load(Ordering::SeqCst), 2, "already empty");
    }

    #[tokio::test]
    async fn model_json_needs_a_login_a_connected_device_and_bridge_mode() {
        let logged_out = bridge(None);
        let err = logged_out.get_model_json("dev", None).await.unwrap_err();
        assert_eq!(err.to_string(), "Not logged in");

        let bridge = bridge(Some(credentials("token")));
        let err = bridge.get_model_json("dev", None).await.unwrap_err();
        assert_eq!(err.to_string(), "Unknown device");
        let err = bridge.get_model_json("dev", Some("M")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Bridge mode is not enabled for this device"
        );
    }
}
