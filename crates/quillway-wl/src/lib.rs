//! Clipboard I/O over wlr/ext-data-control: needs no keyboard focus, so the
//! daemon can read the clipboard before mapping the popup.

mod watch;

use std::io::Read;

use anyhow::Context;
pub use watch::ClipboardWatch;
use wl_clipboard_rs::copy::{self, Options};
use wl_clipboard_rs::paste::{self, ClipboardType, Error, MimeType, Seat};

/// Cap on captured text; larger contents aren't rewrite material.
const MAX_BYTES: u64 = 1 << 20;

/// Text on the clipboard; `Ok(None)` when it is empty or non-text.
pub fn read() -> anyhow::Result<Option<String>> {
    match paste::get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text) {
        Ok((pipe, _mime)) => {
            let mut buf = Vec::new();
            pipe.take(MAX_BYTES).read_to_end(&mut buf).context("reading the clipboard")?;
            let text = String::from_utf8_lossy(&buf).into_owned();
            Ok((!text.trim().is_empty()).then_some(text))
        }
        Err(Error::NoSeats | Error::ClipboardEmpty | Error::NoMimeType) => Ok(None),
        Err(e) => Err(e).context("reading the Wayland clipboard (does the compositor support data-control?)"),
    }
}

/// Put `text` on the clipboard. A background thread keeps serving it until
/// something else is copied, so the caller must stay alive (the daemon does).
pub fn copy(text: &str) -> anyhow::Result<()> {
    Options::new()
        .copy(copy::Source::Bytes(text.as_bytes().into()), copy::MimeType::Text)
        .context("setting the Wayland clipboard")
}
