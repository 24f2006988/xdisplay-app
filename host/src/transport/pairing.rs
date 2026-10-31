//! Pairing state: this host's identity, the one-time PIN, and the tablets allowed to reconnect.
//!
//! The PIN lets a *new* tablet pair. After a successful pairing the PIN is replaced and the
//! tablet receives a long random secret that authenticates every later connection, so a PIN seen
//! over someone's shoulder is useless once it has been used. Five wrong PINs also replace it.
//!
//! Brute force: replacing the PIN alone does not stop a patient attacker on the same network,
//! who could keep guessing against each new PIN (one in a million per guess, forever). So every
//! lockout also *pauses pairing*, for twice as long each time (30 s, 1 min, 2 min ... 1 h), and
//! PIN attempts are serialized ([`PairingStore::pair_gate`]), so parallel connections cannot
//! multiply the guess rate. Pressing "New PIN" on the dashboard ends the current pause.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use xdisplay_common::{hex, random_bytes, random_pin, ClientId, DeviceSecret, HostId};

/// Wrong PIN attempts tolerated before the PIN is replaced.
pub const MAX_PIN_FAILURES: u32 = 5;
/// First pairing pause after a lockout; doubles with every further lockout.
const BASE_LOCKOUT: Duration = Duration::from_secs(30);
/// Longest pairing pause.
const MAX_LOCKOUT: Duration = Duration::from_secs(3600);
/// Longest accepted device name (characters); longer names are cut.
pub const MAX_DEVICE_NAME_CHARS: usize = 48;

/// How long the PC waits for the user to answer a new USB tablet's approval prompt.
pub const USB_APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
/// After a "No", the same tablet is refused without asking again for this long (an old app
/// that retries must not flood the user with prompts).
const USB_DENIAL_MEMORY: Duration = Duration::from_secs(60);

/// Asks the user whether a new USB tablet (by name) may control this PC. Blocking; returns
/// `true` only on an explicit yes. Production uses a dialog (`gui::usb_approval_prompt`).
pub type UsbApprover = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Outcome of asking to admit a new USB tablet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbApproval {
    Approved,
    /// Meanwhile another connection from the same tablet was approved: it must resume.
    AlreadyPaired,
    Denied,
}

/// Pause after the `n`-th consecutive lockout (n >= 1).
pub fn lockout_duration(n: u32) -> Duration {
    let factor = 1u32 << n.saturating_sub(1).min(16);
    BASE_LOCKOUT.saturating_mul(factor).min(MAX_LOCKOUT)
}

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
    /// Lockouts since the last successful pairing; sets the length of the next pause.
    lockouts: u32,
    /// Pairing is refused until then (after too many wrong PINs).
    paused_until: Option<Instant>,
    devices: Vec<PairedDevice>,
}

