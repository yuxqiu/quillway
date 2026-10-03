//! Clipboard I/O over wlr/ext-data-control: needs no keyboard focus, so the
//! daemon can read the clipboard before mapping the popup.

mod watch;

use std::io::Read;

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

/// Text on the clipboard; `Ok(None)` when it is empty or non-text.
///
/// # Errors
///
/// The compositor can't be reached or lacks data-control.
pub fn read() -> anyhow::Result<Option<String>> {
    match paste::get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text) {
        Ok((pipe, _mime)) => read_text(pipe),
        Err(Error::NoSeats | Error::ClipboardEmpty | Error::NoMimeType) => Ok(None),
        Err(e) => Err(e).context("reading the Wayland clipboard (does the compositor support data-control?)"),
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
    fn clipboard_limit_never_returns_truncated_text() {
        let exact = vec![b'x'; usize::try_from(MAX_BYTES).unwrap()];
        assert_eq!(read_text(exact.as_slice()).unwrap().unwrap().len(), exact.len());
        let mut oversized = exact;
        oversized.push(b'y');
        let error = read_text(oversized.as_slice()).unwrap_err();
        assert!(error.to_string().contains("1 MiB limit"), "{error}");
    }
}
