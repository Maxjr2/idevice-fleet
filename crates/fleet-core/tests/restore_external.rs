//! The idevicerestore engine and engine selection, driven by a fake `idevicerestore` script.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fleet_core::devices::Registry;
use fleet_core::jobs::RunnerFuture;
use fleet_core::native::NativeBackend;
use fleet_core::restore::{Engine, RestoreParams, run};
use fleet_core::store::Store;
use fleet_core::{ErrorClass, JobContext, JobEngine, JobKind, JobSpec, JobState, Limits, RetryPolicy};

fn script(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("idevicerestore");
    std::fs::write(&p, format!("#!/bin/bash\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// An IPSW whose system image is AEA-encrypted, which the native engine refuses up front.
fn aea_ipsw(dir: &Path) -> PathBuf {
    let path = dir.join("fw.ipsw");
    let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    z.start_file("BuildManifest.plist", zip::write::SimpleFileOptions::default()).unwrap();
    let mut info = plist::Dictionary::new();
    info.insert("Path".into(), "058-12345-678.dmg.aea".into());
    let mut os = plist::Dictionary::new();
    os.insert("Info".into(), plist::Value::Dictionary(info));
    let mut manifest = plist::Dictionary::new();
    manifest.insert("OS".into(), plist::Value::Dictionary(os));
    let mut bi = plist::Dictionary::new();
    bi.insert("Manifest".into(), plist::Value::Dictionary(manifest));
    let mut root = plist::Dictionary::new();
    root.insert("BuildIdentities".into(), plist::Value::Array(vec![plist::Value::Dictionary(bi)]));
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &plist::Value::Dictionary(root)).unwrap();
    std::io::Write::write_all(&mut z, &buf).unwrap();
    z.finish().unwrap();
    path
}

struct Rig {
    engine: JobEngine,
    _dir: tempfile::TempDir,
    work: PathBuf,
}

fn rig(tool: Option<PathBuf>, engine_choice: Engine) -> (Rig, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().to_path_buf();
    let store = Arc::new(Store::open(&work.join("db")).unwrap());
    let engine = JobEngine::new(store, work.join("logs"), Limits::default()).unwrap();
    let backend = NativeBackend::new(Arc::new(Registry::new(None)));
    let ipsw = aea_ipsw(&work);
    let cache = work.join("cache").join("job");
    let params = RestoreParams { ecid: 0xABC, udid: None, ipsw, erase: true, engine: engine_choice, cache_dir: cache.clone() };
    engine.register(JobKind::Restore, move |ctx: JobContext| {
        let (backend, tool, params) = (backend.clone(), tool.clone(), params.clone());
        Box::pin(async move { run(&backend, &ctx, &params, tool.as_deref()).await }) as RunnerFuture
    });
    (Rig { engine, _dir: dir, work }, cache)
}

fn spec() -> JobSpec {
    let policy = RetryPolicy { max_attempts: 1, base_delay_ms: 10, max_delay_ms: 10, stall_timeout_s: 60, attempt_timeout_s: None };
    JobSpec::new(JobKind::Restore, "restore", None, policy)
}

#[tokio::test]
async fn external_engine_reports_progress_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let tool = script(dir.path(), r#"echo "args: $*"; for p in 0.2 0.6 1.0; do echo "progress: 2 $p"; sleep 0.05; done; echo "progress: 4 1.000000""#);
    let (r, cache) = rig(Some(tool), Engine::IdeviceRestore);
    let id = r.engine.submit(spec()).unwrap();
    r.engine.wait_idle().await;
    let v = r.engine.get(&id).unwrap();
    assert_eq!(v.state, JobState::Succeeded, "{:?}", v.error);
    let log = r.engine.log_tail(&id, 100).join("\n");
    assert!(log.contains("-i 0xabc") && log.contains("-e"), "targets the device and erases: {log}");
    assert!(!cache.exists(), "scratch folder removed");
}

#[tokio::test]
async fn external_engine_failure_is_explained_and_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let tool = script(dir.path(), r#"echo "ERROR: TSS request failed" >&2; echo "This device isn't eligible for the requested build." >&2; exit 255"#);
    let (r, cache) = rig(Some(tool), Engine::IdeviceRestore);
    let id = r.engine.submit(spec()).unwrap();
    r.engine.wait_idle().await;
    let v = r.engine.get(&id).unwrap();
    assert_eq!(v.state, JobState::Failed);
    let e = v.error.unwrap();
    assert_eq!(e.class, ErrorClass::Permanent);
    assert!(e.message.contains("no longer signs"), "{}", e.message);
    assert!(!cache.exists());
}

#[tokio::test]
async fn cancel_stops_idevicerestore_with_a_signal() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("got-sigint");
    let tool = script(dir.path(), &format!(r#"trap 'echo got > {}; exit 130' INT; echo "progress: 2 0.1"; while true; do sleep 0.1; done"#, marker.display()));
    let (r, _) = rig(Some(tool), Engine::IdeviceRestore);
    let id = r.engine.submit(spec()).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(r.engine.get(&id).unwrap().state, JobState::Running);
    let t0 = std::time::Instant::now();
    assert!(r.engine.cancel(&id));
    r.engine.wait_idle().await;
    assert_eq!(r.engine.get(&id).unwrap().state, JobState::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(5), "stopped promptly");
    assert!(marker.exists(), "the tool was asked to stop gracefully (SIGINT), not killed");
}

#[tokio::test]
async fn auto_falls_back_to_idevicerestore_for_aea_firmware() {
    let dir = tempfile::tempdir().unwrap();
    let tool = script(dir.path(), r#"echo "progress: 4 1.000000""#);
    let (r, _) = rig(Some(tool), Engine::Auto);
    let id = r.engine.submit(spec()).unwrap();
    r.engine.wait_idle().await;
    let v = r.engine.get(&id).unwrap();
    assert_eq!(v.state, JobState::Succeeded, "{:?}", v.error);
    assert!(r.engine.log_tail(&id, 100).iter().any(|l| l.contains("Switching to idevicerestore")));
}

#[tokio::test]
async fn auto_without_the_tool_explains_what_to_install() {
    let (r, _) = rig(None, Engine::Auto);
    let id = r.engine.submit(spec()).unwrap();
    r.engine.wait_idle().await;
    let e = r.engine.get(&id).unwrap().error.unwrap();
    assert!(e.message.contains("encrypted") && e.message.contains("apt install idevicerestore"), "{}", e.message);
}

#[tokio::test]
async fn native_only_refuses_aea_without_touching_a_device() {
    let (r, cache) = rig(None, Engine::Native);
    let id = r.engine.submit(spec()).unwrap();
    r.engine.wait_idle().await;
    let v = r.engine.get(&id).unwrap();
    assert_eq!(v.state, JobState::Failed);
    assert!(v.error.unwrap().fallback_suggested);
    assert!(!cache.exists());
    let _ = r.work;
}
