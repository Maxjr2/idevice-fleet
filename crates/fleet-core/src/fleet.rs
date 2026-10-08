//! The facade the UI uses: owns the registry, backend and job engine.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::devices::{Health, Registry};
use crate::error::{FleetError, Result};
use crate::jobs::{JobContext, JobEngine, JobId, JobKind, JobSpec, Limits, RetryPolicy, RunnerFuture};
use crate::model::{Device, DeviceKey};
use crate::native::NativeBackend;
use crate::store::Store;

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub limits: Limits,
}

pub struct Paths {
    pub data: PathBuf,
    pub logs: PathBuf,
    pub job_logs: PathBuf,
    pub firmware: PathBuf,
    pub backups: PathBuf,
}

impl Paths {
    fn new(data: &Path) -> Self {
        Self {
            data: data.to_path_buf(),
            logs: data.join("logs"),
            job_logs: data.join("logs").join("jobs"),
            firmware: data.join("firmware"),
            backups: data.join("backups"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UdidParams {
    udid: String,
    ecid: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EcidParams {
    ecid: u64,
}

pub struct Fleet {
    pub engine: JobEngine,
    pub registry: Arc<Registry>,
    pub paths: Paths,
    backend: Arc<NativeBackend>,
    shutdown: CancellationToken,
    _instance_lock: File,
}

impl Fleet {
    /// Open the data folder and start watching devices. Call inside a Tokio runtime.
    pub fn start(config: Config) -> Result<Self> {
        let paths = Paths::new(&config.data_dir);
        for d in [&paths.data, &paths.logs, &paths.job_logs, &paths.firmware, &paths.backups] {
            std::fs::create_dir_all(d).map_err(|e| FleetError::permanent(format!("Can't create {}: {e}", d.display())))?;
        }
        let lock = File::create(paths.data.join("fleet.lock"))?;
        if lock.try_lock().is_err() {
            return Err(FleetError::permanent(format!(
                "iDevice Fleet is already running with the data folder {}. Close the other window first.",
                paths.data.display()
            )));
        }

        let store = Arc::new(Store::open(&paths.data.join("fleet.db"))?);
        let registry = Arc::new(Registry::new(Some(store.clone())));
        let engine = JobEngine::new(store, paths.job_logs.clone(), config.limits)?;
        let backend = NativeBackend::new(registry.clone());
        let shutdown = CancellationToken::new();
        backend.start(shutdown.clone());

        let fleet = Self { engine, registry, paths, backend, shutdown, _instance_lock: lock };
        fleet.register_runners();
        Ok(fleet)
    }

    fn register_runners(&self) {
        let b = self.backend.clone();
        self.engine.register(JobKind::Pair, move |ctx: JobContext| {
            let b = b.clone();
            Box::pin(async move {
                let p: UdidParams = ctx.params()?;
                b.pair(&ctx, &p.udid).await
            }) as RunnerFuture
        });
        let b = self.backend.clone();
        self.engine.register(JobKind::EnterRecovery, move |ctx: JobContext| {
            let b = b.clone();
            Box::pin(async move {
                let p: UdidParams = ctx.params()?;
                b.enter_recovery(&ctx, &p.udid, p.ecid).await
            }) as RunnerFuture
        });
        let b = self.backend.clone();
        self.engine.register(JobKind::ExitRecovery, move |ctx: JobContext| {
            let b = b.clone();
            Box::pin(async move {
                let p: EcidParams = ctx.params()?;
                b.exit_recovery(&ctx, p.ecid).await
            }) as RunnerFuture
        });
    }

    pub fn devices(&self) -> Vec<Device> {
        self.registry.snapshot(&self.engine.busy_devices())
    }

    /// (usbmuxd, USB) health for the status bar.
    pub fn health(&self) -> (Health, Health) {
        self.registry.health()
    }

    /// Changes whenever devices or jobs change.
    pub fn revision(&self) -> u64 {
        self.registry.revision().wrapping_add(self.engine.revision())
    }

    fn device(&self, key: &DeviceKey) -> Result<Device> {
        self.devices().into_iter().find(|d| &d.key == key).ok_or_else(|| FleetError::needs_user("That device is no longer connected"))
    }

    fn normal_udid(&self, key: &DeviceKey) -> Result<String> {
        self.registry
            .normal_udid(key)
            .map(|(u, _)| u)
            .ok_or_else(|| FleetError::needs_user("This needs the device in normal mode: booted and connected"))
    }

    pub fn pair(&self, key: &DeviceKey) -> Result<JobId> {
        let d = self.device(key)?;
        let udid = self.normal_udid(key)?;
        let spec = JobSpec::new(JobKind::Pair, format!("Pair {}", d.label()), Some(key.clone()), RetryPolicy::quick())
            .with_params(UdidParams { udid, ecid: d.ecid });
        self.engine.submit(spec)
    }

    pub fn enter_recovery(&self, key: &DeviceKey) -> Result<JobId> {
        let d = self.device(key)?;
        let udid = self.normal_udid(key)?;
        let spec = JobSpec::new(JobKind::EnterRecovery, format!("Enter recovery: {}", d.label()), Some(key.clone()), RetryPolicy::quick())
            .with_params(UdidParams { udid, ecid: d.ecid });
        self.engine.submit(spec)
    }

    pub fn exit_recovery(&self, key: &DeviceKey) -> Result<JobId> {
        let d = self.device(key)?;
        let ecid = d.ecid.ok_or_else(|| FleetError::needs_user("The device's ECID isn't known"))?;
        let spec = JobSpec::new(JobKind::ExitRecovery, format!("Exit recovery: {}", d.label()), Some(key.clone()), RetryPolicy::quick())
            .with_params(EcidParams { ecid });
        self.engine.submit(spec)
    }

    /// Stop watchers and give running jobs a moment to stop cleanly.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        self.engine.shutdown(Duration::from_secs(20)).await;
    }
}
