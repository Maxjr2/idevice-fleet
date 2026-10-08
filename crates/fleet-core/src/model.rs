//! Shared data types the UI renders.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable identity for a device. An ECID survives every mode change, so it's
/// preferred; a UDID is used only until the ECID of a normal-mode device is known.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceKey(pub String);

impl DeviceKey {
    pub fn ecid(ecid: u64) -> Self {
        Self(format!("ecid:{ecid:x}"))
    }
    pub fn udid(udid: &str) -> Self {
        Self(format!("udid:{udid}"))
    }
    pub fn as_ecid(&self) -> Option<u64> {
        self.0.strip_prefix("ecid:").and_then(|h| u64::from_str_radix(h, 16).ok())
    }
}

impl fmt::Display for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceMode {
    /// Booted, reachable through usbmuxd.
    Normal,
    Recovery,
    Dfu,
    /// Not visible right now but a job is still working on it (it is rebooting
    /// between stages), so it stays in the list.
    Reconnecting,
}

impl DeviceMode {
    pub fn label(self) -> &'static str {
        match self {
            DeviceMode::Normal => "Normal",
            DeviceMode::Recovery => "Recovery",
            DeviceMode::Dfu => "DFU",
            DeviceMode::Reconnecting => "Reconnecting",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairState {
    Paired,
    NotPaired,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub key: DeviceKey,
    pub mode: DeviceMode,
    pub udid: Option<String>,
    pub ecid: Option<u64>,
    pub name: Option<String>,
    pub product_type: Option<String>,
    pub os_version: Option<String>,
    pub build: Option<String>,
    pub serial: Option<String>,
    pub activation_state: Option<String>,
    pub battery_percent: Option<u8>,
    pub pair_state: PairState,
    pub cpid: Option<u64>,
    pub bdid: Option<u64>,
    /// Why the last info lookup failed, if it did.
    pub problem: Option<String>,
    /// Seconds since the Unix epoch.
    pub last_seen: u64,
}

impl Device {
    pub fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.product_type.clone())
            .or_else(|| self.serial.clone())
            .unwrap_or_else(|| self.key.0.clone())
    }
}

/// What we remember about a device after it has been seen once, so it keeps
/// its name and serial in recovery mode and across app restarts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnownIdentity {
    pub ecid: u64,
    pub udid: Option<String>,
    pub name: Option<String>,
    pub product_type: Option<String>,
    pub serial: Option<String>,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
