use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use fleet_core::jobs::RunnerFuture;
use fleet_core::store::Store;
use fleet_core::{DeviceKey, ErrorClass, FleetError, JobContext, JobEngine, JobKind, JobSpec, JobState, Limits, RetryPolicy};

fn policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy { max_attempts, base_delay_ms: 1_000, max_delay_ms: 10_000, stall_timeout_s: 30, attempt_timeout_s: None }
}

fn engine(dir: &tempfile::TempDir) -> JobEngine {
    let store = Arc::new(Store::open(&dir.path().join("fleet.db")).unwrap());
    JobEngine::new(store, dir.path().join("logs"), Limits::default()).unwrap()
}

fn spec(device: Option<&str>, max_attempts: u32) -> JobSpec {
    JobSpec::new(JobKind::Test, "test job", device.map(|d| DeviceKey(d.into())), policy(max_attempts))
}

/// Runner that fails with `class` for the first `failures` attempts, then succeeds.
fn flaky(calls: Arc<AtomicU32>, failures: u32, class: ErrorClass) -> impl Fn(JobContext) -> RunnerFuture {
    move |ctx: JobContext| {
        let calls = calls.clone();
        Box::pin(async move {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            ctx.stage(format!("attempt {n}"));
            ctx.progress(50.0);
            if n <= failures {
                return Err(FleetError::new(class, format!("failure {n}")));
            }
            Ok(())
        })
    }
}

#[tokio::test(start_paused = true)]
async fn transient_failures_are_retried_until_success() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&dir);
    let calls = Arc::new(AtomicU32::new(0));
    e.register(JobKind::Test, flaky(calls.clone(), 2, ErrorClass::Transient));
    let id = e.submit(spec(None, 3)).unwrap();
    e.wait_idle().await;
    let v = e.get(&id).unwrap();
    assert_eq!(v.state, JobState::Succeeded);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(v.attempt, 3);
    assert_eq!(v.progress, Some(100.0));
    assert!(e.log_tail(&id, 100).iter().any(|l| l.contains("Retrying in")));
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_max_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&dir);
    let calls = Arc::new(AtomicU32::new(0));
    e.register(JobKind::Test, flaky(calls.clone(), 99, ErrorClass::Transient));
    let id = e.submit(spec(None, 3)).unwrap();
    e.wait_idle().await;
    let v = e.get(&id).unwrap();
    assert_eq!(v.state, JobState::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(v.error.unwrap().message, "failure 3");
}

#[tokio::test(start_paused = true)]
async fn permanent_and_needs_user_errors_are_not_retried() {
    for class in [ErrorClass::Permanent, ErrorClass::NeedsUser] {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(&dir);
        let calls = Arc::new(AtomicU32::new(0));
        e.register(JobKind::Test, flaky(calls.clone(), 99, class));
        let id = e.submit(spec(None, 5)).unwrap();
        e.wait_idle().await;
        assert_eq!(e.get(&id).unwrap().state, JobState::Failed);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{class:?} must not be retried");
    }
}

#[tokio::test(start_paused = true)]
async fn watchdog_cancels_a_stalled_attempt_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&dir);
    let calls = Arc::new(AtomicU32::new(0));
    let c = calls.clone();
    e.register(JobKind::Test, move |ctx: JobContext| {
        let c = c.clone();
        Box::pin(async move {
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                // First attempt hangs forever without reporting anything.
                ctx.cancellation().cancelled().await;
                return Err(FleetError::cancelled());
            }
            Ok(())
        }) as RunnerFuture
    });
    let id = e.submit(spec(None, 2)).unwrap();
    e.wait_idle().await;
    let v = e.get(&id).unwrap();
    assert_eq!(v.state, JobState::Succeeded);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(e.log_tail(&id, 100).iter().any(|l| l.contains("No progress for")));
}

#[tokio::test(start_paused = true)]
async fn a_panicking_job_fails_alone() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&dir);
    e.register(JobKind::Test, |ctx: JobContext| {
        Box::pin(async move {
            if ctx.params::<String>().unwrap_or_default() == "boom" {
                panic!("simulated bug");
            }
            Ok(())
        }) as RunnerFuture
    });
    let bad = e.submit(spec(Some("a"), 3).with_params("boom")).unwrap();
    let good = e.submit(spec(Some("b"), 3).with_params("fine")).unwrap();
    e.wait_idle().await;
    let bad = e.get(&bad).unwrap();
    assert_eq!(bad.state, JobState::Failed);
    assert!(bad.error.unwrap().message.contains("simulated bug"));
    assert_eq!(e.get(&good).unwrap().state, JobState::Succeeded);
    // The device lock is released after the panic.
    assert!(e.busy_devices().is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancel_stops_a_running_job() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&dir);
    e.register(JobKind::Test, |ctx: JobContext| {
        Box::pin(async move {
            loop {
                ctx.heartbeat();
                ctx.sleep(Duration::from_secs(1)).await?;
            }
            #[allow(unreachable_code)]
            Ok(())
        }) as RunnerFuture
    });
    let id = e.submit(spec(Some("dev"), 3)).unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(e.get(&id).unwrap().state, JobState::Running);
    assert!(e.cancel(&id));
    e.wait_idle().await;
    assert_eq!(e.get(&id).unwrap().state, JobState::Cancelled);
    assert!(e.busy_devices().is_empty());
}

