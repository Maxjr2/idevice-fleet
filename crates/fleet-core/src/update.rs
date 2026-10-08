//! "Is there a newer version?" Looks at the latest GitHub release of this project.
//! Nothing is installed automatically: the app shows the news and a download link.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{FleetError, Result};

pub const RELEASES_API: &str = "https://api.github.com/repos/Maxjr2/idevice-fleet/releases/latest";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub page_url: String,
    /// The `.deb` attached to the release, if there is one.
    pub deb_url: Option<String>,
    /// `SHA256SUMS` attached to the release, used to verify the download.
    #[serde(default)]
    pub sums_url: Option<String>,
}

/// Release downloads may only come from GitHub.
pub fn check_update_url(url: &str) -> Result<String> {
    let rest = url.strip_prefix("https://").ok_or_else(|| FleetError::permanent("Updates must be downloaded over https"))?;
    let (host, path) = rest.split_once('/').ok_or_else(|| FleetError::permanent("Bad update URL"))?;
    let ok_host = host == "github.com" || host.ends_with(".githubusercontent.com");
    let name = path.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
    if !ok_host || !name.starts_with("idevice-fleet_") || !name.ends_with(".deb") || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-+~".contains(c)) {
        return Err(FleetError::permanent("That isn't an iDevice Fleet release file on GitHub"));
    }
    Ok(name.to_string())
}

/// Find a file's SHA-256 in `SHA256SUMS` text (`<hash>  <name>` per line).
pub fn checksum_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (h, n) = l.split_once(char::is_whitespace)?;
        (n.trim().trim_start_matches('*') == file && h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).then(|| h.to_ascii_lowercase())
    })
}

/// A `.deb` that is safe to hand to the package installer: inside `dir`, our name pattern.
pub fn installable_deb(dir: &std::path::Path, file: &str) -> Result<std::path::PathBuf> {
    let name = std::path::Path::new(file).file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !name.starts_with("idevice-fleet_") || !name.ends_with(".deb") {
        return Err(FleetError::permanent("Not an iDevice Fleet package"));
    }
    let path = dir.join(name);
    if !path.is_file() {
        return Err(FleetError::permanent("The update file isn't downloaded yet"));
    }
    Ok(path)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum UpdateState {
    #[default]
    Unknown,
    Checking,
    UpToDate,
    Available(Box<UpdateInfo>),
    Failed(String),
}

/// Compare dotted numeric versions ("0.10.1" > "0.9.9"). A pre-release ("1.0.0-rc1") is older than its release.
pub fn is_newer(latest: &str, current: &str) -> bool {
    fn parse(v: &str) -> (Vec<u64>, bool) {
        let v = v.trim().trim_start_matches('v');
        let (core, pre) = match v.split_once('-') {
            Some((c, _)) => (c, true),
            None => (v, false),
        };
        (core.split('.').map(|p| p.parse().unwrap_or(0)).collect(), pre)
    }
    let (mut a, ap) = parse(latest);
    let (mut b, bp) = parse(current);
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    match a.cmp(&b) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => bp && !ap,
    }
}

pub fn parse_release(json: &str) -> Result<UpdateInfo> {
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
        #[serde(default)]
        body: Option<String>,
        html_url: String,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        assets: Vec<Asset>,
    }
    let r: Release = serde_json::from_str(json).map_err(|e| FleetError::transient(format!("Unreadable answer from GitHub: {e}")))?;
    if r.draft {
        return Err(FleetError::transient("Latest release is a draft"));
    }
    let deb = r.assets.iter().find(|a| a.name.ends_with(".deb") && a.name.contains("amd64")).map(|a| a.browser_download_url.clone());
    let sums = r.assets.iter().find(|a| a.name == "SHA256SUMS").map(|a| a.browser_download_url.clone());
    // Only ever point the person at github.com.
    let safe = |u: &str| u.starts_with("https://github.com/");
    Ok(UpdateInfo {
        version: r.tag_name.trim_start_matches('v').to_string(),
        notes: r.body.unwrap_or_default().trim().to_string(),
        page_url: if safe(&r.html_url) { r.html_url } else { "https://github.com/Maxjr2/idevice-fleet/releases".into() },
        deb_url: deb.filter(|u| safe(u)),
        sums_url: sums.filter(|u| safe(u)),
    })
}

/// Blocking; run it from `spawn_blocking`.
pub fn check(current: &str) -> Result<Option<UpdateInfo>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .user_agent(format!("idevice-fleet/{current}"))
        .build()
        .into();
    let mut resp = agent.get(RELEASES_API).header("Accept", "application/vnd.github+json").call().map_err(|e| FleetError::transient(format!("Couldn't reach GitHub: {e}")))?;
    match resp.status().as_u16() {
        200 => {}
        404 => return Ok(None), // no release published yet
        403 | 429 => return Err(FleetError::transient("GitHub is limiting requests right now. Try again later")),
        s => return Err(FleetError::transient(format!("GitHub answered HTTP {s}"))),
    }
    let body = resp.body_mut().read_to_string().map_err(|e| FleetError::transient(format!("Couldn't read GitHub's answer: {e}")))?;
    let info = parse_release(&body)?;
    Ok(is_newer(&info.version, current).then_some(info))
}