pub struct PairingStore {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
    /// Held for the whole of a PIN attempt, so attempts run one at a time.
    pair_gate: tokio::sync::Mutex<()>,
    /// One USB approval prompt at a time.
    approval_gate: tokio::sync::Mutex<()>,
    /// `None`: no way to ask the user, so new USB tablets are refused (fail closed).
    usb_approver: Mutex<Option<UsbApprover>>,
    /// Tablets the user refused recently.
    usb_denied: Mutex<HashMap<ClientId, Instant>>,
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
                lockouts: 0,
                paused_until: None,
                devices,
            }),
            pair_gate: tokio::sync::Mutex::new(()),
            approval_gate: tokio::sync::Mutex::new(()),
            usb_approver: Mutex::new(None),
            usb_denied: Mutex::new(HashMap::new()),
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

    /// The dashboard's "New PIN": replaces the PIN and ends a pairing pause (the user is at the
    /// PC). The lockout count is kept, so an attacker who locks it again waits longer.
    pub fn rotate_pin(&self) {
        let mut inner = self.lock();
        Self::rotate(&mut inner);
        inner.paused_until = None;
    }

    /// Installs the function that asks the user about new USB tablets.
    pub fn set_usb_approver(&self, approver: UsbApprover) {
        *self.usb_approver.lock().unwrap_or_else(|e| e.into_inner()) = Some(approver);
    }

    /// First USB connection of an unknown tablet: asks the user (one prompt at a time) and, on
    /// yes, pairs it. Any app on the tablet can reach the USB link, so this is the only thing
    /// standing between such an app and the PC's keyboard and mouse.
    pub async fn approve_usb_device(&self, client_id: ClientId, name: &str) -> (UsbApproval, Option<DeviceSecret>) {
        let _gate = self.approval_gate.lock().await;
        if self.device_secret(&client_id).is_some() {
            return (UsbApproval::AlreadyPaired, None);
        }
        {
            let mut denied = self.usb_denied.lock().unwrap_or_else(|e| e.into_inner());
            denied.retain(|_, at| at.elapsed() < USB_DENIAL_MEMORY);
            if denied.contains_key(&client_id) {
                return (UsbApproval::Denied, None);
            }
        }
        let Some(approver) = self.usb_approver.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            warn!("New USB tablet '{name}' refused: no approval prompt is available");
            return (UsbApproval::Denied, None);
        };
        let name = sanitize_name(name);
        info!("Asking the user to approve new USB tablet '{name}'");
        let ask_name = name.clone();
        // The prompt itself times out (see `gui::usb_approval_prompt`); this is the backstop.
        let answer = tokio::time::timeout(
            USB_APPROVAL_TIMEOUT + Duration::from_secs(5),
            tokio::task::spawn_blocking(move || approver(&ask_name)),
        )
        .await;
        if matches!(answer, Ok(Ok(true))) {
            match self.complete_pairing(client_id, &name) {
                Some(secret) => (UsbApproval::Approved, Some(secret)),
                None => (UsbApproval::Denied, None),
            }
        } else {
            warn!("New USB tablet '{name}' was not approved");
            self.usb_denied.lock().unwrap_or_else(|e| e.into_inner()).insert(client_id, Instant::now());
            (UsbApproval::Denied, None)
        }
    }

    /// Serializes PIN attempts: hold the guard from `AuthStart` until the attempt is decided
    /// (including the failure delay).
    pub async fn pair_gate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.pair_gate.lock().await
    }

    /// Time left before pairing is accepted again, if it is paused after too many wrong PINs.
    pub fn pairing_paused(&self) -> Option<Duration> {
        let mut inner = self.lock();
        let left = inner.paused_until?.checked_duration_since(Instant::now()).filter(|d| !d.is_zero());
        if left.is_none() {
            inner.paused_until = None;
        }
        left
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

    /// Counts a wrong PIN. Returns true when the limit was hit: the PIN was replaced and
    /// pairing is paused.
    pub fn record_pin_failure(&self) -> bool {
        let mut inner = self.lock();
        inner.pin_failures += 1;
        if inner.pin_failures >= MAX_PIN_FAILURES {
            inner.lockouts = inner.lockouts.saturating_add(1);
            let pause = lockout_duration(inner.lockouts);
            inner.paused_until = Some(Instant::now() + pause);
            warn!(
                "{MAX_PIN_FAILURES} wrong pairing PINs in a row; new PIN generated and pairing paused for {} s \
                 (lockout {}). Press New PIN on the dashboard to pair now.",
                pause.as_secs(),
                inner.lockouts
            );
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
        inner.lockouts = 0;
        inner.paused_until = None;
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

/// Strips control characters (a newline would forge log lines or add a line to `pairing.conf`)
/// and bounds the length of a name supplied by the tablet.
pub fn sanitize_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).take(MAX_DEVICE_NAME_CHARS).collect();
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
    fn lockout_pauses_pairing_and_new_pin_resumes_it() {
        let store = PairingStore::load(None);
        assert!(store.pairing_paused().is_none());
        for _ in 0..MAX_PIN_FAILURES {
            store.record_pin_failure();
        }
        let left = store.pairing_paused().expect("pairing is paused after a lockout");
        assert!(left <= lockout_duration(1) && left > Duration::from_secs(25), "{left:?}");
        store.rotate_pin();
        assert!(store.pairing_paused().is_none(), "New PIN ends the pause");
    }

    #[test]
    fn lockouts_escalate_and_reset_after_a_pairing() {
        assert_eq!(lockout_duration(1), Duration::from_secs(30));
        assert_eq!(lockout_duration(2), Duration::from_secs(60));
        assert_eq!(lockout_duration(5), Duration::from_secs(480));
        assert_eq!(lockout_duration(50), MAX_LOCKOUT);
        assert_eq!(lockout_duration(u32::MAX), MAX_LOCKOUT);

        let store = PairingStore::load(None);
        for _ in 0..2 * MAX_PIN_FAILURES {
            store.record_pin_failure();
        }
        assert!(store.pairing_paused().unwrap() > lockout_duration(1), "second lockout is longer");
        store.complete_pairing([8; 16], "Tab").unwrap();
        assert!(store.pairing_paused().is_none());
        assert_eq!(store.lock().lockouts, 0);
    }

    #[test]
    fn hostile_names_are_sanitized() {
        assert_eq!(sanitize_name("a\r\ndevice 00 11 evil"), "adevice 00 11 evil");
        assert_eq!(sanitize_name(&"x".repeat(500)).chars().count(), MAX_DEVICE_NAME_CHARS);
        assert_eq!(sanitize_name("\u{7}\u{1b}[2J"), "[2J");
        assert_eq!(sanitize_name("   "), "Tablet");
        // A crafted name cannot add a device line to pairing.conf.
        let store = PairingStore::load(None);
        let evil = format!("x\ndevice {} {} injected", "00".repeat(16), "11".repeat(32));
        store.complete_pairing([1; 16], &evil).unwrap();
        assert_eq!(parse(&serialize(&store.lock())).1.len(), 1);
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
