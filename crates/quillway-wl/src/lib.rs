//! Clipboard I/O over wlr/ext-data-control: needs no keyboard focus, so the
//! daemon can read the clipboard before mapping the popup.

mod watch;

use std::io::Read;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};

use anyhow::Context;
pub use watch::ClipboardWatch;
use wl_clipboard_rs::copy::{self, Options};
use wl_clipboard_rs::paste::{self, ClipboardType, Error, MimeType, Seat};

/// Cap on text taken from the clipboard (and, in the CLI, from `--stdin`);
/// larger contents aren't rewrite material.
pub const MAX_BYTES: u64 = 1 << 20;

/// The clipboard holds more text than [`MAX_BYTES`].
#[derive(Debug)]
pub struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("clipboard text is larger than the 1 MiB limit")
    }
}

impl std::error::Error for TooLarge {}

/// How long the app that owns the clipboard has to send its text.
pub const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The app that owns the clipboard didn't send its text within [`READ_TIMEOUT`].
#[derive(Debug)]
pub struct NoAnswer;

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the app that owns the clipboard didn't respond")
    }
}

impl std::error::Error for NoAnswer {}

/// Text on the clipboard; `Ok(None)` when it is empty or non-text.
///
/// # Errors
///
/// The compositor can't be reached or lacks data-control.
pub fn read() -> anyhow::Result<Option<String>> {
    match paste::get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text) {
        Ok((pipe, _mime)) => read_text(Deadline { inner: pipe, until: Instant::now() + READ_TIMEOUT }),
        Err(Error::NoSeats | Error::ClipboardEmpty | Error::NoMimeType) => Ok(None),
        Err(e) => Err(e).context("reading the Wayland clipboard"),
    }
}

/// A pipe that fails with [`NoAnswer`] once `until` passes, so a hung clipboard
/// owner can't block the reading thread forever.
struct Deadline<R> {
    inner: R,
    until: Instant,
}

impl<R: Read + AsFd> Read for Deadline<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        let timeout =
            Timespec { tv_sec: left.as_secs().try_into().unwrap_or(i64::MAX), tv_nsec: left.subsec_nanos().into() };
        let mut fds = [PollFd::new(&self.inner, PollFlags::IN)];
        if left.is_zero() || rustix::event::poll(&mut fds, Some(&timeout))? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, NoAnswer));
        }
        self.inner.read(buf)
    }
}

fn read_text(pipe: impl Read) -> anyhow::Result<Option<String>> {
    let mut buf = Vec::new();
    pipe.take(MAX_BYTES + 1).read_to_end(&mut buf).context("reading the clipboard")?;
    if buf.len() as u64 > MAX_BYTES {
        return Err(TooLarge.into());
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok((!text.trim().is_empty()).then_some(text))
}

/// Put `text` on the clipboard. A background thread keeps serving it until
/// something else is copied, so the caller must stay alive (the daemon does).
///
/// # Errors
///
/// The compositor can't be reached or lacks data-control.
pub fn copy(text: &str) -> anyhow::Result<()> {
    Options::new()
        .copy(copy::Source::Bytes(text.as_bytes().into()), copy::MimeType::Text)
        .context("setting the Wayland clipboard")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clipboard_owner_that_never_sends_times_out() {
        let (pipe, _owner) = std::os::unix::net::UnixStream::pair().unwrap(); // open, silent
        let started = Instant::now();
        let error = read_text(Deadline { inner: pipe, until: started + Duration::from_millis(100) }).unwrap_err();
        assert!(error.chain().any(|e| e.is::<NoAnswer>() || e.to_string().contains("didn't respond")), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn clipboard_limit_never_returns_truncated_text() {
        let exact = vec![b'x'; usize::try_from(MAX_BYTES).unwrap()];
        assert_eq!(read_text(exact.as_slice()).unwrap().unwrap().len(), exact.len());
        let mut oversized = exact;
        oversized.push(b'y');
        let error = read_text(oversized.as_slice()).unwrap_err();
        assert!(error.to_string().contains("1 MiB limit"), "{error}");
    }
}
