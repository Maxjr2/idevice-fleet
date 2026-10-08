//! Firmware restore ("reset and reinstall"): engines, pre-flight checks and cleanup.
//!
//! Two engines do the actual work:
//! * **Native**: the pure-Rust restore in the `idevice` crate (see `restore_native`).
//! * **idevicerestore**: the well-tested C tool, supervised as a child process.
//!
//! `Auto` runs the native engine and falls back to `idevicerestore` for cases
//! the native one can't handle (for example AEA-encrypted system images).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::error::{ErrorClass, FleetError, Result};
use crate::firmware::LocalIpsw;
use crate::jobs::JobContext;
use crate::model::{Device, DeviceMode, PairState};
use crate::native::NativeBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Engine {
    /// Native first; `idevicerestore` for what native can't do.
    Auto,
    Native,
    IdeviceRestore,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Auto => "Automatic",
            Engine::Native => "Built-in (Rust)",
            Engine::IdeviceRestore => "idevicerestore",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreParams {
    pub ecid: u64,
    /// Normal-mode UDID if the device was booted when the job was created.
    pub udid: Option<String>,
    pub ipsw: PathBuf,
    /// Erase everything (a reset) instead of updating in place.
    pub erase: bool,
    pub engine: Engine,
    /// Scratch space for this job (extracted files). Deleted when the job ends.
    pub cache_dir: PathBuf,
}

/// Run a restore end to end, then always remove the job's scratch folder.
pub async fn run(backend: &NativeBackend, ctx: &JobContext, p: &RestoreParams, external: Option<&Path>) -> Result<()> {
    std::fs::create_dir_all(&p.cache_dir).map_err(|e| FleetError::permanent(format!("Can't create {}: {e}", p.cache_dir.display())))?;
    let result = run_engines(backend, ctx, p, external).await;
    if let Err(e) = std::fs::remove_dir_all(&p.cache_dir) {
        tracing::warn!("could not remove {}: {e}", p.cache_dir.display());
    }
    result
}

async fn run_engines(backend: &NativeBackend, ctx: &JobContext, p: &RestoreParams, external: Option<&Path>) -> Result<()> {
    match p.engine {
        Engine::Native => crate::restore_native::run(backend, ctx, p).await,
        Engine::IdeviceRestore => match external {
            Some(tool) => run_external(ctx, p, tool).await,
            None => Err(FleetError::permanent("idevicerestore isn't installed. Install it (sudo apt install idevicerestore) or choose the built-in engine")),
        },
        Engine::Auto => match crate::restore_native::run(backend, ctx, p).await {
            Err(e) if e.fallback_suggested => match external {
                Some(tool) => {
                    ctx.log(format!("The built-in engine can't do this one ({}). Switching to idevicerestore.", e.message));
                    ctx.stage("Switching to idevicerestore");
                    // The failed attempt may have left the device rebooting; give it time to settle.
                    ctx.sleep(Duration::from_secs(5)).await?;
                    run_external(ctx, p, tool).await
                }
                None => Err(FleetError { message: format!("{}. Installing idevicerestore (sudo apt install idevicerestore) would let this work", e.message), ..e }),
            },
            other => other,
        },
    }
}

// ---- idevicerestore as a supervised child process -----------------------------------

/// Parse `idevicerestore -P` output: `progress: <step> <fraction>`.
pub fn parse_progress_line(line: &str) -> Option<(&'static str, f32)> {
    const STEPS: [&str; 8] = ["Detecting the device", "Preparing", "Sending the system image", "Verifying the system image", "Flashing firmware", "Flashing the baseband", "Updating firmware", "Sending images"];
    let rest = line.strip_prefix("progress:")?;
    let mut it = rest.split_whitespace();
    let step: usize = it.next()?.parse().ok()?;
    let frac: f32 = it.next()?.parse().ok()?;
    Some((STEPS.get(step).copied().unwrap_or("Restoring"), frac.clamp(0.0, 1.0)))
}

pub async fn run_external(ctx: &JobContext, p: &RestoreParams, tool: &Path) -> Result<()> {
    let log_file = p.cache_dir.join("idevicerestore.log");
    let mut cmd = tokio::process::Command::new(tool);
    cmd.arg("-i").arg(format!("0x{:x}", p.ecid)).arg("-y").arg("-P").arg("-C").arg(&p.cache_dir).arg(format!("--logfile={}", log_file.display()));
    if p.erase {
        cmd.arg("-e");
    }
    cmd.arg(&p.ipsw).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    ctx.log(format!("$ {} -i 0x{:x} {}{}", tool.display(), p.ecid, if p.erase { "-e " } else { "" }, p.ipsw.display()));
    ctx.stage("Starting idevicerestore");
    let mut child = cmd.spawn().map_err(|e| FleetError::permanent(format!("Couldn't start idevicerestore: {e}")))?;
    let stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");

    // stderr carries the tool's log; keep the tail for error messages.
    let tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let err_ctx = ctx.clone();
    let err_tail = tail.clone();
    let err_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut chunk = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
            while let Some(i) = buf.find(['\n', '\r']) {
                let line: String = buf.drain(..=i).collect();
                let line = line.trim();
                if !line.is_empty() {
                    err_ctx.heartbeat();
                    err_ctx.log(line);
                    let mut t = err_tail.lock().unwrap_or_else(|p| p.into_inner());
                    t.push(line.to_string());
                    if t.len() > 30 {
                        t.remove(0);
                    }
                }
            }
        }
    });

    let mut lines = BufReader::new(stdout).lines();
    let cancel = ctx.cancellation();
    let mut cancelled = false;
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    ctx.heartbeat();
                    match parse_progress_line(&line) {
                        Some((stage, frac)) => {
                            ctx.stage_quiet(stage);
                            ctx.progress(frac * 100.0);
                        }
                        None if !line.trim().is_empty() => ctx.log(line.trim()),
                        None => {}
                    }
                }
                Ok(None) | Err(_) => break,
            },
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                ctx.log("Cancelling idevicerestore: asking it to stop, the device returns to recovery mode");
                interrupt(&mut child);
            }
        }
    }
    let status = if cancelled {
        match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
            Ok(s) => s,
            Err(_) => {
                let _ = child.kill().await;
                child.wait().await
            }
        }
    } else {
        child.wait().await
    }
    .map_err(|e| FleetError::permanent(format!("idevicerestore: {e}")))?;
    let _ = err_task.await;
    if cancelled {
        return Err(FleetError::cancelled());
    }
    if status.success() {
        return Ok(());
    }
    let tail = tail.lock().unwrap_or_else(|p| p.into_inner()).clone();
    Err(explain_external_failure(status.code(), &tail))
}

