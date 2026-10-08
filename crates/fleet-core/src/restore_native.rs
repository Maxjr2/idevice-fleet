//! The pure-Rust restore, built on the `idevice` crate. The flow follows the
//! `restore` example in https://github.com/jkcoxson/idevice (MIT licensed):
//! get the device into recovery mode, ask Apple to sign a ticket for it, boot the
//! restore ramdisk, then let the device-driven restore run while we feed it data.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use idevice::IdeviceError;
use idevice::lockdown::LockdownClient;
use idevice::preboard_service::{PreboardServiceClient, StashbagOutcome};
use idevice::provider::IdeviceProvider;
use idevice::restore::recovery::RecoveryDevice;
use idevice::restore::restored::RestoredClient;
use idevice::restore::state_machine::DataPortConnector;
use idevice::restore::{
    FdrClient, FdrConnector, RestoreCancel, RestoreContext, RestoreError, RestoreOptions, RestoreProgressEvent, img4, ipsw::Ipsw, progress_channel, run_restore,
};
use idevice::tss::{TSSRequest, extract_img4_ticket, select_build_identity};
use idevice::usbmuxd::{Connection, UsbmuxdAddr};
use idevice::{IdeviceService, restore::fdr::FDR_CTRL_PORT};
use tokio::io::BufReader;

use crate::error::{FleetError, Result};
use crate::jobs::JobContext;
use crate::native::NativeBackend;
use crate::restore::RestoreParams;
use crate::usb;

type Fs = BufReader<tokio::fs::File>;

fn unsupported(msg: impl Into<String>) -> FleetError {
    FleetError::permanent(msg).with_fallback()
}

fn restore_err(e: impl Into<IdeviceError>, what: &str) -> FleetError {
    FleetError::from(e.into()).context(what)
}

