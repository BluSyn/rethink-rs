//! Device metadata shared across ThinQ platforms.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "modelName")]
    pub model_name: String,
    #[serde(rename = "deviceType", default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<String>,
    #[serde(rename = "swVersion", default, skip_serializing_if = "Option::is_none")]
    pub sw_version: Option<String>,
    /// ThinQ2 `appInfo.packageId` from the deploy message. Only some devices send it; LG's
    /// backend takes it into account when choosing the modelJSON.
    #[serde(rename = "packageId", default, skip_serializing_if = "Option::is_none")]
    pub package_id: Option<String>,
}

impl Metadata {
    pub fn new(model_id: impl Into<String>, model_name: impl Into<String>, sw_version: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            model_name: model_name.into(),
            device_type: None,
            sw_version: Some(sw_version.into()),
            package_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_id_is_serialized_only_when_the_device_sent_one() {
        let mut meta = Metadata::new("2REF11EIDG__4", "2REF11EIDG__4", "");
        let json = serde_json::to_value(&meta).unwrap();
        assert!(json.get("packageId").is_none());

        meta.package_id = Some("4456820143800201|1".into());
        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(json["packageId"], "4456820143800201|1");
        assert_eq!(serde_json::from_value::<Metadata>(json).unwrap(), meta);
    }
}
