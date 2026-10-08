use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use fleet_core::jobs::RunnerFuture;
use fleet_core::store::Store;
use fleet_core::{ErrorClass, JobContext, JobEngine, JobKind, JobSpec, JobState, Limits, RetryPolicy};

// One test, because it sets an environment variable for the installer.
#[tokio::test(flavor = "multi_thread")]
async fn installer_outcomes_are_explained() {
    let dir = tempfile::tempdir().unwrap();
    let deb = dir.path().join("idevice-fleet_9.9.9_amd64.deb");
    std::fs::write(&deb, b"x").unwrap();
    let store = Arc::new(Store::open(&dir.path().join("db")).unwrap());
    let engine = JobEngine::new(store, dir.path().join("logs"), Limits::default()).unwrap();
    let deb2 = deb.clone();
    engine.register(JobKind::Install, move |ctx: JobContext| {
        let deb = deb2.clone();
        Box::pin(async move { fleet_core::update::run_installer(&ctx, &deb).await }) as RunnerFuture
    });
    let policy = RetryPolicy { max_attempts: 1, base_delay_ms: 10, max_delay_ms: 10, stall_timeout_s: 60, attempt_timeout_s: None };

    for (script, expect) in [("echo 'Setting up idevice-fleet'; exit 0", JobState::Succeeded), ("exit 126", JobState::Failed), ("echo boom >&2; exit 100", JobState::Failed)] {
        let tool = dir.path().join("fake-pkexec");
        std::fs::write(&tool, format!("#!/bin/bash\n{script}\n")).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: this is the only test in this binary, so nothing else reads the environment concurrently.
        unsafe { std::env::set_var("IDEVICE_FLEET_INSTALLER", &tool) };
        let id = engine.submit(JobSpec::new(JobKind::Install, "Install update", None, policy)).unwrap();
        engine.wait_idle().await;
        let v = engine.get(&id).unwrap();
        assert_eq!(v.state, expect, "{script}: {:?}", v.error);
        match script {
            "exit 126" => assert_eq!(v.error.unwrap().class, ErrorClass::NeedsUser),
            "echo boom >&2; exit 100" => {
                assert!(v.error.as_ref().unwrap().message.contains("code 100"));
                assert!(engine.log_tail(&id, 50).iter().any(|l| l.contains("boom")), "installer output is in the log");
            }
            _ => assert!(v.stage.unwrap().contains("Installed")),
        }
    }
}
