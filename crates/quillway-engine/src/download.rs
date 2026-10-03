//! Resumable, sha256-verified model downloads.
//!
//! `<dest>.part` grows with `Range` requests; the hash covers the bytes already
//! on disk, and the file is renamed into place only after the digest matches.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bytes on disk so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes downloaded, including any resumed part.
    pub done: u64,
    /// Expected size.
    pub total: u64,
}

/// One file to fetch.
pub struct Job<'a> {
    /// Source URL.
    pub url: &'a str,
    /// Final path; `<dest>.part` holds the download until it is verified.
    pub dest: &'a Path,
    /// Expected size in bytes.
    pub size: u64,
    /// Expected sha256, hex.
    pub sha256: &'a str,
}

/// `dest` with `suffix` appended to its file name.
fn sibling(dest: &Path, suffix: &str) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

fn part_path(dest: &Path) -> PathBuf {
    sibling(dest, ".part")
}

fn lock_path(dest: &Path) -> PathBuf {
    sibling(dest, ".lock")
}

/// Download `job`, resuming a previous `.part`, and verify it.
///
/// # Errors
///
/// Network or disk failure (the `.part` is kept for resuming), not enough
/// free space, a size/sha256 mismatch (the `.part` is deleted), or another
/// download of the same file already running.
pub async fn download(job: Job<'_>, mut on_progress: impl FnMut(Progress)) -> anyhow::Result<()> {
    let dir = job.dest.parent().context("destination has no parent directory")?;
    tokio::fs::create_dir_all(dir).await?;
    // Held until we return, so a CLI and a popup install can't both append to the `.part`.
    let _lock = lock(job.dest)?;
    if job.dest.is_file() {
        on_progress(Progress { done: job.size, total: job.size });
        return Ok(());
    }
    let part = part_path(job.dest);

    let mut hasher = Sha256::new();
    let mut have = hash_existing(&part, &mut hasher).await?;
    if have > job.size {
        tokio::fs::remove_file(&part).await?;
        hasher = Sha256::new();
        have = 0;
    }
    ensure_space(dir, job.size - have)?;
    on_progress(Progress { done: have, total: job.size });

    if have < job.size {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // A stalled connection fails (keeping the `.part`) instead of hanging.
            .read_timeout(Duration::from_secs(60))
            .build()?;
        let mut req = http.get(job.url);
        if let Ok(token) = std::env::var("HF_TOKEN") {
            req = req.bearer_auth(token);
        }
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let resp = req.send().await.with_context(|| format!("GET {}", job.url))?;
        let status = resp.status();
        if have > 0 && status == reqwest::StatusCode::OK {
            // Server ignored the range: start over.
            hasher = Sha256::new();
            have = 0;
        } else if !status.is_success() {
            bail!("GET {}: {status}", job.url);
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(have > 0)
            .truncate(have == 0)
            .open(&part)
            .await?;
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.context("download interrupted; run the install again to resume")?;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            have += chunk.len() as u64;
            on_progress(Progress { done: have, total: job.size });
        }
        file.sync_all().await?;
    }

    if have < job.size {
        bail!("the download ended early at {have} of {} bytes; run the install again to resume", job.size);
    }
    if have != job.size {
        bail!("size mismatch: got {have} bytes, expected {}", job.size);
    }
    let digest = format!("{:x}", hasher.finalize());
    if !digest.eq_ignore_ascii_case(job.sha256) {
        tokio::fs::remove_file(&part).await.ok();
        bail!("sha256 mismatch (got {digest}); the partial file was removed");
    }
    tokio::fs::rename(&part, job.dest).await?;
    Ok(())
}

/// Delete `dest` and its partial download; `Ok(false)` if it wasn't installed.
/// The lock file stays so a downloader that already opened it keeps coordinating
/// with new downloaders.
///
/// # Errors
///
/// A download of it is running, or a file can't be removed.
pub fn remove(dest: &Path) -> anyhow::Result<bool> {
    if !dest.parent().is_some_and(Path::is_dir) {
        return Ok(false);
    }
    let held = lock(dest)?;
    let removed = remove_if_present(dest)?;
    remove_if_present(&part_path(dest))?;
    drop(held);
    Ok(removed)
}

fn remove_if_present(path: &Path) -> anyhow::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// An exclusive `flock` (what std uses on Linux), held until the file is dropped.
fn lock(dest: &Path) -> anyhow::Result<std::fs::File> {
    let path = lock_path(dest);
    let f = std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => bail!("another download of {} is already running", dest.display()),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

async fn hash_existing(part: &Path, hasher: &mut Sha256) -> anyhow::Result<u64> {
    let mut f = match tokio::fs::File::open(part).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let read = f.read(&mut buf).await?;
        if read == 0 {
            return Ok(n);
        }
        hasher.update(&buf[..read]);
        n += read as u64;
    }
}

fn ensure_space(dir: &Path, need: u64) -> anyhow::Result<()> {
    let free = free_bytes(dir)?;
    // 5% headroom so we never fill the disk to the last byte.
    if free < need + need / 20 {
        bail!("not enough disk space in {}: need {}, have {}", dir.display(), human(need), human(free));
    }
    Ok(())
}

fn free_bytes(dir: &Path) -> anyhow::Result<u64> {
    let s = rustix::fs::statvfs(dir).with_context(|| format!("statvfs {}", dir.display()))?;
    Ok(s.f_bavail * s.f_frsize)
}

/// `1.3 GB` / `731.0 MB` / `420.0 kB`: decimal (SI) units, as model sizes are published.
#[must_use]
pub fn human(bytes: u64) -> String {
    bytesize::ByteSize(bytes).display().si().to_string()
}

