//! Firmware library: signed-version lookup (ipsw.me), resumable verified
//! downloads from Apple's CDN, and a reader for the IPSW files on disk.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{FleetError, Result};
use crate::jobs::JobContext;

const CATALOG_URL: &str = "https://api.ipsw.me/v4/device/";
const ALLOWED_HOSTS: &[&str] = &["updates.cdn-apple.com", "updates-http.cdn-apple.com", "appldnld.apple.com", "secure-appldnld.apple.com"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Firmware {
    pub version: String,
    pub build: String,
    pub url: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub signed: bool,
    pub released: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub identifier: String,
    pub name: String,
    pub firmwares: Vec<Firmware>,
}

/// An `.ipsw` file in the library folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalIpsw {
    pub file: String,
    pub path: PathBuf,
    pub size: u64,
    pub version: Option<String>,
    pub build: Option<String>,
    pub product_types: Vec<String>,
    pub error: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .user_agent("idevice-fleet")
        .build()
        .into()
}

fn net_err(e: ureq::Error) -> FleetError {
    FleetError::transient(format!("Network problem: {e}"))
}

fn status_err(status: u16, what: &str) -> FleetError {
    match status {
        404 => FleetError::permanent(format!("{what}: not found (HTTP 404)")),
        408 | 429 | 500..=599 => FleetError::transient(format!("{what}: server busy (HTTP {status})")),
        _ => FleetError::permanent(format!("{what}: HTTP {status}")),
    }
}

pub fn valid_identifier(id: &str) -> bool {
    let Some((model, rev)) = id.split_once(',') else { return false };
    model.len() < 24
        && model.starts_with(|c: char| c.is_ascii_alphabetic())
        && model.ends_with(|c: char| c.is_ascii_digit())
        && model.chars().all(|c| c.is_ascii_alphanumeric())
        && !rev.is_empty()
        && rev.chars().all(|c| c.is_ascii_digit())
}

/// Only Apple firmware over https may be downloaded. Returns the file name.
pub fn check_download_url(url: &str) -> Result<String> {
    let rest = url.strip_prefix("https://").ok_or_else(|| FleetError::permanent("Firmware must be downloaded over https"))?;
    let (host, path) = rest.split_once('/').ok_or_else(|| FleetError::permanent("Bad firmware URL"))?;
    if !ALLOWED_HOSTS.contains(&host) {
        return Err(FleetError::permanent(format!("{host} isn't an Apple firmware server")));
    }
    let name = path.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
    if !name.to_ascii_lowercase().ends_with(".ipsw") || name.contains("..") || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._,+-".contains(c)) {
        return Err(FleetError::permanent("Unexpected firmware file name"));
    }
    Ok(name.to_string())
}

pub fn parse_catalog(json: &str, identifier: &str) -> Result<Catalog> {
    #[derive(Deserialize)]
    struct Raw {
        name: Option<String>,
        identifier: Option<String>,
        #[serde(default)]
        firmwares: Vec<Fw>,
    }
    #[derive(Deserialize)]
    struct Fw {
        version: String,
        buildid: String,
        url: String,
        #[serde(default)]
        filesize: u64,
        sha256sum: Option<String>,
        #[serde(default)]
        signed: bool,
        releasedate: Option<String>,
    }
    let raw: Raw = serde_json::from_str(json).map_err(|e| FleetError::transient(format!("Unreadable answer from ipsw.me: {e}")))?;
    let mut firmwares: Vec<Firmware> = raw
        .firmwares
        .into_iter()
        .map(|f| Firmware { version: f.version, build: f.buildid, url: f.url, size: f.filesize, sha256: f.sha256sum.filter(|s| s.len() == 64), signed: f.signed, released: f.releasedate })
        .collect();
    firmwares.sort_by_key(|f| std::cmp::Reverse(version_key(&f.version)));
    Ok(Catalog { identifier: raw.identifier.unwrap_or_else(|| identifier.into()), name: raw.name.unwrap_or_default(), firmwares })
}

pub fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().unwrap_or(0)).collect()
}

/// Look up the firmware ipsw.me lists for a model. Blocking; call from `spawn_blocking`.
pub fn fetch_catalog(identifier: &str) -> Result<Catalog> {
    if !valid_identifier(identifier) {
        return Err(FleetError::permanent("Use a model identifier such as iPad13,18 or iPhone15,2"));
    }
    let mut resp = agent().get(&format!("{CATALOG_URL}{identifier}?type=ipsw")).call().map_err(net_err)?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(if status == 404 { FleetError::permanent("ipsw.me doesn't know that model identifier") } else { status_err(status, "ipsw.me") });
    }
    let body = resp.body_mut().read_to_string().map_err(net_err)?;
    parse_catalog(&body, identifier)
}

