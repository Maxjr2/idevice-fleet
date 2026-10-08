//! Device backups through `mobilebackup2` (the protocol Finder and iTunes use),
//! plus a reader for the backups already on disk.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use idevice::IdeviceError;
use idevice::services::mobilebackup2::{BackupDelegate, BackupProgress, DirEntryInfo, FsBackupDelegate};
use serde::{Deserialize, Serialize};

use crate::jobs::JobContext;

/// Filesystem work goes to the stock delegate; this adds real free-space
/// reporting (the stock one reports a fixed value) and feeds job progress.
pub struct FleetDelegate {
    fs: FsBackupDelegate,
    ctx: JobContext,
}

impl FleetDelegate {
    pub fn new(ctx: JobContext) -> Self {
        Self { fs: FsBackupDelegate, ctx }
    }
}

pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let mut probe = path;
        // Walk up to a directory that exists.
        while !probe.exists() {
            probe = probe.parent()?;
        }
        let c = CString::new(probe.to_string_lossy().as_bytes()).ok()?;
        let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `c` is a valid NUL-terminated path and `st` is a valid out-pointer.
        if unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: statvfs succeeded, so the struct is initialised.
        let st = unsafe { st.assume_init() };
        #[allow(clippy::unnecessary_cast)] // field widths differ between platforms
        let free = Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64));
        free
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl BackupDelegate for FleetDelegate {
    fn get_free_disk_space(&self, path: &Path) -> u64 {
        free_space(path).unwrap_or(u64::MAX)
    }
    fn open_file_read<'a>(&'a self, path: &'a Path) -> BoxFut<'a, Result<Box<dyn std::io::Read + Send>, IdeviceError>> {
        self.fs.open_file_read(path)
    }
    fn create_file_write<'a>(&'a self, path: &'a Path) -> BoxFut<'a, Result<Box<dyn std::io::Write + Send>, IdeviceError>> {
        self.fs.create_file_write(path)
    }
    fn create_dir_all<'a>(&'a self, path: &'a Path) -> BoxFut<'a, Result<(), IdeviceError>> {
        self.fs.create_dir_all(path)
    }
    fn remove<'a>(&'a self, path: &'a Path) -> BoxFut<'a, Result<(), IdeviceError>> {
        self.fs.remove(path)
    }
    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFut<'a, Result<(), IdeviceError>> {
        self.fs.rename(from, to)
    }
    fn copy<'a>(&'a self, src: &'a Path, dst: &'a Path) -> BoxFut<'a, Result<(), IdeviceError>> {
        self.fs.copy(src, dst)
    }
    fn exists<'a>(&'a self, path: &'a Path) -> BoxFut<'a, bool> {
        self.fs.exists(path)
    }
    fn is_dir<'a>(&'a self, path: &'a Path) -> BoxFut<'a, bool> {
        self.fs.is_dir(path)
    }
    fn list_dir<'a>(&'a self, path: &'a Path) -> BoxFut<'a, Result<Vec<DirEntryInfo>, IdeviceError>> {
        self.fs.list_dir(path)
    }
    fn on_file_received(&self, _path: &str, count: u32) {
        if count.is_multiple_of(200) {
            self.ctx.heartbeat();
        }
    }
    fn on_progress(&self, p: BackupProgress) {
        self.ctx.heartbeat();
        if p.overall_progress >= 0.0 {
            self.ctx.progress(p.overall_progress as f32);
        }
        if p.session_bytes_done > 0 {
            self.ctx.stage(format!("Backing up · {:.2} GB received", p.session_bytes_done as f64 / 1e9));
        }
    }
}

/// A backup folder on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupEntry {
    pub folder: String,
    pub path: PathBuf,
    pub device_name: Option<String>,
    pub product_type: Option<String>,
    pub os_version: Option<String>,
    pub serial: Option<String>,
    /// Last backup, seconds since the Unix epoch.
    pub date: Option<u64>,
    pub encrypted: Option<bool>,
    /// A finished backup has a `Status.plist` marking it complete.
    pub complete: bool,
    pub size: u64,
}

pub fn list_backups(root: &Path) -> Vec<BackupEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return out };
    for e in rd.flatten() {
        let path = e.path();
        let Some(folder) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        let Ok(info) = plist::Value::from_file(path.join("Info.plist")) else { continue };
        let Some(info) = info.into_dictionary() else { continue };
        let s = |k: &str| info.get(k).and_then(|v| v.as_string()).map(str::to_string);
        let encrypted = plist::Value::from_file(path.join("Manifest.plist")).ok().and_then(|m| m.into_dictionary()).and_then(|d| d.get("IsEncrypted").and_then(|v| v.as_boolean()));
        let date = info.get("Last Backup Date").and_then(|v| v.as_date()).map(|d| std::time::SystemTime::from(d).duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
        out.push(BackupEntry {
            folder,
            device_name: s("Device Name").or_else(|| s("Display Name")),
            product_type: s("Product Type"),
            os_version: s("Product Version"),
            serial: s("Serial Number"),
            date,
            encrypted,
            complete: path.join("Status.plist").is_file(),
            size: dir_size(&path),
            path,
        });
    }
    out.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.folder.cmp(&b.folder)));
    out
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_backup_folders() {
        let root = tempfile::tempdir().unwrap();
        let b = root.path().join("00008101-AAAA");
        std::fs::create_dir(&b).unwrap();
        let mut info = plist::Dictionary::new();
        info.insert("Device Name".into(), "CEO iPhone".into());
        info.insert("Product Type".into(), "iPhone17,1".into());
        info.insert("Serial Number".into(), "ABC".into());
        plist::Value::Dictionary(info).to_file_xml(b.join("Info.plist")).unwrap();
        let mut man = plist::Dictionary::new();
        man.insert("IsEncrypted".into(), true.into());
        plist::Value::Dictionary(man).to_file_xml(b.join("Manifest.plist")).unwrap();
        std::fs::write(b.join("data.bin"), vec![0u8; 1000]).unwrap();
        std::fs::create_dir(root.path().join("junk")).unwrap();

        let list = list_backups(root.path());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].device_name.as_deref(), Some("CEO iPhone"));
        assert_eq!(list[0].encrypted, Some(true));
        assert!(!list[0].complete, "no Status.plist yet: an unfinished backup");
        assert!(list[0].size >= 1000);
        std::fs::write(b.join("Status.plist"), b"x").unwrap();
        assert!(list_backups(root.path())[0].complete);
    }

    #[test]
    fn free_space_is_reported() {
        let d = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        assert!(free_space(&d.path().join("not/yet/created")).unwrap() > 0);
        let _ = d;
    }
}