/// How often a progress display redraws: often enough to look alive on a slow
/// link, rarely enough not to flood a terminal or the UI.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Download speed and time left from indicatif's estimator (on a hidden bar),
/// so the CLI line and the popup card show the same numbers.
#[derive(Debug, Clone, Default)]
pub struct Rate(Option<indicatif::ProgressBar>);

impl Rate {
    /// Note the bytes done so far.
    pub fn update(&mut self, done: u64, total: u64) {
        if let Some(bar) = &self.0 {
            // A position going backwards (the server ignored `Range`) resets the estimate.
            bar.set_position(done);
            return;
        }
        // The first update is the baseline: bytes resumed from a `.part` aren't speed.
        let bar = indicatif::ProgressBar::hidden();
        bar.set_length(total);
        bar.set_position(done);
        bar.tick();
        bar.reset_eta();
        self.0 = Some(bar);
    }

    /// `45.0 MB/s, 2m left`; `None` until a second of transfer has been measured.
    #[must_use]
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "whole bytes per second")]
    pub fn describe(&self) -> Option<String> {
        let bar = self.0.as_ref().filter(|b| b.elapsed() >= Duration::from_secs(1))?;
        let speed = bar.per_sec();
        (speed >= 1.0).then(|| format!("{}/s, {:#} left", human(speed as u64), indicatif::HumanDuration(bar.eta())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers with the bytes from the `Range: bytes=N-` offset on, like a CDN.
    struct Ranged(Vec<u8>);

    impl wiremock::Respond for Ranged {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let start = request
                .headers
                .get("range")
                .and_then(|v| v.to_str().ok()?.strip_prefix("bytes=")?.trim_end_matches('-').parse().ok())
                .unwrap_or(0);
            wiremock::ResponseTemplate::new(if start > 0 { 206 } else { 200 }).set_body_bytes(&self.0[start..])
        }
    }

    /// Serve `data`; the server stops when the returned handle is dropped.
    async fn serve(data: Vec<u8>) -> (String, wiremock::MockServer) {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/model.gguf")).respond_with(Ranged(data)).mount(&server).await;
        (format!("{}/model.gguf", server.uri()), server)
    }

    fn sha(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    #[test]
    fn sizes_use_decimal_units() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(731_000_000), "731.0 MB");
        assert_eq!(human(2_740_000_000), "2.7 GB");
    }

    #[test]
    fn rate_does_not_count_resumed_bytes() {
        let mut rate = Rate::default();
        rate.update(1_000_000_000, 2_000_000_000); // resumed a 1 GB `.part`
        assert_eq!(rate.describe(), None, "nothing measured yet");
        std::thread::sleep(Duration::from_millis(1100));
        rate.update(1_001_000_000, 2_000_000_000); // about 1 MB/s since the resume
        rate.0.as_ref().unwrap().tick();
        let text = rate.describe().expect("a second was measured");
        assert!(text.ends_with(" left"), "{text}");
        let speed: f64 = text.split(' ').next().unwrap().parse().unwrap();
        assert!(text.contains(" kB/s") || (text.contains(" MB/s") && speed < 2.0), "resumed bytes counted: {text}");
    }

    #[tokio::test]
    async fn downloads_and_verifies() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let (url, _server) = serve(data.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a/b/model.gguf");
        let mut last = None;
        download(Job { url: &url, dest: &dest, size: data.len() as u64, sha256: &sha(&data) }, |p| last = Some(p))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(last, Some(Progress { done: data.len() as u64, total: data.len() as u64 }));
    }

    #[tokio::test]
    async fn resumes_from_partial_file() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 13) as u8).collect();
        let (url, _server) = serve(data.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        std::fs::write(part_path(&dest), &data[..40_000]).unwrap();
        let mut first = None;
        download(Job { url: &url, dest: &dest, size: data.len() as u64, sha256: &sha(&data) }, |p| {
            first.get_or_insert(p);
        })
        .await
        .unwrap();
        assert_eq!(first.unwrap().done, 40_000);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
    }

    #[tokio::test]
    async fn rejects_corrupt_download() {
        let data = vec![7u8; 5000];
        let (url, _server) = serve(data.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let err =
            download(Job { url: &url, dest: &dest, size: 5000, sha256: &sha(b"other") }, |_| {}).await.unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
        assert!(!dest.exists() && !part_path(&dest).exists());
    }

    #[test]
    fn remove_deletes_model_and_partial_but_respects_a_running_download() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        std::fs::write(&dest, b"m").unwrap();
        std::fs::write(part_path(&dest), b"p").unwrap();
        {
            let _running = lock(&dest).unwrap();
            assert!(remove(&dest).unwrap_err().to_string().contains("already running"));
        }
        assert!(remove(&dest).unwrap());
        assert!(!dest.exists() && !part_path(&dest).exists());
        assert!(lock_path(&dest).exists());
        assert!(!remove(&dest).unwrap());
    }

    #[test]
    fn removal_keeps_the_lock_identity_for_a_waiting_downloader() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        std::fs::write(&dest, b"model").unwrap();
        let waiting = std::fs::File::create(lock_path(&dest)).unwrap();

        assert!(remove(&dest).unwrap());
        // This descriptor represents a downloader that opened the lock before removal.
        // Blocking: a test forking in parallel can briefly hold a copy of `remove`'s lock fd.
        waiting.lock().unwrap();
        assert!(lock(&dest).unwrap_err().to_string().contains("already running"));
    }

    #[tokio::test]
    async fn refuses_concurrent_download() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let _held = lock(&dest).unwrap();
        let err = download(Job { url: "http://127.0.0.1:1/x", dest: &dest, size: 1, sha256: "00" }, |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already running"), "{err}");
    }
}
