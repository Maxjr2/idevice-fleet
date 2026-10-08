//! The job engine: supervised, persisted, retried, watched.
//!
//! Every device operation is a job. A job runs as attempts; each attempt is a
//! separate task, so a panic inside one device's work fails that attempt only.
//! A watchdog cancels attempts that stop reporting activity. Transient
//! failures are retried with exponential backoff. Every state change is
//! written to SQLite, so after a crash the job shows up as interrupted.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

// Tokio's clock, so tests can fast-forward through backoff and watchdog timeouts.
use tokio::time::Instant;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorClass, FleetError, Result};
use crate::model::{DeviceKey, now_secs};
use crate::store::Store;

pub type JobId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobKind {
    Pair,
    EnterRecovery,
    ExitRecovery,
    Backup,
    Restore,
    Download,
    /// Used by tests.
    Test,
}

/// Which concurrency limit a job counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    Restore,
    Download,
    Other,
}

impl JobKind {
    pub fn resource(self) -> Resource {
        match self {
            JobKind::Restore => Resource::Restore,
            JobKind::Download => Resource::Download,
            _ => Resource::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Queued,
    Running,
    /// Waiting before the next attempt.
    Retrying,
    Succeeded,
    Failed,
    Cancelled,
    /// The app stopped while the job was active.
    Interrupted,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "Queued",
            JobState::Running => "Running",
            JobState::Retrying => "Retrying",
            JobState::Succeeded => "Succeeded",
            JobState::Failed => "Failed",
            JobState::Cancelled => "Cancelled",
            JobState::Interrupted => "Interrupted",
        }
    }
    pub fn is_active(self) -> bool {
        matches!(self, JobState::Queued | JobState::Running | JobState::Retrying)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    /// Cancel and retry an attempt that reports nothing for this long.
    pub stall_timeout_s: u64,
    /// Give up on an attempt that runs longer than this.
    pub attempt_timeout_s: Option<u64>,
}

impl RetryPolicy {
    /// Short device operations: pairing, mode changes.
    pub fn quick() -> Self {
        Self { max_attempts: 3, base_delay_ms: 2_000, max_delay_ms: 30_000, stall_timeout_s: 180, attempt_timeout_s: Some(600) }
    }
    /// Restores, backups and downloads: hours long, stalls are what we guard against.
    pub fn long() -> Self {
        Self { max_attempts: 3, base_delay_ms: 10_000, max_delay_ms: 120_000, stall_timeout_s: 900, attempt_timeout_s: Some(4 * 3600) }
    }
    pub fn delay(&self, attempt: u32) -> Duration {
        let exp = self.base_delay_ms.saturating_mul(1u64 << attempt.saturating_sub(1).min(16));
        Duration::from_millis(exp.min(self.max_delay_ms))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub kind: JobKind,
    pub title: String,
    pub device: Option<DeviceKey>,
    #[serde(default)]
    pub params: serde_json::Value,
    pub policy: RetryPolicy,
}

impl JobSpec {
    pub fn new(kind: JobKind, title: impl Into<String>, device: Option<DeviceKey>, policy: RetryPolicy) -> Self {
        Self { kind, title: title.into(), device, params: serde_json::Value::Null, policy }
    }
    pub fn with_params(mut self, params: impl Serialize) -> Self {
        self.params = serde_json::to_value(params).expect("job params serialize");
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobView {
    pub id: JobId,
    pub kind: JobKind,
    pub title: String,
    pub device: Option<DeviceKey>,
    pub state: JobState,
    pub attempt: u32,
    pub max_attempts: u32,
    pub stage: Option<String>,
    pub progress: Option<f32>,
    pub error: Option<FleetError>,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub next_retry_at: Option<u64>,
    pub retry_of: Option<JobId>,
}

const LOG_RING: usize = 2000;

struct JobShared {
    view: Mutex<JobView>,
    log: Mutex<VecDeque<String>>,
    log_file: Mutex<Option<File>>,
    clock: Instant,
    last_activity_ms: AtomicU64,
    dirty: AtomicBool,
    revision: Arc<AtomicU64>,
    /// Set when the running attempt reported forward progress.
    progressed: AtomicBool,
}

impl JobShared {
    fn view(&self) -> std::sync::MutexGuard<'_, JobView> {
        self.view.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn touch(&self) {
        self.last_activity_ms.store(self.clock.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
    fn idle_for(&self) -> Duration {
        let now = self.clock.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_activity_ms.load(Ordering::Relaxed)))
    }
    fn changed(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        self.revision.fetch_add(1, Ordering::Relaxed);
    }
    fn log_line(&self, line: &str) {
        let secs = self.clock.elapsed().as_secs();
        let stamped = format!("[+{:02}:{:02}:{:02}] {line}", secs / 3600, secs / 60 % 60, secs % 60);
        tracing::debug!(target: "job", "{stamped}");
        if let Some(f) = self.log_file.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            // Write through so the log survives a crash.
            let _ = writeln!(f, "{stamped}");
            let _ = f.flush();
        }
        let mut log = self.log.lock().unwrap_or_else(|p| p.into_inner());
        if log.len() >= LOG_RING {
            log.pop_front();
        }
        log.push_back(stamped);
        drop(log);
        self.touch();
        self.revision.fetch_add(1, Ordering::Relaxed);
    }
}

/// Handed to a job's runner: report progress, log, and observe cancellation.
#[derive(Clone)]
pub struct JobContext {
    shared: Arc<JobShared>,
    cancel: CancellationToken,
    attempt: u32,
    params: serde_json::Value,
}

impl JobContext {
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
    pub fn stage(&self, stage: impl Into<String>) {
        let stage = stage.into();
        self.shared.log_line(&stage);
        self.shared.view().stage = Some(stage);
        self.shared.changed();
    }
    /// Percent, 0–100.
    pub fn progress(&self, percent: f32) {
        let percent = percent.clamp(0.0, 100.0);
        {
            let mut v = self.shared.view();
            if v.progress.is_none_or(|old| percent > old) {
                self.shared.progressed.store(true, Ordering::Relaxed);
            }
            v.progress = Some(percent);
        }
        self.shared.touch();
        self.shared.changed();
    }
    pub fn log(&self, line: impl AsRef<str>) {
        self.shared.log_line(line.as_ref());
    }
    /// Tell the watchdog the job is alive without changing what it shows.
    pub fn heartbeat(&self) {
        self.shared.touch();
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }
    /// Fails with `Cancelled` if the job was cancelled; call between steps.
    pub fn check_cancelled(&self) -> Result<()> {
        if self.cancel.is_cancelled() { Err(FleetError::cancelled()) } else { Ok(()) }
    }
    /// Sleep that ends early (with `Cancelled`) when the job is cancelled.
    pub async fn sleep(&self, d: Duration) -> Result<()> {
        tokio::select! {
            _ = tokio::time::sleep(d) => Ok(()),
            _ = self.cancel.cancelled() => Err(FleetError::cancelled()),
        }
    }
    pub fn params<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.params.clone()).map_err(|e| FleetError::permanent(format!("bad job parameters: {e}")))
    }
}

pub type RunnerFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
pub type Runner = Arc<dyn Fn(JobContext) -> RunnerFuture + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub restores: usize,
    pub downloads: usize,
    pub other: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { restores: 4, downloads: 2, other: 16 }
    }
}

