//! Downloads the real published release the way the app does. Opt-in:
//! `cargo test -p fleet-core --test update_live -- --ignored`

use std::sync::Arc;

use fleet_core::firmware::{DownloadParams, Source, download, fetch_text};
use fleet_core::jobs::RunnerFuture;
use fleet_core::store::Store;
use fleet_core::{JobContext, JobEngine, JobKind, JobSpec, JobState, Limits, RetryPolicy};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn downloads_and_verifies_the_published_deb() {
    let info = tokio::task::spawn_blocking(|| fleet_core::update::check("0.0.1")).await.unwrap().unwrap().expect("a release exists");
    let name = fleet_core::update::check_update_url(info.deb_url.as_ref().unwrap()).unwrap();
    let sums = fetch_text(info.sums_url.as_ref().unwrap()).unwrap();
    let sha = fleet_core::update::checksum_for(&sums, &name).expect("checksum listed");

    let dir = tempfile::tempdir().unwrap();
    let engine = JobEngine::new(Arc::new(Store::open(&dir.path().join("db")).unwrap()), dir.path().join("logs"), Limits::default()).unwrap();
    engine.register(JobKind::Download, |ctx: JobContext| {
        Box::pin(async move {
            let p: DownloadParams = ctx.params()?;
            tokio::task::spawn_blocking(move || download(&ctx, &p)).await.unwrap()
        }) as RunnerFuture
    });
    let params = DownloadParams { url: info.deb_url.clone().unwrap(), dest_dir: dir.path().to_path_buf(), sha256: Some(sha), size: None, source: Source::GitHubRelease };
    let policy = RetryPolicy { max_attempts: 2, base_delay_ms: 100, max_delay_ms: 100, stall_timeout_s: 60, attempt_timeout_s: None };
    let id = engine.submit(JobSpec::new(JobKind::Download, "update", None, policy).with_params(params)).unwrap();
    engine.wait_idle().await;
    let v = engine.get(&id).unwrap();
    assert_eq!(v.state, JobState::Succeeded, "{:?}", v.error);
    let got = std::fs::metadata(dir.path().join(&name)).unwrap().len();
    assert!(got > 1_000_000, "downloaded {got} bytes");
    assert!(fleet_core::update::installable_deb(dir.path(), &name).is_ok());
}