/// Scan the library folder. Blocking.
pub fn scan_library(dir: &Path) -> Vec<LocalIpsw> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let path = e.path();
        let Some(file) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        if !file.to_ascii_lowercase().ends_with(".ipsw") {
            continue;
        }
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        let mut item = LocalIpsw { file, path: path.clone(), size, version: None, build: None, product_types: vec![], error: None };
        match read_manifest(&path) {
            Ok((v, b, p)) => (item.version, item.build, item.product_types) = (Some(v), Some(b), p),
            Err(e) => item.error = Some(e),
        }
        out.push(item);
    }
    out.sort_by(|a, b| version_key(b.version.as_deref().unwrap_or("")).cmp(&version_key(a.version.as_deref().unwrap_or(""))).then_with(|| a.file.cmp(&b.file)));
    out
}

fn read_manifest(path: &Path) -> std::result::Result<(String, String, Vec<String>), String> {
    let f = File::open(path).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("Not a readable IPSW: {e}"))?;
    let mut buf = Vec::new();
    z.by_name("BuildManifest.plist").map_err(|_| "No BuildManifest.plist: incomplete download?".to_string())?.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let v: plist::Value = plist::from_bytes(&buf).map_err(|e| format!("Bad BuildManifest: {e}"))?;
    let d = v.as_dictionary().ok_or("Bad BuildManifest")?;
    let s = |k: &str| d.get(k).and_then(|x| x.as_string()).map(str::to_string);
    let types = d.get("SupportedProductTypes").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_string().map(str::to_string)).collect()).unwrap_or_default();
    Ok((s("ProductVersion").ok_or("No version in manifest")?, s("ProductBuildVersion").unwrap_or_default(), types))
}

/// Which library files can restore a model, newest first.
pub fn matching<'a>(lib: &'a [LocalIpsw], product_type: &str) -> Vec<&'a LocalIpsw> {
    lib.iter().filter(|i| i.error.is_none() && i.product_types.iter().any(|p| p == product_type)).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadParams {
    pub url: String,
    pub dest_dir: PathBuf,
    pub sha256: Option<String>,
    pub size: Option<u64>,
}

/// Download with resume and checksum verification. Blocking; the job runner
/// calls it through `spawn_blocking`. A cancelled download keeps its `.part`
/// file so the next attempt continues where it stopped.
pub fn download(ctx: &JobContext, p: &DownloadParams) -> Result<()> {
    let name = check_download_url(&p.url)?;
    download_unchecked(ctx, p, &name)
}

/// The download itself, without the Apple-host allowlist (used by tests).
#[doc(hidden)]
pub fn download_unchecked(ctx: &JobContext, p: &DownloadParams, name: &str) -> Result<()> {
    let final_path = p.dest_dir.join(name);
    if final_path.exists() {
        ctx.stage("Already in the library");
        ctx.progress(100.0);
        return Ok(());
    }
    let part = p.dest_dir.join(format!("{name}.part"));
    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

    let mut hasher = Sha256::new();
    if have > 0 {
        ctx.stage("Checking the partial download");
        let mut f = File::open(&part)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            ctx.check_cancelled()?;
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            ctx.heartbeat();
        }
    }

    let mut req = agent().get(&p.url);
    if have > 0 {
        req = req.header("Range", &format!("bytes={have}-"));
    }
    let mut resp = req.call().map_err(net_err)?;
    let status = resp.status().as_u16();
    if status == 416 && p.size == Some(have) {
        // Already complete; fall through to verification.
    } else if status == 200 && have > 0 {
        ctx.log("The server ignored the resume request, starting over");
        have = 0;
        hasher = Sha256::new();
    } else if status != 200 && status != 206 && !(status == 416) {
        return Err(status_err(status, "Download"));
    }

    let remaining: u64 = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
    let total = p.size.unwrap_or(have + remaining).max(have + remaining);
    ctx.log(format!("{name}: {:.2} GB{}", total as f64 / 1e9, if have > 0 { format!(", resuming at {:.2} GB", have as f64 / 1e9) } else { String::new() }));
    ctx.stage("Downloading");

    if status != 416 {
        let mut out = OpenOptions::new().create(true).write(true).append(have > 0).truncate(have == 0).open(&part)?;
        let mut body = resp.body_mut().as_reader();
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = have;
        let mut last = std::time::Instant::now();
        loop {
            ctx.check_cancelled()?;
            let n = body.read(&mut buf).map_err(|e| FleetError::transient(format!("Download interrupted: {e}")))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| {
                if e.kind() == std::io::ErrorKind::StorageFull { FleetError::permanent("The disk is full") } else { e.into() }
            })?;
            hasher.update(&buf[..n]);
            done += n as u64;
            if last.elapsed() > Duration::from_millis(400) {
                if total > 0 {
                    ctx.progress(done as f32 / total as f32 * 100.0);
                }
                ctx.heartbeat();
                last = std::time::Instant::now();
            }
        }
        out.flush()?;
        if total > 0 && done < total {
            return Err(FleetError::transient(format!("The download ended early ({:.2} of {:.2} GB); it will resume", done as f64 / 1e9, total as f64 / 1e9)));
        }
    }

    if let Some(expected) = &p.sha256 {
        ctx.stage("Verifying checksum");
        let got = hex::encode(hasher.finalize());
        if !got.eq_ignore_ascii_case(expected) {
            let _ = std::fs::remove_file(&part);
            return Err(FleetError::transient("The file failed its checksum and was deleted; downloading it again"));
        }
        ctx.log("Checksum OK");
    }
    std::fs::rename(&part, &final_path)?;
    ctx.progress(100.0);
    ctx.stage("In the library");
    Ok(())
}

