//! Native backend built on the `idevice` crate: watches usbmuxd and USB, and
//! implements the device actions jobs run.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use idevice::lockdown::LockdownClient;
use idevice::provider::IdeviceProvider;
use idevice::restore::recovery::RecoveryDevice;
use idevice::usbmuxd::{Connection, UsbmuxdConnection, UsbmuxdDevice, UsbmuxdListenEvent};
use idevice::{IdeviceError, IdeviceService};
use plist::Value;
use tokio_util::sync::CancellationToken;

use crate::devices::{Health, NormalInfo, Registry};
use crate::error::{FleetError, Result};
use crate::backup::{self, FleetDelegate};
use crate::jobs::JobContext;
use idevice::services::mobilebackup2::MobileBackup2Client;
use crate::usb;

const LABEL: &str = "idevice-fleet";
const LOCKDOWN_TIMEOUT: Duration = Duration::from_secs(15);

async fn with_timeout<T>(what: &str, fut: impl Future<Output = std::result::Result<T, IdeviceError>>) -> Result<T> {
    match tokio::time::timeout(LOCKDOWN_TIMEOUT, fut).await {
        Ok(r) => r.map_err(|e| FleetError::from(e).context(what)),
        Err(_) => Err(FleetError::transient(format!("{what}: the device didn't answer within {}s", LOCKDOWN_TIMEOUT.as_secs()))),
    }
}

pub struct NativeBackend {
    pub registry: Arc<Registry>,
}

impl NativeBackend {
    pub fn new(registry: Arc<Registry>) -> Arc<Self> {
        Arc::new(Self { registry })
    }