pub async fn run(backend: &NativeBackend, ctx: &JobContext, p: &RestoreParams) -> Result<()> {
    ctx.check_cancelled()?;
    let behavior = if p.erase { "Erase" } else { "Update" };

    // 1. Make sure the firmware can be used by this engine at all.
    ctx.stage("Reading the firmware file");
    let mut ipsw = open_ipsw(&p.ipsw).await?;
    let manifest = ipsw.build_manifest().await.map_err(|e| restore_err(e, "Reading BuildManifest"))?;

    // Decide from the file alone, before touching the device at all.
    if manifest_uses_aea(&manifest) {
        return Err(unsupported("This firmware's system image is encrypted (AEA)"));
    }

    // An update that keeps user data needs a "stashbag" made while the device still boots.
    if !p.erase
        && !backend.registry.is_in_recovery(p.ecid)
        && let Some(udid) = &p.udid
    {
        stashbag(backend, ctx, udid, &manifest).await?;
    }

    // 2. Get a device in recovery/DFU mode (entering it from normal mode if needed).
    let acquired = acquire(backend, ctx, p).await?;
    let recovery = acquired.recovery;
    let info = recovery.info().clone();
    let (Some(board_id), Some(chip_id)) = (info.bdid, info.cpid) else {
        return Err(FleetError::transient("The device in recovery mode didn't report its chip ID. Reconnect it and try again"));
    };
    let ecid = info.ecid.unwrap_or(p.ecid);
    if ecid != p.ecid {
        return Err(FleetError::permanent("A different device is in recovery mode than the one chosen. Disconnect the others"));
    }
    let ap_nonce = acquired.ap_nonce.or_else(|| info.ap_nonce.clone());
    let sep_nonce = acquired.sep_nonce.or_else(|| info.sep_nonce.clone());
    ctx.log(format!("Recovery device: board {board_id:#x}, chip {chip_id:#x}, ECID {ecid:#x}"));

    let build_identity = select_build_identity(&manifest, board_id, chip_id, Some(behavior))
        .map_err(|e| FleetError::permanent(format!("This firmware has no {behavior} variant for this device: {e}")))?
        .clone();

    // 3. The system image. Newer firmware encrypts it (AEA); the native engine can't read that.
    let os_path = idevice::restore::ipsw::component_path(&build_identity, "OS").map_err(|_| FleetError::permanent("The firmware has no system image"))?;
    if os_path.to_lowercase().ends_with(".aea") {
        return Err(unsupported("This firmware's system image is encrypted (AEA)"));
    }
    ctx.stage("Unpacking the system image");
    let fs_path = p.cache_dir.join("system.dmg");
    extract_with_progress(ctx, &mut ipsw, &os_path, &fs_path).await?;
    let mut fs_file = tokio::fs::File::open(&fs_path).await.map_err(|e| FleetError::permanent(format!("Can't open the unpacked system image: {e}")))?;

    // 4. Ask Apple to sign a ticket for exactly this device and firmware.
    ctx.check_cancelled()?;
    ctx.stage("Asking Apple to approve the restore");
    ctx.progress(9.0);
    let ticket = request_ticket(&build_identity, board_id, chip_id, ecid, ap_nonce, sep_nonce).await?;
    ctx.log(format!("Apple signed the restore ({} byte ticket)", ticket.len()));

    // 5. Boot the restore ramdisk.
    ctx.stage("Starting the restore on the device");
    ctx.progress(10.0);
    boot_to_restore(ctx, recovery, &mut ipsw, &build_identity, &ticket, ecid).await?;

    // 6. The device is now in restore mode and drives the rest; we answer its requests.
    ctx.check_cancelled()?;
    let addr = UsbmuxdAddr::default();
    let mut restored = RestoredClient::connect_by_ecid(&addr, ecid, "idevice-fleet", Duration::from_secs(90))
        .await
        .map_err(|e| restore_err(e, "Connecting to the device's restore mode"))?;
    let device_id = restored.device_id;
    let connector = Arc::new(UsbmuxFdrConnector { addr: addr.clone(), device_id });
    match start_fdr(connector.clone()).await {
        Ok(()) => ctx.log("Device trust channel started"),
        Err(e) => ctx.log(format!("Device trust channel didn't start ({e}); continuing")),
    }
    let mut data_ports = UsbmuxDataPorts { addr, device_id };

    let (progress_tx, mut progress_rx) = progress_channel();
    let progress = Arc::new(Mutex::new(10.0f32));
    let ctx2 = ctx.clone();
    let prog = progress.clone();
    let reporter = tokio::spawn(async move {
        while let Some(ev) = progress_rx.recv().await {
            report(&ctx2, &prog, ev);
        }
    });

    let cancel = RestoreCancel::new();
    let cancel_watch = {
        let (c, token) = (cancel.clone(), ctx.cancellation());
        tokio::spawn(async move {
            token.cancelled().await;
            c.cancel();
        })
    };

    let restore_ctx = RestoreContext {
        restored: &mut restored,
        build_identity: &build_identity,
        board_id,
        chip_id,
        ecid,
        tss_ticket: &ticket,
        components: &mut ipsw,
        filesystem: Some(&mut fs_file as &mut dyn idevice::restore::FilesystemImage),
        data_ports: &mut data_ports,
        progress: Some(progress_tx),
        cancel: Some(cancel),
    };
    let result = run_restore(restore_ctx, RestoreOptions::new().build()).await;
    cancel_watch.abort();
    let _ = reporter.await;
    match result {
        Ok(()) => {
            ctx.progress(100.0);
            ctx.stage("Restore finished: the device is restarting");
            Ok(())
        }
        Err(IdeviceError::Restore(RestoreError::Cancelled)) => Err(FleetError::cancelled()),
        Err(e) => Err(FleetError::from(e).context("Restore")),
    }
}