#[tokio::test(start_paused = true)]
async fn one_job_per_device() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&dir);
    e.register(JobKind::Test, |ctx: JobContext| {
        Box::pin(async move {
            ctx.heartbeat();
            ctx.sleep(Duration::from_secs(5)).await
        }) as RunnerFuture
    });
    e.submit(spec(Some("dev"), 1)).unwrap();
    let err = e.submit(spec(Some("dev"), 1)).unwrap_err();
    assert_eq!(err.class, ErrorClass::NeedsUser);
    assert!(e.submit(spec(Some("other"), 1)).is_ok());
    e.wait_idle().await;
    // Free again once the first job is done.
    assert!(e.submit(spec(Some("dev"), 1)).is_ok());
    e.wait_idle().await;
}

#[tokio::test(start_paused = true)]
async fn restore_slots_limit_parallel_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("fleet.db")).unwrap());
    let e = JobEngine::new(store, dir.path().join("logs"), Limits { restores: 2, downloads: 1, other: 4 }).unwrap();
    let running = Arc::new(AtomicU32::new(0));
    let peak = Arc::new(AtomicU32::new(0));
    let (r, p) = (running.clone(), peak.clone());
    e.register(JobKind::Restore, move |ctx: JobContext| {
        let (r, p) = (r.clone(), p.clone());
        Box::pin(async move {
            let now = r.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(now, Ordering::SeqCst);
            ctx.sleep(Duration::from_secs(3)).await?;
            r.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }) as RunnerFuture
    });
    for i in 0..5 {
        e.submit(JobSpec::new(JobKind::Restore, "restore", Some(DeviceKey(format!("d{i}"))), policy(1))).unwrap();
    }
    e.wait_idle().await;
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert!(e.snapshot().iter().all(|j| j.state == JobState::Succeeded));
}

#[tokio::test(start_paused = true)]
async fn jobs_active_at_a_crash_come_back_as_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let id;
    {
        let e = engine(&dir);
        e.register(JobKind::Test, |ctx: JobContext| {
            Box::pin(async move {
                ctx.stage("working");
                ctx.cancellation().cancelled().await;
                Ok(())
            }) as RunnerFuture
        });
        id = e.submit(spec(Some("dev"), 1)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(e.get(&id).unwrap().state, JobState::Running);
        // Simulate a crash: drop the engine without shutting it down.
    }
    let e = engine(&dir);
    let v = e.get(&id).expect("job is loaded from disk");
    assert_eq!(v.state, JobState::Interrupted);
    assert!(e.log_tail(&id, 10).iter().any(|l| l.contains("working")), "log is reloaded from disk");

    // It can be started again with the same settings.
    e.register(JobKind::Test, |_ctx: JobContext| Box::pin(async { Ok(()) }) as RunnerFuture);
    let again = e.run_again(&id).unwrap();
    e.wait_idle().await;
    let v2 = e.get(&again).unwrap();
    assert_eq!(v2.state, JobState::Succeeded);
    assert_eq!(v2.retry_of.as_deref(), Some(id.as_str()));
}

#[test]
fn backoff_grows_and_is_capped() {
    let p = RetryPolicy { max_attempts: 10, base_delay_ms: 1_000, max_delay_ms: 30_000, stall_timeout_s: 60, attempt_timeout_s: None };
    assert_eq!(p.delay(1), Duration::from_secs(1));
    assert_eq!(p.delay(2), Duration::from_secs(2));
    assert_eq!(p.delay(4), Duration::from_secs(8));
    assert_eq!(p.delay(9), Duration::from_secs(30));
}

/// The UI thread has no Tokio runtime; submitting from it must still work.
#[test]
fn submit_works_from_a_thread_outside_the_runtime() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let e = {
        let _g = rt.enter();
        engine(&dir)
    };
    e.register(JobKind::Test, |_ctx: JobContext| Box::pin(async { Ok(()) }) as RunnerFuture);
    let e2 = e.clone();
    let id = std::thread::spawn(move || e2.submit(spec(Some("dev"), 1)).unwrap()).join().unwrap();
    rt.block_on(e.wait_idle());
    assert_eq!(e.get(&id).unwrap().state, JobState::Succeeded);
}
