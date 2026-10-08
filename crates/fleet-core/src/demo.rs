//! Invented devices, jobs, backups and firmware so the UI can be shown (and
//! screenshotted) without hardware. Enabled with `--demo`; nothing here runs
//! in normal use.

use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::devices::{NormalInfo, RecoveryInfo};
use crate::error::{FleetError, Result};
use crate::fleet::Fleet;
use crate::jobs::{JobContext, JobKind, JobSpec, RetryPolicy, RunnerFuture};
use crate::model::DeviceKey;

#[derive(Serialize, Deserialize)]
struct Script {
    mode: String,
    stage: String,
    progress: Option<f32>,
    message: Option<String>,
}

fn script(mode: &str, stage: &str, progress: Option<f32>, message: Option<&str>) -> Script {
    Script { mode: mode.into(), stage: stage.into(), progress, message: message.map(str::to_string) }
}

struct Demo {
    udid: &'static str,
    ecid: u64,
    name: &'static str,
    product: &'static str,
    os: &'static str,
    serial: &'static str,
    battery: u8,
    paired: bool,
}

const DEVICES: &[Demo] = &[
    Demo { udid: "00008101-001A2B3C4D5E6F78", ecid: 0x1A2B3C4D5E6F78, name: "Front desk iPad", product: "iPad13,18", os: "27.0.1", serial: "F9FZ2ABCD34E", battery: 82, paired: true },
    Demo { udid: "00008140-000A11B22C33D44E", ecid: 0x0A11B22C33D44E, name: "Meeting room iPad", product: "iPad13,19", os: "26.7", serial: "DMPXK1QRS5T6", battery: 64, paired: true },
    Demo { udid: "00008130-001C55E66F77A88B", ecid: 0x1C55E66F77A88B, name: "Warehouse iPad", product: "iPad13,18", os: "26.6.2", serial: "GG7H8JKLMN90", battery: 14, paired: false },
    Demo { udid: "00008140-000D99C0AA1B2C3D", ecid: 0x0D99C0AA1B2C3D, name: "Reception iPhone", product: "iPhone17,1", os: "27.0", serial: "C39XQ4WERT21", battery: 91, paired: true },
];