    /// Start the background watchers. They restart themselves on failure and
    /// stop when `shutdown` is cancelled.
    pub fn start(self: &Arc<Self>, shutdown: CancellationToken) {
        // idevice 0.1.68's usbmuxd event stream isn't `Send`, so it gets its own
        // thread and single-threaded runtime. Info lookups go back to the main runtime.
        let me = self.clone();
        let stop = shutdown.clone();
        let main = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("usbmuxd-watch".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("usbmuxd watcher runtime");
                rt.block_on(me.watch_usbmuxd(stop, main));
            })
            .expect("spawn usbmuxd watcher thread");
        let me = self.clone();
        tokio::spawn(async move { me.watch_usb(shutdown).await });
    }

    async fn watch_usbmuxd(self: Arc<Self>, shutdown: CancellationToken, main: tokio::runtime::Handle) {
        let mut backoff = Duration::from_millis(500);
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            match UsbmuxdConnection::default().await {
                Err(e) => {
                    self.registry.set_usbmuxd_health(Health::Down(usbmuxd_down_message(&e)));
                }
                Ok(mut conn) => {
                    backoff = Duration::from_millis(500);
                    match conn.listen().await {
                        Err(e) => self.registry.set_usbmuxd_health(Health::Down(format!("usbmuxd: {e}"))),
                        Ok(mut events) => {
                            self.registry.set_usbmuxd_health(Health::Ok);
                            loop {
                                let ev = tokio::select! {
                                    ev = events.next() => ev,
                                    _ = shutdown.cancelled() => return,
                                };
                                match ev {
                                    Some(Ok(UsbmuxdListenEvent::Connected(dev))) => {
                                        if dev.connection_type == Connection::Usb {
                                            self.registry.normal_attached(&dev.udid, dev.device_id);
                                            let me = self.clone();
                                            main.spawn(async move { me.refresh_info(dev).await });
                                        }
                                    }
                                    Some(Ok(UsbmuxdListenEvent::Disconnected(id))) => self.registry.normal_detached(id),
                                    Some(Err(e)) => {
                                        tracing::warn!("usbmuxd event stream failed: {e}");
                                        break;
                                    }
                                    None => break,
                                }
                            }
                            self.registry.normal_clear();
                            self.registry.set_usbmuxd_health(Health::Down("Lost the connection to usbmuxd, reconnecting".into()));
                        }
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown.cancelled() => return,
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }

    async fn watch_usb(self: Arc<Self>, shutdown: CancellationToken) {
        let mut failures = 0u32;
        loop {
            match usb::list_recovery_devices().await {
                Ok(list) => {
                    failures = 0;
                    self.registry.set_usb_health(Health::Ok);
                    self.registry.set_recovery(list);
                }
                Err(e) => {
                    failures += 1;
                    self.registry.set_usb_health(Health::Down(e.message));
                    if failures >= 3 {
                        self.registry.set_recovery(Vec::new());
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(1500)) => {}
                _ = shutdown.cancelled() => return,
            }
        }
    }

    /// Read device info over lockdown, retrying while the device finishes booting.
    pub async fn refresh_info(&self, dev: UsbmuxdDevice) {
        let mut last = None;
        for attempt in 0..4u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(1500 * attempt)).await;
            }
            match read_info(&dev).await {
                Ok(info) => {
                    self.registry.normal_info(&dev.udid, info);
                    return;
                }
                Err(e) if e.is_retryable() => last = Some(e),
                Err(e) => {
                    last = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = last {
            self.registry.normal_problem(&dev.udid, e.message);
        }
    }

    async fn usbmux_device(&self, udid: &str) -> Result<UsbmuxdDevice> {
        let mut conn = UsbmuxdConnection::default().await.map_err(|e| FleetError::transient(usbmuxd_down_message(&e)))?;
        conn.get_device(udid).await.map_err(|_| FleetError::transient("The device isn't connected in normal mode"))
    }

    /// Pair (trust) the device, waiting for someone to tap Trust.
    pub async fn pair(&self, ctx: &JobContext, udid: &str) -> Result<()> {
        let dev = self.usbmux_device(udid).await?;
        let provider = dev.to_provider(idevice::usbmuxd::UsbmuxdAddr::default(), LABEL);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        loop {
            ctx.check_cancelled()?;
            let mut conn = UsbmuxdConnection::default().await.map_err(|e| FleetError::transient(usbmuxd_down_message(&e)))?;
            let buid = with_timeout("Reading this computer's ID from usbmuxd", conn.get_buid()).await?;
            let mut lockdown = with_timeout("Connecting to the device", LockdownClient::connect(&provider)).await?;
            let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();
            match tokio::time::timeout(Duration::from_secs(30), lockdown.pair(host_id, buid, Some("iDevice Fleet"))).await {
                Ok(Ok(mut record)) => {
                    with_timeout("Checking the new pairing", lockdown.start_session(&record)).await?;
                    record.udid = Some(udid.to_string());
                    let bytes = record.serialize().map_err(FleetError::from)?;
                    with_timeout("Saving the pairing with usbmuxd", conn.save_pair_record(udid, bytes)).await?;
                    ctx.stage("Paired");
                    self.refresh_info(dev).await;
                    return Ok(());
                }
                Ok(Err(e @ (IdeviceError::PairingDialogResponsePending | IdeviceError::PasswordProtected))) => {
                    let msg = FleetError::from(e).message;
                    if tokio::time::Instant::now() >= deadline {
                        return Err(FleetError::needs_user(format!("{msg} (waited 3 minutes)")));
                    }
                    ctx.stage(msg);
                    ctx.sleep(Duration::from_secs(2)).await?;
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => ctx.stage("Waiting for the device to answer"),
            }
        }
    }

    /// Full device backup into `backup_root/<udid>/`.
    pub async fn backup(&self, ctx: &JobContext, udid: &str, backup_root: &std::path::Path) -> Result<()> {
        let dev = self.usbmux_device(udid).await?;
        let provider = dev.to_provider(idevice::usbmuxd::UsbmuxdAddr::default(), LABEL);

        ctx.stage("Checking the device");
        let mut lockdown = with_timeout("Connecting to the device", LockdownClient::connect(&provider)).await?;
        let pairing = provider.get_pairing_file().await.map_err(|_| FleetError::needs_user("Pair the device first: unlock it and tap Trust"))?;
        with_timeout("Starting a session", lockdown.start_session(&pairing)).await?;

        // A first backup needs roughly as much space as the device uses.
        let capacity = disk_value(&mut lockdown, "TotalDataCapacity").await;
        let available = disk_value(&mut lockdown, "TotalDataAvailable").await;
        std::fs::create_dir_all(backup_root).map_err(|e| FleetError::permanent(format!("Can't create the backup folder: {e}")))?;
        if let (Some(cap), Some(avail), Some(free)) = (capacity, available, backup::free_space(backup_root)) {
            let used = cap.saturating_sub(avail);
            let existing = backup::list_backups(backup_root).iter().filter(|b| b.folder == udid).map(|b| b.size).sum::<u64>();
            let needed = used.saturating_sub(existing).saturating_add(512 << 20);
            ctx.log(format!("Device uses {:.1} GB, {:.1} GB free on this computer", used as f64 / 1e9, free as f64 / 1e9));
            if free < needed {
                return Err(FleetError::permanent(format!(
                    "Not enough disk space for the backup: about {:.1} GB needed, {:.1} GB free in {}",
                    needed as f64 / 1e9,
                    free as f64 / 1e9,
                    backup_root.display()
                )));
            }
        }
        drop(lockdown);

        ctx.stage("Starting the backup");
        let mut client = tokio::time::timeout(LOCKDOWN_TIMEOUT, MobileBackup2Client::connect(&provider))
            .await
            .map_err(|_| FleetError::transient("The backup service didn't answer"))?
            .map_err(|e| FleetError::from(e).context("Connecting to the backup service"))?;
        let delegate = FleetDelegate::new(ctx.clone());
        let cancel = ctx.cancellation();
        let result = tokio::select! {
            r = client.backup_from_path(backup_root, None, None, &delegate) => r,
            _ = cancel.cancelled() => {
                let _ = client.disconnect().await;
                return Err(FleetError::cancelled());
            }
        };
        let reply = result.map_err(|e| FleetError::from(e).context("Backup"))?;
        if let Some(d) = reply
            && let Some(code) = d.get("ErrorCode").and_then(|v| v.as_signed_integer())
            && code != 0
        {
            let text = d.get("ErrorDescription").and_then(|v| v.as_string()).unwrap_or("unknown error");
            return Err(match text {
                t if t.to_lowercase().contains("passcode") || t.to_lowercase().contains("lock") => FleetError::needs_user(format!("Unlock the device and try again ({t})")),
                t => FleetError::transient(format!("The device reported an error: {t} (code {code})")),
            });
        }
        let _ = client.disconnect().await;
        ctx.stage("Backup complete");
        Ok(())
    }

    pub async fn enter_recovery(&self, ctx: &JobContext, udid: &str, ecid: Option<u64>) -> Result<()> {
        let dev = self.usbmux_device(udid).await?;
        let provider = dev.to_provider(idevice::usbmuxd::UsbmuxdAddr::default(), LABEL);
        let mut lockdown = with_timeout("Connecting to the device", LockdownClient::connect(&provider)).await?;
        let pairing = with_timeout("Reading the pairing record", provider.get_pairing_file()).await
            .map_err(|e| FleetError::needs_user(format!("Pair the device first ({e})")))?;
        with_timeout("Starting a session", lockdown.start_session(&pairing)).await?;
        ctx.stage("Asking the device to restart into recovery mode");
        with_timeout("Entering recovery mode", lockdown.enter_recovery()).await?;
        let Some(ecid) = ecid else { return Ok(()) };
        ctx.stage("Waiting for the device to appear in recovery mode");
        self.wait_for(ctx, Duration::from_secs(90), || self.registry.is_in_recovery(ecid)).await
            .map_err(|_| FleetError::transient("The device didn't show up in recovery mode within 90 seconds"))
    }

    pub async fn exit_recovery(&self, ctx: &JobContext, ecid: u64) -> Result<()> {
        ctx.stage("Opening the device over USB");
        let transport = usb::open_recovery(ecid, Duration::from_secs(20)).await?;
        let mut recovery = RecoveryDevice::new(transport).await.map_err(FleetError::from)?;
        ctx.stage("Restarting into normal mode");
        recovery.set_autoboot(true).await.map_err(FleetError::from)?;
        // The device drops off the bus while rebooting, so the reply may never arrive.
        let _ = recovery.reboot().await;
        drop(recovery);
        ctx.stage("Waiting for the device to leave recovery mode");
        self.wait_for(ctx, Duration::from_secs(60), || !self.registry.is_in_recovery(ecid)).await
            .map_err(|_| FleetError::transient("The device is still in recovery mode. It may need a firmware restore"))
    }

    async fn wait_for(&self, ctx: &JobContext, limit: Duration, mut done: impl FnMut() -> bool) -> Result<()> {
        let deadline = tokio::time::Instant::now() + limit;
        while !done() {
            if tokio::time::Instant::now() >= deadline {
                return Err(FleetError::transient("timed out"));
            }
            ctx.heartbeat();
            ctx.sleep(Duration::from_millis(500)).await?;
        }
        Ok(())
    }
}

async fn disk_value(lockdown: &mut LockdownClient, key: &str) -> Option<u64> {
    lockdown.get_value(Some(key), Some("com.apple.disk_usage")).await.ok().and_then(|v| v.as_unsigned_integer())
}

fn usbmuxd_down_message(e: &IdeviceError) -> String {
    if matches!(e, IdeviceError::Socket(io) if matches!(io.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)) {
        if cfg!(target_os = "linux") {
            "usbmuxd isn't running. It starts when a device is plugged in (package: usbmuxd)".into()
        } else {
            "Apple Mobile Device Service isn't running. Install the Apple Devices app".into()
        }
    } else {
        format!("Can't reach usbmuxd: {e}")
    }
}

async fn read_info(dev: &UsbmuxdDevice) -> Result<NormalInfo> {
    let provider = dev.to_provider(idevice::usbmuxd::UsbmuxdAddr::default(), LABEL);
    let mut lockdown = with_timeout("Connecting to lockdown", LockdownClient::connect(&provider)).await?;
    let basic = with_timeout("Reading device info", lockdown.get_value(None, None)).await?;
    let mut info = parse_lockdown(&basic);

    // Full info and battery need a trusted session.
    let Ok(pairing) = provider.get_pairing_file().await else { return Ok(info) };
    match with_timeout("Starting a session", lockdown.start_session(&pairing)).await {
        Ok(_) => {
            if let Ok(full) = with_timeout("Reading device info", lockdown.get_value(None, None)).await {
                info = parse_lockdown(&full);
            }
            info.paired = true;
            if let Ok(v) = with_timeout("Reading battery", lockdown.get_value(Some("BatteryCurrentCapacity"), Some("com.apple.mobile.battery"))).await {
                info.battery_percent = v.as_unsigned_integer().map(|b| b.min(100) as u8);
            }
        }
        Err(e) if e.class == crate::ErrorClass::NeedsUser => {}
        Err(e) => return Err(e),
    }
    Ok(info)
}

pub fn parse_lockdown(v: &Value) -> NormalInfo {
    let d = v.as_dictionary();
    let s = |k: &str| d.and_then(|d| d.get(k)).and_then(Value::as_string).map(str::to_string);
    NormalInfo {
        ecid: d.and_then(|d| d.get("UniqueChipID")).and_then(Value::as_unsigned_integer),
        name: s("DeviceName"),
        product_type: s("ProductType"),
        os_version: s("ProductVersion"),
        build: s("BuildVersion"),
        serial: s("SerialNumber"),
        activation_state: s("ActivationState"),
        battery_percent: None,
        paired: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lockdown_values() {
        let mut d = plist::Dictionary::new();
        d.insert("UniqueChipID".into(), Value::Integer(0x1A2B.into()));
        d.insert("DeviceName".into(), Value::String("Front desk".into()));
        d.insert("ProductType".into(), Value::String("iPad13,18".into()));
        d.insert("ProductVersion".into(), Value::String("27.0.1".into()));
        let info = parse_lockdown(&Value::Dictionary(d));
        assert_eq!(info.ecid, Some(0x1A2B));
        assert_eq!(info.name.as_deref(), Some("Front desk"));
        assert_eq!(info.serial, None);
    }
}
