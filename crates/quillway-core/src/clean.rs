//! Strip chat-assistant wrapping that small models add around a rewrite.
//!
//! `clean` is applied to the whole accumulated output on every token, so it
//! must be idempotent on prefixes of the final text.

/// Raw characters to accumulate before showing anything, so a preamble line
/// can be recognised and dropped before it flashes on screen.
pub const DISPLAY_AFTER: usize = 60;

pub fn ready(raw: &str, finished: bool) -> bool {
    finished || raw.chars().count() >= DISPLAY_AFTER
}

const PREAMBLE_STARTS: [&str; 10] =
    ["sure", "here is", "here's", "certainly", "of course", "below is", "rewritten", "revised", "corrected", "okay"];

const OUTRO_STARTS: [&str; 5] = ["let me know", "i hope", "feel free", "hope this", "if you'd like"];

pub fn clean(raw: &str, input: &str, finished: bool) -> String {
    let mut s = strip_think(raw).to_owned();

    s = s.trim_start().to_owned();
    if let Some(rest) = s.strip_prefix("<text>") {
        s = rest.trim_start().to_owned();
    }
    if let Some(i) = s.find("</text>") {
        s.truncate(i);
    }

    // "Sure! Here's the rewritten text:" on its own line.
    if let Some((first, rest)) = s.split_once('\n') {
        let f = first.trim().to_lowercase();
        if f.len() < 90 && f.ends_with(':') && PREAMBLE_STARTS.iter().any(|p| f.starts_with(p)) {
            s = rest.trim_start().to_owned();
        }
    }

    if !input.trim_start().starts_with("```") && s.starts_with("```") {
        s = match s.split_once('\n') {
            Some((_, body)) => body.to_owned(),
            None => String::new(),
        };
        if let Some(i) = s.rfind("```") {
            s.truncate(i);
        }
    }

    if finished {
        if let Some(i) = s.trim_end().rfind("\n\n") {
            let last = s[i..].trim().to_lowercase();
            if OUTRO_STARTS.iter().any(|p| last.starts_with(p)) {
                s.truncate(i);
            }
        }
        s = strip_wrapping_quotes(s.trim_end(), input).to_owned();
        if input.ends_with('\n') {
            s.push('\n');
        }
    }
    s
}

/// Drop `<think>…</think>`; an unterminated block hides everything after it.
fn strip_think(s: &str) -> &str {
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix("<think>") {
        return match rest.find("</think>") {
            Some(i) => &rest[i + "</think>".len()..],
            None => "",
        };
    }
    s
}

fn strip_wrapping_quotes<'a>(s: &'a str, input: &str) -> &'a str {
    let input = input.trim();
    for (open, close) in [('"', '"'), ('“', '”'), ('\'', '\'')] {
        let quoted = |x: &str| x.starts_with(open) && x.ends_with(close) && x.chars().count() >= 2;
        if quoted(s) && !quoted(input) {
            let inner = &s[open.len_utf8()..s.len() - close.len_utf8()];
            if !inner.contains(open) && !inner.contains(close) {
                return inner;
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(raw: &str, input: &str) -> String {
        clean(raw, input, true)
    }

    #[test]
    fn passes_clean_output_through() {
        assert_eq!(done("They're here.", "their here"), "They're here.");
    }

    #[test]
    fn drops_preamble_line() {
        assert_eq!(done("Sure! Here's the rewritten text:\n\nThey're here.", "x"), "They're here.");
        assert_eq!(done("Here is the corrected version:\nA\nB", "x"), "A\nB");
    }

    #[test]
    fn keeps_a_first_line_that_is_content() {
        assert_eq!(done("Agenda:\n- one\n- two", "agenda:\n- one\n- two"), "Agenda:\n- one\n- two");
    }

    #[test]
    fn strips_think_blocks() {
        assert_eq!(done("<think>\n\n</think>\n\nHello.", "hello"), "Hello.");
        assert_eq!(clean("<think>still going", "x", false), "");
    }

    #[test]
    fn strips_fences_and_tags() {
        assert_eq!(done("```\nHello.\n```", "hello"), "Hello.");
        assert_eq!(done("```text\nHello.\n```", "hello"), "Hello.");
        assert_eq!(done("<text>\nHello.\n</text>", "hello"), "Hello.");
        assert_eq!(done("```rust\nfn x() {}\n```", "```rust\nfn x(){}\n```"), "```rust\nfn x() {}\n```");
    }

    #[test]
    fn strips_wrapping_quotes_only_when_input_unquoted() {
        assert_eq!(done("\"Hello.\"", "hello"), "Hello.");
        assert_eq!(done("“Hello.”", "hello"), "Hello.");
        assert_eq!(done("\"Hello.\"", "\"hello\""), "\"Hello.\"");
        assert_eq!(done("\"A\" and \"B\"", "a and b"), "\"A\" and \"B\"");
    }

    #[test]
    fn strips_outro_paragraph() {
        assert_eq!(done("Hello.\n\nLet me know if you need anything else!", "hi"), "Hello.");
        assert_eq!(done("Hello.\n\nWorld.", "hi\n\nworld"), "Hello.\n\nWorld.");
    }

    #[test]
    fn mirrors_trailing_newline() {
        assert_eq!(done("Hello.", "hello\n"), "Hello.\n");
    }

    #[test]
    fn streaming_prefixes_are_stable() {
        let full = "Sure, here is the text:\nThey're going to the park tomorrow, weather permitting.";
        let final_out = done(full, "x");
        // Once past the display threshold every streamed view is a prefix of the final one.
        for end in (DISPLAY_AFTER..=full.len()).filter(|&i| full.is_char_boundary(i)) {
            let partial = clean(&full[..end], "x", false);
            assert!(final_out.starts_with(partial.trim_end()), "{partial:?}");
        }
    }

    #[test]
    fn ready_threshold() {
        assert!(!ready("short", false));
        assert!(ready("short", true));
        assert!(ready(&"x".repeat(DISPLAY_AFTER), false));
    }
}
