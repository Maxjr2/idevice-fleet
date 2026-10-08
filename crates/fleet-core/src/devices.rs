//! The device registry: merges normal-mode devices (usbmuxd) and
//! recovery/DFU devices (raw USB) into one list keyed by ECID.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::model::{Device, DeviceKey, DeviceMode, KnownIdentity, PairState, now_secs};
use crate::store::Store;

/// Health of a device source, shown in the status bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Health {
    Starting,
    Ok,
    Down(String),
}

/// What lockdown told us about a normal-mode device.
#[derive(Debug, Clone, Default)]
pub struct NormalInfo {
    pub ecid: Option<u64>,
    pub name: Option<String>,
    pub product_type: Option<String>,
    pub os_version: Option<String>,
    pub build: Option<String>,
    pub serial: Option<String>,
    pub activation_state: Option<String>,
    pub battery_percent: Option<u8>,
    pub paired: bool,
}

/// A device seen on USB in recovery or DFU mode.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryInfo {
    pub ecid: u64,
    pub dfu: bool,
    pub cpid: Option<u64>,
    pub bdid: Option<u64>,
    pub serial: Option<String>,
}

struct NormalEntry {
    mux_id: u32,
    info: Option<NormalInfo>,
    problem: Option<String>,
    seen: u64,
}

#[derive(Default)]
struct State {
    normal: HashMap<String, NormalEntry>,
    recovery: HashMap<u64, RecoveryInfo>,
    identities: HashMap<u64, KnownIdentity>,
    last_known: HashMap<DeviceKey, Device>,
    usbmuxd: Option<Health>,
    usb: Option<Health>,
}

pub struct Registry {
    state: Mutex<State>,
    revision: AtomicU64,
    store: Option<Arc<Store>>,
}

