//! The facade the UI uses: owns the registry, backend and job engine.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::devices::{Health, Registry};
use crate::error::{FleetError, Result};
use crate::firmware::{self, Catalog, DownloadParams, Firmware, LocalIpsw};
use crate::jobs::{JobContext, JobEngine, JobId, JobKind, JobSpec, Limits, RetryPolicy, RunnerFuture};
use crate::model::{Device, DeviceKey, PairState};
use crate::restore::{Check, Engine, RestoreParams};
use std::collections::HashMap;
use crate::native::NativeBackend;
use crate::store::Store;

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub limits: Limits,
    /// Show invented devices and jobs and don't touch USB. For screenshots and trying the UI.
    pub demo: bool,
    /// Look for a newer release on GitHub at startup.
    pub check_updates: bool,
}

pub struct Paths {
    pub data: PathBuf,
    pub logs: PathBuf,
    pub job_logs: PathBuf,
    pub firmware: PathBuf,
    pub backups: PathBuf,
    /// Scratch space: unpacked firmware for running restores. Safe to delete.
    pub cache: PathBuf,
    /// Downloaded app updates (`.deb` files).
    pub updates: PathBuf,
}

impl Paths {
    fn new(data: &Path) -> Self {
        Self {
            data: data.to_path_buf(),
            logs: data.join("logs"),
            job_logs: data.join("logs").join("jobs"),
            firmware: data.join("firmware"),
            backups: data.join("backups"),
            cache: data.join("cache"),
            updates: data.join("updates"),
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

/// Result slot a lookup writes into; the UI polls it.
#[derive(Debug, Clone, Default)]
pub enum Lookup {
    #[default]
    Idle,
    Loading,
    Done(Box<Catalog>),
    Failed(String),
}

/// Progress of "download the newest signed firmware for this model".
#[derive(Debug, Clone)]
pub enum LatestState {
    Looking,
    Downloading(JobId),
    /// Already in the library.
    Have(String),
    Failed(String),
}

/// What is using disk space.
#[derive(Debug, Clone, Default)]
pub struct StorageReport {
    pub firmware: u64,
    pub backups: u64,
    pub logs: u64,
    /// Unfinished downloads (`.part` files).
    pub partial_downloads: u64,
    /// Unpacked files of restores (normally empty, since each restore cleans up after itself).
    pub restore_cache: u64,
}

impl StorageReport {
    pub fn cache_total(&self) -> u64 {
        self.partial_downloads + self.restore_cache
    }
}

#[derive(Debug, Clone)]
pub struct RestoreTarget {
    pub key: DeviceKey,
    pub ipsw_file: String,
}

pub struct Fleet {
    update: Arc<Mutex<crate::update::UpdateState>>,
    storage: Arc<Mutex<StorageReport>>,
    catalogs: Arc<Mutex<HashMap<String, Catalog>>>,
    latest: Arc<Mutex<HashMap<String, LatestState>>>,
    max_restores: usize,
    pub engine: JobEngine,
    pub registry: Arc<Registry>,
    pub paths: Paths,
    backend: Arc<NativeBackend>,
    runtime: tokio::runtime::Handle,
    library: Arc<Mutex<Vec<LocalIpsw>>>,
    backups: Arc<Mutex<Vec<crate::backup::BackupEntry>>>,
    shutdown: CancellationToken,
    _instance_lock: File,
}

impl Fleet {
    /// Open the data folder and start watching devices. Call inside a Tokio runtime.
    pub fn start(config: Config) -> Result<Self> {
        let paths = Paths::new(&config.data_dir);
        for d in [&paths.data, &paths.logs, &paths.job_logs, &paths.firmware, &paths.backups, &paths.cache, &paths.updates] {
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
        if !config.demo {
            backend.start(shutdown.clone());
        }

        // Anything left in the scratch folder belongs to a restore that was cut off by a crash.
        clear_dir_contents(&paths.cache);
        let library = Arc::new(Mutex::new(Vec::new()));
        let fleet = Self { update: Arc::new(Mutex::new(crate::update::UpdateState::Unknown)), storage: Arc::new(Mutex::new(StorageReport::default())), catalogs: Arc::new(Mutex::new(HashMap::new())), latest: Arc::new(Mutex::new(HashMap::new())), max_restores: config.limits.restores.max(1), engine, registry, paths, backend, runtime: tokio::runtime::Handle::current(), library, backups: Arc::new(Mutex::new(Vec::new())), shutdown, _instance_lock: lock };
        fleet.refresh_library();
        fleet.refresh_backups();
        fleet.register_runners();
        if config.demo {
            crate::demo::load(&fleet)?;
        } else if config.check_updates {
            fleet.check_for_updates();
        }
        Ok(fleet)
    }

    fn register_runners(&self) {
        self.engine.register(JobKind::Install, |ctx: JobContext| {
            Box::pin(async move {
                let path: PathBuf = ctx.params()?;
                crate::update::run_installer(&ctx, &path).await
            }) as RunnerFuture
        });
        let b = self.backend.clone();
        self.engine.register(JobKind::Restore, move |ctx: JobContext| {
            let b = b.clone();
            Box::pin(async move {
                let p: RestoreParams = ctx.params()?;
                crate::restore::run(&b, &ctx, &p, find_idevicerestore().as_deref()).await
            }) as RunnerFuture
        });
        let (lib_dir, lib) = (self.paths.firmware.clone(), self.library.clone());
        self.engine.register(JobKind::Download, move |ctx: JobContext| {
            let (lib_dir, lib) = (lib_dir.clone(), lib.clone());
            Box::pin(async move {
                // Firmware downloads carry plain parameters; update downloads also name a checksum file.
                let raw: serde_json::Value = ctx.params()?;
                let (mut p, sums_url): (DownloadParams, Option<String>) = match raw.get("download") {
                    Some(d) => (serde_json::from_value(d.clone()).map_err(|e| FleetError::permanent(format!("bad job parameters: {e}")))?, raw.get("sums_url").and_then(|v| v.as_str()).map(str::to_string)),
                    None => (serde_json::from_value(raw).map_err(|e| FleetError::permanent(format!("bad job parameters: {e}")))?, None),
                };
                let c = ctx.clone();
                let r = tokio::task::spawn_blocking(move || {
                    if p.source == firmware::Source::GitHubRelease {
                        let name = crate::update::check_update_url(&p.url)?;
                        let url = sums_url.ok_or_else(|| FleetError::permanent("This release has no SHA256SUMS file, so the download can't be verified"))?;
                        let text = firmware::fetch_text(&url)?;
                        p.sha256 = Some(crate::update::checksum_for(&text, &name).ok_or_else(|| FleetError::permanent("The release's checksum list doesn't include this file"))?);
                    }
                    firmware::download(&c, &p)
                }).await.map_err(|e| FleetError::permanent(format!("download crashed: {e}")))?;
                *lib.lock().unwrap_or_else(|p| p.into_inner()) = firmware::scan_library(&lib_dir);
                r
            }) as RunnerFuture
        });
        let b = self.backend.clone();
        self.engine.register(JobKind::Pair, move |ctx: JobContext| {
            let b = b.clone();
            Box::pin(async move {
                let p: UdidParams = ctx.params()?;
                b.pair(&ctx, &p.udid).await
            }) as RunnerFuture
        });
        let (b, root, cache) = (self.backend.clone(), self.paths.backups.clone(), self.backups.clone());
        self.engine.register(JobKind::Backup, move |ctx: JobContext| {
            let (b, root, cache) = (b.clone(), root.clone(), cache.clone());
            Box::pin(async move {
                let p: UdidParams = ctx.params()?;
                let r = b.backup(&ctx, &p.udid, &root).await;
                let dir = root.clone();
                if let Ok(found) = tokio::task::spawn_blocking(move || crate::backup::list_backups(&dir)).await {
                    *cache.lock().unwrap_or_else(|p| p.into_inner()) = found;
                }
                r
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

    /// Rescan the firmware folder in the background.
    pub fn refresh_library(&self) {
        let (dir, lib) = (self.paths.firmware.clone(), self.library.clone());
        self.runtime.spawn_blocking(move || {
            let found = firmware::scan_library(&dir);
            *lib.lock().unwrap_or_else(|p| p.into_inner()) = found;
        });
    }

    pub fn library(&self) -> Vec<LocalIpsw> {
        self.library.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Look up the firmware ipsw.me lists for a model. Retries network errors a few times.
    pub async fn lookup_firmware(&self, identifier: &str) -> Result<Catalog> {
        let id = identifier.trim().to_string();
        let mut last = None;
        for attempt in 0..3u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt * 2)).await;
            }
            let id = id.clone();
            let r = tokio::task::spawn_blocking(move || firmware::fetch_catalog(&id)).await.map_err(|e| FleetError::permanent(format!("lookup crashed: {e}")))?;
            match r {
                Ok(c) => {
                    self.catalogs.lock().unwrap_or_else(|p| p.into_inner()).insert(c.identifier.clone(), c.clone());
                    return Ok(c);
                }
                Err(e) if e.is_retryable() => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| FleetError::transient("lookup failed")))
    }

    /// Start a lookup in the background, writing progress into `slot`.
    pub fn start_lookup(self: &Arc<Self>, identifier: &str, slot: Arc<Mutex<Lookup>>) {
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Lookup::Loading;
        let (me, id) = (self.clone(), identifier.to_string());
        self.runtime.spawn(async move {
            let r = match me.lookup_firmware(&id).await {
                Ok(c) => Lookup::Done(Box::new(c)),
                Err(e) => Lookup::Failed(e.message),
            };
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = r;
        });
    }

    /// Whether Apple still signs this build for the model: `None` if we haven't been able to look it up.
    pub fn is_signed(&self, model: &str, build: &str) -> Option<bool> {
        let cats = self.catalogs.lock().unwrap_or_else(|p| p.into_inner());
        cats.get(model)?.firmwares.iter().find(|f| f.build == build).map(|f| f.signed)
    }

    /// Fetch the catalog for a model in the background (so `is_signed` can answer).
    pub fn ensure_catalog(self: &Arc<Self>, model: &str) {
        if self.catalogs.lock().unwrap_or_else(|p| p.into_inner()).contains_key(model) {
            return;
        }
        let (me, m) = (self.clone(), model.to_string());
        self.runtime.spawn(async move {
            let _ = me.lookup_firmware(&m).await;
        });
    }

    /// One click: find the newest signed firmware for a model and download it.
    pub fn download_latest_signed(self: &Arc<Self>, model: &str) {
        let key = model.to_string();
        {
            let mut l = self.latest.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(l.get(&key), Some(LatestState::Looking | LatestState::Downloading(_))) {
                return;
            }
            l.insert(key.clone(), LatestState::Looking);
        }
        let me = self.clone();
        self.runtime.spawn(async move {
            let state = match me.lookup_firmware(&key).await {
                Err(e) => LatestState::Failed(e.message),
                Ok(cat) => match cat.firmwares.iter().find(|f| f.signed) {
                    None => LatestState::Failed("Apple isn't signing any firmware for this model right now".into()),
                    Some(fw) => {
                        let have = me.library().into_iter().find(|l| l.build.as_deref() == Some(fw.build.as_str()) && l.product_types.contains(&key));
                        match have {
                            Some(l) => LatestState::Have(l.file),
                            None => match me.download_firmware(fw) {
                                Ok(id) => LatestState::Downloading(id),
                                // Already downloading: find that job.
                                Err(e) => match me.engine.snapshot().into_iter().find(|j| j.kind == JobKind::Download && j.state.is_active()) {
                                    Some(j) => LatestState::Downloading(j.id),
                                    None => LatestState::Failed(e.message),
                                },
                            },
                        }
                    }
                },
            };
            me.latest.lock().unwrap_or_else(|p| p.into_inner()).insert(key, state);
        });
    }

    pub fn latest_state(&self, model: &str) -> Option<LatestState> {
        self.latest.lock().unwrap_or_else(|p| p.into_inner()).get(model).cloned()
    }

    pub fn external_engine_available(&self) -> bool {
        find_idevicerestore().is_some()
    }

    /// Pre-flight checks for one device and firmware, in plain language.
    pub fn check_restore(&self, key: &DeviceKey, ipsw_file: Option<&str>, erase: bool, concurrent: usize) -> Vec<Check> {
        let Some(device) = self.devices().into_iter().find(|d| &d.key == key) else {
            return vec![Check { level: crate::restore::Level::Block, text: "The device is no longer connected".into() }];
        };
        let lib = self.library();
        let ipsw = ipsw_file.and_then(|f| lib.iter().find(|l| l.file == f));
        let signed = match (&device.product_type, ipsw.and_then(|i| i.build.as_deref())) {
            (Some(m), Some(b)) => self.is_signed(m, b),
            _ => None,
        };
        let share = concurrent.clamp(1, self.max_restores) as u64;
        let free = crate::backup::free_space(&self.paths.cache).map(|f| f / share);
        crate::restore::preflight(&device, ipsw, signed, erase, free)
    }

    /// Start restores. One job per device; at most the configured number run at once.
    pub fn restore(&self, targets: &[RestoreTarget], erase: bool, engine: Engine) -> Vec<(DeviceKey, Result<JobId>)> {
        let lib = self.library();
        let devices = self.devices();
        targets
            .iter()
            .map(|t| {
                let r = (|| {
                    let d = devices.iter().find(|d| d.key == t.key).ok_or_else(|| FleetError::needs_user("That device is no longer connected"))?;
                    let ecid = d.ecid.ok_or_else(|| FleetError::needs_user("The device's ID isn't known yet. Reconnect it and try again"))?;
                    let ipsw = lib.iter().find(|l| l.file == t.ipsw_file).ok_or_else(|| FleetError::permanent("That firmware is no longer in the library"))?;
                    let checks = self.check_restore(&t.key, Some(&t.ipsw_file), erase, targets.len());
                    if let Some(c) = checks.iter().find(|c| c.level == crate::restore::Level::Block) {
                        return Err(FleetError::permanent(c.text.clone()));
                    }
                    let udid = self.registry.normal_udid(&t.key).map(|(u, _)| u);
                    let params = RestoreParams { ecid, udid, ipsw: ipsw.path.clone(), erase, engine, cache_dir: self.paths.cache.join(uuid::Uuid::new_v4().to_string()) };
                    let title = format!("{} {} to iOS {}", if erase { "Reset" } else { "Update" }, d.label(), ipsw.version.clone().unwrap_or_else(|| ipsw.file.clone()));
                    let policy = RetryPolicy { max_attempts: 2, base_delay_ms: 20_000, max_delay_ms: 60_000, stall_timeout_s: 900, attempt_timeout_s: Some(4 * 3600) };
                    self.engine.submit(JobSpec::new(JobKind::Restore, title, Some(t.key.clone()), policy).with_params(params))
                })();
                (t.key.clone(), r)
            })
            .collect()
    }

    /// Run a failed restore again, this time with a different engine.
    pub fn retry_restore_with(&self, job: &str, engine: Engine) -> Result<JobId> {
        self.engine.run_again_with(job, |spec| {
            if let Ok(mut p) = serde_json::from_value::<RestoreParams>(spec.params.clone()) {
                p.engine = engine;
                p.cache_dir = self.paths.cache.join(uuid::Uuid::new_v4().to_string());
                spec.params = serde_json::to_value(p).unwrap_or_default();
            }
        })
    }

    /// Cached disk-usage numbers for the UI; `refresh_storage` updates them in the background.
    pub fn storage(&self) -> StorageReport {
        self.storage.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn refresh_storage(self: &Arc<Self>) {
        let me = self.clone();
        self.runtime.spawn(async move {
            let r = me.storage_report().await;
            *me.storage.lock().unwrap_or_else(|p| p.into_inner()) = r;
        });
    }

    pub async fn storage_report(&self) -> StorageReport {
        let p = (self.paths.firmware.clone(), self.paths.backups.clone(), self.paths.logs.clone(), self.paths.cache.clone());
        tokio::task::spawn_blocking(move || {
            let part: u64 = std::fs::read_dir(&p.0).into_iter().flatten().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".part")).map(|e| e.metadata().map(|m| m.len()).unwrap_or(0)).sum();
            StorageReport { firmware: dir_bytes(&p.0).saturating_sub(part), backups: dir_bytes(&p.1), logs: dir_bytes(&p.2), partial_downloads: part, restore_cache: dir_bytes(&p.3) }
        })
        .await
        .unwrap_or_default()
    }

    /// Delete scratch files: unfinished downloads and unpacked restore files. Refuses while
    /// a restore or download is running, since it would pull files out from under it.
    pub fn clear_cache(&self) -> Result<u64> {
        if self.engine.snapshot().iter().any(|j| j.state.is_active() && matches!(j.kind, JobKind::Restore | JobKind::Download)) {
            return Err(FleetError::needs_user("A restore or download is running. Wait for it to finish or cancel it first"));
        }
        let mut freed = dir_bytes(&self.paths.cache);
        clear_dir_contents(&self.paths.cache);
        for e in std::fs::read_dir(&self.paths.firmware)?.flatten() {
            if e.file_name().to_string_lossy().ends_with(".part") {
                freed += e.metadata().map(|m| m.len()).unwrap_or(0);
                let _ = std::fs::remove_file(e.path());
            }
        }
        self.catalogs.lock().unwrap_or_else(|p| p.into_inner()).clear();
        self.latest.lock().unwrap_or_else(|p| p.into_inner()).clear();
        Ok(freed)
    }

    pub fn download_firmware(&self, fw: &Firmware) -> Result<JobId> {
        let name = firmware::check_download_url(&fw.url)?;
        if self.library().iter().any(|i| i.file == name) {
            return Err(FleetError::needs_user("That firmware is already in the library"));
        }
        if self.engine.snapshot().iter().any(|j| j.state.is_active() && j.kind == JobKind::Download && j.title.ends_with(&name)) {
            return Err(FleetError::needs_user("That firmware is already downloading"));
        }
        let params = DownloadParams { url: fw.url.clone(), dest_dir: self.paths.firmware.clone(), sha256: fw.sha256.clone(), size: Some(fw.size).filter(|s| *s > 0), source: crate::firmware::Source::Apple };
        self.engine.submit(JobSpec::new(JobKind::Download, format!("Download {name}"), None, RetryPolicy::long()).with_params(params))
    }

    /// Download the new `.deb`, verified against the release's `SHA256SUMS`.
    pub fn download_update(&self, info: &crate::update::UpdateInfo) -> Result<JobId> {
        let url = info.deb_url.clone().ok_or_else(|| FleetError::needs_user("This release has no package for your system. Open the release page instead"))?;
        let name = crate::update::check_update_url(&url)?;
        let sums_url = info.sums_url.clone();
        let dest = self.paths.updates.clone();
        // The checksum is fetched when the job starts, so a failure shows up in the job.
        let params = DownloadParams { url, dest_dir: dest, sha256: None, size: None, source: crate::firmware::Source::GitHubRelease };
        let spec = JobSpec::new(JobKind::Download, format!("Download update {name}"), None, RetryPolicy::long()).with_params(serde_json::json!({ "download": params, "sums_url": sums_url }));
        self.engine.submit(spec)
    }

    /// Install a downloaded update with the system package manager. `pkexec` shows the
    /// usual password prompt; nothing else about the system is touched.
    pub fn install_update(&self, file: &str) -> Result<JobId> {
        let path = crate::update::installable_deb(&self.paths.updates, file)?;
        self.engine.submit(JobSpec::new(JobKind::Install, "Install update", None, RetryPolicy { max_attempts: 1, base_delay_ms: 1000, max_delay_ms: 1000, stall_timeout_s: 900, attempt_timeout_s: Some(1800) }).with_params(path))
    }

    pub fn update_state(&self) -> crate::update::UpdateState {
        self.update.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Ask GitHub whether a newer release exists (in the background).
    pub fn check_for_updates(&self) {
        use crate::update::UpdateState;
        {
            let mut u = self.update.lock().unwrap_or_else(|p| p.into_inner());
            if *u == UpdateState::Checking {
                return;
            }
            *u = UpdateState::Checking;
        }
        let slot = self.update.clone();
        self.runtime.spawn(async move {
            let r = tokio::task::spawn_blocking(|| crate::update::check(env!("CARGO_PKG_VERSION"))).await;
            let state = match r {
                Ok(Ok(Some(info))) => UpdateState::Available(Box::new(info)),
                Ok(Ok(None)) => UpdateState::UpToDate,
                Ok(Err(e)) => UpdateState::Failed(e.message),
                Err(e) => UpdateState::Failed(e.to_string()),
            };
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = state;
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
        self.registry.revision().wrapping_add(self.engine.revision()).wrapping_add(self.library().len() as u64 * 1_000_003)
            .wrapping_add(self.backups().len() as u64 * 7_000_001)
            .wrapping_add(match self.update_state() { crate::update::UpdateState::Available(_) => 11, crate::update::UpdateState::Failed(_) => 13, crate::update::UpdateState::UpToDate => 17, _ => 0 })
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

    pub fn backup(&self, key: &DeviceKey) -> Result<JobId> {
        let d = self.device(key)?;
        let udid = self.normal_udid(key)?;
        if d.pair_state != PairState::Paired {
            return Err(FleetError::needs_user("Trust this computer first: unlock the device, tap Trust, then use the Trust button"));
        }
        let spec = JobSpec::new(JobKind::Backup, format!("Back up {}", d.label()), Some(key.clone()), RetryPolicy::long()).with_params(UdidParams { udid, ecid: d.ecid });
        self.engine.submit(spec)
    }

    /// Cached list; call `refresh_backups` to update it.
    pub fn backups(&self) -> Vec<crate::backup::BackupEntry> {
        self.backups.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Rescan the backup folder in the background (sizing large folders is slow).
    pub fn refresh_backups(&self) {
        let (dir, cache) = (self.paths.backups.clone(), self.backups.clone());
        self.runtime.spawn_blocking(move || {
            let found = crate::backup::list_backups(&dir);
            *cache.lock().unwrap_or_else(|p| p.into_inner()) = found;
        });
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

/// Find the `idevicerestore` tool: on PATH, or in the usual install folders.
pub fn find_idevicerestore() -> Option<PathBuf> {
    let name = if cfg!(windows) { "idevicerestore.exe" } else { "idevicerestore" };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/local/bin", "/usr/local/bin", "/opt/homebrew/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

fn dir_bytes(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

fn clear_dir_contents(p: &Path) {
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        let path = e.path();
        let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    }
}