/// Run `pkexec apt-get install ./file.deb` and report its output as job log lines.
pub async fn run_installer(ctx: &crate::jobs::JobContext, deb: &std::path::Path) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let program = std::env::var("IDEVICE_FLEET_INSTALLER").unwrap_or_else(|_| "pkexec".into());
    ctx.stage("Waiting for your password");
    ctx.log(format!("$ {program} apt-get install -y {}", deb.display()));
    let mut child = tokio::process::Command::new(&program)
        .args(["apt-get", "install", "-y", "--allow-downgrades"])
        .arg(deb)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| FleetError::permanent(format!("Couldn't start the installer ({program}): {e}. Install it by hand: sudo apt install {}", deb.display())))?;
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut err = BufReader::new(child.stderr.take().expect("piped")).lines();
    let cancel = ctx.cancellation();
    loop {
        tokio::select! {
            l = lines.next_line() => match l { Ok(Some(l)) => { ctx.heartbeat(); ctx.stage_quiet("Installing"); ctx.log(l) }, _ => break },
            l = err.next_line() => if let Ok(Some(l)) = l { ctx.heartbeat(); ctx.log(l) },
            _ = cancel.cancelled() => { let _ = child.start_kill(); return Err(FleetError::cancelled()); }
        }
    }
    let status = child.wait().await.map_err(|e| FleetError::permanent(e.to_string()))?;
    match status.code() {
        Some(0) => {
            ctx.stage("Installed. Restart iDevice Fleet to use the new version");
            Ok(())
        }
        Some(126) | Some(127) => Err(FleetError::needs_user("The password prompt was cancelled or isn't allowed. Install it by hand with the command shown in the log")),
        c => Err(FleetError::permanent(format!("The installer stopped (code {}). The log has the details", c.map(|c| c.to_string()).unwrap_or_else(|| "?".into())))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.2.0"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("1.0.0", "1.0.0-rc1"), "a release is newer than its pre-release");
        assert!(!is_newer("1.0.0-rc1", "1.0.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("0.2", "0.2.0"), "missing parts count as zero");
    }

    #[test]
    fn reads_a_release() {
        let json = r#"{"tag_name":"v0.3.0","html_url":"https://github.com/Maxjr2/idevice-fleet/releases/tag/v0.3.0","body":"  Faster restores.\n ","draft":false,
          "assets":[{"name":"idevice-fleet_0.3.0_amd64.deb","browser_download_url":"https://github.com/Maxjr2/idevice-fleet/releases/download/v0.3.0/idevice-fleet_0.3.0_amd64.deb"},
                    {"name":"SHA256SUMS","browser_download_url":"https://github.com/x/SHA256SUMS"}]}"#;
        let r = parse_release(json).unwrap();
        assert_eq!(r.version, "0.3.0");
        assert_eq!(r.notes, "Faster restores.");
        assert!(r.deb_url.unwrap().ends_with("amd64.deb"));
    }

    #[test]
    fn never_points_outside_github() {
        let json = r#"{"tag_name":"v9.0.0","html_url":"https://evil.example/x","assets":[{"name":"a_amd64.deb","browser_download_url":"https://evil.example/a_amd64.deb"}]}"#;
        let r = parse_release(json).unwrap();
        assert!(r.page_url.starts_with("https://github.com/"));
        assert!(r.deb_url.is_none());
    }

    #[test]
    fn update_downloads_only_from_github() {
        let ok = "https://github.com/Maxjr2/idevice-fleet/releases/download/v0.3.0/idevice-fleet_0.3.0_amd64.deb";
        assert_eq!(check_update_url(ok).unwrap(), "idevice-fleet_0.3.0_amd64.deb");
        assert!(check_update_url("https://release-assets.githubusercontent.com/x/idevice-fleet_0.3.0_amd64.deb").is_ok());
        for bad in ["http://github.com/a/idevice-fleet_1_amd64.deb", "https://evil.io/idevice-fleet_1_amd64.deb", "https://github.com.evil.io/idevice-fleet_1_amd64.deb", "https://github.com/a/other.deb", "https://github.com/a/idevice-fleet_1_amd64.deb;rm"] {
            assert!(check_update_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn finds_checksums() {
        let h = "a".repeat(64);
        let sums = format!("{h}  idevice-fleet_0.3.0_amd64.deb
{}  other
", "b".repeat(64));
        assert_eq!(checksum_for(&sums, "idevice-fleet_0.3.0_amd64.deb"), Some(h));
        assert_eq!(checksum_for(&sums, "missing"), None);
        assert_eq!(checksum_for("short  idevice-fleet_0.3.0_amd64.deb", "idevice-fleet_0.3.0_amd64.deb"), None);
    }

    #[test]
    fn only_our_packages_are_installable() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("idevice-fleet_0.3.0_amd64.deb"), b"x").unwrap();
        std::fs::write(d.path().join("evil.deb"), b"x").unwrap();
        assert!(installable_deb(d.path(), "idevice-fleet_0.3.0_amd64.deb").is_ok());
        assert!(installable_deb(d.path(), "evil.deb").is_err());
        assert!(installable_deb(d.path(), "../../etc/idevice-fleet_1.deb").is_err(), "paths are reduced to a name inside the folder");
        assert!(installable_deb(d.path(), "idevice-fleet_9.9.9_amd64.deb").is_err(), "not downloaded");
    }

    /// Hits the real GitHub API: `cargo test -p fleet-core -- --ignored`.
    #[test]
    #[ignore]
    fn live_check() {
        let r = check("0.0.1");
        assert!(r.is_ok(), "{r:?}");
    }
}
