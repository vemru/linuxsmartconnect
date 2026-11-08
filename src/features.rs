//! Per-device feature switches and persistence.
//!
//! Stored in `$XDG_STATE_HOME/smartconnect-linux/features.json` (mode 0600).
//! Sensitive features (`cross_control`, `storage_mount`, `folder_share`) default to OFF (false).
//! Other features default to ON (true).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::identity::write_private;

const FEATURES_FILE: &str = "features.json";

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

/// Feature flags for a single paired or connected device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceFeatures {
    #[serde(default = "default_true")]
    pub mirror: bool,
    #[serde(default = "default_true")]
    pub webcam: bool,
    #[serde(default = "default_true")]
    pub app_streaming: bool,
    #[serde(default = "default_false")]
    pub cross_control: bool,
    #[serde(default = "default_true")]
    pub phone_display: bool,
    #[serde(default = "default_false")]
    pub storage_mount: bool,
    #[serde(default = "default_false")]
    pub folder_share: bool,
    #[serde(default = "default_true")]
    pub clipboard: bool,
    #[serde(default = "default_true")]
    pub notifications: bool,
}

impl Default for DeviceFeatures {
    fn default() -> Self {
        Self {
            mirror: true,
            webcam: true,
            app_streaming: true,
            cross_control: false,
            phone_display: true,
            storage_mount: false,
            folder_share: false,
            clipboard: true,
            notifications: true,
        }
    }
}

impl DeviceFeatures {
    pub const ALL_FEATURES: &'static [&'static str] = &[
        "mirror",
        "webcam",
        "app_streaming",
        "cross_control",
        "phone_display",
        "storage_mount",
        "folder_share",
        "clipboard",
        "notifications",
    ];

    pub fn get(&self, feature: &str) -> Option<bool> {
        match feature {
            "mirror" => Some(self.mirror),
            "webcam" => Some(self.webcam),
            "app_streaming" | "appStreaming" => Some(self.app_streaming),
            "cross_control" | "crossControl" => Some(self.cross_control),
            "phone_display" | "phoneDisplay" => Some(self.phone_display),
            "storage_mount" | "storageMount" => Some(self.storage_mount),
            "folder_share" | "folderShare" => Some(self.folder_share),
            "clipboard" => Some(self.clipboard),
            "notifications" => Some(self.notifications),
            _ => None,
        }
    }

    pub fn set(&mut self, feature: &str, enabled: bool) -> Result<()> {
        match feature {
            "mirror" => self.mirror = enabled,
            "webcam" => self.webcam = enabled,
            "app_streaming" | "appStreaming" => self.app_streaming = enabled,
            "cross_control" | "crossControl" => self.cross_control = enabled,
            "phone_display" | "phoneDisplay" => self.phone_display = enabled,
            "storage_mount" | "storageMount" => self.storage_mount = enabled,
            "folder_share" | "folderShare" => self.folder_share = enabled,
            "clipboard" => self.clipboard = enabled,
            "notifications" => self.notifications = enabled,
            other => bail!("unknown feature '{other}'"),
        }
        Ok(())
    }
}

/// Persistent store for per-device features.
pub struct FeatureStore {
    path: PathBuf,
    devices: BTreeMap<String, DeviceFeatures>,
}

impl FeatureStore {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(FEATURES_FILE);
        let devices = if path.exists() {
            let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))?
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, devices })
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_vec_pretty(&self.devices)?;
        write_private(&self.path, &json)
    }

    /// Returns the features for a device ID or device name, falling back to default.
    pub fn get_or_default(&self, device: &str) -> DeviceFeatures {
        self.devices.get(device).cloned().unwrap_or_default()
    }

    /// Resolves features for an optional device ID or returns defaults.
    pub fn get_for_optional_device(&self, device: Option<&str>) -> DeviceFeatures {
        match device {
            Some(d) => self.get_or_default(d),
            None => {
                // If there is only one device configured, return its features, else default
                if self.devices.len() == 1 {
                    self.devices.values().next().cloned().unwrap_or_default()
                } else {
                    DeviceFeatures::default()
                }
            }
        }
    }

    /// Checks if a feature is enabled for a device.
    pub fn is_enabled(&self, device: Option<&str>, feature: &str) -> bool {
        self.get_for_optional_device(device)
            .get(feature)
            .unwrap_or(false)
    }

    /// Sets a feature on/off for a device and persists to disk.
    pub fn set(&mut self, device: &str, feature: &str, enabled: bool) -> Result<()> {
        let entry = self.devices.entry(device.to_string()).or_default();
        entry.set(feature, enabled)?;
        self.save()
    }

    /// Returns all configured devices and their features.
    pub fn all(&self) -> &BTreeMap<String, DeviceFeatures> {
        &self.devices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_features_have_sensitive_off() {
        let df = DeviceFeatures::default();
        assert!(df.mirror);
        assert!(df.webcam);
        assert!(df.app_streaming);
        assert!(df.phone_display);
        assert!(df.clipboard);
        assert!(df.notifications);

        // Sensitive features must default to OFF
        assert!(!df.cross_control);
        assert!(!df.storage_mount);
        assert!(!df.folder_share);
    }

    #[test]
    fn get_and_set_features() {
        let mut df = DeviceFeatures::default();
        assert!(df.get("clipboard").unwrap());
        df.set("clipboard", false).unwrap();
        assert!(!df.get("clipboard").unwrap());

        assert!(!df.get("cross_control").unwrap());
        df.set("cross_control", true).unwrap();
        assert!(df.get("cross_control").unwrap());

        assert!(df.set("non_existent", true).is_err());
    }

    #[test]
    fn store_load_save_persistence() {
        let tmp = std::env::temp_dir().join(format!("sc-feat-test-{}", rand::random::<u64>()));
        let _ = fs::create_dir_all(&tmp);
        let mut store = FeatureStore::load(&tmp).unwrap();
        assert!(store.all().is_empty());

        // Default lookup for unknown device returns default features
        let feat = store.get_or_default("phone-1");
        assert!(feat.clipboard);
        assert!(!feat.cross_control);

        // Modify and save
        store.set("phone-1", "clipboard", false).unwrap();
        store.set("phone-1", "cross_control", true).unwrap();

        // Reload store from disk
        let store2 = FeatureStore::load(&tmp).unwrap();
        let feat2 = store2.get_or_default("phone-1");
        assert!(!feat2.clipboard);
        assert!(feat2.cross_control);

        // Check file permissions are 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = fs::metadata(tmp.join(FEATURES_FILE)).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }

        let _ = fs::remove_dir_all(&tmp);
    }
}