#[cfg(unix)]
fn interrupt(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: plain signal delivery to our own child process.
        unsafe { libc::kill(pid as i32, libc::SIGINT) };
    }
}

#[cfg(not(unix))]
fn interrupt(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

/// Turn idevicerestore's last log lines into something a person can act on.
pub fn explain_external_failure(code: Option<i32>, tail: &[String]) -> FleetError {
    let all = tail.join("\n").to_lowercase();
    let last = tail.iter().rev().find(|l| l.to_lowercase().contains("error") || l.to_lowercase().contains("fail")).cloned().unwrap_or_else(|| format!("exited with code {}", code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())));
    if all.contains("not being signed") || all.contains("status=94") || all.contains("this device isn't eligible") {
        return FleetError::permanent("Apple no longer signs this firmware version. Choose a newer one");
    }
    if all.contains("unable to connect to") && all.contains("tss") || all.contains("could not resolve") || all.contains("connection timed out") {
        return FleetError::transient(format!("Couldn't reach Apple's servers: {last}"));
    }
    if all.contains("no device found") || all.contains("unable to discover device") {
        return FleetError::transient("The device wasn't found. Check the cable and that it's in recovery mode");
    }
    if all.contains("no space left") {
        return FleetError::permanent("The disk is full");
    }
    FleetError::new(ErrorClass::Transient, format!("idevicerestore stopped: {last}"))
}

// ---- pre-flight checks shown in the guided flow ----------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    Ok,
    Warn,
    /// Starting would fail or be pointless.
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub level: Level,
    pub text: String,
}

impl Check {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Self { level, text: text.into() }
    }
}

/// What to verify before wiping a device, with plain-language results.
pub fn preflight(device: &Device, ipsw: Option<&LocalIpsw>, signed: Option<bool>, erase: bool, free_bytes: Option<u64>) -> Vec<Check> {
    let mut out = Vec::new();
    if device.ecid.is_none() {
        out.push(Check::new(Level::Block, "The device's ID (ECID) isn't known yet. Wait a moment, or unplug and reconnect it"));
    }
    match ipsw {
        None => out.push(Check::new(Level::Block, "No firmware chosen for this model")),
        Some(f) => {
            if let Some(e) = &f.error {
                out.push(Check::new(Level::Block, format!("The firmware file is damaged: {e}")));
            } else if let Some(pt) = &device.product_type {
                if !f.product_types.iter().any(|m| m == pt) {
                    out.push(Check::new(Level::Block, format!("This firmware is not for {pt}")));
                }
            } else if device.mode != DeviceMode::Normal {
                out.push(Check::new(Level::Warn, "The device model isn't known, so the firmware can't be matched to it. Double-check the model"));
            }
            match signed {
                Some(true) => out.push(Check::new(Level::Ok, "Apple is still signing this version")),
                Some(false) => out.push(Check::new(Level::Block, "Apple no longer signs this version, so it can't be installed. Pick a newer one")),
                None => out.push(Check::new(Level::Warn, "Couldn't check whether Apple still signs this version. The restore will fail early if it doesn't")),
            }
            if let Some(free) = free_bytes {
                let need = f.size + (f.size / 5);
                if free < need {
                    out.push(Check::new(Level::Block, format!("Not enough disk space for the restore: about {:.0} GB needed, {:.0} GB free", need as f64 / 1e9, free as f64 / 1e9)));
                }
            }
        }
    }
    if device.mode == DeviceMode::Normal {
        match device.battery_percent {
            Some(b) if b < 20 => out.push(Check::new(Level::Block, format!("Battery is at {b}%. Charge it above 20% first, a restore can't be interrupted safely"))),
            Some(b) if b < 40 => out.push(Check::new(Level::Warn, format!("Battery is at {b}%. Keep it charging during the restore"))),
            Some(_) => out.push(Check::new(Level::Ok, "Battery is fine")),
            None => {}
        }
        if device.pair_state == PairState::NotPaired {
            out.push(Check::new(Level::Warn, "The device isn't trusted. It will still be put into recovery mode, but only if it's unlocked and you tap Trust. You can also put it into recovery mode by hand"));
        }
        match device.find_my {
            Some(true) if erase => out.push(Check::new(Level::Warn, "Find My is on. After the reset the device asks for the Apple ID that locked it (Activation Lock). Turn Find My off first, or have the ID ready. Company devices are usually released in your MDM")),
            Some(false) => out.push(Check::new(Level::Ok, "Find My is off, so there's no Activation Lock")),
            _ => {}
        }
    } else {
        out.push(Check::new(Level::Ok, format!("The device is in {} mode, ready to restore", device.mode.label())));
        if erase {
            out.push(Check::new(Level::Warn, "Activation Lock can't be checked in recovery mode. If Find My was on, the device asks for the Apple ID afterwards"));
        }
    }
    out
}