struct EngineInner {
    store: Arc<Store>,
    runners: RwLock<HashMap<JobKind, Runner>>,
    jobs: Mutex<HashMap<JobId, Arc<JobShared>>>,
    cancels: Mutex<HashMap<JobId, CancellationToken>>,
    device_busy: Mutex<HashMap<DeviceKey, JobId>>,
    limits: HashMap<Resource, Arc<Semaphore>>,
    log_dir: PathBuf,
    revision: Arc<AtomicU64>,
    shutdown: CancellationToken,
    grace: Duration,
    /// Jobs are submitted from the UI thread, which isn't inside the runtime.
    runtime: tokio::runtime::Handle,
}

#[derive(Clone)]
pub struct JobEngine {
    inner: Arc<EngineInner>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl JobEngine {
    /// Load history and mark jobs that were active when the app last stopped as
    /// interrupted. Must be called inside a Tokio runtime.
    pub fn new(store: Arc<Store>, log_dir: PathBuf, limits: Limits) -> Result<Self> {
        std::fs::create_dir_all(&log_dir)?;
        let revision = Arc::new(AtomicU64::new(1));
        let inner = Arc::new(EngineInner {
            store: store.clone(),
            runners: RwLock::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            device_busy: Mutex::new(HashMap::new()),
            limits: HashMap::from([
                (Resource::Restore, Arc::new(Semaphore::new(limits.restores.max(1)))),
                (Resource::Download, Arc::new(Semaphore::new(limits.downloads.max(1)))),
                (Resource::Other, Arc::new(Semaphore::new(limits.other.max(1)))),
            ]),
            log_dir,
            revision: revision.clone(),
            shutdown: CancellationToken::new(),
            grace: Duration::from_secs(15),
            runtime: tokio::runtime::Handle::current(),
        });

        for (mut view, spec) in store.load_jobs(500)? {
            if view.state.is_active() {
                view.state = JobState::Interrupted;
                view.finished_at = Some(now_secs());
                view.next_retry_at = None;
                view.error = Some(FleetError::transient(
                    "The app stopped while this job was running. Check the device, then use Run again.",
                ));
                store.save_job(&view, &spec)?;
            }
            let shared = Arc::new(JobShared {
                view: Mutex::new(view.clone()),
                log: Mutex::new(read_log_tail(&inner.log_dir, &view.id)),
                log_file: Mutex::new(None),
                clock: Instant::now(),
                last_activity_ms: AtomicU64::new(0),
                dirty: AtomicBool::new(false),
                revision: revision.clone(),
                progressed: AtomicBool::new(false),
            });
            lock(&inner.jobs).insert(view.id.clone(), shared);
        }

        // Progress updates are frequent; persist them in batches.
        let flusher = Arc::downgrade(&inner);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(inner) = flusher.upgrade() else { break };
                flush_dirty(&inner);
                if inner.shutdown.is_cancelled() {
                    break;
                }
            }
        });

        Ok(Self { inner })
    }

    pub fn register(&self, kind: JobKind, runner: impl Fn(JobContext) -> RunnerFuture + Send + Sync + 'static) {
        self.inner.runners.write().unwrap_or_else(|p| p.into_inner()).insert(kind, Arc::new(runner));
    }

    /// Changes whenever any job changes; cheap way for the UI to skip work.
    pub fn revision(&self) -> u64 {
        self.inner.revision.load(Ordering::Relaxed)
    }

    pub fn submit(&self, spec: JobSpec) -> Result<JobId> {
        self.submit_inner(spec, None)
    }

    fn submit_inner(&self, spec: JobSpec, retry_of: Option<JobId>) -> Result<JobId> {
        if self.inner.shutdown.is_cancelled() {
            return Err(FleetError::permanent("The app is shutting down"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        if let Some(dev) = &spec.device {
            let mut busy = lock(&self.inner.device_busy);
            if let Some(other) = busy.get(dev) {
                let title = lock(&self.inner.jobs).get(other).map(|j| j.view().title.clone()).unwrap_or_default();
                return Err(FleetError::needs_user(format!("This device is busy with \"{title}\". Wait for it or cancel it first.")));
            }
            busy.insert(dev.clone(), id.clone());
        }
        let view = JobView {
            id: id.clone(),
            kind: spec.kind,
            title: spec.title.clone(),
            device: spec.device.clone(),
            state: JobState::Queued,
            attempt: 0,
            max_attempts: spec.policy.max_attempts.max(1),
            stage: None,
            progress: None,
            error: None,
            created_at: now_secs(),
            started_at: None,
            finished_at: None,
            next_retry_at: None,
            retry_of,
        };
        let log_file = OpenOptions::new().create(true).append(true).open(self.inner.log_dir.join(format!("{id}.log"))).ok();
        let shared = Arc::new(JobShared {
            view: Mutex::new(view.clone()),
            log: Mutex::new(VecDeque::new()),
            log_file: Mutex::new(log_file),
            clock: Instant::now(),
            last_activity_ms: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            revision: self.inner.revision.clone(),
            progressed: AtomicBool::new(false),
        });
        if let Err(e) = self.inner.store.save_job(&view, &spec) {
            if let Some(dev) = &spec.device {
                lock(&self.inner.device_busy).remove(dev);
            }
            return Err(e);
        }
        let cancel = self.inner.shutdown.child_token();
        lock(&self.inner.jobs).insert(id.clone(), shared.clone());
        lock(&self.inner.cancels).insert(id.clone(), cancel.clone());
        shared.changed();
        shared.log_line(&format!("Job created: {}", spec.title));
        self.inner.runtime.spawn(supervise(self.inner.clone(), shared, spec, cancel));
        Ok(id)
    }

    /// Start a finished job again with the same parameters, as a new job.
    pub fn run_again(&self, id: &str) -> Result<JobId> {
        let state = self.get(id).map(|v| v.state).ok_or_else(|| FleetError::permanent("No such job"))?;
        if state.is_active() {
            return Err(FleetError::needs_user("That job is still running"));
        }
        let spec = self.inner.store.get_spec(id)?.ok_or_else(|| FleetError::permanent("The job's settings were not saved"))?;
        self.submit_inner(spec, Some(id.to_string()))
    }

    /// Like `run_again`, with changed parameters (e.g. a different restore engine).
    pub fn run_again_with(&self, id: &str, edit: impl FnOnce(&mut JobSpec)) -> Result<JobId> {
        let mut spec = self.inner.store.get_spec(id)?.ok_or_else(|| FleetError::permanent("The job's settings were not saved"))?;
        edit(&mut spec);
        self.submit_inner(spec, Some(id.to_string()))
    }

    pub fn cancel(&self, id: &str) -> bool {
        match lock(&self.inner.cancels).get(id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    pub fn get(&self, id: &str) -> Option<JobView> {
        lock(&self.inner.jobs).get(id).map(|j| j.view().clone())
    }

    /// All jobs, newest first.
    pub fn snapshot(&self) -> Vec<JobView> {
        let mut v: Vec<JobView> = lock(&self.inner.jobs).values().map(|j| j.view().clone()).collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
        v
    }

    pub fn log_tail(&self, id: &str, max: usize) -> Vec<String> {
        lock(&self.inner.jobs)
            .get(id)
            .map(|j| {
                let log = lock(&j.log);
                log.iter().skip(log.len().saturating_sub(max)).cloned().collect()
            })
            .unwrap_or_default()
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.inner.log_dir.join(format!("{id}.log"))
    }

    pub fn busy_devices(&self) -> HashSet<DeviceKey> {
        lock(&self.inner.device_busy).keys().cloned().collect()
    }

    pub fn clear_finished(&self) -> Result<()> {
        let finished = [JobState::Succeeded, JobState::Failed, JobState::Cancelled, JobState::Interrupted];
        self.inner.store.delete_jobs_in_states(&finished)?;
        lock(&self.inner.jobs).retain(|_, j| j.view().state.is_active());
        self.inner.revision.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Cancel everything and wait (bounded) for jobs to stop; used on app exit.
    pub async fn shutdown(&self, wait: Duration) {
        self.inner.shutdown.cancel();
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline && self.snapshot().iter().any(|j| j.state.is_active()) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        flush_dirty(&self.inner);
    }

    /// Wait until no job is active. For tests and orderly shutdown.
    pub async fn wait_idle(&self) {
        while self.snapshot().iter().any(|j| j.state.is_active()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn read_log_tail(dir: &std::path::Path, id: &str) -> VecDeque<String> {
    let Ok(text) = std::fs::read_to_string(dir.join(format!("{id}.log"))) else { return VecDeque::new() };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_RING)..].iter().map(|s| s.to_string()).collect()
}

fn flush_dirty(inner: &EngineInner) {
    let jobs: Vec<Arc<JobShared>> = lock(&inner.jobs).values().cloned().collect();
    for j in jobs {
        if j.dirty.swap(false, Ordering::Relaxed) {
            let view = j.view().clone();
            if let Ok(Some(spec)) = inner.store.get_spec(&view.id) {
                let _ = inner.store.save_job(&view, &spec);
            }
        }
    }
}

fn persist(inner: &EngineInner, shared: &JobShared, spec: &JobSpec) {
    shared.dirty.store(false, Ordering::Relaxed);
    let view = shared.view().clone();
    if let Err(e) = inner.store.save_job(&view, spec) {
        tracing::error!("could not save job {}: {e}", view.id);
    }
    inner.revision.fetch_add(1, Ordering::Relaxed);
}

fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

async fn supervise(inner: Arc<EngineInner>, shared: Arc<JobShared>, spec: JobSpec, cancel: CancellationToken) {
    let policy = spec.policy;
    let max_attempts = policy.max_attempts.max(1);
    let semaphore = inner.limits[&spec.kind.resource()].clone();
    let finish = |state: JobState, error: Option<FleetError>| {
        let mut v = shared.view();
        v.state = state;
        v.error = error;
        v.finished_at = Some(now_secs());
        v.next_retry_at = None;
        if state == JobState::Succeeded && v.progress.is_some() {
            v.progress = Some(100.0);
        }
        drop(v);
        shared.log_line(&format!("Finished: {}", state.as_str()));
        persist(&inner, &shared, &spec);
    };

    // Attempts that made progress don't count against `max_attempts`: a 10 GB
    // download over a flaky link can legitimately need dozens of resumes. A hard
    // cap still stops a job that keeps failing after each tiny step forward.
    const HARD_CAP: u32 = 60;
    let mut attempt = 0;
    let mut total_attempts = 0u32;
    loop {
        attempt += 1;
        total_attempts += 1;
        shared.progressed.store(false, Ordering::Relaxed);
        {
            let mut v = shared.view();
            v.attempt = attempt;
            v.state = JobState::Queued;
            v.next_retry_at = None;
            if semaphore.available_permits() == 0 {
                v.stage = Some("Waiting for a free slot".into());
            }
        }
        persist(&inner, &shared, &spec);

        let permit = tokio::select! {
            p = semaphore.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => { finish(JobState::Failed, Some(FleetError::permanent("Job engine stopped"))); break; }
            },
            _ = cancel.cancelled() => { finish(JobState::Cancelled, None); break; }
        };

        let runner = inner.runners.read().unwrap_or_else(|p| p.into_inner()).get(&spec.kind).cloned();
        let Some(runner) = runner else {
            finish(JobState::Failed, Some(FleetError::permanent(format!("No handler for {:?} jobs", spec.kind))));
            break;
        };

        {
            let mut v = shared.view();
            v.state = JobState::Running;
            v.started_at.get_or_insert(now_secs());
            v.stage = None;
            v.error = None;
        }
        if total_attempts > 1 {
            shared.log_line(&format!("Attempt {total_attempts}"));
        }
        persist(&inner, &shared, &spec);
        shared.touch();

        let attempt_cancel = cancel.child_token();
        let ctx = JobContext { shared: shared.clone(), cancel: attempt_cancel.clone(), attempt, params: spec.params.clone() };
        let mut handle = tokio::spawn(runner(ctx));
        let attempt_started = Instant::now();
        let stall = Duration::from_secs(policy.stall_timeout_s.max(1));
        let limit = policy.attempt_timeout_s.map(Duration::from_secs);

        let outcome: Result<()> = loop {
            tokio::select! {
                res = &mut handle => break match res {
                    Ok(r) => r,
                    Err(e) if e.is_panic() => Err(FleetError::permanent(format!("Internal error (the job crashed): {}", panic_message(e.into_panic())))),
                    Err(_) => Err(FleetError::cancelled()),
                },
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    let timed_out = limit.is_some_and(|l| attempt_started.elapsed() > l);
                    if shared.idle_for() > stall || timed_out {
                        attempt_cancel.cancel();
                        stop_within(&mut handle, inner.grace).await;
                        let why = if timed_out {
                            format!("Took longer than {} minutes", limit.unwrap_or_default().as_secs() / 60)
                        } else {
                            format!("No progress for {} minutes", stall.as_secs().div_ceil(60))
                        };
                        break Err(FleetError::transient(why));
                    }
                }
                _ = cancel.cancelled() => {
                    attempt_cancel.cancel();
                    stop_within(&mut handle, inner.grace).await;
                    break Err(FleetError::cancelled());
                }
            }
        };
        drop(permit);

        match outcome {
            Ok(()) => {
                finish(JobState::Succeeded, None);
                break;
            }
            Err(e) if e.class == ErrorClass::Cancelled || cancel.is_cancelled() => {
                finish(JobState::Cancelled, None);
                break;
            }
            Err(e) if e.is_retryable() && (attempt < max_attempts || (shared.progressed.load(Ordering::Relaxed) && total_attempts < HARD_CAP)) => {
                if attempt >= max_attempts {
                    // Progress was made: start the count over.
                    attempt = 0;
                }
                let delay = policy.delay(attempt.max(1));
                shared.log_line(&format!("Attempt {total_attempts} failed: {e}. Retrying in {}s", delay.as_secs()));
                {
                    let mut v = shared.view();
                    v.state = JobState::Retrying;
                    v.error = Some(e);
                    v.next_retry_at = Some(now_secs() + delay.as_secs());
                }
                persist(&inner, &shared, &spec);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => continue,
                    _ = cancel.cancelled() => { finish(JobState::Cancelled, None); break; }
                }
            }
            Err(e) => {
                shared.log_line(&format!("Failed: {e}"));
                finish(JobState::Failed, Some(e));
                break;
            }
        }
    }

    lock(&inner.cancels).remove(&shared.view().id);
    if let Some(dev) = &spec.device {
        let id = shared.view().id.clone();
        let mut busy = lock(&inner.device_busy);
        if busy.get(dev) == Some(&id) {
            busy.remove(dev);
        }
    }
    inner.revision.fetch_add(1, Ordering::Relaxed);
}

/// Give a cancelled task time to clean up (e.g. reboot a device out of a
/// half-finished state), then abort it.
async fn stop_within(handle: &mut tokio::task::JoinHandle<Result<()>>, grace: Duration) {
    if tokio::time::timeout(grace, &mut *handle).await.is_err() {
        handle.abort();
        let _ = handle.await;
    }
}
