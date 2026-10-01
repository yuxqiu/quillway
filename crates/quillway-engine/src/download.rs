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

fn part_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
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

    if have != job.size {
        bail!("size mismatch: got {have} bytes, expected {}", job.size);
    }
    let digest = hex(&hasher.finalize());
    if !digest.eq_ignore_ascii_case(job.sha256) {
        tokio::fs::remove_file(&part).await.ok();
        bail!("sha256 mismatch (got {digest}); the partial file was removed");
    }
    tokio::fs::rename(&part, job.dest).await?;
    Ok(())
}

/// Delete `dest` and its download leftovers; `Ok(false)` if it wasn't installed.
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
    remove_if_present(&lock_path(dest))?;
    Ok(removed)
}

fn remove_if_present(path: &Path) -> anyhow::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn lock_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

fn lock(dest: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let path = lock_path(dest);
    let f = std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    // SAFETY: `f` is an open file descriptor for the duration of the call.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another download of {} is already running", dest.display());
    }
    Ok(f)
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
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `s` a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &raw mut s) } != 0 {
        return Err(std::io::Error::last_os_error()).context("statvfs");
    }
    Ok(s.f_bavail as u64 * s.f_frsize as u64)
}

/// `1.3 GB` / `731 MB`.
#[must_use]
#[expect(clippy::cast_precision_loss, reason = "display only")]
pub fn human(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 { format!("{:.1} GB", b / 1e9) } else { format!("{:.0} MB", b / 1e6) }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve `data` over HTTP, honouring `Range: bytes=N-`.
    async fn serve(data: Vec<u8>) -> String {
        use tokio::io::AsyncBufReadExt;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/model.gguf", l.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (sock, _) = l.accept().await.unwrap();
                let data = data.clone();
                tokio::spawn(async move {
                    let (r, mut w) = sock.into_split();
                    let mut lines = tokio::io::BufReader::new(r).lines();
                    let mut start = 0usize;
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.is_empty() {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                            start = v.trim_end_matches('-').parse().unwrap();
                        }
                    }
                    let body = &data[start..];
                    let status = if start > 0 { "206 Partial Content" } else { "200 OK" };
                    let head =
                        format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    w.write_all(head.as_bytes()).await.unwrap();
                    w.write_all(body).await.unwrap();
                });
            }
        });
        url
    }

    fn sha(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    #[tokio::test]
    async fn downloads_and_verifies() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let url = serve(data.clone()).await;
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
        let url = serve(data.clone()).await;
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
        let url = serve(data.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let err =
            download(Job { url: &url, dest: &dest, size: 5000, sha256: &sha(b"other") }, |_| {}).await.unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
        assert!(!dest.exists() && !part_path(&dest).exists());
    }

    #[test]
    fn remove_cleans_up_and_respects_a_running_download() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        std::fs::write(&dest, b"m").unwrap();
        std::fs::write(part_path(&dest), b"p").unwrap();
        {
            let _running = lock(&dest).unwrap();
            assert!(remove(&dest).unwrap_err().to_string().contains("already running"));
        }
        assert!(remove(&dest).unwrap());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(!remove(&dest).unwrap());
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