pub fn worst(checks: &[Check]) -> Level {
    if checks.iter().any(|c| c.level == Level::Block) {
        Level::Block
    } else if checks.iter().any(|c| c.level == Level::Warn) {
        Level::Warn
    } else {
        Level::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceKey;

    fn device(mode: DeviceMode) -> Device {
        Device {
            key: DeviceKey::ecid(1),
            mode,
            udid: None,
            ecid: Some(1),
            name: Some("iPad".into()),
            product_type: Some("iPad13,18".into()),
            os_version: None,
            build: None,
            serial: None,
            activation_state: None,
            battery_percent: Some(80),
            find_my: None,
            pair_state: PairState::Paired,
            cpid: None,
            bdid: None,
            problem: None,
            last_seen: 0,
        }
    }

    fn ipsw(models: &[&str]) -> LocalIpsw {
        LocalIpsw { file: "f.ipsw".into(), path: "f.ipsw".into(), size: 10_000_000_000, version: Some("27.0.1".into()), build: Some("24A446".into()), product_types: models.iter().map(|s| s.to_string()).collect(), error: None }
    }

    #[test]
    fn parses_idevicerestore_progress() {
        assert_eq!(parse_progress_line("progress: 2 0.453000"), Some(("Sending the system image", 0.453)));
        assert_eq!(parse_progress_line("progress: 4 1.000000"), Some(("Flashing firmware", 1.0)));
        assert_eq!(parse_progress_line("progress: 99 0.5").unwrap().0, "Restoring");
        assert_eq!(parse_progress_line("Sending iBEC (1234 bytes)..."), None);
    }

    #[test]
    fn explains_common_failures() {
        let unsigned = explain_external_failure(Some(255), &["ERROR: TSS request failed".into(), "This device isn't eligible for the requested build.".into()]);
        assert_eq!(unsigned.class, ErrorClass::Permanent);
        let net = explain_external_failure(Some(255), &["ERROR: Unable to connect to TSS server".into(), "Could not resolve host".into()]);
        assert!(net.is_retryable());
        assert!(explain_external_failure(Some(1), &["something odd".into()]).message.contains("code 1"));
    }

    #[test]
    fn preflight_blocks_what_would_fail() {
        let d = device(DeviceMode::Normal);
        let f = ipsw(&["iPad13,18"]);
        assert_eq!(worst(&preflight(&d, Some(&f), Some(true), true, Some(100_000_000_000))), Level::Ok);
        assert_eq!(worst(&preflight(&d, Some(&ipsw(&["iPhone15,2"])), Some(true), true, None)), Level::Block, "wrong model");
        assert_eq!(worst(&preflight(&d, Some(&f), Some(false), true, None)), Level::Block, "unsigned");
        assert_eq!(worst(&preflight(&d, Some(&f), None, true, None)), Level::Warn, "unknown signing state");
        assert_eq!(worst(&preflight(&d, Some(&f), Some(true), true, Some(5_000_000_000))), Level::Block, "no disk space");
        assert_eq!(worst(&preflight(&d, None, None, true, None)), Level::Block, "no firmware");
        let mut low = d.clone();
        low.battery_percent = Some(12);
        assert_eq!(worst(&preflight(&low, Some(&f), Some(true), true, None)), Level::Block, "battery");
        let mut locked = d.clone();
        locked.find_my = Some(true);
        let checks = preflight(&locked, Some(&f), Some(true), true, None);
        assert!(checks.iter().any(|c| c.text.contains("Activation Lock")));
        assert!(!preflight(&locked, Some(&f), Some(true), false, None).iter().any(|c| c.text.contains("Activation Lock")), "an update keeps the account");
        let rec = device(DeviceMode::Recovery);
        assert_eq!(worst(&preflight(&rec, Some(&f), Some(true), true, None)), Level::Warn);
    }
}
