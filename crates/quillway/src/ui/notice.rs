//! The popup's one message line: an error or a note, with optional details
//! (`docs/research/popup-error-display.md`). It sits under the text it is
//! about, takes no space when there is nothing to say, and stays until its
//! cause is gone: no timeouts.

use iced::widget::{Space, column, container, row, scrollable, text};
use iced::{Element, Length};

use super::Message;
use super::style::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Something to fix or a heads-up: "Nothing to rewrite", "Stopped by a reload".
    Info,
    /// Something failed: the model server, the clipboard, a download.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub severity: Severity,
    /// One sentence.
    pub summary: String,
    /// The rest, e.g. llama-server's log; shown with Ctrl+E.
    pub detail: Option<String>,
}

impl Notice {
    pub fn info(message: &str) -> Self {
        Self::new(Severity::Info, message)
    }

    pub fn error(message: &str) -> Self {
        Self::new(Severity::Error, message)
    }

    /// The first line is the summary; the rest is the detail.
    fn new(severity: Severity, message: &str) -> Self {
        let message = message.trim();
        let (summary, rest) = message.split_once('\n').unwrap_or((message, ""));
        let rest = rest.trim();
        // A trailing colon introduces the detail, which is shown apart (or is empty).
        let summary = capitalized(summary.trim().trim_end_matches(':'));
        Self { severity, summary, detail: (!rest.is_empty()).then(|| rest.to_owned()) }
    }
}

/// Error chains start in lower case ("starting llama-server: …"); the line reads as a sentence.
fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// The message line: a badge (not color alone), the summary, and the details if open.
pub fn strip<'a>(n: Notice, open: bool, pal: Palette) -> Element<'a, Message> {
    let (color, glyph) = match n.severity {
        Severity::Error => (pal.error, "!"),
        Severity::Info => (pal.dim, "i"),
    };
    let badge = container(text(glyph).size(11).color(pal.panel)).center(16).style(Palette::badge(color));
    let has_detail = n.detail.is_some();
    let mut line = row![badge, text(n.summary).size(13).color(pal.text).width(Length::Fill)].spacing(8);
    if has_detail {
        line = line.push(text(if open { "^E hide" } else { "^E details" }).size(12).color(pal.faint));
    }
    let mut col = column![line].spacing(6);
    if let (true, Some(d)) = (open, n.detail) {
        let detail = scrollable(row![Space::new().width(24), text(d).size(12).color(pal.dim)]);
        col = col.push(container(detail).max_height(120.0));
    }
    container(col).padding([6, 10]).width(Length::Fill).style(Palette::notice(color)).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_line_is_the_summary_and_the_rest_the_detail() {
        let n = Notice::error("starting llama-server: boom\n--- llama-server log (tail) ---\nline 1\n");
        assert_eq!(n.summary, "Starting llama-server: boom");
        assert_eq!(n.detail.as_deref(), Some("--- llama-server log (tail) ---\nline 1"));
        assert_eq!(Notice::info("Nothing to rewrite.").detail, None);
        let quiet = Notice::error("llama-server exited during startup (exit status: 1):\n");
        assert_eq!(
            (quiet.summary.as_str(), quiet.detail),
            ("Llama-server exited during startup (exit status: 1)", None)
        );
    }
}