pub fn load(fleet: &Fleet) -> Result<()> {
    fleet.engine.register(JobKind::Test, |ctx: JobContext| {
        Box::pin(async move {
            let s: Script = ctx.params()?;
            ctx.stage(s.stage.clone());
            if let Some(p) = s.progress {
                ctx.progress(p);
            }
            match s.mode.as_str() {
                "run" => loop {
                    ctx.heartbeat();
                    ctx.sleep(Duration::from_secs(1)).await?;
                },
                "fail" => Err(FleetError::permanent(s.message.unwrap_or_default())),
                "retry" => Err(FleetError::transient(s.message.unwrap_or_default())),
                _ => Ok(()),
            }
        }) as RunnerFuture
    });

    // Simulated restores: the whole flow is visible without a device.
    fleet.engine.register(JobKind::Restore, |ctx: JobContext| {
        Box::pin(async move {
            let p: crate::restore::RestoreParams = ctx.params()?;
            let secs: f32 = std::env::var("IDEVICE_FLEET_DEMO_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(40.0);
            let start = 20.0 + (p.ecid % 40) as f32;
            let stages = [(0.0, "Unpacking the system image"), (0.15, "Asking Apple to approve the restore"), (0.3, "Starting the restore on the device"), (0.45, "Restoring (step 15)"), (0.8, "Flashing firmware")];
            let steps = 100;
            for i in 0..=steps {
                let f = i as f32 / steps as f32;
                ctx.heartbeat();
                let stage = stages.iter().rev().find(|(t, _)| f >= *t).map(|(_, s)| *s).unwrap_or("Starting");
                ctx.stage_quiet(stage);
                ctx.progress(if secs < 10.0 { f * 100.0 } else { start + (100.0 - start) * f });
                if i == 70 && p.ecid % 2 == 1 {
                    return Err(FleetError::permanent("The device stopped answering while it was being restored. Check the cable, use one plugged straight into the computer"));
                }
                ctx.sleep(Duration::from_secs_f32(secs / steps as f32)).await?;
            }
            Ok(())
        }) as RunnerFuture
    });

    for (i, d) in DEVICES.iter().enumerate() {
        fleet.registry.normal_attached(d.udid, i as u32 + 1);
        fleet.registry.normal_info(
            d.udid,
            NormalInfo {
                ecid: Some(d.ecid),
                name: Some(d.name.into()),
                product_type: Some(d.product.into()),
                os_version: Some(d.os.into()),
                serial: Some(d.serial.into()),
                activation_state: Some("Activated".into()),
                battery_percent: Some(d.battery),
                paired: d.paired,
                ..Default::default()
            },
        );
    }
    // A device that has been seen before and is now in recovery mode, and one in DFU mode.
    fleet.registry.normal_attached("00008101-0099AABBCCDDEEFF", 9);
    fleet.registry.normal_info(
        "00008101-0099AABBCCDDEEFF",
        NormalInfo { ecid: Some(0x99AABBCCDDEEFF), name: Some("Back office iPad".into()), product_type: Some("iPad13,18".into()), serial: Some("H2XY3Z4A5B6C".into()), paired: true, ..Default::default() },
    );
    fleet.registry.normal_detached(9);
    fleet.registry.set_recovery(vec![
        RecoveryInfo { ecid: 0x99AABBCCDDEEFF, dfu: false, cpid: Some(0x8101), bdid: Some(0x14), serial: None },
        RecoveryInfo { ecid: 0x5566778899AA, dfu: true, cpid: Some(0x8030), bdid: Some(0x0C), serial: None },
    ]);
    fleet.registry.set_usbmuxd_health(crate::Health::Ok);
    fleet.registry.set_usb_health(crate::Health::Ok);

    let key = |i: usize| Some(DeviceKey::ecid(DEVICES[i].ecid));
    let jobs = [
        (key(3), "Back up Reception iPhone", script("run", "Backing up · 14.20 GB received", Some(63.0), None)),
        (None, "Download iPad_Fall_2022_27.0.1_24A446_Restore.ipsw", script("run", "Downloading", Some(41.0), None)),
        (key(2), "Pair Warehouse iPad", script("retry", "Waiting for the device to answer", None, Some("Unlock the device and tap Trust on the \"Trust This Computer?\" prompt"))),
        (key(1), "Back up Meeting room iPad", script("ok", "Backup complete", Some(100.0), None)),
        (Some(DeviceKey::ecid(0x5566778899AA)), "Exit recovery: DFU device", script("fail", "Opening the device over USB", None, Some("Couldn't open the device over USB: permission denied. On Linux, check the udev rules (see README)"))),
    ];
    let mut done = HashSet::new();
    for (device, title, sc) in jobs {
        let policy = match sc.mode.as_str() {
            "retry" => RetryPolicy { max_attempts: 5, base_delay_ms: 3_600_000, max_delay_ms: 3_600_000, stall_timeout_s: 3600, attempt_timeout_s: None },
            _ => RetryPolicy::long(),
        };
        if let Some(d) = &device
            && !done.insert(d.clone())
        {
            continue;
        }
        fleet.engine.submit(JobSpec::new(JobKind::Test, title, device, policy).with_params(sc))?;
    }

    // Backups on disk. Sparse files give them realistic sizes without using disk space.
    let backups = [("00008140-000A11B22C33D44E", "Meeting room iPad", "iPad13,19", "26.7", "DMPXK1QRS5T6", true, true, 21_400_000_000u64, 3600u64 * 3),
        ("00008140-000D99C0AA1B2C3D", "Reception iPhone", "iPhone17,1", "27.0", "C39XQ4WERT21", false, true, 48_900_000_000, 86400 * 2),
        ("00008101-001A2B3C4D5E6F78", "Front desk iPad", "iPad13,18", "27.0.1", "F9FZ2ABCD34E", true, false, 3_100_000_000, 60 * 25)];
    for (udid, name, product, os, serial, encrypted, complete, size, age) in backups {
        let dir = fleet.paths.backups.join(udid);
        std::fs::create_dir_all(&dir)?;
        let mut info = plist::Dictionary::new();
        for (k, v) in [("Device Name", name), ("Product Type", product), ("Product Version", os), ("Serial Number", serial)] {
            info.insert(k.into(), v.into());
        }
        let when = std::time::SystemTime::now() - Duration::from_secs(age);
        info.insert("Last Backup Date".into(), plist::Value::Date(when.into()));
        plist::Value::Dictionary(info).to_file_xml(dir.join("Info.plist")).map_err(|e| FleetError::permanent(e.to_string()))?;
        let mut man = plist::Dictionary::new();
        man.insert("IsEncrypted".into(), encrypted.into());
        plist::Value::Dictionary(man).to_file_xml(dir.join("Manifest.plist")).map_err(|e| FleetError::permanent(e.to_string()))?;
        if complete {
            std::fs::write(dir.join("Status.plist"), b"<plist/>")?;
        }
        std::fs::File::create(dir.join("Manifest.db"))?.set_len(size)?;
    }
    fleet.refresh_backups();

    // Firmware in the library: small real zips with the manifest, so scanning works as usual.
    for (file, version, build, models) in [
        ("iPad_Fall_2022_27.0.1_24A446_Restore.ipsw", "27.0.1", "24A446", vec!["iPad13,18", "iPad13,19"]),
        ("iPad_Fall_2022_26.7_23H31_Restore.ipsw", "26.7", "23H31", vec!["iPad13,18", "iPad13,19"]),
    ] {
        let path = fleet.paths.firmware.join(file);
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path)?);
        z.start_file("BuildManifest.plist", zip::write::SimpleFileOptions::default()).map_err(|e| FleetError::permanent(e.to_string()))?;
        let mut d = plist::Dictionary::new();
        d.insert("ProductVersion".into(), version.into());
        d.insert("ProductBuildVersion".into(), build.into());
        d.insert("SupportedProductTypes".into(), plist::Value::Array(models.into_iter().map(Into::into).collect()));
        let mut buf = Vec::new();
        plist::to_writer_xml(&mut buf, &plist::Value::Dictionary(d)).map_err(|e| FleetError::permanent(e.to_string()))?;
        std::io::Write::write_all(&mut z, &buf)?;
        z.finish().map_err(|e| FleetError::permanent(e.to_string()))?;
    }
    fleet.refresh_library();
    Ok(())
}
