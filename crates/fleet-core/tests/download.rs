use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fleet_core::firmware::{DownloadParams, download_unchecked};
use fleet_core::jobs::RunnerFuture;
use fleet_core::store::Store;
use fleet_core::{JobContext, JobEngine, JobKind, JobSpec, JobState, Limits, RetryPolicy};
use sha2::{Digest, Sha256};

/// Tiny HTTP server: serves `data`, honours Range, and can cut the first
/// response short to simulate a dropped connection.
fn serve(data: Arc<Vec<u8>>, cut_first_at: Option<usize>, cut_all: bool) -> (String, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/fw.ipsw", l.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        for stream in l.incoming().flatten() {
            let n = h.fetch_add(1, Ordering::SeqCst);
            let data = data.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut start = 0usize;
                let mut line = String::new();
                while r.read_line(&mut line).is_ok() && line.trim() != "" {
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        start = v.trim().trim_end_matches('-').parse().unwrap_or(0);
                    }
                    line.clear();
                }
                let mut s = stream;
                let body = &data[start..];
                let (status, extra) = if start > 0 { ("206 Partial Content", format!("Content-Range: bytes {start}-{}/{}\r\n", data.len() - 1, data.len())) } else { ("200 OK", String::new()) };
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n", body.len());
                let send = if n == 0 || cut_all { cut_first_at.map(|c| c.min(body.len())).unwrap_or(body.len()) } else { body.len() };
                let _ = s.write_all(&body[..send]);
                let _ = s.flush();
                if send < body.len() {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                    return;
                }
                let mut sink = [0u8; 1];
                let _ = s.read(&mut sink);
            });
        }
    });
    (url, hits)
}

fn run_download(dir: &std::path::Path, url: String, sha: Option<String>, size: u64, attempts: u32) -> (JobState, Option<String>) {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    rt.block_on(async {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let e = JobEngine::new(store, dir.join("logs"), Limits::default()).unwrap();
        e.register(JobKind::Download, |ctx: JobContext| {
            Box::pin(async move {
                let p: DownloadParams = ctx.params()?;
                tokio::task::spawn_blocking(move || download_unchecked(&ctx, &p, "fw.ipsw")).await.unwrap()
            }) as RunnerFuture
        });
        let policy = RetryPolicy { max_attempts: attempts, base_delay_ms: 10, max_delay_ms: 10, stall_timeout_s: 30, attempt_timeout_s: None };
        let spec = JobSpec::new(JobKind::Download, "dl", None, policy).with_params(DownloadParams { url, dest_dir: dir.to_path_buf(), sha256: sha, size: Some(size) });
        let id = e.submit(spec).unwrap();
        e.wait_idle().await;
        let v = e.get(&id).unwrap();
        (v.state, v.error.map(|e| e.message))
    })
}

fn sample(n: usize) -> Arc<Vec<u8>> {
    Arc::new((0..n).map(|i| (i * 31 % 251) as u8).collect())
}

#[test]
fn resumes_after_a_dropped_connection_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let data = sample(3_000_000);
    let sha = hex::encode(Sha256::digest(&*data));
    let (url, hits) = serve(data.clone(), Some(1_200_000), false);
    let (state, err) = run_download(dir.path(), url, Some(sha), data.len() as u64, 3);
    assert_eq!(state, JobState::Succeeded, "{err:?}");
    assert_eq!(std::fs::read(dir.path().join("fw.ipsw")).unwrap(), *data);
    assert!(!dir.path().join("fw.ipsw.part").exists());
    assert!(hits.load(Ordering::SeqCst) >= 2, "the second request resumed the transfer");
}

#[test]
fn bad_checksum_is_rejected_and_nothing_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let data = sample(500_000);
    let (url, _) = serve(data.clone(), None, false);
    let (state, err) = run_download(dir.path(), url, Some("0".repeat(64)), data.len() as u64, 1);
    assert_eq!(state, JobState::Failed);
    assert!(err.unwrap().contains("checksum"));
    assert!(!dir.path().join("fw.ipsw").exists());
    assert!(!dir.path().join("fw.ipsw.part").exists());
}

#[test]
fn continues_from_an_existing_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = sample(2_000_000);
    std::fs::write(dir.path().join("fw.ipsw.part"), &data[..700_000]).unwrap();
    let sha = hex::encode(Sha256::digest(&*data));
    let (url, _) = serve(data.clone(), None, false);
    let (state, err) = run_download(dir.path(), url, Some(sha), data.len() as u64, 1);
    assert_eq!(state, JobState::Succeeded, "{err:?}");
    assert_eq!(std::fs::read(dir.path().join("fw.ipsw")).unwrap(), *data);
}

#[test]
fn survives_a_server_that_drops_every_connection() {
    let dir = tempfile::tempdir().unwrap();
    let data = sample(3_000_000);
    let sha = hex::encode(Sha256::digest(&*data));
    let (url, hits) = serve(data.clone(), Some(400_000), true);
    // One job attempt is enough: the download reconnects by itself.
    let (state, err) = run_download(dir.path(), url, Some(sha), data.len() as u64, 1);
    assert_eq!(state, JobState::Succeeded, "{err:?}");
    assert_eq!(std::fs::read(dir.path().join("fw.ipsw")).unwrap(), *data);
    assert!(hits.load(Ordering::SeqCst) >= 7, "resumed many times: {}", hits.load(Ordering::SeqCst));
}