fn report(ctx: &JobContext, progress: &Mutex<f32>, ev: RestoreProgressEvent) {
    ctx.heartbeat();
    let set = |pct: f32| {
        let mut cur = progress.lock().unwrap_or_else(|p| p.into_inner());
        if pct > *cur {
            *cur = pct;
            ctx.progress(pct);
        }
    };
    match ev {
        RestoreProgressEvent::Step(name) => ctx.stage(name),
        RestoreProgressEvent::Operation { operation, progress: p } => {
            ctx.stage_quiet(format!("Restoring (step {operation})"));
            // The device reports 0-100 for each phase; spread them over 10-99%.
            set(10.0 + (p.min(100) as f32) * 0.89);
        }
        RestoreProgressEvent::Transfer { component, sent, total } => {
            ctx.stage_quiet(format!("Sending {component} ({} MB)", sent / 1_000_000));
            if let Some(t) = total.filter(|t| *t > 0) {
                set(10.0 + (sent as f32 / t as f32) * 20.0);
            }
        }
    }
}

async fn open_ipsw(path: &std::path::Path) -> Result<Ipsw<Fs>> {
    let file = tokio::fs::File::open(path).await.map_err(|e| FleetError::permanent(format!("Can't open {}: {e}", path.display())))?;
    Ipsw::new(BufReader::new(file)).await.map_err(|e| FleetError::permanent(format!("The firmware file is damaged or incomplete: {e}")))
}

/// Unpack the system image into the job's scratch folder, reporting progress by file size.
async fn extract_with_progress(ctx: &JobContext, ipsw: &mut Ipsw<Fs>, name: &str, dest: &std::path::Path) -> Result<()> {
    let mut out = tokio::fs::File::create(dest).await.map_err(|e| FleetError::permanent(format!("Can't write {}: {e}", dest.display())))?;
    let watch = {
        let (ctx, dest) = (ctx.clone(), dest.to_path_buf());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(700)).await;
                if let Ok(m) = tokio::fs::metadata(&dest).await {
                    ctx.heartbeat();
                    ctx.stage_quiet(format!("Unpacking the system image ({:.1} GB)", m.len() as f64 / 1e9));
                }
            }
        })
    };
    let r = ipsw.extract_to_writer(name, &mut out).await;
    watch.abort();
    r.map_err(|e| {
        let text = e.to_string();
        if text.to_lowercase().contains("no space") { FleetError::permanent("The disk is full while unpacking the firmware") } else { restore_err(e, "Unpacking the system image") }
    })?;
    ctx.progress(8.0);
    Ok(())
}

async fn request_ticket(build_identity: &plist::Dictionary, board_id: u64, chip_id: u64, ecid: u64, ap_nonce: Option<Vec<u8>>, sep_nonce: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let mut params = plist::Dictionary::new();
    for k in ["ApProductionMode", "ApSecurityMode", "ApSupportsImg4"] {
        params.insert(k.into(), true.into());
    }
    let mut request = TSSRequest::new();
    request.set_ap_img4_ticket(true);
    request.add_common_tags(board_id, chip_id, ecid, ap_nonce, sep_nonce.or(Some(vec![0u8; 20])));
    request.add_ap_tags(build_identity);
    request.add_ap_manifest_tags(build_identity, &params).map_err(|e| restore_err(e, "Preparing the signing request"))?;
    let response = request.send().await.map_err(|e| {
        let t = e.to_string();
        if t.contains("94") || t.to_lowercase().contains("not eligible") { FleetError::permanent("Apple no longer signs this firmware version. Choose a newer one") } else { restore_err(e, "Apple's signing server") }
    })?;
    let plist::Value::Dictionary(d) = response else {
        return Err(FleetError::transient("Apple's signing server sent an unreadable answer"));
    };
    extract_img4_ticket(&d).map_err(|e| FleetError::permanent(format!("Apple's answer has no ticket (the version may no longer be signed): {e}")))
}

struct Acquired {
    recovery: RecoveryDevice,
    ap_nonce: Option<Vec<u8>>,
    sep_nonce: Option<Vec<u8>>,
}

