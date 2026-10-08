//! Errors, sorted by what the job engine should do about them.

use idevice::{IdeviceError, restore::RestoreError, usbmuxd::errors::UsbmuxdError};
use serde::{Deserialize, Serialize};

/// What kind of failure this is, which decides retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorClass {
    /// Likely to work on another attempt: USB hiccup, device re-enumerating,
    /// usbmuxd restarting, network trouble reaching Apple.
    Transient,
    /// Waiting on a person: unlock the device, tap Trust, enter a passcode.
    NeedsUser,
    /// Retrying won't help: bad firmware file, unsupported device, rejected by Apple.
    Permanent,
    /// Stopped on request.
    Cancelled,
}

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct FleetError {
    pub class: ErrorClass,
    pub message: String,
    /// The native restore engine can't handle this; another engine might.
    #[serde(default)]
    pub fallback_suggested: bool,
}

impl FleetError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self { class, message: message.into(), fallback_suggested: false }
    }
    pub fn transient(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Transient, message)
    }
    pub fn needs_user(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::NeedsUser, message)
    }
    pub fn permanent(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Permanent, message)
    }
    pub fn cancelled() -> Self {
        Self::new(ErrorClass::Cancelled, "Cancelled")
    }
    pub fn with_fallback(mut self) -> Self {
        self.fallback_suggested = true;
        self
    }
    pub fn is_retryable(&self) -> bool {
        self.class == ErrorClass::Transient
    }
    /// Prefix the message with what was being done, keeping the class.
    pub fn context(mut self, what: &str) -> Self {
        self.message = format!("{what}: {}", self.message);
        self
    }
}

impl From<IdeviceError> for FleetError {
    fn from(e: IdeviceError) -> Self {
        classify(e)
    }
}

impl From<std::io::Error> for FleetError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match e.kind() {
            ConnectionRefused | ConnectionReset | ConnectionAborted | BrokenPipe | TimedOut
            | UnexpectedEof | Interrupted | NotConnected | WouldBlock => FleetError::transient(e.to_string()),
            _ => FleetError::permanent(e.to_string()),
        }
    }
}

impl From<rusqlite::Error> for FleetError {
    fn from(e: rusqlite::Error) -> Self {
        FleetError::permanent(format!("database: {e}"))
    }
}

fn classify(e: IdeviceError) -> FleetError {
    use IdeviceError as E;
    let text = e.to_string();
    match e {
        E::Socket(io) => FleetError::from(io),
        E::Timeout | E::DeviceNotFound | E::NoEstablishedConnection | E::SessionInactive
        | E::Heartbeat(_) | E::Http(_) | E::NotEnoughBytes(..) => FleetError::transient(text),
        E::Usbmuxd(u) => match u {
            UsbmuxdError::ConnectionRefused => {
                FleetError::transient("usbmuxd refused the connection (device unplugged or still starting)")
            }
            other => FleetError::transient(format!("usbmuxd: {other}")),
        },
        E::PairingDialogResponsePending => {
            FleetError::needs_user("Unlock the device and tap Trust on the \"Trust This Computer?\" prompt")
        }
        E::PasswordProtected | E::DeviceLocked => FleetError::needs_user("Unlock the device with its passcode"),
        E::UserDeniedPairing => FleetError::needs_user(
            "Trust was declined on the device. Unplug it, plug it back in and tap Trust",
        ),
        E::InvalidHostID => FleetError::needs_user(
            "The device no longer trusts this computer. Pair it again",
        ),
        E::CanceledByUser => FleetError::cancelled(),
        E::Restore(r) => classify_restore(r),
        _ => FleetError::permanent(text),
    }
}

fn classify_restore(e: RestoreError) -> FleetError {
    use RestoreError as R;
    let text = e.to_string();
    match e {
        R::Cancelled => FleetError::cancelled(),
        R::Unsupported(_) => FleetError::permanent(text).with_fallback(),
        // USB/data-port trouble while the device reboots between stages.
        R::Recovery(_) | R::DataPortConnect(_) | R::RestoredCrashed => FleetError::transient(text),
        R::TssResponse(_) => FleetError::transient(format!("Apple's signing server: {text}")),
        R::BasebandRejected(_) => FleetError::permanent(text).with_fallback(),
        _ => FleetError::permanent(text),
    }
}

pub type Result<T, E = FleetError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_failures() {
        assert_eq!(FleetError::from(IdeviceError::Timeout).class, ErrorClass::Transient);
        assert_eq!(FleetError::from(IdeviceError::PairingDialogResponsePending).class, ErrorClass::NeedsUser);
        assert_eq!(FleetError::from(IdeviceError::CanceledByUser).class, ErrorClass::Cancelled);
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(FleetError::from(IdeviceError::Socket(io)).is_retryable());
        let unsupported = FleetError::from(IdeviceError::Restore(RestoreError::Unsupported("x".into())));
        assert_eq!(unsupported.class, ErrorClass::Permanent);
        assert!(unsupported.fallback_suggested);
        assert!(FleetError::from(IdeviceError::Restore(RestoreError::TssResponse("503".into()))).is_retryable());
    }
}