impl Registry {
    pub fn new(store: Option<Arc<Store>>) -> Self {
        let mut state = State::default();
        if let Some(s) = &store {
            for id in s.load_identities().unwrap_or_default() {
                state.identities.insert(id.ecid, id);
            }
        }
        Self { state: Mutex::new(state), revision: AtomicU64::new(1), store }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    pub fn set_usbmuxd_health(&self, h: Health) {
        let mut s = self.lock();
        if s.usbmuxd.as_ref() != Some(&h) {
            s.usbmuxd = Some(h);
            drop(s);
            self.bump();
        }
    }

    pub fn set_usb_health(&self, h: Health) {
        let mut s = self.lock();
        if s.usb.as_ref() != Some(&h) {
            s.usb = Some(h);
            drop(s);
            self.bump();
        }
    }

    pub fn health(&self) -> (Health, Health) {
        let s = self.lock();
        (s.usbmuxd.clone().unwrap_or(Health::Starting), s.usb.clone().unwrap_or(Health::Starting))
    }

    pub fn normal_attached(&self, udid: &str, mux_id: u32) {
        let mut s = self.lock();
        let e = s.normal.entry(udid.to_string()).or_insert(NormalEntry { mux_id, info: None, problem: None, seen: now_secs() });
        e.mux_id = mux_id;
        e.seen = now_secs();
        drop(s);
        self.bump();
    }

    pub fn normal_info(&self, udid: &str, info: NormalInfo) {
        let mut s = self.lock();
        if let Some(ecid) = info.ecid {
            let identity = KnownIdentity {
                ecid,
                udid: Some(udid.to_string()),
                name: info.name.clone(),
                product_type: info.product_type.clone(),
                serial: info.serial.clone(),
            };
            if s.identities.get(&ecid) != Some(&identity) {
                if let Some(store) = &self.store
                    && let Err(e) = store.save_identity(&identity)
                {
                    tracing::warn!("could not remember device identity: {e}");
                }
                s.identities.insert(ecid, identity);
            }
        }
        if let Some(e) = s.normal.get_mut(udid) {
            e.info = Some(info);
            e.problem = None;
        }
        drop(s);
        self.bump();
    }

    pub fn normal_problem(&self, udid: &str, problem: String) {
        if let Some(e) = self.lock().normal.get_mut(udid) {
            e.problem = Some(problem);
        }
        self.bump();
    }

    pub fn normal_detached(&self, mux_id: u32) {
        self.lock().normal.retain(|_, e| e.mux_id != mux_id);
        self.bump();
    }

    /// usbmuxd went away: we no longer know which normal devices are attached.
    pub fn normal_clear(&self) {
        self.lock().normal.clear();
        self.bump();
    }

    pub fn set_recovery(&self, list: Vec<RecoveryInfo>) {
        let mut s = self.lock();
        let new: HashMap<u64, RecoveryInfo> = list.into_iter().map(|r| (r.ecid, r)).collect();
        if new != s.recovery {
            s.recovery = new;
            drop(s);
            self.bump();
        }
    }

    pub fn is_in_recovery(&self, ecid: u64) -> bool {
        self.lock().recovery.contains_key(&ecid)
    }

    /// UDID and usbmuxd ID of a device currently in normal mode.
    pub fn normal_udid(&self, key: &DeviceKey) -> Option<(String, u32)> {
        let s = self.lock();
        if let Some(udid) = key.0.strip_prefix("udid:") {
            return s.normal.get(udid).map(|e| (udid.to_string(), e.mux_id));
        }
        let ecid = key.as_ecid()?;
        s.normal.iter().find(|(_, e)| e.info.as_ref().and_then(|i| i.ecid) == Some(ecid)).map(|(u, e)| (u.clone(), e.mux_id))
    }

    /// The merged device list. Devices with an active job that are not visible
    /// right now (rebooting between stages) are kept as `Reconnecting`.
    pub fn snapshot(&self, busy: &HashSet<DeviceKey>) -> Vec<Device> {
        let mut s = self.lock();
        let mut out: HashMap<DeviceKey, Device> = HashMap::new();

        for (udid, e) in &s.normal {
            let info = e.info.clone().unwrap_or_default();
            let key = info.ecid.map(DeviceKey::ecid).unwrap_or_else(|| DeviceKey::udid(udid));
            let known = info.ecid.and_then(|c| s.identities.get(&c));
            out.insert(key.clone(), Device {
                key,
                mode: DeviceMode::Normal,
                udid: Some(udid.clone()),
                ecid: info.ecid,
                name: info.name.or_else(|| known.and_then(|k| k.name.clone())),
                product_type: info.product_type,
                os_version: info.os_version,
                build: info.build,
                serial: info.serial.or_else(|| known.and_then(|k| k.serial.clone())),
                activation_state: info.activation_state,
                battery_percent: info.battery_percent,
                pair_state: match &e.info {
                    None => PairState::Unknown,
                    Some(i) if i.paired => PairState::Paired,
                    Some(_) => PairState::NotPaired,
                },
                cpid: None,
                bdid: None,
                problem: e.problem.clone(),
                last_seen: e.seen,
            });
        }

        for r in s.recovery.values() {
            let key = DeviceKey::ecid(r.ecid);
            if out.contains_key(&key) {
                continue;
            }
            let known = s.identities.get(&r.ecid);
            out.insert(key.clone(), Device {
                key,
                mode: if r.dfu { DeviceMode::Dfu } else { DeviceMode::Recovery },
                udid: known.and_then(|k| k.udid.clone()),
                ecid: Some(r.ecid),
                name: known.and_then(|k| k.name.clone()),
                product_type: known.and_then(|k| k.product_type.clone()),
                os_version: None,
                build: None,
                serial: known.and_then(|k| k.serial.clone()).or_else(|| r.serial.clone()),
                activation_state: None,
                battery_percent: None,
                pair_state: PairState::Unknown,
                cpid: r.cpid,
                bdid: r.bdid,
                problem: None,
                last_seen: now_secs(),
            });
        }

        for key in busy {
            if !out.contains_key(key)
                && let Some(d) = s.last_known.get(key)
            {
                out.insert(key.clone(), Device { mode: DeviceMode::Reconnecting, ..d.clone() });
            }
        }

        for (k, d) in &out {
            if d.mode != DeviceMode::Reconnecting {
                s.last_known.insert(k.clone(), d.clone());
            }
        }

        let mut list: Vec<Device> = out.into_values().collect();
        list.sort_by(|a, b| a.label().to_lowercase().cmp(&b.label().to_lowercase()).then_with(|| a.key.cmp(&b.key)));
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(ecid: u64) -> NormalInfo {
        NormalInfo { ecid: Some(ecid), name: Some("Front desk".into()), product_type: Some("iPad13,18".into()), serial: Some("F9FZ".into()), paired: true, ..Default::default() }
    }

    #[test]
    fn device_keeps_identity_from_normal_to_recovery() {
        let r = Registry::new(None);
        r.normal_attached("udid-1", 7);
        let before = r.snapshot(&HashSet::new());
        assert_eq!(before[0].key, DeviceKey::udid("udid-1"));
        assert_eq!(before[0].pair_state, PairState::Unknown);

        r.normal_info("udid-1", info(0xAB));
        let normal = r.snapshot(&HashSet::new());
        assert_eq!(normal[0].key, DeviceKey::ecid(0xAB));
        assert_eq!(normal[0].mode, DeviceMode::Normal);

        r.normal_detached(7);
        r.set_recovery(vec![RecoveryInfo { ecid: 0xAB, dfu: false, cpid: Some(0x8101), bdid: Some(0x14), serial: None }]);
        let rec = r.snapshot(&HashSet::new());
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].mode, DeviceMode::Recovery);
        assert_eq!(rec[0].name.as_deref(), Some("Front desk"));
        assert_eq!(rec[0].serial.as_deref(), Some("F9FZ"));
    }

    #[test]
    fn busy_device_stays_listed_while_it_reboots() {
        let r = Registry::new(None);
        r.normal_attached("udid-1", 1);
        r.normal_info("udid-1", info(0xCD));
        let key = DeviceKey::ecid(0xCD);
        r.snapshot(&HashSet::new());
        r.normal_detached(1);
        assert!(r.snapshot(&HashSet::new()).is_empty());
        let busy = HashSet::from([key.clone()]);
        let list = r.snapshot(&busy);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].mode, DeviceMode::Reconnecting);
        assert_eq!(list[0].name.as_deref(), Some("Front desk"));
    }

    #[test]
    fn identities_survive_a_restart() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let r = Registry::new(Some(store.clone()));
        r.normal_attached("udid-1", 1);
        r.normal_info("udid-1", info(0xEF));
        let r2 = Registry::new(Some(store));
        r2.set_recovery(vec![RecoveryInfo { ecid: 0xEF, dfu: true, cpid: None, bdid: None, serial: None }]);
        let list = r2.snapshot(&HashSet::new());
        assert_eq!(list[0].mode, DeviceMode::Dfu);
        assert_eq!(list[0].name.as_deref(), Some("Front desk"));
    }
}
