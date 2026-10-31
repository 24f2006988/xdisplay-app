//! Pairing state: this host's identity, the one-time PIN, and the tablets allowed to reconnect.
//!
//! The PIN lets a *new* tablet pair. After a successful pairing the PIN is replaced and the
//! tablet receives a long random secret that authenticates every later connection, so a PIN seen
//! over someone's shoulder is useless once it has been used. Five wrong PINs also replace it.

use std::path::PathBuf;
use std::sync::Mutex;

use tracing::{info, warn};
use xdisplay_common::{hex, random_bytes, random_pin, ClientId, DeviceSecret, HostId};

/// Wrong PIN attempts tolerated before the PIN is replaced.
pub const MAX_PIN_FAILURES: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub client_id: ClientId,
    pub secret: DeviceSecret,
    pub name: String,
}

struct Inner {
    host_id: HostId,
    pin: String,
    pin_failures: u32,
    devices: Vec<PairedDevice>,
}

pub struct PairingStore {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl PairingStore {
    /// Loads `%APPDATA%\XDisplay\pairing.conf`, creating the host identity on first run.
    pub fn load_default() -> Self {
        let path = std::env::var_os("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("XDisplay").join("pairing.conf"));
        Self::load(path)
    }

    pub fn load(path: Option<PathBuf>) -> Self {
        let (host_id, devices) = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| parse(&text))
            .unwrap_or_default();
        let created = host_id.is_none();
        let host_id = host_id.unwrap_or_else(|| random_bytes().expect("operating system randomness unavailable"));
        let store = Self {
            path,
            inner: Mutex::new(Inner {
                host_id,
                pin: random_pin().expect("operating system randomness unavailable"),
                pin_failures: 0,
                devices,
            }),
        };
        if created {
            store.persist(&store.lock());
        }
        store
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn host_id(&self) -> HostId {
        self.lock().host_id
    }

    pub fn pin(&self) -> String {
        self.lock().pin.clone()
    }

    pub fn device_count(&self) -> usize {
        self.lock().devices.len()
    }

    pub fn device_names(&self) -> Vec<String> {
        self.lock().devices.iter().map(|d| d.name.clone()).collect()
    }

    pub fn device_secret(&self, client_id: &ClientId) -> Option<DeviceSecret> {
        self.lock().devices.iter().find(|d| &d.client_id == client_id).map(|d| d.secret)
    }

    pub fn rotate_pin(&self) {
        Self::rotate(&mut self.lock());
    }

    fn rotate(inner: &mut Inner) {
        match random_pin() {
            Ok(pin) => {
                inner.pin = pin;
                inner.pin_failures = 0;
            }
            Err(e) => warn!("Could not generate a new pairing PIN ({e}); keeping the current one"),
        }
    }

    /// Counts a wrong PIN. Returns true when the limit was hit and the PIN was replaced.
    pub fn record_pin_failure(&self) -> bool {
        let mut inner = self.lock();
        inner.pin_failures += 1;
        if inner.pin_failures >= MAX_PIN_FAILURES {
            warn!("{MAX_PIN_FAILURES} wrong pairing PINs in a row; generating a new PIN");
            Self::rotate(&mut inner);
            true
        } else {
            false
        }
    }

    /// Registers (or re-registers) a tablet after a verified PIN and returns its new secret.
    /// The PIN is single-use: it is replaced here.
    pub fn complete_pairing(&self, client_id: ClientId, name: &str) -> Option<DeviceSecret> {
        let secret = random_bytes::<32>().ok()?;
        let name = sanitize_name(name);
        let mut inner = self.lock();
        inner.devices.retain(|d| d.client_id != client_id);
        inner.devices.push(PairedDevice { client_id, secret, name: name.clone() });
        Self::rotate(&mut inner);
        self.persist(&inner);
        info!("Paired new device '{name}' ({} paired in total)", inner.devices.len());
        Some(secret)
    }

    pub fn forget_all(&self) {
        let mut inner = self.lock();
        inner.devices.clear();
        Self::rotate(&mut inner);
        self.persist(&inner);
        info!("All paired devices forgotten");
    }

    fn persist(&self, inner: &Inner) {
        let Some(path) = &self.path else { return };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serialize(inner))?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            warn!("Could not save pairing data to {}: {e}", path.display());
        }
    }
}

fn sanitize_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).take(48).collect();
    if clean.trim().is_empty() { "Tablet".to_string() } else { clean }
}

fn serialize(inner: &Inner) -> String {
    let mut out = format!("host_id {}\n", hex::encode(&inner.host_id));
    for d in &inner.devices {
        out.push_str(&format!("device {} {} {}\n", hex::encode(&d.client_id), hex::encode(&d.secret), d.name));
    }
    out
}

fn parse(text: &str) -> (Option<HostId>, Vec<PairedDevice>) {
    let mut host_id = None;
    let mut devices = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(4, ' ');
        match parts.next() {
            Some("host_id") => host_id = parts.next().and_then(hex::decode_array),
            Some("device") => {
                let client_id = parts.next().and_then(hex::decode_array);
                let secret = parts.next().and_then(hex::decode_array);
                if let (Some(client_id), Some(secret)) = (client_id, secret) {
                    devices.push(PairedDevice { client_id, secret, name: parts.next().unwrap_or("Tablet").to_string() });
                }
            }
            _ => {}
        }
    }
    (host_id, devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xdisplay-pairing-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("pairing.conf")
    }

    #[test]
    fn identity_and_devices_survive_restart() {
        let path = temp_path("persist");
        let store = PairingStore::load(Some(path.clone()));
        let secret = store.complete_pairing([5; 16], "Pad 7\nPro").unwrap();

        let reloaded = PairingStore::load(Some(path.clone()));
        assert_eq!(reloaded.host_id(), store.host_id());
        assert_eq!(reloaded.device_secret(&[5; 16]), Some(secret));
        assert_eq!(reloaded.device_names(), vec!["Pad 7Pro".to_string()]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn pairing_consumes_the_pin() {
        let store = PairingStore::load(None);
        let before = store.pin();
        store.complete_pairing([1; 16], "A").unwrap();
        // 1 in a million collision is acceptable noise; retry once to keep the test deterministic.
        if store.pin() == before {
            store.complete_pairing([2; 16], "B").unwrap();
        }
        assert_ne!(store.pin(), before);
    }

    #[test]
    fn repairing_replaces_instead_of_duplicating() {
        let store = PairingStore::load(None);
        let first = store.complete_pairing([9; 16], "Tab").unwrap();
        let second = store.complete_pairing([9; 16], "Tab").unwrap();
        assert_ne!(first, second);
        assert_eq!(store.device_count(), 1);
        assert_eq!(store.device_secret(&[9; 16]), Some(second));
    }

    #[test]
    fn pin_is_replaced_after_too_many_failures() {
        let store = PairingStore::load(None);
        for _ in 0..MAX_PIN_FAILURES - 1 {
            assert!(!store.record_pin_failure());
        }
        assert!(store.record_pin_failure());
    }

    #[test]
    fn forget_all_clears_devices() {
        let store = PairingStore::load(None);
        store.complete_pairing([3; 16], "A").unwrap();
        store.forget_all();
        assert_eq!(store.device_count(), 0);
        assert_eq!(store.device_secret(&[3; 16]), None);
    }

    #[test]
    fn corrupt_file_lines_are_ignored() {
        let (id, devices) = parse("host_id zz\ndevice 00 11 x\nnonsense\n");
        assert!(id.is_none());
        assert!(devices.is_empty());
    }
}