async fn acquire(backend: &NativeBackend, ctx: &JobContext, p: &RestoreParams) -> Result<Acquired> {
    // Already in recovery or DFU mode?
    if backend.registry.is_in_recovery(p.ecid) {
        ctx.stage("Opening the device in recovery mode");
        let t = usb::open_recovery(p.ecid, Duration::from_secs(20)).await?;
        let recovery = RecoveryDevice::new(t).await.map_err(|e| restore_err(e, "Talking to the device in recovery mode"))?;
        return Ok(Acquired { recovery, ap_nonce: None, sep_nonce: None });
    }
    // Otherwise it must be booted: put it into recovery mode first.
    let udid = p.udid.clone().ok_or_else(|| FleetError::transient("The device isn't visible. Check the cable, or put it into recovery mode by hand"))?;
    ctx.stage("Putting the device into recovery mode");
    let dev = backend.usbmux_device_pub(&udid).await?;
    let provider = dev.to_provider(UsbmuxdAddr::default(), "idevice-fleet");
    let mut lockdown = LockdownClient::connect(&provider).await.map_err(|e| restore_err(e, "Connecting to the device"))?;
    let pairing = provider.get_pairing_file().await.map_err(|_| FleetError::needs_user("The device isn't trusted. Trust it, or put it into recovery mode by hand"))?;
    lockdown.start_session(&pairing).await.map_err(|e| restore_err(e, "Starting a session"))?;
    // Nonces can only be read from normal mode and are needed for a personalised ticket.
    let ap_nonce = read_nonce(&mut lockdown, "ApNonce").await;
    let sep_nonce = read_nonce(&mut lockdown, "SEPNonce").await;
    lockdown.enter_recovery().await.map_err(|e| restore_err(e, "Entering recovery mode"))?;
    ctx.stage("Waiting for the device to restart into recovery mode");
    let t = usb::open_recovery(p.ecid, Duration::from_secs(90)).await?;
    let recovery = RecoveryDevice::new(t).await.map_err(|e| restore_err(e, "Talking to the device in recovery mode"))?;
    Ok(Acquired { recovery, ap_nonce, sep_nonce })
}

/// Secure-in-Data-Protection devices keep their data across an update only if a
/// stashbag is committed first (this may prompt for the passcode on the device).
async fn stashbag(backend: &NativeBackend, ctx: &JobContext, udid: &str, manifest: &plist::Dictionary) -> Result<()> {
    let dev = backend.usbmux_device_pub(udid).await?;
    let provider = dev.to_provider(UsbmuxdAddr::default(), "idevice-fleet");
    let mut lockdown = LockdownClient::connect(&provider).await.map_err(|e| restore_err(e, "Connecting to the device"))?;
    let pairing = provider.get_pairing_file().await.map_err(|_| FleetError::needs_user("The device isn't trusted: an update that keeps data needs it trusted"))?;
    lockdown.start_session(&pairing).await.map_err(|e| restore_err(e, "Starting a session"))?;
    let has_sidp = lockdown.get_value(Some("HasSiDP"), None).await.ok().and_then(|v| v.as_boolean()).unwrap_or(false);
    if !has_sidp {
        return Ok(());
    }
    ctx.stage("Protecting the user data for the update (you may need to enter the passcode on the device)");
    let get = |v: Option<plist::Value>, what: &str| v.and_then(|v| v.as_unsigned_integer()).ok_or_else(|| FleetError::transient(format!("The device didn't report {what}")));
    let board_id = get(lockdown.get_value(Some("BoardId"), None).await.ok(), "BoardId")?;
    let chip_id = get(lockdown.get_value(Some("ChipID"), None).await.ok(), "ChipID")?;
    let ecid = get(lockdown.get_value(Some("UniqueChipID"), None).await.ok(), "its ECID")?;
    let ap_nonce = read_nonce(&mut lockdown, "ApNonce").await;
    let sep_nonce = read_nonce(&mut lockdown, "SEPNonce").await;
    drop(lockdown);
    let bi = select_build_identity(manifest, board_id, chip_id, Some("Update")).map_err(|e| FleetError::permanent(format!("This firmware has no Update variant for this device: {e}")))?.clone();
    let boot_manifest = img4::build_preboard_manifest(&bi, board_id, chip_id).map_err(|e| restore_err(e, "Preparing the data protection manifest"))?;
    let mut preboard = PreboardServiceClient::connect(&provider).await.map_err(|e| restore_err(e, "Connecting to the preboard service"))?;
    match preboard.create_stashbag(&boot_manifest).await.map_err(|e| restore_err(e, "Creating the stashbag"))? {
        StashbagOutcome::NotRequired => return Ok(()),
        StashbagOutcome::CommitRequired => {}
    }
    drop(preboard);
    let ticket = request_ticket(&bi, board_id, chip_id, ecid, ap_nonce, sep_nonce).await?;
    let mut preboard = PreboardServiceClient::connect(&provider).await.map_err(|e| restore_err(e, "Connecting to the preboard service"))?;
    preboard.commit_stashbag(&ticket).await.map_err(|e| restore_err(e, "Committing the stashbag"))?;
    ctx.log("User data protected for the update");
    Ok(())
}

