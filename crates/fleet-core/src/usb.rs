//! Raw USB access to devices in recovery and DFU mode, via `nusb`.
//!
//! The transport is adapted from the `restore_usb` example in the idevice
//! repository (https://github.com/jkcoxson/idevice, MIT licensed).

use std::time::Duration;

use idevice::IdeviceError;
use idevice::restore::RestoreError;
use idevice::restore::recovery::{ControlSetup, DeviceInfo, Mode, RecoveryFuture, RecoveryTransport};
use nusb::transfer::{Buffer, Bulk, ControlIn, ControlOut, ControlType, Out, Recipient};

use crate::devices::RecoveryInfo;
use crate::error::{FleetError, Result};

pub const APPLE_VENDOR_ID: u16 = 0x05AC;

fn usb_err<E: std::fmt::Display>(e: E) -> IdeviceError {
    IdeviceError::Restore(RestoreError::Recovery(format!("usb: {e}")))
}

/// Apple devices currently in recovery or DFU mode. Only reads descriptors
/// the OS already has; it never opens a device, so it can't disturb a running
/// restore.
pub async fn list_recovery_devices() -> Result<Vec<RecoveryInfo>> {
    let devices = nusb::list_devices().await.map_err(|e| FleetError::transient(format!("USB scan failed: {e}")))?;
    let mut out = Vec::new();
    for d in devices {
        if d.vendor_id() != APPLE_VENDOR_ID {
            continue;
        }
        let Some(mode) = Mode::from_product_id(d.product_id()) else { continue };
        let info = DeviceInfo::parse(d.serial_number().unwrap_or_default());
        let Some(ecid) = info.ecid else { continue };
        out.push(RecoveryInfo { ecid, dfu: !mode.is_recovery(), cpid: info.cpid, bdid: info.bdid, serial: info.srnm.clone() });
    }
    Ok(out)
}

/// Open the recovery/DFU device with this ECID, waiting up to `timeout` for it
/// to show up (it may still be re-enumerating).
pub async fn open_recovery(ecid: u64, timeout: Duration) -> Result<Box<dyn RecoveryTransport>> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_err = None;
    loop {
        if let Ok(devices) = nusb::list_devices().await {
            for info in devices {
                if info.vendor_id() != APPLE_VENDOR_ID || Mode::from_product_id(info.product_id()).is_none() {
                    continue;
                }
                let serial = info.serial_number().unwrap_or_default().to_string();
                if DeviceInfo::parse(&serial).ecid != Some(ecid) {
                    continue;
                }
                // Right after re-enumeration the OS may still hold the device.
                match info.open().await {
                    Ok(device) => {
                        return Ok(Box::new(NusbRecoveryTransport { product_id: info.product_id(), serial, device, interface: None }));
                    }
                    Err(e) => last_err = Some(e.to_string()),
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(match last_err {
                Some(e) => FleetError::transient(format!("Couldn't open the device over USB: {e}. On Linux, check the udev rules (see README)")),
                None => FleetError::transient("The device didn't appear in recovery or DFU mode"),
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub struct NusbRecoveryTransport {
    device: nusb::Device,
    interface: Option<nusb::Interface>,
    product_id: u16,
    serial: String,
}

impl std::fmt::Debug for NusbRecoveryTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NusbRecoveryTransport").field("product_id", &format_args!("{:#06x}", self.product_id)).finish_non_exhaustive()
    }
}

fn decode_request_type(rt: u8) -> (ControlType, Recipient) {
    let control_type = match (rt >> 5) & 0b11 {
        0 => ControlType::Standard,
        1 => ControlType::Class,
        _ => ControlType::Vendor,
    };
    let recipient = match rt & 0x1F {
        1 => Recipient::Interface,
        2 => Recipient::Endpoint,
        _ => Recipient::Device,
    };
    (control_type, recipient)
}

impl RecoveryTransport for NusbRecoveryTransport {
    fn control_out<'a>(&'a mut self, setup: ControlSetup, data: &'a [u8], timeout_ms: u32) -> RecoveryFuture<'a, usize> {
        let device = self.device.clone();
        let data = data.to_vec();
        Box::pin(async move {
            let (control_type, recipient) = decode_request_type(setup.request_type);
            device
                .control_out(
                    ControlOut { control_type, recipient, request: setup.request, value: setup.value, index: setup.index, data: &data },
                    Duration::from_millis(timeout_ms as u64),
                )
                .await
                .map_err(usb_err)?;
            Ok(data.len())
        })
    }

    fn control_in<'a>(&'a mut self, setup: ControlSetup, length: u16, timeout_ms: u32) -> RecoveryFuture<'a, Vec<u8>> {
        let device = self.device.clone();
        Box::pin(async move {
            let (control_type, recipient) = decode_request_type(setup.request_type);
            device
                .control_in(
                    ControlIn { control_type, recipient, request: setup.request, value: setup.value, index: setup.index, length },
                    Duration::from_millis(timeout_ms as u64),
                )
                .await
                .map_err(usb_err)
        })
    }

    fn bulk_out<'a>(&'a mut self, endpoint: u8, data: &'a [u8], _timeout_ms: u32) -> RecoveryFuture<'a, usize> {
        let interface = self.interface.clone();
        let data = data.to_vec();
        Box::pin(async move {
            let interface = interface.ok_or_else(|| usb_err("no claimed interface for bulk transfer"))?;
            let mut ep = interface.endpoint::<Bulk, Out>(endpoint).map_err(usb_err)?;
            let len = data.len();
            ep.submit(Buffer::from(data));
            ep.next_complete().await.status.map_err(usb_err)?;
            Ok(len)
        })
    }

    fn serial_number(&mut self) -> RecoveryFuture<'_, String> {
        let serial = self.serial.clone();
        Box::pin(async move { Ok(serial) })
    }

    fn product_id(&self) -> u16 {
        self.product_id
    }

    fn set_configuration(&mut self, configuration: u8) -> RecoveryFuture<'_, ()> {
        let device = self.device.clone();
        Box::pin(async move { device.set_configuration(configuration).await.map_err(usb_err) })
    }

    fn claim_interface(&mut self, interface: u8, alt_setting: u8) -> RecoveryFuture<'_, ()> {
        Box::pin(async move {
            let iface = self.device.claim_interface(interface).await.map_err(usb_err)?;
            if alt_setting != 0 {
                iface.set_alt_setting(alt_setting).await.map_err(usb_err)?;
            }
            self.interface = Some(iface);
            Ok(())
        })
    }

    fn reset(&mut self) -> RecoveryFuture<'_, ()> {
        let device = self.device.clone();
        Box::pin(async move { device.reset().await.map_err(usb_err) })
    }
}