/// Group helper for the UI: which catalog builds are already local.
pub fn local_builds(lib: &[LocalIpsw]) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for i in lib {
        if let Some(b) = &i.build {
            m.entry(b.clone()).or_default().extend(i.product_types.clone());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        for ok in ["iPad13,18", "iPhone15,2", "iPod9,1", "AppleTV11,1"] {
            assert!(valid_identifier(ok), "{ok}");
        }
        for bad in ["", "iPad", "iPad13", "../etc,1", "iPad13,1a", "iPad 13,1", "iPad13,"] {
            assert!(!valid_identifier(bad), "{bad}");
        }
    }

    #[test]
    fn url_allowlist() {
        let ok = "https://updates.cdn-apple.com/2026FallFCS/x/iPad_Fall_2022_27.0.1_24A446_Restore.ipsw";
        assert_eq!(check_download_url(ok).unwrap(), "iPad_Fall_2022_27.0.1_24A446_Restore.ipsw");
        for bad in ["http://updates.cdn-apple.com/a.ipsw", "https://evil.example.com/a.ipsw", "https://updates.cdn-apple.com/a.zip", "https://updates.cdn-apple.com.evil.io/a.ipsw", "https://updates.cdn-apple.com/x/..%2f.ipsw"] {
            assert!(check_download_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn catalog_is_sorted_newest_first() {
        let json = r#"{"name":"iPad","identifier":"iPad13,18","firmwares":[
          {"version":"26.6.2","buildid":"A","url":"https://updates.cdn-apple.com/a.ipsw","filesize":1,"sha256sum":"","signed":false},
          {"version":"27.0.1","buildid":"B","url":"https://updates.cdn-apple.com/b.ipsw","filesize":2,"sha256sum":"72e697e0c4963c2d0ca99635b4b28a20fab86e445f1779397acb2b540f162896","signed":true},
          {"version":"27.0","buildid":"C","url":"https://updates.cdn-apple.com/c.ipsw","filesize":3,"signed":false}]}"#;
        let c = parse_catalog(json, "iPad13,18").unwrap();
        assert_eq!(c.firmwares.iter().map(|f| f.version.as_str()).collect::<Vec<_>>(), ["27.0.1", "27.0", "26.6.2"]);
        assert!(c.firmwares[0].signed && c.firmwares[0].sha256.is_some());
        assert!(c.firmwares[2].sha256.is_none(), "empty checksum is dropped");
    }

    #[test]
    fn library_reads_manifests_and_flags_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut z = zip::ZipWriter::new(File::create(dir.path().join("ok.ipsw")).unwrap());
        z.start_file("BuildManifest.plist", zip::write::SimpleFileOptions::default()).unwrap();
        let mut d = plist::Dictionary::new();
        d.insert("ProductVersion".into(), "27.0.1".into());
        d.insert("ProductBuildVersion".into(), "24A446".into());
        d.insert("SupportedProductTypes".into(), plist::Value::Array(vec!["iPad13,18".into()]));
        let mut buf = Vec::new();
        plist::to_writer_xml(&mut buf, &plist::Value::Dictionary(d)).unwrap();
        z.write_all(&buf).unwrap();
        z.finish().unwrap();
        std::fs::write(dir.path().join("broken.ipsw"), b"nope").unwrap();
        std::fs::write(dir.path().join("note.txt"), b"x").unwrap();
        let lib = scan_library(dir.path());
        assert_eq!(lib.len(), 2);
        assert_eq!(lib[0].version.as_deref(), Some("27.0.1"));
        assert!(lib[1].error.is_some());
        assert_eq!(matching(&lib, "iPad13,18").len(), 1);
        assert!(matching(&lib, "iPhone15,2").is_empty());
    }

    /// Hits the real ipsw.me API: `cargo test -p fleet-core -- --ignored`.
    #[test]
    #[ignore]
    fn live_catalog_lookup() {
        let c = fetch_catalog("iPad13,18").unwrap();
        assert!(c.firmwares.iter().any(|f| f.signed), "at least one signed version");
        assert!(c.firmwares.iter().all(|f| check_download_url(&f.url).is_ok()), "all URLs pass the allowlist");
        assert!(fetch_catalog("iPad99,99").is_err());
    }
}