async fn read_nonce(lockdown: &mut LockdownClient, key: &str) -> Option<Vec<u8>> {
    lockdown.get_value(Some(key), None).await.ok().and_then(|v| v.as_data().map(|d| d.to_vec()))
}

async fn send_component(ctx: &JobContext, recovery: &mut RecoveryDevice, ipsw: &mut Ipsw<Fs>, bi: &plist::Dictionary, ticket: &[u8], name: &str) -> Result<()> {
    ctx.check_cancelled()?;
    ctx.heartbeat();
    let raw = ipsw.read_component(bi, name).await.map_err(|e| restore_err(e, &format!("Reading {name}")))?;
    let personalised = img4::stitch_component(&raw, ticket, img4::restore_fourcc_override(name), &[]).map_err(|e| restore_err(e, &format!("Preparing {name}")))?;
    ctx.log(format!("Sending {name} ({} KB)", personalised.len() / 1000));
    recovery.send_buffer(&personalised).await.map_err(|e| restore_err(e, &format!("Sending {name}")))
}

/// True if any build identity's system image is an AEA-encrypted file.
pub fn manifest_uses_aea(manifest: &plist::Dictionary) -> bool {
    manifest
        .get("BuildIdentities")
        .and_then(|b| b.as_array())
        .into_iter()
        .flatten()
        .filter_map(|bi| bi.as_dictionary())
        .filter_map(|bi| bi.get("Manifest")?.as_dictionary()?.get("OS")?.as_dictionary()?.get("Info")?.as_dictionary()?.get("Path")?.as_string())
        .any(|path| path.to_lowercase().ends_with(".aea"))
}

fn has_component(bi: &plist::Dictionary, name: &str) -> bool {
    bi.get("Manifest").and_then(|m| m.as_dictionary()).is_some_and(|m| m.contains_key(name))
}

