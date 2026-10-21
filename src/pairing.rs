//! Paired phones, keyed by the phone's stable `realDeviceId`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::identity::write_private;
use crate::invite::gen_key;

const PAIRED_FILE: &str = "paired.json";

#[derive(Clone, Serialize, Deserialize)]
pub struct PairedPhone {
    pub device_name: Option<String>,
    pub real_device_id: Option<String>,
    /// Per-pair key the PC hands the phone (verify reply + EXCHANGE_KEY); the phone stores it as
    /// `paired_device.secretKey`, and Windows uses it as the AES-256-GCM discovery-channel key.
    pub key: String,
    /// The phone's own key from its svc 0 `cmd:1` (sendSecretKey) — the other key of the pair.
    #[serde(default)]
    pub phone_key: Option<String>,
}

pub struct PairingStore {
    path: PathBuf,
    phones: BTreeMap<String, PairedPhone>,
}

impl PairingStore {
    pub fn load(dir: &std::path::Path) -> Result<Self> {
        let path = dir.join(PAIRED_FILE);
        let phones = if path.exists() { serde_json::from_slice(&fs::read(&path)?)? } else { BTreeMap::new() };
        Ok(Self { path, phones })
    }

    pub fn is_empty(&self) -> bool {
        self.phones.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &PairedPhone)> {
        self.phones.iter()
    }

    /// Look up a paired phone by its stable key (realDeviceId).
    pub fn get(&self, key: &str) -> Option<&PairedPhone> {
        self.phones.get(key)
    }

    /// Returns the existing record or creates one with a fresh key. Keyed by the phone's stable
    /// `realDeviceId` — the envelope deviceId is a per-connection nonce and changes on reconnect.
    pub fn pair(
        &mut self,
        key: &str,
        device_name: Option<&str>,
        real_device_id: Option<&str>,
    ) -> Result<PairedPhone> {
        let entry = self.phones.entry(key.to_owned()).or_insert_with(|| PairedPhone {
            device_name: None,
            real_device_id: None,
            key: gen_key(),
            phone_key: None,
        });
        if device_name.is_some() {
            entry.device_name = device_name.map(str::to_owned);
        }
        if real_device_id.is_some() {
            entry.real_device_id = real_device_id.map(str::to_owned);
        }
        let record = entry.clone();
        write_private(&self.path, &serde_json::to_vec_pretty(&self.phones)?)?;
        Ok(record)
    }

    /// Saves the phone's own key from its key exchange (svc 0 `cmd:1`).
    pub fn save_phone_key(&mut self, key: &str, phone_key: &str) -> Result<()> {
        if let Some(entry) = self.phones.get_mut(key) {
            entry.phone_key = Some(phone_key.to_owned());
            write_private(&self.path, &serde_json::to_vec_pretty(&self.phones)?)?;
        }
        Ok(())
    }
}