async fn boot_to_restore(ctx: &JobContext, mut recovery: RecoveryDevice, ipsw: &mut Ipsw<Fs>, bi: &plist::Dictionary, ticket: &[u8], ecid: u64) -> Result<()> {
    if recovery.mode().is_recovery() {
        send_component(ctx, &mut recovery, ipsw, bi, ticket, "iBEC").await?;
        let _ = recovery.send_command_with_request("go", 1).await;
        let _ = recovery.finish_transfer().await;
        drop(recovery);
        ctx.sleep(Duration::from_secs(3)).await?;
        ctx.stage("Waiting for the device's bootloader");
        let t = usb::open_recovery(ecid, Duration::from_secs(40)).await?;
        recovery = RecoveryDevice::new(t).await.map_err(|e| restore_err(e, "Reconnecting after the bootloader"))?;
    }
    for _ in 0..30 {
        ctx.check_cancelled()?;
        if recovery.getenv("build-version").await.is_ok_and(|b| !b.is_empty()) {
            break;
        }
        ctx.sleep(Duration::from_secs(1)).await?;
    }
    let e = |what: &'static str| move |err: IdeviceError| restore_err(err, what);
    recovery.set_autoboot(false).await.map_err(e("Configuring the bootloader"))?;
    if send_component(ctx, &mut recovery, ipsw, bi, ticket, "RestoreLogo").await.is_ok() {
        let _ = recovery.send_command("setpicture 4").await;
        let _ = recovery.send_command("bgcolor 0 0 0").await;
    }
    for name in idevice::restore::ipsw::components_loaded_by_iboot(bi) {
        send_component(ctx, &mut recovery, ipsw, bi, ticket, &name).await?;
        recovery.send_command("firmware").await.map_err(e("Loading firmware"))?;
    }
    send_component(ctx, &mut recovery, ipsw, bi, ticket, "RestoreRamDisk").await?;
    recovery.send_command("ramdisk").await.map_err(e("Loading the restore system"))?;
    ctx.sleep(Duration::from_secs(2)).await?;
    send_component(ctx, &mut recovery, ipsw, bi, ticket, "RestoreDeviceTree").await?;
    recovery.send_command("devicetree").await.map_err(e("Loading the device tree"))?;
    if has_component(bi, "RestoreSEP") && send_component(ctx, &mut recovery, ipsw, bi, ticket, "RestoreSEP").await.is_ok() {
        let _ = recovery.send_command("rsepfirmware").await;
    }
    send_component(ctx, &mut recovery, ipsw, bi, ticket, "RestoreKernelCache").await?;
    let _ = recovery.finish_transfer().await;
    recovery.send_command("setenv boot-args rd=md0 nand-enable-reformat=1 -progress").await.map_err(e("Setting boot options"))?;
    let _ = recovery.send_command_with_request("bootx", 1).await;
    Ok(())
}

async fn start_fdr(connector: Arc<UsbmuxFdrConnector>) -> std::result::Result<(), IdeviceError> {
    let mut last = None;
    for attempt in 0..5 {
        match connector.connect_device_port(FDR_CTRL_PORT).await {
            Ok(ctrl) => {
                let mut fdr = FdrClient::new(ctrl);
                match fdr.ctrl_handshake().await {
                    Ok(port) => {
                        tokio::spawn(idevice::restore::run_fdr_listener(fdr, connector, port));
                        return Ok(());
                    }
                    Err(e) => last = Some(e),
                }
            }
            Err(e) => last = Some(e),
        }
        if attempt < 4 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    Err(last.unwrap_or_else(|| IdeviceError::Restore(RestoreError::Other("FDR failed to start".into()))))
}

async fn connect_usb_device_port(addr: &UsbmuxdAddr, device_id: Option<u32>, port: u16, label: &str) -> std::result::Result<idevice::Idevice, IdeviceError> {
    let device_id = match device_id {
        Some(id) => id,
        None => {
            let mut mux = addr.connect(1).await?;
            mux.get_devices().await?.into_iter().find(|d| d.connection_type == Connection::Usb).ok_or(IdeviceError::DeviceNotFound)?.device_id
        }
    };
    let mux = addr.connect(1).await?;
    mux.connect_to_device(device_id, port, label).await
}

type DevFut = std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<idevice::Idevice, IdeviceError>> + Send>>;

struct UsbmuxDataPorts {
    addr: UsbmuxdAddr,
    device_id: Option<u32>,
}

impl DataPortConnector for UsbmuxDataPorts {
    fn connect(&self, port: u16) -> DevFut {
        let (addr, id) = (self.addr.clone(), self.device_id);
        Box::pin(async move { connect_usb_device_port(&addr, id, port, "idevice-fleet-data").await })
    }
}

struct UsbmuxFdrConnector {
    addr: UsbmuxdAddr,
    device_id: Option<u32>,
}

impl FdrConnector for UsbmuxFdrConnector {
    fn connect_device_port(&self, port: u16) -> DevFut {
        let (addr, id) = (self.addr.clone(), self.device_id);
        Box::pin(async move { connect_usb_device_port(&addr, id, port, "idevice-fleet-fdr").await })
    }
}

